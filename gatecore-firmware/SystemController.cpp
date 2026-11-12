#include "SystemController.h"
#include "Config.h"
#include "ImportProfiler.h"
#include "SyncProfiler.h"
#include <cstring>
#include <time.h>
#include <esp_heap_caps.h>
#include <esp_task_wdt.h>
#include <vector>
#include <utility>
#include <algorithm>

namespace {
  // Format local time in the exact shape the CLI displays (YYYY-MM-DD HH:MM:SS).
  String formatLocalTime(time_t epoch) {
    struct tm* t = localtime(&epoch);
    if (!t) return String("??");
    char buf[20];
    snprintf(buf, sizeof(buf), "%04d-%02d-%02d %02d:%02d:%02d",
             t->tm_year + 1900, t->tm_mon + 1, t->tm_mday,
             t->tm_hour, t->tm_min, t->tm_sec);
    return String(buf);
  }

  // Howard Hinnant's civil-calendar algorithm; converts a date to a Unix
  // epoch without depending on libc timezone state.
  time_t civilDateToEpoch(int year, int month, int day) {
    if (month <= 2) { year--; month += 12; }
    int era = (year >= 0 ? year : year - 399) / 400;
    unsigned yoe = static_cast<unsigned>(year - era * 400);
    unsigned doy = (153u * (static_cast<unsigned>(month) - 3u) + 2u) / 5u + static_cast<unsigned>(day) - 1u;
    unsigned doe = yoe * 365u + yoe / 4u - yoe / 100u + doy;
    int days = static_cast<int>(era * 146097) + static_cast<int>(doe) - 719468;
    return static_cast<time_t>(days) * 86400LL;
  }

  // Input is pre-validated by DatabaseManager::isValidRegisteredDate.
  time_t parseRegisteredDateUtc(const String& registered) {
    int year  = registered.substring(0, 4).toInt();
    int month = registered.substring(5, 7).toInt();
    int day   = registered.substring(8, 10).toInt();
    return civilDateToEpoch(year, month, day);
  }
}

void SystemController::begin() {
  // Sized from kLineBufCapacity so the two can't drift apart.
  Serial.setRxBufferSize(SerialProtocol::kLineBufCapacity);
  Serial.begin(SERIAL_BAUD);
  uint32_t waitStart = millis();
  while (!Serial && (millis() - waitStart) < 3000) { /* wait briefly for native USB CDC */ }

  Serial.println(F("[boot] display_.begin() ...")); Serial.flush();
  display_.begin();
  Serial.println(F("[boot] display_.begin() OK")); Serial.flush();

  enterState_(SystemState::BOOT);
  display_.showBoot();

  Serial.println(F("[boot] db_.begin() ...")); Serial.flush();
  if (!db_.begin()) {
    display_.showError("DB INIT FAIL");
  }
  Serial.println(F("[boot] db_.begin() OK")); Serial.flush();

  Serial.println(F("[boot] rfid_.begin() ...")); Serial.flush();
  bool rfidOk = rfid_.begin();
  if (!rfidOk) {
    display_.showError("PN532 NOT FOUND");
  }
  Serial.println(F("[boot] rfid_.begin() OK")); Serial.flush();

  Serial.println(F("[boot] buzzer_.begin() ...")); Serial.flush();
  buzzer_.begin();
  Serial.println(F("[boot] buzzer_.begin() OK")); Serial.flush();

  // Blocking but bounded. No credentials = badges fail-safe as expired.
  Serial.println(F("[boot] network_.begin() ...")); Serial.flush();
  network_.begin();
  Serial.println(F("[boot] network_.begin() OK")); Serial.flush();

  Serial.println(F("[boot] serial_.begin() ...")); Serial.flush();
  serial_.begin([this](JsonDocument& doc) {
    lastSerialActivityMs_ = millis();
    handleSerialMessage_(doc);
    lastSerialActivityMs_ = millis();  // long handlers (import_bin) count as activity
  });
  Serial.println(F("[boot] serial_.begin() OK")); Serial.flush();

  serialWasConnected_ = (bool)Serial;

  uint32_t bootElapsed = millis() - stateEnteredMs_;
  if (bootElapsed < BOOT_SCREEN_MIN_MS) {
    delay(BOOT_SCREEN_MIN_MS - bootElapsed);
  }

  enterState_(SystemState::IDLE);
  display_.showIdle();
}

void SystemController::enterState_(SystemState s) {
  state_ = s;
  stateEnteredMs_ = millis();
}

void SystemController::restoreIdleOrScanScreen_() {
  if (renewalActive_) {
    state_ = SystemState::SCAN_MODE;
    display_.showRenewingTag();
  } else if (scanModeActive_) {
    state_ = SystemState::SCAN_MODE;
    display_.showScanMode();
  } else {
    state_ = SystemState::IDLE;
    display_.showIdle();
  }
}

