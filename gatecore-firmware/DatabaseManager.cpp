#include "DatabaseManager.h"
#include "Config.h"
#include <LittleFS.h>
#include <ArduinoJson.h>
#include <esp_task_wdt.h>

bool DatabaseManager::begin() {
  // false = do not format on mount failure yet; we want to try recovery first.
  if (!LittleFS.begin(false)) {
    // Formatting the 12MB partition can block past the 5s Task Watchdog
    // timeout on first boot, so unsubscribe this task while it runs.
    esp_err_t wdtErr = esp_task_wdt_delete(NULL);
    bool formatted = LittleFS.format();
    if (wdtErr == ESP_OK) esp_task_wdt_add(NULL);

    if (!formatted) {
      return false;
    }
    if (!LittleFS.begin(false)) {
      return false;
    }
  }

  if (!load()) {
    recreateEmpty_();
    save();
  }
  return true;
}

void DatabaseManager::recreateEmpty_() {
  users_.clear();
}

static inline bool uidLess_(const UserRecord& a, const UserRecord& b) {
  return a.uid < b.uid;  // Must match the strcmp order used by binary search.
}

bool DatabaseManager::load() {
  bool ok = loadBinary_();
  if (!ok) ok = loadLegacyJsonAndMigrate_();
  if (!ok) return false;

  // Re-sort if file predates the "always sorted" invariant. persist
  // so suffix-rewrite saves can trust index-derived offsets.
  if (!std::is_sorted(users_.begin(), users_.end(), uidLess_)) {
    std::sort(users_.begin(), users_.end(), uidLess_);
    save();
  }
  return true;
}

bool DatabaseManager::loadBinary_() {
  if (!LittleFS.exists(USERS_DB_PATH)) {
    recreateEmpty_();
    return false;
  }

  File f = LittleFS.open(USERS_DB_PATH, "r");
  if (!f) {
    recreateEmpty_();
    return false;
  }

  uint8_t header[DB_HEADER_SIZE];
  if (f.read(header, DB_HEADER_SIZE) != (int)DB_HEADER_SIZE ||
      memcmp(header, DB_MAGIC, 4) != 0 ||
      header[4] != DB_VERSION ||
      (uint16_t)(header[5] | (header[6] << 8)) != (uint16_t)DB_RECORD_SIZE) {
    // Unusable binary DB; load() will try the legacy JSON migration.
    f.close();
    recreateEmpty_();
    return false;
  }

  size_t fileSize = f.size();
  size_t payloadBytes = (fileSize > DB_HEADER_SIZE) ? (fileSize - DB_HEADER_SIZE) : 0;
  // Floor division intentionally drops a torn trailing record after power loss.
  size_t numRecords = payloadBytes / DB_RECORD_SIZE;

  std::vector<UserRecord, PsramAllocator<UserRecord>> loaded;
  loaded.reserve(numRecords);
  bool sawAnyContent = numRecords > 0;
  bool sawParseError = false;

  uint8_t rec[DB_RECORD_SIZE];
  for (size_t i = 0; i < numRecords; i++) {
    if (f.read(rec, DB_RECORD_SIZE) != (int)DB_RECORD_SIZE) { sawParseError = true; break; }

    UserRecord u;
    if (!decodeRecord_(rec, u)) { sawParseError = true; continue; }  // CRC/shape failure - skip this record only
    if (!isValidUidFormat(u.uid) || !isValidName(u.name)) { sawParseError = true; continue; }
    if (!isValidRegisteredDate(u.registered)) { sawParseError = true; continue; }
    if (!isValidValidDays(u.validDays)) { sawParseError = true; continue; }
    loaded.push_back(u);
  }
  f.close();

  // Fail only if the file had records but none were usable.
  if (sawAnyContent && loaded.empty() && sawParseError) {
    return false;
  }

  users_ = loaded;
  return true;
}

bool DatabaseManager::loadLegacyJsonAndMigrate_() {
  if (!LittleFS.exists(USERS_DB_LEGACY_JSON_PATH)) return false;

  File f = LittleFS.open(USERS_DB_LEGACY_JSON_PATH, "r");
  if (!f) return false;

  // One-time backup migration only: parse the legacy NDJSON format, then
  // rewrite it as users.bin.
  std::vector<UserRecord, PsramAllocator<UserRecord>> loaded;
  bool sawAnyContent = false;
  bool sawParseError = false;
  while (f.available()) {
    String line = f.readStringUntil('\n');
    line.trim();
    if (line.length() == 0) continue;
    sawAnyContent = true;

    JsonDocument doc;
    DeserializationError err = deserializeJson(doc, line);
    if (err || !doc.is<JsonObject>()) {
      sawParseError = true;
      continue;
    }
    JsonObject obj = doc.as<JsonObject>();
    if (!obj.containsKey("uid") || !obj.containsKey("name")) continue;

    UserRecord u;
    u.uid = normalizeUid(String((const char*)obj["uid"]));
    u.name = String((const char*)obj["name"]);
    u.registered = obj.containsKey("registered") ? String((const char*)obj["registered"]) : String("1970-01-01");
    u.validDays = obj.containsKey("valid_days") ? obj["valid_days"].as<double>() : 0.0;
    if (!isValidUidFormat(u.uid) || !isValidName(u.name)) continue;
    if (!isValidRegisteredDate(u.registered)) continue;
    if (!isValidValidDays(u.validDays)) continue;
    loaded.push_back(u);
  }
  f.close();

  if (sawAnyContent && loaded.empty() && sawParseError) {
    return false;  // legacy file itself was unreadable, caller wipes clean
  }

  users_ = loaded;

  // Persist in binary form, then keep the original as .bak so migration
  // never runs again but recovery remains possible.
  if (!save()) return false;
  LittleFS.remove(USERS_DB_LEGACY_JSON_PATH ".bak");  // stale backup from an earlier partial migration
  LittleFS.rename(USERS_DB_LEGACY_JSON_PATH, USERS_DB_LEGACY_JSON_PATH ".bak");
  return true;
}

