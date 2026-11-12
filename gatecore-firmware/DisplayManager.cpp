#include "DisplayManager.h"
#include <Wire.h>

void DisplayManager::begin() {
  Wire.begin(LCD_SDA, LCD_SCL);
  lcd_.init();
  lcd_.backlight();
  lcd_.clear();
}

String DisplayManager::truncate_(const String& s, uint8_t maxLen) {
  if (s.length() <= maxLen) return s;
  return s.substring(0, maxLen);
}

void DisplayManager::printTwoLines_(const String& line1, const String& line2) {
  lcd_.clear();
  lcd_.setCursor(0, 0);
  lcd_.print(truncate_(line1, LCD_COLS));
  lcd_.setCursor(0, 1);
  lcd_.print(truncate_(line2, LCD_COLS));
}

void DisplayManager::showBoot() {
  printTwoLines_("RFID Access Ctrl", "Booting...");
}

void DisplayManager::showIdle() {
  printTwoLines_("RFID SYSTEM", "Waiting Card...");
}

void DisplayManager::showAccessGranted(const String& name) {
  printTwoLines_("ACCESS GRANTED", name);
}

void DisplayManager::showAccessDenied(const String& reason) {
  printTwoLines_("ACCESS DENIED", reason);
}

void DisplayManager::showUid(const String& uid) {
  printTwoLines_("UID:", uid);
}

void DisplayManager::showDatabaseUpdating() {
  printTwoLines_("DATABASE", "UPDATING...");
}

void DisplayManager::showSavingDatabase() {
  printTwoLines_("DATABASE", "Saving...");
}

void DisplayManager::showLoadingDatabase() {
  printTwoLines_("DATABASE", "Loading...");
}

void DisplayManager::showScanMode() {
  printTwoLines_("SCAN MODE", "Present Card");
}

void DisplayManager::showSerialConnected() {
  printTwoLines_("SERIAL", "Connected");
}

void DisplayManager::showSerialDisconnected() {
  printTwoLines_("SERIAL", "Disconnected");
}

void DisplayManager::showWifiConnecting() {
  printTwoLines_("WIFI", "Connecting...");
}

void DisplayManager::showWifiResult(bool connected) {
  printTwoLines_("WIFI", connected ? "Connected" : "Failed");
}

void DisplayManager::showError(const String& message) {
  printTwoLines_("ERROR", message);
}

void DisplayManager::showRenewingTag() {
  printTwoLines_("RENEWING NFC TAG", "Present Card...");
}

void DisplayManager::showLockout(uint32_t secondsRemaining) {
  printTwoLines_("LOCKED - TOO MANY", "TRIES (" + String(secondsRemaining) + "s)");
}
