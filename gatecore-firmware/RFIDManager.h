#pragma once
/*
 * RFIDManager.h
 * Wraps Adafruit_PN532 over hardware SPI.
 * The short poll timeout (RFID_POLL_TIMEOUT_MS) lets update() poll every
 * loop() iteration without stalling the serial/LCD state machine.
 */

#include <Arduino.h>
#include <SPI.h>
#include <Adafruit_PN532.h>
#include "Config.h"

class RFIDManager {
public:
  bool begin();

  // Non-blocking-ish: returns true if a NEW card (different from the one
  // currently under cooldown) was detected within RFID_POLL_TIMEOUT_MS.
  // uidOut is the normalized (uppercase hex, no separators) UID string.
  bool poll(String& uidOut);

  bool isReady() const { return ready_; }

private:
  Adafruit_PN532 nfc_{PN532_SS, &SPI};
  bool ready_ = false;

  String lastUid_;
  uint32_t lastDetectMs_ = 0;

  static String uidBytesToHex_(const uint8_t* uid, uint8_t len);
};