void DatabaseManager::encodeRecord_(const UserRecord& u, uint8_t* rec) {
  size_t off = 0;

  // uid: hex -> raw bytes. Read off FixedStr's inline buffer directly --
  // do NOT assign to String (heap-allocates on SRAM per call).
  const char* uidC = u.uid.c_str();
  uint8_t uidLen = (uint8_t)(u.uid.length() / 2);
  rec[off++] = uidLen;
  uint8_t* uidBytes = &rec[off];
  memset(uidBytes, 0, DB_UID_BYTES);
  for (uint8_t b = 0; b < uidLen; b++) {
    char hi = uidC[b * 2], lo = uidC[b * 2 + 1];
    auto nib = [](char c) -> uint8_t { return (c <= '9') ? (c - '0') : (toupper(c) - 'A' + 10); };
    uidBytes[b] = (uint8_t)((nib(hi) << 4) | nib(lo));
  }
  off += DB_UID_BYTES;

  uint8_t nameLen = (uint8_t)u.name.length();  // isValidName() caps this at MAX_NAME_LEN
  rec[off++] = nameLen;
  uint8_t* nameBytes = &rec[off];
  memset(nameBytes, 0, MAX_NAME_LEN);
  memcpy(nameBytes, u.name.c_str(), nameLen);
  off += MAX_NAME_LEN;

  uint16_t regDays = dateToDays_(u.registered.c_str());
  rec[off++] = (uint8_t)(regDays & 0xFF);
  rec[off++] = (uint8_t)((regDays >> 8) & 0xFF);

  memcpy(&rec[off], &u.validDays, sizeof(double));
  off += sizeof(double);

  uint32_t crc = crc32_(rec, off);
  rec[off++] = (uint8_t)(crc & 0xFF);
  rec[off++] = (uint8_t)((crc >> 8) & 0xFF);
  rec[off++] = (uint8_t)((crc >> 16) & 0xFF);
  rec[off++] = (uint8_t)((crc >> 24) & 0xFF);
}

// Inverse of encodeRecord_(). Returns false on CRC mismatch or bad
// uidLen/nameLen -- caller skips this record. Content validation
// (uid/name/date format) is still the caller's job.
bool DatabaseManager::decodeRecord_(const uint8_t* rec, UserRecord& out) {
  uint32_t storedCrc = (uint32_t)rec[DB_RECORD_SIZE - 4] |
                        ((uint32_t)rec[DB_RECORD_SIZE - 3] << 8) |
                        ((uint32_t)rec[DB_RECORD_SIZE - 2] << 16) |
                        ((uint32_t)rec[DB_RECORD_SIZE - 1] << 24);
  uint32_t actualCrc = crc32_(rec, DB_RECORD_SIZE - 4);
  if (storedCrc != actualCrc) return false;

  size_t off = 0;
  uint8_t uidLen = rec[off++];
  const uint8_t* uidBytes = &rec[off]; off += DB_UID_BYTES;
  uint8_t nameLen = rec[off++];
  const uint8_t* nameBytes = &rec[off]; off += MAX_NAME_LEN;
  uint16_t regDays = (uint16_t)rec[off] | ((uint16_t)rec[off + 1] << 8); off += 2;
  double validDays; memcpy(&validDays, &rec[off], sizeof(double));

  if (uidLen > DB_UID_BYTES || nameLen > MAX_NAME_LEN) return false;

  static const char kHex[] = "0123456789ABCDEF";
  char uidHex[MAX_UID_HEX_LEN + 1];
  for (size_t b = 0; b < uidLen; b++) {
    uidHex[b * 2]     = kHex[(uidBytes[b] >> 4) & 0xF];
    uidHex[b * 2 + 1] = kHex[uidBytes[b] & 0xF];
  }
  uidHex[uidLen * 2] = '\0';

  out.uid = String(uidHex);
  char nameBuf[MAX_NAME_LEN + 1];
  memcpy(nameBuf, nameBytes, nameLen);
  nameBuf[nameLen] = '\0';
  out.name = String(nameBuf);
  out.registered = daysToDate_(regDays);
  out.validDays = validDays;
  return true;
}

