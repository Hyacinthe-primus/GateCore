#include "NetworkManager.h"
#include "Config.h"
#include <WiFi.h>
#include <Preferences.h>
#include <esp_task_wdt.h>

namespace {
  const char* kPrefsNamespace = "wifi";
  const char* kKeySsid = "ssid";
  const char* kKeyPass = "pass";
  // Timezone lives in the same NVS namespace as the Wi-Fi creds: both are
  // runtime-provisioned settings owned by WifiTimeManager.
  const char* kKeyGmtOffset = "gmt_off";
  const char* kKeyDstOffset = "dst_off";
  const char* kKeyTzSet     = "tz_set";   // "1" once configure_timezone has run
}

void WifiTimeManager::loadCredentials_(String& ssidOut, String& passOut) {
  Preferences prefs;
  prefs.begin(kPrefsNamespace, true);
  ssidOut = prefs.getString(kKeySsid, "");
  passOut = prefs.getString(kKeyPass, "");
  prefs.end();
}

void WifiTimeManager::saveCredentials_(const String& ssid, const String& password) {
  Preferences prefs;
  prefs.begin(kPrefsNamespace, false);
  prefs.putString(kKeySsid, ssid);
  prefs.putString(kKeyPass, password);
  prefs.end();
}

void WifiTimeManager::loadTimezone_() {
  Preferences prefs;
  prefs.begin(kPrefsNamespace, true);
  bool tzSet = prefs.getString(kKeyTzSet, "") == "1";
  if (tzSet) {
    gmtOffsetSec_ = prefs.getLong(kKeyGmtOffset, NTP_GMT_OFFSET_SEC);
    daylightOffsetSec_ = prefs.getInt(kKeyDstOffset, NTP_DAYLIGHT_OFFSET_SEC);
  } else {
    gmtOffsetSec_ = NTP_GMT_OFFSET_SEC;
    daylightOffsetSec_ = NTP_DAYLIGHT_OFFSET_SEC;
  }
  prefs.end();
}

void WifiTimeManager::saveTimezone_(long gmtOffsetSec, int daylightOffsetSec) {
  Preferences prefs;
  prefs.begin(kPrefsNamespace, false);
  prefs.putLong(kKeyGmtOffset, gmtOffsetSec);
  prefs.putInt(kKeyDstOffset, daylightOffsetSec);
  prefs.putString(kKeyTzSet, "1");
  prefs.end();
}

bool WifiTimeManager::setTimezone(long gmtOffsetSec, int daylightOffsetSec) {
  long oldGmt = gmtOffsetSec_;
  int oldDst = daylightOffsetSec_;
  gmtOffsetSec_ = gmtOffsetSec;
  daylightOffsetSec_ = daylightOffsetSec;

  if (!isWifiConnected()) {
    // No network to verify against NTP yet -- persist anyway and apply the
    // timezone immediately so localtime() agrees with isUserExpired_().
    configTime(gmtOffsetSec_, daylightOffsetSec_, NTP_SERVER_1, NTP_SERVER_2);
    saveTimezone_(gmtOffsetSec_, daylightOffsetSec_);
    return true;
  }

  if (!syncTimeBlocking_(NTP_SYNC_TIMEOUT_MS)) {
    // Roll back in RAM so a bad offset doesn't leave isUserExpired_()
    // computing against a half-applied value until the caller retries.
    gmtOffsetSec_ = oldGmt;
    daylightOffsetSec_ = oldDst;
    return false;
  }

  saveTimezone_(gmtOffsetSec_, daylightOffsetSec_);
  return true;
}

bool WifiTimeManager::connectBlocking_(const String& ssid, const String& password, uint32_t timeoutMs) {
  if (ssid.length() == 0) return false;
  WiFi.mode(WIFI_STA);
  WiFi.begin(ssid.c_str(), password.c_str());
  uint32_t start = millis();
  while (WiFi.status() != WL_CONNECTED && (millis() - start) < timeoutMs) {
    delay(100);
    // Required: this loop runs after loopTask is subscribed to the TWDT;
    // skipping the reset can panic.
    esp_task_wdt_reset();
  }
  return WiFi.status() == WL_CONNECTED;
}

bool WifiTimeManager::syncTimeBlocking_(uint32_t timeoutMs) {
  configTime(gmtOffsetSec_, daylightOffsetSec_, NTP_SERVER_1, NTP_SERVER_2);
  uint32_t start = millis();
  time_t now = time(nullptr);
  while (now < 1600000000 && (millis() - start) < timeoutMs) {
    delay(100);
    esp_task_wdt_reset();  // same TWDT requirement as connectBlocking_
    now = time(nullptr);
  }
  lastSyncAttemptMs_ = millis();
  timeSynced_ = (now >= 1600000000);
  return timeSynced_;
}

void WifiTimeManager::begin() {
  loadTimezone_();
  String ssid, password;
  loadCredentials_(ssid, password);
  if (ssid.length() == 0) {
    return;
  }
  if (connectBlocking_(ssid, password, WIFI_CONNECT_TIMEOUT_MS)) {
    syncTimeBlocking_(NTP_SYNC_TIMEOUT_MS);
  }
}

bool WifiTimeManager::configure(const String& ssid, const String& password) {
  saveCredentials_(ssid, password);
  bool connected = connectBlocking_(ssid, password, WIFI_CONNECT_TIMEOUT_MS);
  if (connected) { syncTimeBlocking_(NTP_SYNC_TIMEOUT_MS); }
  return connected;
}

void WifiTimeManager::update() {
  if (!isWifiConnected()) return;
  if (millis() - lastSyncAttemptMs_ < NTP_RESYNC_INTERVAL_MS) return;
  if (importActive_) {
    // syncTimeBlocking_() can block serial up to 8s, blowing the CLI's
    // retry budget -- defer resync until import finishes.
    lastSyncAttemptMs_ = millis();
    return;
  }
  syncTimeBlocking_(NTP_SYNC_TIMEOUT_MS);
}

bool WifiTimeManager::isWifiConnected() const {
  return WiFi.status() == WL_CONNECTED;
}

time_t WifiTimeManager::nowUtc() const {
  if (!timeSynced_) return 0;
  return time(nullptr);
}

String WifiTimeManager::currentSsid() const {
  if (!isWifiConnected()) return String("");
  return WiFi.SSID();
}

String WifiTimeManager::currentIp() const {
  if (!isWifiConnected()) return String("");
  return WiFi.localIP().toString();
}

int32_t WifiTimeManager::currentRssi() const {
  if (!isWifiConnected()) return 0;
  return WiFi.RSSI();
}

bool WifiTimeManager::resyncTime() {
  if (!isWifiConnected()) return false;
  return syncTimeBlocking_(NTP_SYNC_TIMEOUT_MS);
}