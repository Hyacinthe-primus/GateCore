#pragma once
/*
 * SystemController.h
 * Orchestrates DatabaseManager + DisplayManager + RFIDManager + SerialProtocol.
 * Only class that knows the full state machine.
 */

#include <Arduino.h>
#include "DatabaseManager.h"
#include "DisplayManager.h"
#include "RFIDManager.h"
#include "SerialProtocol.h"
#include "NetworkManager.h"
#include "BuzzerManager.h"

enum class SystemState {
  BOOT,
  IDLE,
  SCAN_MODE,
  RESULT_DISPLAY,
  UID_DISPLAY,       // follows RESULT_DISPLAY
  DB_BUSY,
  LOCKOUT            // too many denied badges
};

class SystemController {
public:
  void begin();
  void update(); // call every loop()

private:
  DatabaseManager db_;
  DisplayManager display_;
  RFIDManager rfid_;
  SerialProtocol serial_;
  WifiTimeManager network_;
  BuzzerManager buzzer_;

  SystemState state_ = SystemState::BOOT;
  uint32_t stateEnteredMs_ = 0;

  // Pending card result, consumed by RESULT_DISPLAY -> UID_DISPLAY.
  bool pendingAccessGranted_ = false;
  String pendingName_;
  String pendingUid_;

  bool scanModeActive_ = false;
  bool scanContinuous_ = false;      // stay in scan mode after each card (CLI scan --infinite)
  bool renewalActive_ = false;
  double renewalValidDays_ = 0.0;

  // Anti-brute-force lockout state (see Config.h).
  int consecutiveDenials_ = 0;
  int32_t lockoutLastShownSec_ = -1;  // LCD refresh throttle

  // Serial connection tracking (USB CDC reflects host DTR)
  bool serialWasConnected_ = false;
  bool serialNoticeActive_ = false;
  uint32_t serialNoticeStartMs_ = 0;

  void enterState_(SystemState s);
  void handleCardDetected_(const String& uid);
  void handleSerialMessage_(JsonDocument& doc);
  void handleSerialConnectionChange_();

  // Granted while now <= registered + valid_days. Denies (fails safe)
  // when the clock has not been NTP-synced.
  bool isUserExpired_(const UserRecord& user, bool& timeAvailableOut) const;

  // Pauses RFID, shows "DATABASE UPDATING", runs the operation, then
  // restores the previous screen.
  template <typename Fn>
  bool withDbBusyScreen_(Fn&& operation) {
    SystemState previous = state_;
    state_ = SystemState::DB_BUSY;
    display_.showDatabaseUpdating();

    bool result = operation();

    state_ = previous;
    restoreIdleOrScanScreen_();
    return result;
  }

  void restoreIdleOrScanScreen_();

  // Discards an unfinished import (import mode, RAM-only changes) and
  // reloads the last persisted database. Safe to call when not importing.
  void abortImport_();
  uint32_t lastSerialActivityMs_ = 0;
};