size_t DatabaseManager::recordSize() {
  return DB_RECORD_SIZE;
}

void DatabaseManager::encodeUserAt(size_t i, uint8_t* outRec) const {
  encodeRecord_(users_[i], outRec);
}

bool DatabaseManager::addUserFromRawRecord(const uint8_t* rec, String& errorOut) {
  UserRecord u;
  if (!decodeRecord_(rec, u)) {
    errorOut = "Corrupt record (CRC32 mismatch or invalid length)";
    return false;
  }
  // Reuse addUserNoSave() for validation/dedup (shared with batch_add).
  return addUserNoSave(u.uid, u.name, u.registered, u.validDays, errorOut);
}

size_t DatabaseManager::manifestEntrySize() {
  return 1 + DB_UID_BYTES + 4;
}

size_t DatabaseManager::uidEntrySize() {
  return 1 + DB_UID_BYTES;
}

void DatabaseManager::encodeManifestEntryAt(size_t i, uint8_t* outEntry) const {
  // Reuse encodeRecord_ for uid bytes and CRC. Layout: [0]=uidLen,
  // [1..DB_UID_BYTES]=uidBytes, [-4:]=per-record CRC32.
  uint8_t rec[DB_RECORD_SIZE];
  encodeRecord_(users_[i], rec);
  memcpy(outEntry, rec, 1 + DB_UID_BYTES);
  memcpy(outEntry + 1 + DB_UID_BYTES, rec + DB_RECORD_SIZE - 4, 4);
}

void DatabaseManager::beginRemoveBatch() {
  removeTombstones_.assign(users_.size(), false);
  removeBatchActive_ = true;
}

size_t DatabaseManager::endRemoveBatch() {
  if (!removeBatchActive_) return 0;
  size_t writeIdx = 0;
  size_t removedCount = 0;
  for (size_t readIdx = 0; readIdx < users_.size(); readIdx++) {
    bool tomb = readIdx < removeTombstones_.size() && removeTombstones_[readIdx];
    if (tomb) {
      removedCount++;
    } else {
      if (writeIdx != readIdx) users_[writeIdx] = users_[readIdx];
      writeIdx++;
    }
    if ((readIdx & 0xFF) == 0xFF) esp_task_wdt_reset();
  }
  users_.resize(writeIdx);
  removeTombstones_.clear();
  removeTombstones_.shrink_to_fit();
  removeBatchActive_ = false;
  return removedCount;
}

bool DatabaseManager::removeUserRawNoSave(const uint8_t* uidEntry, String& errorOut) {
  uint8_t uidLen = uidEntry[0];
  if (uidLen == 0 || uidLen > DB_UID_BYTES) {
    errorOut = "Corrupt remove entry (bad uidLen)";
    return false;
  }
  static const char kHex[] = "0123456789ABCDEF";
  char uidHex[MAX_UID_HEX_LEN + 1];
  for (uint8_t b = 0; b < uidLen; b++) {
    uint8_t byte = uidEntry[1 + b];
    uidHex[b * 2]     = kHex[(byte >> 4) & 0xF];
    uidHex[b * 2 + 1] = kHex[byte & 0xF];
  }
  uidHex[uidLen * 2] = '\0';
  String uid(uidHex);

  int idx = indexOfUid_(uid);
  if (idx < 0) { errorOut = "UID not found: " + uid; return false; }

  if (removeBatchActive_) {
    // Tombstone rather than erase: keeps users_ and every index found
    // during the batch stable until endRemoveBatch() compacts. Re-marking
    // a tombstone is harmless; the host normally dedupes the diff.
    if ((size_t)idx < removeTombstones_.size()) removeTombstones_[idx] = true;
    return true;
  }
  users_.erase(users_.begin() + idx);
  return true;
}

bool DatabaseManager::replaceUserFromRawRecord(const uint8_t* rec, String& errorOut) {
  UserRecord u;
  if (!decodeRecord_(rec, u)) {
    errorOut = "Corrupt record (CRC32 mismatch or invalid length)";
    return false;
  }
  if (!isValidUidFormat(u.uid) || !isValidName(u.name) ||
      !isValidRegisteredDate(u.registered) || !isValidValidDays(u.validDays)) {
    errorOut = "Invalid decoded record contents";
    return false;
  }

  String uid = u.uid;
  int idx = indexOfUid_(uid);
  if (idx < 0) { errorOut = "Replace target not found: " + uid; return false; }
  users_[idx] = u;  // uid unchanged -- sort position/index unaffected
  return true;
}

