#include "SerialProtocol.h"
#include "DatabaseManager.h"
#include "NetworkManager.h"
#include "Config.h"
#include "ImportProfiler.h"
#include "SyncProfiler.h"
#include <esp_task_wdt.h>
#include <algorithm>

// Print a double as a valid, round-trippable JSON number. Arduino's
// Print::print(double) rounds to 2 decimals and emits nan/inf, neither of
// which is acceptable in JSON.
static void printJsonDouble(double v) {
  if (isnan(v) || isinf(v)) { Serial.print('0'); return; }
  char buf[32];
  snprintf(buf, sizeof(buf), "%.15g", v);
  if (strtod(buf, nullptr) != v) snprintf(buf, sizeof(buf), "%.17g", v);
  Serial.print(buf);
}

void SerialProtocol::begin(SerialMessageHandler handler) {
  handler_ = handler;
  lineLen_ = 0;
}

void SerialProtocol::poll() {
  while (Serial.available() > 0) {
    char c = (char)Serial.read();

    if (c == '\n') {
      lineBuf_[lineLen_] = '\0';
      if (lineLen_ > 0) {
        handleLine_(lineBuf_);
      }
      lineLen_ = 0;
      continue;
    }
    if (c == '\r') continue; // tolerate CRLF

    if (lineLen_ < kLineBufCapacity - 1) {
      lineBuf_[lineLen_++] = c;
    } else {
      // Line too long -- drop it to avoid a stuck buffer / memory issue.
      lineLen_ = 0;
      sendError("Message too long");
    }
  }
}

void SerialProtocol::handleLine_(char* line) {
  // Transport-wait: time since last response was sent (see ImportProfiler.h).
  if (g_importProfile.lastResponseSentUs != 0) {
    unsigned long gap = micros() - g_importProfile.lastResponseSentUs;
    g_importProfile.transportWaitUs += gap;
    g_importProfile.transportWaitCount++;
  }

  // Mutable char* enables ArduinoJson zero-copy: strings point into the
  // line buffer instead of being duplicated, which matters for large batches.
  DynamicJsonDocument doc(kLineBufCapacity);
  DeserializationError err;
  {
    ScopedMicroTimer t(g_importProfile.jsonParseUs);
    err = deserializeJson(doc, line);
  }
  g_importProfile.jsonParseCount++;

  if (err) {
    sendError(String("Malformed JSON: ") + err.c_str() + " len=" + String(strlen(line)));
    g_importProfile.lastResponseSentUs = micros();
    return;
  }
  if (!doc.containsKey("type")) {
    sendError("Missing 'type' field");
    g_importProfile.lastResponseSentUs = micros();
    return;
  }

  if (handler_) handler_(doc);
  g_importProfile.lastResponseSentUs = micros();  // reply sent
}

void SerialProtocol::sendOk() {
  Serial.println("{\"status\":\"ok\"}");
}

void SerialProtocol::sendError(const String& message) {
  DynamicJsonDocument doc(256);
  doc["status"] = "error";
  doc["message"] = message;
  serializeJson(doc, Serial);
  Serial.println();
}

void SerialProtocol::sendUidDetected(const String& uid) {
  DynamicJsonDocument doc(256);
  doc["status"] = "ok";
  doc["type"] = "uid_detected";
  doc["uid"] = uid;
  serializeJson(doc, Serial);
  Serial.println();
}

void SerialProtocol::sendRemovedCount(size_t removedCount) {
  DynamicJsonDocument doc(256);
  doc["status"] = "ok";
  doc["type"] = "remove_all_except";
  doc["removed_count"] = removedCount;
  serializeJson(doc, Serial);
  Serial.println();
}

void SerialProtocol::sendWifiResult(bool connected, const String& message) {
  DynamicJsonDocument doc(256);
  doc["status"] = connected ? "ok" : "error";
  doc["type"] = "wifi_status";
  doc["connected"] = connected;
  doc["message"] = message;
  serializeJson(doc, Serial);
  Serial.println();
}

