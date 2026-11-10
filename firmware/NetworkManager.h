#pragma once
// Renamed to avoid a name clash with the ESP32 Core v3.x NetworkManager.

#include <Arduino.h>
#include <time.h>

class WifiTimeManager {
public:
  void begin();
  void update();

  bool configure(const String& ssid, const String& password);

  bool isWifiConnected() const;
  bool isTimeSynced() const { return timeSynced_; }
  time_t nowUtc() const;
  bool resyncTime();

  // Returns false if the NTP resync fails.
  bool setTimezone(long gmtOffsetSec, int daylightOffsetSec);
  long gmtOffsetSec() const { return gmtOffsetSec_; }
  int daylightOffsetSec() const { return daylightOffsetSec_; }

  String currentSsid() const;
  String currentIp() const;
  int32_t currentRssi() const;

  // Defers NTP resync during batch imports.
  void setImportActive(bool active) { importActive_ = active; }

private:
  bool timeSynced_ = false;
  bool importActive_ = false;
  uint32_t lastSyncAttemptMs_ = 0;

  long gmtOffsetSec_ = 0;
  int daylightOffsetSec_ = 0;

  bool connectBlocking_(const String& ssid,
                        const String& password,
                        uint32_t timeoutMs);
  bool syncTimeBlocking_(uint32_t timeoutMs);

  void loadCredentials_(String& ssidOut, String& passOut);
  void saveCredentials_(const String& ssid, const String& password);

  // Loads the persisted timezone (or Config.h defaults).
  void loadTimezone_();
  void saveTimezone_(long gmtOffsetSec, int daylightOffsetSec);
};