bool DatabaseManager::save(SaveTiming* timing) {
  // Remove from TWDT during save() -- a single LittleFS.open()/write()
  // can block past the 5s WDT timeout (same issue as begin() + format).
  esp_err_t wdtErr = esp_task_wdt_delete(NULL);
  struct WdtGuard {
    bool reAdd;
    ~WdtGuard() { if (reAdd) esp_task_wdt_add(NULL); }
  } wdtGuard{wdtErr == ESP_OK};

  // Defensive: re-sort if import mode left users_ unsorted. O(n) in the
  // already-sorted common case.
  if (!std::is_sorted(users_.begin(), users_.end(), uidLess_)) {
    std::sort(users_.begin(), users_.end(), uidLess_);
  }

  // Full rewrite. Used for bulk ops; single-record ops use
  // saveSingleRecord_() / saveSuffixFrom_() instead.
  File f = LittleFS.open(USERS_DB_TMP_PATH, "w");
  if (!f) return false;

  uint8_t header[DB_HEADER_SIZE];
  memcpy(header, DB_MAGIC, 4);
  header[4] = DB_VERSION;
  header[5] = (uint8_t)(DB_RECORD_SIZE & 0xFF);
  header[6] = (uint8_t)((DB_RECORD_SIZE >> 8) & 0xFF);
  if (f.write(header, DB_HEADER_SIZE) != DB_HEADER_SIZE) { f.close(); return false; }

  // Chunked flush: ~110 records fit in 8KB. PSRAM-allocated (must not be
  // on stack -- overflows the 8KB Arduino loop task).
  static const size_t kRecordsPerFlush = 8192 / DB_RECORD_SIZE;
  static const size_t kFlushBytes = kRecordsPerFlush * DB_RECORD_SIZE;
  uint8_t* buf = (uint8_t*)heap_caps_malloc(kFlushBytes, MALLOC_CAP_SPIRAM);
  if (!buf) { f.close(); return false; }
  size_t bufLen = 0;

  // RAII guard: frees buf on every early return from the write loop.
  struct BufGuard {
    uint8_t* p;
    ~BufGuard() { if (p) heap_caps_free(p); }
  } bufGuard{buf};

  for (size_t i = 0; i < users_.size(); i++) {
    unsigned long encodeStart = micros();
    encodeRecord_(users_[i], &buf[bufLen]);
    if (timing) timing->encodeUs += micros() - encodeStart;

    bufLen += DB_RECORD_SIZE;
    if (bufLen >= kFlushBytes) {
      unsigned long writeStart = micros();
      bool wok = (f.write(buf, bufLen) == bufLen);
      if (timing) timing->writeUs += micros() - writeStart;
      if (!wok) { f.close(); return false; }
      bufLen = 0;
    }
  }
  if (bufLen > 0) {
    unsigned long writeStart = micros();
    bool wok = (f.write(buf, bufLen) == bufLen);
    if (timing) timing->writeUs += micros() - writeStart;
    if (!wok) { f.close(); return false; }
  }
  f.close();

  // Verify by re-reading before touching the canonical file. Catches a
  // silently short/garbled write (buffer bug, flash wear-out, etc.)
  // *before* we commit to it -- otherwise a bad tmp file would still pass
  // the rename below and become the new users.bin.
  {
    unsigned long verifyStart = micros();
    File vf = LittleFS.open(USERS_DB_TMP_PATH, "r");
    if (!vf) return false;
    uint8_t vHeader[DB_HEADER_SIZE];
    if (vf.read(vHeader, DB_HEADER_SIZE) != (int)DB_HEADER_SIZE ||
        memcmp(vHeader, header, DB_HEADER_SIZE) != 0) {
      vf.close();
      return false;
    }
    uint32_t actualCrc = 0;
    uint8_t rec[DB_RECORD_SIZE];
    bool readOk = true;
    for (size_t i = 0; i < users_.size(); i++) {
      if (vf.read(rec, DB_RECORD_SIZE) != (int)DB_RECORD_SIZE) { readOk = false; break; }
      actualCrc = crc32_(rec, DB_RECORD_SIZE - 4, actualCrc);
      // No TWDT reset: WdtGuard above unsubscribed this task for the
      // duration of save().
    }
    vf.close();
    if (timing) timing->verifyUs += micros() - verifyStart;
    if (!readOk || actualCrc != computeCrc32()) return false;
  }

  unsigned long finalizeStart = micros();
  // On this port rename() atomically replaces the destination, so there
  // is no window without a valid users.bin. Fall back to remove+rename
  // only if the direct rename is rejected; that path can lose the file.
  bool ok = LittleFS.rename(USERS_DB_TMP_PATH, USERS_DB_PATH);
  if (!ok) {
    LittleFS.remove(USERS_DB_PATH);
    ok = LittleFS.rename(USERS_DB_TMP_PATH, USERS_DB_PATH);
  }
  if (timing) timing->finalizeUs += micros() - finalizeStart;
  return ok;
}

bool DatabaseManager::saveSingleRecord_(size_t idx) {
  // Single seek+write for rename/renew (same uid, same position).
  if (!LittleFS.exists(USERS_DB_PATH)) return save();

  // "r+": read/write without truncating. Falls back to full save() if
  // this open mode isn't supported by the target core version.
  File f = LittleFS.open(USERS_DB_PATH, "r+");
  if (!f) return save();

  size_t offset = DB_HEADER_SIZE + idx * DB_RECORD_SIZE;
  if (!f.seek(offset)) { f.close(); return save(); }

  uint8_t rec[DB_RECORD_SIZE];
  encodeRecord_(users_[idx], rec);
  bool ok = (f.write(rec, DB_RECORD_SIZE) == DB_RECORD_SIZE);
  f.close();
  return ok ? true : save();
}