void SerialProtocol::sendNtpSyncResult(bool synced, const String& message) {
  DynamicJsonDocument doc(256);
  doc["status"] = synced ? "ok" : "error";
  doc["type"] = "ntp_sync";
  doc["synced"] = synced;
  doc["message"] = message;
  serializeJson(doc, Serial);
  Serial.println();
}

void SerialProtocol::sendStatus(DatabaseManager& db) {
  DynamicJsonDocument doc(640);
  doc["status"] = "ok";
  doc["type"] = "status";
  doc["firmware_version"] = kFirmwareVersion;
  doc["protocol_version"] = kProtocolVersion;
  doc["db_path"] = db.dbPath();
  size_t total = db.fsTotalBytes();
  size_t used = db.fsUsedBytes();
  doc["fs_total_bytes"] = total;
  doc["fs_used_bytes"] = used;
  doc["fs_free_bytes"] = (total >= used) ? (total - used) : 0;
  doc["user_count"] = db.userCount();
  // db_crc32: see DatabaseManager::computeCrc32().
  doc["db_crc32"] = db.computeCrc32();
  serializeJson(doc, Serial);
  Serial.println();
}

void SerialProtocol::sendUserList(DatabaseManager& db) {
  // Stream the response directly to Serial, one user at a time.
  // Avoids a single huge DynamicJsonDocument (~500KB for 4500 users).
  Serial.print("{\"status\":\"ok\",\"users\":[");
  for (size_t i = 0; i < db.userCount(); i++) {
    if (i > 0) Serial.print(',');
    const auto& u = db.userAt(i);
    Serial.print("{\"uid\":\"");
    Serial.print(u.uid); // hex-only, never needs escaping
    Serial.print("\",\"name\":\"");
    Serial.print(DatabaseManager::jsonEscape(u.name));
    Serial.print("\",\"registered\":\"");
    Serial.print(u.registered);
    Serial.print("\",\"valid_days\":");
    printJsonDouble(u.validDays);
    Serial.print('}');
    // Feed watchdog -- this loop blocks loop() for large lists.
    if ((i & 0xFF) == 0xFF) {
      esp_task_wdt_reset();
    }
  }
  Serial.println("]}");
}

void SerialProtocol::sendFindNameResult(DatabaseManager& db, const String& queryLower) {
  // Pass 1: find matching indices (scan_us measures this).
  std::vector<size_t> matches;
  uint32_t scanStart = micros();
  for (size_t i = 0; i < db.userCount(); i++) {
    String nameLower = String(db.userAt(i).name);
    nameLower.toLowerCase();
    if (nameLower.indexOf(queryLower) >= 0) matches.push_back(i);
    if ((i & 0xFF) == 0xFF) esp_task_wdt_reset();
  }
  uint32_t scanUs = micros() - scanStart;

  // Pass 2: stream only matches, so wire cost scales with match count.
  Serial.print("{\"status\":\"ok\",\"scan_us\":");
  Serial.print(scanUs);
  Serial.print(",\"users\":[");
  for (size_t k = 0; k < matches.size(); k++) {
    if (k > 0) Serial.print(',');
    const auto& u = db.userAt(matches[k]);
    Serial.print("{\"uid\":\"");
    Serial.print(u.uid);
    Serial.print("\",\"name\":\"");
    Serial.print(DatabaseManager::jsonEscape(u.name));
    Serial.print("\",\"registered\":\"");
    Serial.print(u.registered);
    Serial.print("\",\"valid_days\":");
    printJsonDouble(u.validDays);
    Serial.print('}');
    if ((k & 0xFF) == 0xFF) esp_task_wdt_reset();
  }
  Serial.println("]}");
}

void SerialProtocol::sendNetStatus(WifiTimeManager& net) {
  DynamicJsonDocument doc(512);
  doc["status"] = "ok";
  doc["type"] = "net_status";
  bool connected = net.isWifiConnected();
  doc["connected"] = connected;
  doc["ssid"] = net.currentSsid();
  doc["ip"] = net.currentIp();
  doc["rssi"] = net.currentRssi();
  doc["time_synced"] = net.isTimeSynced();
  serializeJson(doc, Serial);
  Serial.println();
}