void SystemController::update() {
  // Serviced every tick regardless of state so CLI commands are never
  // dropped while a result screen is showing.
  serial_.poll();
  handleSerialConnectionChange_();
  buzzer_.update();
  network_.update(); // non-blocking; only re-syncs every NTP_RESYNC_INTERVAL_MS

  uint32_t now = millis();

  // Host vanished mid-import: abort instead of leaving RFID paused forever.
  if (db_.isImportMode() && (now - lastSerialActivityMs_) >= IMPORT_IDLE_TIMEOUT_MS) {
    abortImport_();  // also restores the idle/scan screen and RFID polling
  }

  switch (state_) {
    case SystemState::BOOT:
      break;

    case SystemState::IDLE:
    case SystemState::SCAN_MODE: {
      String uid;
      if (rfid_.poll(uid)) {
        handleCardDetected_(uid);
      }
      break;
    }

    case SystemState::RESULT_DISPLAY:
      if (now - stateEnteredMs_ >= RESULT_DISPLAY_MS) {
        if (renewalActive_) {
          // Renewal mode skips the UID screen -- straight back to
          // "RENEWING NFC TAG" for the next card.
          restoreIdleOrScanScreen_();
          enterState_(state_);
        } else {
          display_.showUid(pendingUid_);
          enterState_(SystemState::UID_DISPLAY);
        }
      }
      break;

    case SystemState::UID_DISPLAY:
      if (now - stateEnteredMs_ >= UID_DISPLAY_MS) {
        restoreIdleOrScanScreen_();
        enterState_(state_); // refresh timer, harmless in IDLE/SCAN_MODE
      }
      break;

    case SystemState::DB_BUSY:
      break;  // transient -- withDbBusyScreen_ is synchronous

    case SystemState::LOCKOUT:
      // No RFID polling here -- serial/CLI still work for investigation.
      if (now - stateEnteredMs_ >= LOCKOUT_DURATION_MS) {
        consecutiveDenials_ = 0;
        lockoutLastShownSec_ = -1;
        restoreIdleOrScanScreen_();
      } else {
        int32_t remainingSec = (int32_t)((LOCKOUT_DURATION_MS - (now - stateEnteredMs_) + 999) / 1000);
        if (remainingSec != lockoutLastShownSec_) {
          lockoutLastShownSec_ = remainingSec;
          display_.showLockout((uint32_t)remainingSec);
        }
      }
      break;
  }
}

bool SystemController::isUserExpired_(const UserRecord& user, bool& timeAvailableOut) const {
  timeAvailableOut = network_.isTimeSynced();

  if (user.isAdmin()) {
    return false;
  }

  if (!timeAvailableOut) {
    // Fail safe: without a trusted clock, deny.
    return true;
  }

  // registered is a bare YYYY-MM-DD date; parseRegisteredDateUtc() converts
  // it to an epoch for the comparison below.
  time_t registeredEpoch = parseRegisteredDateUtc(user.registered);
  double expirationEpoch = (double)registeredEpoch + user.validDays * 86400.0;
  double currentLocalEpoch = (double)network_.nowUtc() + network_.gmtOffsetSec() + network_.daylightOffsetSec();
  // nowUtc() is raw UTC and configTime() only affects localtime(); add the
  // configured offsets here to compare against the local wall clock.

  return currentLocalEpoch > expirationEpoch;
}