bool DatabaseManager::saveSuffixFrom_(size_t startIdx) {
  if (!LittleFS.exists(USERS_DB_PATH)) return save();

  File f = LittleFS.open(USERS_DB_PATH, "r+");  // see saveSingleRecord_'s
                                                 // comment on "r+" above.
  if (!f) return save();

  // If users_ shrunk (removeUser), we can't truncate in place -- LittleFS
  // fs::File has no truncate(). Fall back to full save() for removals.
  // Additions (growth) still get the suffix-only rewrite.
  size_t curSize = f.size();
  size_t newFileSize = DB_HEADER_SIZE + users_.size() * DB_RECORD_SIZE;
  if (newFileSize < curSize) {
    f.close();
    return save();
  }

  size_t offset = DB_HEADER_SIZE + startIdx * DB_RECORD_SIZE;
  if (!f.seek(offset)) { f.close(); return save(); }

  // Unsubscribe only once the write loop is reached; earlier paths defer
  // to save(), which manages the watchdog itself. Double unsubscribing
  // logs an error that can corrupt the JSON protocol on the shared UART.
  esp_err_t wdtErr = esp_task_wdt_delete(NULL);
  struct WdtGuard {
    bool reAdd;
    ~WdtGuard() { if (reAdd) esp_task_wdt_add(NULL); }
  } wdtGuard{wdtErr == ESP_OK};

  static const size_t kRecordsPerFlush = 8192 / DB_RECORD_SIZE;
  static const size_t kFlushBytes = kRecordsPerFlush * DB_RECORD_SIZE;
  uint8_t* buf = (uint8_t*)heap_caps_malloc(kFlushBytes, MALLOC_CAP_SPIRAM);
  if (!buf) {
    f.close();
    if (wdtGuard.reAdd) { esp_task_wdt_add(NULL); wdtGuard.reAdd = false; }
    return save();
  }
  struct BufGuard {
    uint8_t* p;
    ~BufGuard() { if (p) heap_caps_free(p); }
  } bufGuard{buf};

  size_t bufLen = 0;
  bool writeFailed = false;
  for (size_t i = startIdx; i < users_.size(); i++) {
    encodeRecord_(users_[i], &buf[bufLen]);
    bufLen += DB_RECORD_SIZE;
    if (bufLen >= kFlushBytes) {
      if (f.write(buf, bufLen) != bufLen) { writeFailed = true; break; }
      bufLen = 0;
    }
  }
  if (!writeFailed && bufLen > 0) {
    if (f.write(buf, bufLen) != bufLen) writeFailed = true;
  }
  f.close();
  if (writeFailed) {
    if (wdtGuard.reAdd) { esp_task_wdt_add(NULL); wdtGuard.reAdd = false; }
    return save();
  }
  return true;
}

bool DatabaseManager::existsInBaseline_(const UidKey& norm) const {
  // Binary search restricted to [0, importBaselineCount_) -- see the
  // member comment on importBaselineCount_ in the header for why the
  // search can't safely cover the whole array during an import.
  size_t lo = 0, hi = importBaselineCount_;
  while (lo < hi) {
    size_t mid = lo + (hi - lo) / 2;
    int c = strcmp(users_[mid].uid.c_str(), norm.c_str());
    if (c < 0) lo = mid + 1; else hi = mid;
  }
  return lo < importBaselineCount_ && users_[lo].uid == norm;
}

int DatabaseManager::indexOfUid_(const String& uid) const {
  String norm = normalizeUid(uid);
  size_t lo = 0, hi = users_.size();
  while (lo < hi) {
    size_t mid = lo + (hi - lo) / 2;
    int c = strcmp(users_[mid].uid.c_str(), norm.c_str());
    if (c == 0) return (int)mid;
    if (c < 0) lo = mid + 1; else hi = mid;
  }
  return -1;
}

size_t DatabaseManager::lowerBound_(const String& normUid) const {
  size_t lo = 0, hi = users_.size();
  while (lo < hi) {
    size_t mid = lo + (hi - lo) / 2;
    if (strcmp(users_[mid].uid.c_str(), normUid.c_str()) < 0) lo = mid + 1; else hi = mid;
  }
  return lo;
}