void SerialProtocol::sendImportResult(size_t added, size_t errors, const ImportProfile& prof) {
  DynamicJsonDocument doc(640);
  doc["status"] = (errors == 0) ? "ok" : "error";
  doc["type"] = "import_result";
  doc["added"] = added;
  doc["errors"] = errors;
  // Profiling fields (see ImportProfiler.h), ms, not stable contract.
  doc["json_parse_ms"] = prof.jsonParseUs / 1000;
  doc["batch_loop_ms"] = prof.batchLoopUs / 1000;
  doc["ack_serialize_ms"] = prof.ackSerializeUs / 1000;
  doc["save_ms"] = prof.saveUs / 1000;
  doc["save_encode_ms"] = prof.saveEncodeUs / 1000;
  doc["save_write_ms"] = prof.saveWriteUs / 1000;
  doc["save_verify_ms"] = prof.saveVerifyUs / 1000;
  doc["save_finalize_ms"] = prof.saveFinalizeUs / 1000;
  doc["batches"] = prof.batchCount;
  doc["users_profiled"] = prof.userCount;
  // Transport wait: gap between response sent and next line received.
  doc["transport_wait_ms"] = prof.transportWaitUs / 1000;
  doc["transport_wait_count"] = prof.transportWaitCount;
  serializeJson(doc, Serial);
  Serial.println();
}

void SerialProtocol::sendBatchAddResult(size_t added, size_t errors,
                                        const std::vector<std::pair<String, String>>& failed) {
  // Streamed (consistent with sendUserList, avoids large DynamicJsonDocument).
  Serial.print("{\"status\":\"ok\",\"type\":\"batch_add_result\",\"added\":");
  Serial.print(added);
  Serial.print(",\"errors\":");
  Serial.print(errors);
  Serial.print(",\"failed\":[");
  for (size_t i = 0; i < failed.size(); i++) {
    if (i > 0) Serial.print(',');
    Serial.print("{\"uid\":\"");
    Serial.print(failed[i].first); // hex-only, never needs escaping
    Serial.print("\",\"message\":\"");
    Serial.print(DatabaseManager::jsonEscape(failed[i].second));
    Serial.print("\"}");
  }
  Serial.println("]}");
}

void SerialProtocol::sendRenewalResult(const String& uid, const String& name,
                                       const String& registered, double validDays) {
  DynamicJsonDocument doc(256);
  doc["status"] = "ok";
  doc["type"] = "renewal_result";
  doc["uid"] = uid;
  doc["name"] = name;
  doc["registered"] = registered;
  doc["valid_days"] = validDays;
  serializeJson(doc, Serial);
  Serial.println();
}

void SerialProtocol::sendRenewalError(const String& uid, const String& message) {
  DynamicJsonDocument doc(256);
  doc["status"] = "error";
  doc["type"] = "renewal_result";
  doc["uid"] = uid;
  doc["message"] = message;
  serializeJson(doc, Serial);
  Serial.println();
}

void SerialProtocol::sendFindResult(bool found, const String& uid, const String& name,
                                    const String& registered, double validDays,
                                    uint32_t searchUs) {
  DynamicJsonDocument doc(256);
  doc["type"] = "find_result";
  if (found) {
    doc["status"] = "ok";
    doc["uid"] = uid;
    doc["name"] = name;
    doc["registered"] = registered;
    doc["valid_days"] = validDays;
  } else {
    doc["status"] = "error";
    doc["message"] = "UID not found";
  }
  doc["search_us"] = searchUs;
  serializeJson(doc, Serial);
  Serial.println();
}

void SerialProtocol::sendTime(time_t epoch, const String& formatted) {
  DynamicJsonDocument doc(256);
  doc["status"] = "ok";
  doc["type"] = "time";
  doc["epoch"] = (double)epoch;
  doc["formatted"] = formatted;
  serializeJson(doc, Serial);
  Serial.println();
}