void SystemController::handleCardDetected_(const String& uid) {
  if (scanModeActive_) {
    // Report the UID without checking it against the DB, then fall back.
    serial_.sendUidDetected(uid);
    if (scanContinuous_) {
      // Stay armed: a card presented while the host re-arms must not be
      // evaluated as an access attempt (and count toward the lockout).
      display_.showScanMode();
      return;
    }
    scanModeActive_ = false;
    enterState_(SystemState::IDLE);
    display_.showIdle();
    return;
  }

  if (renewalActive_) {
    UserRecord user;
    bool found = db_.findUser(uid, user);
    // Each failure is reported with a typed renewal_result error so the CLI
    // can tell it apart from a reply to one of its own commands.
    struct tm tmNow;
    time_t now = network_.nowUtc();
    if (!found) {
      serial_.sendRenewalError(uid, "UID not in database");
      display_.showAccessDenied("Unknown");
      buzzer_.playDenied();
    } else if (user.isAdmin()) {
      // Renewing would turn a never-expiring admin badge into an expiring one.
      serial_.sendRenewalError(uid, "Admin badge never expires -- not renewed");
      display_.showError("ADMIN: SKIPPED");
      buzzer_.playDenied();
    } else if (!network_.isTimeSynced() || localtime_r(&now, &tmNow) == nullptr) {
      // An unsynced clock gives nowUtc() == 0; dating the renewal from it
      // would make the badge instantly expired.
      serial_.sendRenewalError(uid, "NTP time not synced -- cannot date the renewal");
      display_.showError("NO TIME SYNC");
      buzzer_.playDenied();
    } else {
      // configTime() already applied the GMT/DST offsets to localtime();
      // do not add them again when stamping today's date.
      char todayBuf[11];
      snprintf(todayBuf, sizeof(todayBuf), "%04d-%02d-%02d",
               tmNow.tm_year + 1900, tmNow.tm_mon + 1, tmNow.tm_mday);
      String today(todayBuf);

      String err;
      if (db_.renewUser(uid, today, renewalValidDays_, err)) {
        db_.findUser(uid, user);
        serial_.sendRenewalResult(uid, user.name, user.registered, user.validDays);
        display_.showAccessGranted("RENEWED " + String(user.name));
        buzzer_.playGranted();
      } else {
        serial_.sendRenewalError(uid, err);
        display_.showError(err);
        buzzer_.playDenied();
      }
    }
    enterState_(SystemState::RESULT_DISPLAY);
    return;
  }

  UserRecord user;
  bool found = db_.findUser(uid, user);

  bool timeAvailable = true;
  bool expired = found ? isUserExpired_(user, timeAvailable) : false;
  bool granted = found && !expired;

  pendingAccessGranted_ = granted;
  pendingUid_ = uid;
  pendingName_ = granted ? String(user.name) : String("");

  if (granted) {
    consecutiveDenials_ = 0;
    display_.showAccessGranted(pendingName_);
    buzzer_.playGranted();
    enterState_(SystemState::RESULT_DISPLAY);
    return;
  }

  if (found && expired && !timeAvailable) {
    display_.showAccessDenied("No Time Sync");
  } else if (found && expired) {
    display_.showAccessDenied("Expired");
  } else {
    display_.showAccessDenied();
  }
  buzzer_.playDenied();

  consecutiveDenials_++;
  if (consecutiveDenials_ >= MAX_CONSECUTIVE_DENIALS) {
    consecutiveDenials_ = 0;
    lockoutLastShownSec_ = -1;
    enterState_(SystemState::LOCKOUT);
    display_.showLockout(LOCKOUT_DURATION_MS / 1000);
    return;
  }
  enterState_(SystemState::RESULT_DISPLAY);
}