bool DatabaseManager::addUser(const String& uid, const String& name, const String& registered,
                               double validDays, String& errorOut) {
  String norm = normalizeUid(uid);
  if (!isValidUidFormat(norm)) { errorOut = "Invalid UID format"; return false; }
  if (!isValidName(name)) { errorOut = "Invalid or empty name"; return false; }
  if (!isValidRegisteredDate(registered)) { errorOut = "Invalid 'registered' date (expected YYYY-MM-DD)"; return false; }
  if (!isValidValidDays(validDays)) { errorOut = "Invalid 'valid_days' (must be a non-negative number)"; return false; }
  if (users_.size() >= MAX_USERS) { errorOut = "Database full (max " + String(MAX_USERS) + " users)"; return false; }

  size_t pos = lowerBound_(norm);
  if (pos < users_.size() && users_[pos].uid == norm) { errorOut = "Duplicate UID"; return false; }

  UserRecord u;
  u.uid = norm;
  u.name = name;
  u.registered = registered;
  u.validDays = validDays;
  users_.insert(users_.begin() + pos, u);

  if (!importMode_) {
    // Everything from pos onward moved to a new offset; rewrite only that
    // suffix instead of the whole file.
    if (!saveSuffixFrom_(pos)) {
      errorOut = "Failed to persist database";
      users_.erase(users_.begin() + pos);
      return false;
    }
  }
  return true;
}

bool DatabaseManager::removeUser(const String& uid, String& errorOut) {
  int idx = indexOfUid_(uid);
  if (idx < 0) { errorOut = "UID not found"; return false; }

  UserRecord backup = users_[idx];
  users_.erase(users_.begin() + idx);

  // Everyone after idx moved up one slot. LittleFS cannot truncate, so
  // saveSuffixFrom_ falls back to a full save() for the shorter file.
  if (!saveSuffixFrom_((size_t)idx)) {
    errorOut = "Failed to persist database";
    users_.insert(users_.begin() + idx, backup);
    return false;
  }
  return true;
}

bool DatabaseManager::removeAllExcept(const std::vector<String>& keepUids, size_t& removedCountOut,
                                       String& errorOut) {
  removedCountOut = 0;

  if (keepUids.empty()) {
    errorOut = "Keep list is empty -- use clear_all to wipe every user";
    return false;
  }

  // Validate keep UIDs up front. Fail closed on bad format (typo = silent
  // user deletion otherwise).
  std::vector<String> keepNorm;
  keepNorm.reserve(keepUids.size());
  for (const auto& raw : keepUids) {
    String norm = normalizeUid(raw);
    if (!isValidUidFormat(norm)) {
      errorOut = "Invalid UID format in keep list: " + raw;
      return false;
    }
    keepNorm.push_back(norm);
  }

  std::vector<UserRecord, PsramAllocator<UserRecord>> backup = users_;

  // Sort + binary search: O(n log k), with watchdog feeds for large
  // keep lists on a full 70k-user database.
  std::sort(keepNorm.begin(), keepNorm.end(),
            [](const String& a, const String& b) { return strcmp(a.c_str(), b.c_str()) < 0; });
  std::vector<UserRecord, PsramAllocator<UserRecord>> kept;
  kept.reserve(users_.size());
  size_t scanned = 0;
  for (const auto& u : users_) {
    bool keep = std::binary_search(keepNorm.begin(), keepNorm.end(), String(u.uid.c_str()),
        [](const String& a, const String& b) { return strcmp(a.c_str(), b.c_str()) < 0; });
    if (keep) kept.push_back(u);  // users_ is sorted, so kept stays sorted.
    if ((++scanned & 0xFF) == 0) esp_task_wdt_reset();
  }

  removedCountOut = users_.size() - kept.size();
  users_ = kept;

  // Bulk op: most records move, so save() rewrites the whole file.
  if (!save()) {
    errorOut = "Failed to persist database";
    users_ = backup;
    removedCountOut = 0;
    return false;
  }
  return true;
}

bool DatabaseManager::clearAll(String& errorOut) {
  // Snapshot in RAM so we can roll back atomically if the save fails --
  // same shape as removeUser/renameUser, just at the whole-database scale.
  std::vector<UserRecord, PsramAllocator<UserRecord>> backup;
  backup.swap(users_);

  if (!save()) {
    errorOut = "Failed to persist database";
    backup.swap(users_);  // restore
    return false;
  }
  return true;
}

bool DatabaseManager::renameUser(const String& uid, const String& newName, String& errorOut) {
  if (!isValidName(newName)) { errorOut = "Invalid or empty name"; return false; }
  int idx = indexOfUid_(uid);
  if (idx < 0) { errorOut = "UID not found"; return false; }

  String oldName = users_[idx].name;
  users_[idx].name = newName;

  // Renaming never changes uid, so idx's sort position and file offset
  // are unchanged -- this is the true single seek+write case.
  if (!saveSingleRecord_((size_t)idx)) {
    errorOut = "Failed to persist database";
    users_[idx].name = oldName;
    return false;
  }
  return true;
}

bool DatabaseManager::findUser(const String& uid, UserRecord& outUser) const {
  int idx = indexOfUid_(uid);
  if (idx < 0) return false;
  outUser = users_[idx];
  return true;
}