void SerialProtocol::sendTimezoneResult(bool applied, long gmtOffsetSec, int daylightOffsetSec,
                                        const String& message) {
  DynamicJsonDocument doc(256);
  doc["status"] = applied ? "ok" : "error";
  doc["type"] = "timezone";
  doc["applied"] = applied;
  doc["gmt_offset_sec"] = gmtOffsetSec;
  doc["daylight_offset_sec"] = daylightOffsetSec;
  doc["message"] = message;
  serializeJson(doc, Serial);
  Serial.println();
}

size_t SerialProtocol::readRawExact(uint8_t* buf, size_t len, unsigned long overallTimeoutMs) {
  size_t got = 0;
  unsigned long deadline = millis() + overallTimeoutMs;
  while (got < len) {
    int avail = Serial.available();
    if (avail > 0) {
      size_t want = std::min((size_t)avail, len - got);
      int n = Serial.readBytes((char*)(buf + got), want);
      if (n > 0) {
        got += (size_t)n;
        deadline = millis() + overallTimeoutMs;  // progress -- extend the deadline
        continue;
      }
    }
    if ((long)(millis() - deadline) >= 0) break;  // stalled with no data for the whole window
    // Feed watchdog -- long transfers block loop().
    esp_task_wdt_reset();
    delay(1);
  }
  return got;
}

void SerialProtocol::writeRaw(const uint8_t* buf, size_t len) {
  Serial.write(buf, len);
}

void SerialProtocol::sendExportBinHeader(size_t totalBytes, size_t count) {
  DynamicJsonDocument doc(256);
  doc["status"] = "ok";
  doc["type"] = "export_bin";
  doc["bytes"] = totalBytes;
  doc["count"] = count;
  serializeJson(doc, Serial);
  Serial.println();
}

void SerialProtocol::sendImportBinResult(size_t added, size_t errors) {
  DynamicJsonDocument doc(256);
  doc["status"] = "ok";
  doc["type"] = "import_bin_result";
  doc["added"] = added;
  doc["errors"] = errors;
  serializeJson(doc, Serial);
  Serial.println();
}

void SerialProtocol::sendSyncBeginResult(DatabaseManager& db) {
  DynamicJsonDocument doc(256);
  doc["status"] = "ok";
  doc["type"] = "sync_begin";
  doc["db_crc32"] = db.computeCrc32();
  doc["count"] = db.userCount();
  serializeJson(doc, Serial);
  Serial.println();
}

void SerialProtocol::sendSyncManifestHeader(size_t totalBytes, size_t count) {
  DynamicJsonDocument doc(256);
  doc["status"] = "ok";
  doc["type"] = "sync_manifest";
  doc["bytes"] = totalBytes;
  doc["count"] = count;
  serializeJson(doc, Serial);
  Serial.println();
}

void SerialProtocol::sendSyncResult(bool ok, const String& errorMessage, size_t removed,
                                    size_t added, size_t replaced, size_t errors,
                                    uint32_t dbCrc32, const SyncProfile& prof) {
  DynamicJsonDocument doc(512);
  doc["status"] = ok ? "ok" : "error";
  doc["type"] = "sync_result";
  if (!ok) doc["message"] = errorMessage;
  doc["removed"] = removed;
  doc["added"] = added;
  doc["replaced"] = replaced;
  doc["errors"] = errors;
  doc["db_crc32"] = dbCrc32;
  // Sync profiling fields (see SyncProfiler.h), ms, not stable contract.
  doc["remove_phase_ms"] = prof.removePhaseUs / 1000;
  doc["add_phase_ms"] = prof.addPhaseUs / 1000;
  doc["replace_phase_ms"] = prof.replacePhaseUs / 1000;
  doc["save_ms"] = prof.saveUs / 1000;
  doc["save_encode_ms"] = prof.saveEncodeUs / 1000;
  doc["save_write_ms"] = prof.saveWriteUs / 1000;
  doc["save_verify_ms"] = prof.saveVerifyUs / 1000;
  doc["save_finalize_ms"] = prof.saveFinalizeUs / 1000;
  serializeJson(doc, Serial);
  Serial.println();
}