void SystemController::handleSerialMessage_(JsonDocument& doc) {
  const char* type = doc["type"] | "";

  if (strcmp(type, "add") == 0) {
    if (!doc.containsKey("uid") || !doc.containsKey("name")) {
      serial_.sendError("Missing 'uid' or 'name'");
      return;
    }
    String uid = String((const char*)doc["uid"]);
    String name = String((const char*)doc["name"]);

    // Admin if registered/valid_days fields are missing (see DatabaseManager.h).
    String registered;
    double validDays;
    bool adminBadge = !doc.containsKey("registered") || !doc.containsKey("valid_days");
    if (adminBadge) {
      registered = String(ADMIN_REGISTERED);
      validDays = ADMIN_VALID_DAYS;
    } else {
      registered = String((const char*)doc["registered"]);
      validDays = doc["valid_days"].as<double>();
    }

    String err;
    bool ok;
    if (db_.isImportMode()) {
      // LCD held by import_begin/import_end -- don't churn per-add.
      ok = db_.addUserNoSave(uid, name, registered, validDays, err);
    } else {
      ok = withDbBusyScreen_([&]() {
        return db_.addUser(uid, name, registered, validDays, err);
      });
    }
    if (ok) serial_.sendOk(); else serial_.sendError(err);
    return;
  }

  if (strcmp(type, "batch_add") == 0) {
    if (!db_.isImportMode()) {
      serial_.sendError("batch_add requires import_begin first");
      return;
    }
    if (!doc.containsKey("users") || !doc["users"].is<JsonArray>()) {
      serial_.sendError("Missing or invalid 'users' array");
      return;
    }
    JsonArray arr = doc["users"].as<JsonArray>();
    size_t added = 0;
    size_t errors = 0;
    std::vector<std::pair<String, String>> failed;
    {
      ScopedMicroTimer t(g_importProfile.batchLoopUs);
      for (JsonObject entry : arr) {
        if (!entry.containsKey("uid") || !entry.containsKey("name")) {
          errors++;
          String uid = entry.containsKey("uid") ? String((const char*)entry["uid"]) : String("");
          failed.push_back({uid, "Missing 'uid' or 'name'"});
          continue;
        }
        String uid = String((const char*)entry["uid"]);
        String name = String((const char*)entry["name"]);

        String registered;
        double validDays;
        bool adminBadge = !entry.containsKey("registered") || !entry.containsKey("valid_days");
        if (adminBadge) {
          registered = String(ADMIN_REGISTERED);
          validDays = ADMIN_VALID_DAYS;
        } else {
          registered = String((const char*)entry["registered"]);
          validDays = entry["valid_days"].as<double>();
        }

        String err;
        bool ok = db_.addUserNoSave(uid, name, registered, validDays, err);
        if (ok) {
          added++;
        } else {
          errors++;
          failed.push_back({uid, err});
        }
      }
    }
    g_importProfile.batchCount++;
    g_importProfile.userCount += arr.size();
    {
      ScopedMicroTimer t(g_importProfile.ackSerializeUs);
      serial_.sendBatchAddResult(added, errors, failed);
    }
    return;
  }

  if (strcmp(type, "import_bin") == 0) {
    if (!db_.isImportMode()) {
      serial_.sendError("import_bin requires import_begin first");
      return;
    }
    if (!doc.containsKey("bytes")) {
      serial_.sendError("Missing 'bytes'");
      return;
    }
    long totalBytesSigned = doc["bytes"].as<long>();
    size_t recSize = DatabaseManager::recordSize();
    if (totalBytesSigned < 0 || ((size_t)totalBytesSigned % recSize) != 0) {
      serial_.sendError("'bytes' must be a non-negative multiple of the record size");
      return;
    }
    size_t totalBytes = (size_t)totalBytesSigned;
    size_t incomingUsers = totalBytes / recSize;

    // Pre-check free PSRAM to avoid StoreProhibited on large imports.
    size_t roughNeededBytes = incomingUsers * sizeof(UserRecord);
    size_t freePsram = heap_caps_get_free_size(MALLOC_CAP_SPIRAM);
    if (roughNeededBytes + 8192 > freePsram) {  // +8192 matches the chunk buffer below
      serial_.sendError("Not enough free PSRAM for an import this size");
      return;
    }
    // Allocate BEFORE the ack: once "ok" is sent the host streams raw
    // bytes, and an error line sent after that point would be followed by
    // the payload being parsed as (garbage) JSON commands.
    const size_t kChunkBytes = (recSize <= 8192) ? (8192 / recSize) * recSize : recSize;
    uint8_t* buf = (uint8_t*)heap_caps_malloc(kChunkBytes, MALLOC_CAP_SPIRAM);
    if (!buf) {
      serial_.sendError("Out of memory for import_bin buffer");
      return;
    }
    db_.reserveForImport(incomingUsers);

    serial_.sendOk();  // ack signals host to start writing raw bytes

    size_t added = 0, errors = 0, consumed = 0;
    unsigned long perChunkTimeoutMs = 5000;  // stall detector, not a flat budget
    bool desynced = false;

    while (consumed < totalBytes) {
      size_t want = std::min(kChunkBytes, totalBytes - consumed);
      size_t got = serial_.readRawExact(buf, want, perChunkTimeoutMs);
      if (got < want) { desynced = true; break; }
      for (size_t off = 0; off < got; off += recSize) {
        String err;
        if (db_.addUserFromRawRecord(&buf[off], err)) added++; else errors++;
      }
      consumed += got;
    }
    heap_caps_free(buf);

    if (desynced) {
      serial_.sendError("import_bin: transfer stalled/incomplete -- reconnect and retry the import");
      return;
    }
    serial_.sendImportBinResult(added, errors);
    return;
  }

  if (strcmp(type, "remove") == 0) {
    if (!doc.containsKey("uid")) {
      serial_.sendError("Missing 'uid'");
      return;
    }
    String uid = String((const char*)doc["uid"]);
    String err;
    bool ok = withDbBusyScreen_([&]() { return db_.removeUser(uid, err); });
    if (ok) serial_.sendOk(); else serial_.sendError(err);
    return;
  }

  if (strcmp(type, "clear_all") == 0) {
    String err;
    bool ok = withDbBusyScreen_([&]() { return db_.clearAll(err); });
    if (ok) serial_.sendOk(); else serial_.sendError(err);
    return;
  }

  if (strcmp(type, "remove_all_except") == 0) {
    if (!doc.containsKey("uids") || !doc["uids"].is<JsonArray>()) {
      serial_.sendError("Missing or invalid 'uids' array");
      return;
    }
    JsonArray arr = doc["uids"].as<JsonArray>();
    if (arr.size() == 0) {
      serial_.sendError("'uids' array is empty -- use clear_all to wipe every user");
      return;
    }
    std::vector<String> keepUids;
    keepUids.reserve(arr.size());
    for (JsonVariant v : arr) {
      keepUids.push_back(String((const char*)v));
    }

    size_t removedCount = 0;
    String err;
    bool ok = withDbBusyScreen_([&]() { return db_.removeAllExcept(keepUids, removedCount, err); });
    if (ok) serial_.sendRemovedCount(removedCount); else serial_.sendError(err);
    return;
  }

  if (strcmp(type, "rename") == 0) {
    if (!doc.containsKey("uid") || !doc.containsKey("name")) {
      serial_.sendError("Missing 'uid' or 'name'");
      return;
    }
    String uid = String((const char*)doc["uid"]);
    String newName = String((const char*)doc["name"]);
    String err;
    bool ok = withDbBusyScreen_([&]() { return db_.renameUser(uid, newName, err); });
    if (ok) serial_.sendOk(); else serial_.sendError(err);
    return;
  }

  if (strcmp(type, "find") == 0) {
    if (!doc.containsKey("uid")) {
      serial_.sendError("Missing 'uid'");
      return;
    }
    String uid = String((const char*)doc["uid"]);
    UserRecord user;
    uint32_t searchStart = micros();
    bool found = db_.findUser(uid, user);
    uint32_t searchUs = micros() - searchStart;
    serial_.sendFindResult(found, uid, found ? String(user.name) : String(""),
                           found ? String(user.registered) : String(""),
                           found ? user.validDays : 0.0, searchUs);
    return;
  }

  if (strcmp(type, "find_name") == 0) {
    if (!doc.containsKey("query")) {
      serial_.sendError("Missing 'query'");
      return;
    }
    String query = String((const char*)doc["query"]);
    query.toLowerCase();
    serial_.sendFindNameResult(db_, query);
    return;
  }

  if (strcmp(type, "list") == 0) {
    serial_.sendUserList(db_);
    return;
  }

  if (strcmp(type, "export_bin") == 0) {
    size_t count = db_.userCount();
    size_t recSize = DatabaseManager::recordSize();
    size_t totalBytes = count * recSize;
    serial_.sendExportBinHeader(totalBytes, count);

    // Encode+flush in chunks; one Serial.write() per user has fixed-call
    // overhead and is much slower.
    static const size_t kRecordsPerFlush = 8192 / DatabaseManager::recordSize();
    static const size_t kChunkBytes = kRecordsPerFlush * DatabaseManager::recordSize();
    uint8_t* buf = (uint8_t*)heap_caps_malloc(kChunkBytes, MALLOC_CAP_SPIRAM);
    if (!buf) {
      // The header is already out; send zeros so the byte count matches.
      // The host detects the corruption via CRC32.
      for (size_t sent = 0; sent < totalBytes; ) {
        uint8_t zero[64] = {0};
        size_t n = std::min(sizeof(zero), totalBytes - sent);
        serial_.writeRaw(zero, n);
        sent += n;
        if ((sent & 0xFFFF) < sizeof(zero)) {
          esp_task_wdt_reset();
        }
      }
      return;
    }
    size_t bufLen = 0;
    for (size_t i = 0; i < count; i++) {
      db_.encodeUserAt(i, &buf[bufLen]);
      bufLen += recSize;
      if (bufLen + recSize > kChunkBytes) {
        serial_.writeRaw(buf, bufLen);
        bufLen = 0;
      }
      // A full export at USB-CDC speeds can outrun the TWDT timeout --
      // writeRaw()/Serial.write() doesn't feed the watchdog on its own.
      if ((i & 0xFF) == 0xFF) {
        esp_task_wdt_reset();
      }
    }
    if (bufLen > 0) serial_.writeRaw(buf, bufLen);
    heap_caps_free(buf);
    return;
  }

  if (strcmp(type, "sync_begin") == 0) {
    // Stateless, like "status": no follow-up state to time out if the host
    // stops after this reply.
    serial_.sendSyncBeginResult(db_);
    return;
  }

  if (strcmp(type, "sync_manifest") == 0) {
    size_t count = db_.userCount();
    size_t entrySize = DatabaseManager::manifestEntrySize();
    size_t totalBytes = count * entrySize;
    serial_.sendSyncManifestHeader(totalBytes, count);

    static const size_t kEntriesPerFlush = 8192 / DatabaseManager::manifestEntrySize();
    static const size_t kChunkBytes = kEntriesPerFlush * DatabaseManager::manifestEntrySize();
    uint8_t* buf = (uint8_t*)heap_caps_malloc(kChunkBytes, MALLOC_CAP_SPIRAM);
    if (!buf) {
      // Header is already out; send zeros to keep the byte count intact.
      // All-zero UIDs match nothing, forcing a safe re-sync.
      for (size_t sent = 0; sent < totalBytes; ) {
        uint8_t zero[64] = {0};
        size_t n = std::min(sizeof(zero), totalBytes - sent);
        serial_.writeRaw(zero, n);
        sent += n;
        if ((sent & 0xFFFF) < sizeof(zero)) esp_task_wdt_reset();
      }
      return;
    }
    size_t bufLen = 0;
    for (size_t i = 0; i < count; i++) {
      db_.encodeManifestEntryAt(i, &buf[bufLen]);
      bufLen += entrySize;
      if (bufLen + entrySize > kChunkBytes) {
        serial_.writeRaw(buf, bufLen);
        bufLen = 0;
      }
      if ((i & 0xFF) == 0xFF) esp_task_wdt_reset();
    }
    if (bufLen > 0) serial_.writeRaw(buf, bufLen);
    heap_caps_free(buf);
    return;
  }

  if (strcmp(type, "sync_apply") == 0) {
    if (!doc.containsKey("remove") || !doc.containsKey("add") || !doc.containsKey("replace")) {
      serial_.sendError("Missing 'remove', 'add', or 'replace'");
      return;
    }
    long removeSigned = doc["remove"].as<long>();
    long addSigned = doc["add"].as<long>();
    long replaceSigned = doc["replace"].as<long>();
    if (removeSigned < 0 || addSigned < 0 || replaceSigned < 0) {
      serial_.sendError("'remove'/'add'/'replace' must be non-negative");
      return;
    }
    size_t removeCount = (size_t)removeSigned;
    size_t addCount = (size_t)addSigned;
    size_t replaceCount = (size_t)replaceSigned;

    size_t recSize = DatabaseManager::recordSize();
    size_t uidEntrySz = DatabaseManager::uidEntrySize();
    size_t removeBytes = removeCount * uidEntrySz;
    size_t addBytes = addCount * recSize;
    size_t replaceBytes = replaceCount * recSize;

    // Pre-check free PSRAM for the net growth, same guard as import_bin.
    size_t roughNeededBytes = (addCount > removeCount ? (addCount - removeCount) : 0) * sizeof(UserRecord);
    size_t freePsram = heap_caps_get_free_size(MALLOC_CAP_SPIRAM);
    if (roughNeededBytes + 8192 > freePsram) {
      serial_.sendError("Not enough free PSRAM for a sync this size");
      return;
    }
    // Allocate BEFORE the ack (same reason as import_bin above).
    size_t chunkCap = 8192;
    uint8_t* buf = (uint8_t*)heap_caps_malloc(chunkCap, MALLOC_CAP_SPIRAM);
    if (!buf) {
      serial_.sendError("Out of memory for sync_apply buffer");
      return;
    }
    struct BufGuard { uint8_t* p; ~BufGuard() { if (p) heap_caps_free(p); } } bufGuard{buf};
    db_.reserveForImport(addCount);

    serial_.sendOk();  // ack signals host to start streaming raw bytes

    size_t removed = 0, added = 0, replaced = 0, errors = 0;
    unsigned long perChunkTimeoutMs = 5000;  // stall detector, not a flat budget
    bool desynced = false;
    String lastErr;

    // g_syncProfile is per-request, unlike g_importProfile, which spans
    // import_begin through import_end.
    g_syncProfile.reset();

    // Phase 1: remove. Mark-then-compact once (O(n)), instead of one
    // erase() per uid (O(n) each) that could starve the watchdog.
    {
      ScopedMicroTimer t(g_syncProfile.removePhaseUs);
      db_.beginRemoveBatch();
      size_t removeOps = 0;
      for (size_t consumed = 0; consumed < removeBytes && !desynced; ) {
        size_t want = std::min(chunkCap - (chunkCap % uidEntrySz), removeBytes - consumed);
        size_t got = serial_.readRawExact(buf, want, perChunkTimeoutMs);
        if (got < want) { desynced = true; break; }
        for (size_t off = 0; off < got; off += uidEntrySz) {
          String err;
          if (db_.removeUserRawNoSave(&buf[off], err)) removed++; else { errors++; lastErr = err; }
          if ((++removeOps & 0xFF) == 0xFF) esp_task_wdt_reset();
        }
        consumed += got;
      }
      db_.endRemoveBatch();
    }

    // Phase 2: add. Append unsorted, then sort once, like import_bin.
    // The duplicate baseline is the post-remove users_.
    {
      ScopedMicroTimer t(g_syncProfile.addPhaseUs);
      db_.setImportMode(true);
      for (size_t consumed = 0; !desynced && consumed < addBytes; ) {
        size_t want = std::min(chunkCap - (chunkCap % recSize), addBytes - consumed);
        size_t got = serial_.readRawExact(buf, want, perChunkTimeoutMs);
        if (got < want) { desynced = true; break; }
        for (size_t off = 0; off < got; off += recSize) {
          String err;
          if (db_.addUserFromRawRecord(&buf[off], err)) added++; else { errors++; lastErr = err; }
          if ((added & 0xFF) == 0xFF) esp_task_wdt_reset();
        }
        consumed += got;
      }
      // One O(n log n) sort instead of per-add shifts. Must precede Phase 3:
      // replaceUserFromRawRecord() binary-searches a sorted users_. This also
      // clears import state even on desync, so the DB cannot stay in import
      // mode for the next command.
      db_.setImportMode(false);
    }

    // Phase 3: replaces (overwrite in place).
    {
      ScopedMicroTimer t(g_syncProfile.replacePhaseUs);
      for (size_t consumed = 0; !desynced && consumed < replaceBytes; ) {
        size_t want = std::min(chunkCap - (chunkCap % recSize), replaceBytes - consumed);
        size_t got = serial_.readRawExact(buf, want, perChunkTimeoutMs);
        if (got < want) { desynced = true; break; }
        for (size_t off = 0; off < got; off += recSize) {
          String err;
          if (db_.replaceUserFromRawRecord(&buf[off], err)) replaced++; else { errors++; lastErr = err; }
          if ((replaced & 0xFF) == 0xFF) esp_task_wdt_reset();
        }
        consumed += got;
      }
    }

    if (desynced) {
      // Transport broke mid-stream and users_ may hold a partial mix of
      // operations. Nothing has been saved, so reload from flash.
      db_.load();
      serial_.sendError("sync_apply: transfer stalled/incomplete -- reconnect and retry the sync");
      return;
    }

    SaveTiming st;
    bool ok;
    {
      ScopedMicroTimer t(g_syncProfile.saveUs);
      ok = withDbBusyScreen_([&]() { return db_.save(&st); });
    }
    g_syncProfile.saveEncodeUs += st.encodeUs;
    g_syncProfile.saveWriteUs += st.writeUs;
    g_syncProfile.saveVerifyUs += st.verifyUs;
    g_syncProfile.saveFinalizeUs += st.finalizeUs;

    if (!ok) {
      // save() failed (verify-by-reread or write error) -- same recovery
      // as a desync: fall back to whatever is actually on flash.
      db_.load();
      serial_.sendSyncResult(false, "Failed to persist database", removed, added, replaced, errors,
                             db_.computeCrc32(), g_syncProfile);
      return;
    }
    serial_.sendSyncResult(true, "", removed, added, replaced, errors, db_.computeCrc32(), g_syncProfile);
    return;
  }

  if (strcmp(type, "status") == 0) {
    serial_.sendStatus(db_);
    return;
  }

  if (strcmp(type, "net_status") == 0) {
    serial_.sendNetStatus(network_);
    return;
  }

  if (strcmp(type, "get_time") == 0) {
    if (!network_.isTimeSynced()) {
      serial_.sendError("NTP time not synced yet");
      return;
    }
    // configTime() already applied the GMT/DST offset, so localtime()
    // is already local -- do NOT add the offset again here.
    time_t now = network_.nowUtc();
    String formatted = formatLocalTime(now);
    serial_.sendTime(now, formatted);
    return;
  }

  if (strcmp(type, "ntp_sync") == 0) {
    if (!network_.isWifiConnected()) {
      serial_.sendNtpSyncResult(false, "Wi-Fi not connected");
      return;
    }
    bool synced = network_.resyncTime();
    if (synced) {
      time_t now = network_.nowUtc();
      String formatted = formatLocalTime(now);
      serial_.sendNtpSyncResult(true, formatted);
    } else {
      serial_.sendNtpSyncResult(false, "NTP sync failed (server unreachable?)");
    }
    return;
  }

  if (strcmp(type, "import_begin") == 0) {
    state_ = SystemState::DB_BUSY;
    display_.showDatabaseUpdating();
    db_.setImportMode(true);
    network_.setImportActive(true);
    g_importProfile.reset();
    serial_.sendOk();
    return;
  }

  if (strcmp(type, "import_abort") == 0) {
    abortImport_();
    serial_.sendOk();
    return;
  }

  if (strcmp(type, "import_end") == 0) {
    size_t added = 0;
    size_t errors = 0;
    String err;
    bool ok = withDbBusyScreen_([&]() {
      added = db_.userCount();
      db_.setImportMode(false);
      network_.setImportActive(false);
      bool persisted;
      SaveTiming st;
      {
        ScopedMicroTimer t(g_importProfile.saveUs);
        persisted = db_.save(&st);
      }
      g_importProfile.saveEncodeUs += st.encodeUs;
      g_importProfile.saveWriteUs += st.writeUs;
      g_importProfile.saveVerifyUs += st.verifyUs;
      g_importProfile.saveFinalizeUs += st.finalizeUs;
      if (!persisted) {
        err = "Failed to persist database";
        return false;
      }
      return true;
    });
    if (ok) {
      serial_.sendImportResult(added, errors, g_importProfile);
    } else {
      serial_.sendError(err);
    }
    return;
  }

  if (strcmp(type, "enter_renewal_mode") == 0) {
    if (!doc.containsKey("valid_days")) {
      serial_.sendError("Missing 'valid_days'");
      return;
    }
    renewalValidDays_ = doc["valid_days"].as<double>();
    renewalActive_ = true;
    scanModeActive_ = false;
    enterState_(SystemState::SCAN_MODE);
    display_.showRenewingTag();
    serial_.sendOk();
    return;
  }

  if (strcmp(type, "exit_renewal_mode") == 0) {
    renewalActive_ = false;
    enterState_(SystemState::IDLE);
    display_.showIdle();
    serial_.sendOk();
    return;
  }

  if (strcmp(type, "enter_scan_mode") == 0) {
    if (!renewalActive_) {
      scanModeActive_ = true;
      // Optional (firmware >= 1.2): keep scanning after each card. Older
      // CLIs omit the key and get the original one-card behaviour.
      scanContinuous_ = doc["continuous"] | false;
      display_.showScanMode();
    }
    enterState_(SystemState::SCAN_MODE);
    serial_.sendOk();
    return;
  }

  if (strcmp(type, "exit_scan_mode") == 0) {
    scanModeActive_ = false;
    scanContinuous_ = false;
    enterState_(SystemState::IDLE);
    display_.showIdle();
    serial_.sendOk();
    return;
  }

  if (strcmp(type, "configure_wifi") == 0) {
    if (!doc.containsKey("ssid") || !doc.containsKey("password")) {
      serial_.sendError("Missing 'ssid' or 'password'");
      return;
    }
    String ssid = String((const char*)doc["ssid"]);
    String password = String((const char*)doc["password"]);

    SystemState previous = state_;
    state_ = SystemState::DB_BUSY; // reuses the "block idle/scan screen" state
    display_.showWifiConnecting();

    bool connected = network_.configure(ssid, password);

    display_.showWifiResult(connected);
    delay(1000); // brief, bounded -- lets the operator read the result

    state_ = previous;
    restoreIdleOrScanScreen_();

    serial_.sendWifiResult(connected,
      connected ? "Wi-Fi connected and time synced" : "Failed to connect with the given credentials");
    return;
  }

  if (strcmp(type, "configure_timezone") == 0) {
    if (!doc.containsKey("gmt_offset_sec")) {
      serial_.sendError("Missing 'gmt_offset_sec'");
      return;
    }
    long gmtOffsetSec = doc["gmt_offset_sec"].as<long>();
    int daylightOffsetSec = doc.containsKey("daylight_offset_sec")
        ? doc["daylight_offset_sec"].as<int>() : 0;

    SystemState previous = state_;
    state_ = SystemState::DB_BUSY;
    display_.showWifiConnecting(); // reused busy screen, close enough for a brief op

    bool applied = network_.setTimezone(gmtOffsetSec, daylightOffsetSec);

    state_ = previous;
    restoreIdleOrScanScreen_();

    serial_.sendTimezoneResult(applied, network_.gmtOffsetSec(), network_.daylightOffsetSec(),
      applied ? "Timezone applied and persisted"
              : "Could not resync NTP with the new offset -- not persisted, previous timezone kept");
    return;
  }

  serial_.sendError("Unknown command type");
}

void SystemController::abortImport_() {
  if (!db_.isImportMode()) return;
  db_.setImportMode(false);
  db_.load();  // discard unsaved changes and reload from flash
  network_.setImportActive(false);
  restoreIdleOrScanScreen_();
}

void SystemController::handleSerialConnectionChange_() {
  bool connected = (bool)Serial;

  if (connected != serialWasConnected_) {
    serialWasConnected_ = connected;

    // Never stomp on a result the user is currently reading.
    if (state_ == SystemState::IDLE || state_ == SystemState::SCAN_MODE) {
      if (connected) display_.showSerialConnected();
      else display_.showSerialDisconnected();
      serialNoticeActive_ = true;
      serialNoticeStartMs_ = millis();
    }
  }

  if (serialNoticeActive_ && (millis() - serialNoticeStartMs_ >= 1200)) {
    serialNoticeActive_ = false;
    if (state_ == SystemState::IDLE || state_ == SystemState::SCAN_MODE) {
      restoreIdleOrScanScreen_();
    }
  }
}