bool DatabaseManager::renewUser(const String& uid, const String& today,
                                double validDays, String& errorOut) {
  String norm = normalizeUid(uid);
  int idx = indexOfUid_(norm);
  if (idx < 0) { errorOut = "UID not found"; return false; }

  String oldRegistered = users_[idx].registered;
  double oldValidDays = users_[idx].validDays;
  users_[idx].registered = today;
  users_[idx].validDays = validDays;

  // Renewal changes registered/validDays only, never uid -- same
  // single-seek+write case as renameUser().
  if (!saveSingleRecord_((size_t)idx)) {
    errorOut = "Failed to persist database";
    users_[idx].registered = oldRegistered;
    users_[idx].validDays = oldValidDays;
    return false;
  }
  return true;
}

uint32_t DatabaseManager::crc32_(const uint8_t* data, size_t len, uint32_t crc) {
  static uint32_t table[256];
  static bool tableReady = false;
  if (!tableReady) {
    for (uint32_t i = 0; i < 256; i++) {
      uint32_t c = i;
      for (int k = 0; k < 8; k++) c = (c & 1) ? (0xEDB88320u ^ (c >> 1)) : (c >> 1);
      table[i] = c;
    }
    tableReady = true;
  }
  // crc=0 is one-shot; passing a previous return continues the CRC
  // (zlib-compatible chaining).
  crc = crc ^ 0xFFFFFFFFu;
  for (size_t i = 0; i < len; i++) crc = table[(crc ^ data[i]) & 0xFF] ^ (crc >> 8);
  return crc ^ 0xFFFFFFFFu;
}

uint32_t DatabaseManager::computeCrc32() const {
  // Canonical DB checksum: CRC32 chained over each record's payload in
  // sorted order. Exclude the per-record trailing CRC (hash only the
  // first DB_RECORD_SIZE - 4 bytes): chaining across CRC "codewords"
  // collapses to a value that depends on the record count, not content.
  // Must stay identical to cli::convert::canonical_crc32().
  uint8_t rec[DB_RECORD_SIZE];
  uint32_t crc = 0;
  for (size_t i = 0; i < users_.size(); i++) {
    encodeRecord_(users_[i], rec);
    crc = crc32_(rec, DB_RECORD_SIZE - 4, crc);
  }
  return crc;
}

// Howard Hinnant's days_from_civil:
// https://howardhinnant.github.io/date_algorithms.html
// Proleptic Gregorian, days since 1970-01-01. Does not validate calendar
// dates; 2024-02-30 maps to a day and does not round-trip.
static int32_t daysFromCivil_(int y, unsigned m, unsigned d) {
  y -= m <= 2;
  const int32_t era = (y >= 0 ? y : y - 399) / 400;
  const unsigned yoe = (unsigned)(y - era * 400);                 // [0, 399]
  const unsigned doy = (153 * (m + (m > 2 ? -3 : 9)) + 2) / 5 + d - 1;  // [0, 365]
  const unsigned doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;      // [0, 146096]
  return era * 146097 + (int32_t)doe - 719468;
}

static void civilFromDays_(int32_t z, int& y, unsigned& m, unsigned& d) {
  z += 719468;
  const int32_t era = (z >= 0 ? z : z - 146096) / 146097;
  const unsigned doe = (unsigned)(z - era * 146097);                        // [0, 146096]
  const unsigned yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // [0, 399]
  const int32_t yy = (int32_t)yoe + era * 400;
  const unsigned doy = doe - (365 * yoe + yoe / 4 - yoe / 100);   // [0, 365]
  const unsigned mp = (5 * doy + 2) / 153;                        // [0, 11]
  d = doy - (153 * mp + 2) / 5 + 1;                                // [1, 31]
  m = mp + (mp < 10 ? 3 : -9);                                     // [1, 12]
  y = yy + (m <= 2);
}

uint16_t DatabaseManager::dateToDays_(const char* registered) {
  if (registered[0] == '\0') return DB_ADMIN_DAYS_SENTINEL;  // ADMIN_REGISTERED
  // Manual digit parsing avoids a String allocation; shape is already
  // validated by isValidRegisteredDate().
  int y = (registered[0]-'0')*1000 + (registered[1]-'0')*100 + (registered[2]-'0')*10 + (registered[3]-'0');
  unsigned mo = (unsigned)((registered[5]-'0')*10 + (registered[6]-'0'));
  unsigned d  = (unsigned)((registered[8]-'0')*10 + (registered[9]-'0'));
  int32_t days = daysFromCivil_(y, mo, d);
  // Clamp to uint16 range (65535 = admin sentinel). Fails safe.
  if (days < 0) days = 0;
  if (days > 65534) days = 65534;
  return (uint16_t)days;
}

String DatabaseManager::daysToDate_(uint16_t days) {
  if (days == DB_ADMIN_DAYS_SENTINEL) return String("");
  int y; unsigned mo, d;
  civilFromDays_((int32_t)days, y, mo, d);
  char buf[11];
  snprintf(buf, sizeof(buf), "%04d-%02u-%02u", y, mo, d);
  return String(buf);
}

String DatabaseManager::normalizeUid(const String& rawUid) {
  String out;
  out.reserve(rawUid.length());
  for (size_t i = 0; i < rawUid.length(); i++) {
    char c = rawUid[i];
    if (isxdigit((unsigned char)c)) out += (char)toupper(c);
  }
  return out;
}

bool DatabaseManager::isValidUidFormat(const String& uid) {
  if (uid.length() < MIN_UID_HEX_LEN || uid.length() > MAX_UID_HEX_LEN) return false;
  if (uid.length() % 2 != 0) return false; // must be whole bytes
  for (size_t i = 0; i < uid.length(); i++) {
    if (!isxdigit((unsigned char)uid[i])) return false;
  }
  return true;
}

const char* DatabaseManager::dbPath() const {
  return USERS_DB_PATH;
}

size_t DatabaseManager::fsTotalBytes() const {
  return LittleFS.totalBytes();
}

size_t DatabaseManager::fsUsedBytes() const {
  return LittleFS.usedBytes();
}

bool DatabaseManager::isValidName(const String& name) {
  if (name.length() == 0 || name.length() > MAX_NAME_LEN) return false;
  bool sawNonSpace = false;
  for (size_t i = 0; i < name.length(); i++) {
    unsigned char c = (unsigned char)name[i];
    // Reject control chars (garble LCD). '"' and '\' are allowed,
    // escaped by jsonEscape() at write sites.
    if (c < 0x20) return false;
    if (!isspace(c)) sawNonSpace = true;
  }
  return sawNonSpace;
}

String DatabaseManager::jsonEscape(const String& in) {
  String out;
  out.reserve(in.length() + 8);
  for (size_t i = 0; i < in.length(); i++) {
    char c = in[i];
    switch (c) {
      case '"':  out += "\\\""; break;
      case '\\': out += "\\\\"; break;
      case '\n': out += "\\n"; break;
      case '\r': out += "\\r"; break;
      case '\t': out += "\\t"; break;
      default:
        if ((unsigned char)c < 0x20) {
          char buf[7];
          snprintf(buf, sizeof(buf), "\\u%04x", (unsigned char)c);
          out += buf;
        } else {
          out += c;
        }
    }
  }
  return out;
}

bool DatabaseManager::isValidRegisteredDate(const String& registered) {
  // Empty means "no registration date"; the CLI sends this for admin
  // badges when `add` has no --valid-days.
  if (registered.length() == 0) return true;

  // Strict YYYY-MM-DD shape check, not a full calendar validator. The
  // host CLI is the only writer and emits ISO dates.
  if (registered.length() != REGISTERED_DATE_LEN) return false;
  for (int i = 0; i < REGISTERED_DATE_LEN; i++) {
    char c = registered[i];
    if (i == 4 || i == 7) {
      if (c != '-') return false;
    } else {
      if (!isdigit((unsigned char)c)) return false;
    }
  }
  int month = registered.substring(5, 7).toInt();
  int day = registered.substring(8, 10).toInt();
  if (month < 1 || month > 12) return false;
  if (day < 1 || day > 31) return false;
  return true;
}

bool DatabaseManager::isValidValidDays(double validDays) {
  if (isnan(validDays) || isinf(validDays)) return false;
  // Admin sentinel: -1.0 means "never expires". Any other negative value
  // is still rejected.
  if (validDays == ADMIN_VALID_DAYS) return true;
  return validDays >= 0.0;
}

void DatabaseManager::reserveForImport(size_t additionalUsers) {
  users_.reserve(users_.size() + additionalUsers);
}

void DatabaseManager::setImportMode(bool on) {
  importMode_ = on;
  if (on) {
    importBaselineCount_ = users_.size();
    importSeen_.clear();
  } else {
    // Re-sort in one O(n log n) pass instead of O(n^2) per-insertion sort.
    // Caller's save() then does one full already-in-order rewrite.
    std::sort(users_.begin(), users_.end(), uidLess_);
    importSeen_.clear();
    importBaselineCount_ = 0;
  }
}

bool DatabaseManager::addUserNoSave(const String& uid, const String& name,
                                    const String& registered, double validDays,
                                    String& errorOut) {
  String norm = normalizeUid(uid);
  if (!isValidUidFormat(norm)) { errorOut = "Invalid UID format"; return false; }
  if (!isValidName(name)) { errorOut = "Invalid or empty name"; return false; }
  if (!isValidRegisteredDate(registered)) { errorOut = "Invalid 'registered' date (expected YYYY-MM-DD)"; return false; }
  if (!isValidValidDays(validDays)) { errorOut = "Invalid 'valid_days' (must be a non-negative number)"; return false; }
  if (users_.size() >= MAX_USERS) { errorOut = "Database full (max " + String(MAX_USERS) + " users)"; return false; }

  UidKey key = norm;
  if (existsInBaseline_(key) || importSeen_.count(key)) {
    errorOut = "Duplicate UID";
    return false;
  }

  UserRecord u;
  u.uid = norm;
  u.name = name;
  u.registered = registered;
  u.validDays = validDays;
  users_.push_back(u);  // deliberately unsorted here -- see setImportMode()
  importSeen_.insert(key);
  return true;
}
