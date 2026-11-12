#pragma once
/*
 * SerialProtocol.h
 * Newline-delimited JSON transport over Serial.
 * Frames/parses messages and forwards JsonDocuments to SystemController.
 * Command semantics live elsewhere.
 */

#include <Arduino.h>
#include <ArduinoJson.h>
#include <functional>
#include <vector>
#include <utility>

using SerialMessageHandler = std::function<void(JsonDocument&)>;

class SerialProtocol {
public:
  // RX line buffer. Must match Serial.setRxBufferSize() in
  // SystemController::begin() to avoid truncated JSON.
  // Sized for batch=100 (~6 KB) with generous headroom.
  static const size_t kLineBufCapacity = 16384;

  // Reported by the "status" command.
  // Bump firmware on releases; bump protocol only for wire-format changes.
  // Control traffic uses JSON; bulk import/export uses raw binary.
  static constexpr const char* kFirmwareVersion = "1.2.0";
  static constexpr const char* kProtocolVersion = "hybrid-json-ctrl+bin-bulk/v1";

  void begin(SerialMessageHandler handler);

  // Non-blocking. Processes complete '\n'-terminated JSON messages.
  void poll();

  void sendOk();
  void sendError(const String& message);
  void sendUidDetected(const String& uid);

  void sendRemovedCount(size_t removedCount);
  void sendWifiResult(bool connected, const String& message);
  void sendNtpSyncResult(bool synced, const String& message);
  void sendUserList(class DatabaseManager& db);
  void sendStatus(class DatabaseManager& db);
  void sendNetStatus(class WifiTimeManager& net);

  // Import profiling is included in the response.
  void sendImportResult(size_t added, size_t errors,
                        const struct ImportProfile& prof);

  // "ok" allows per-entry failures; "error" rejects the whole request.
  void sendBatchAddResult(
      size_t added,
      size_t errors,
      const std::vector<std::pair<String, String>>& failed);

  void sendRenewalResult(const String& uid, const String& name,
                         const String& registered, double validDays);
  // Renewal failure, typed so the host can tell it from a command reply.
  void sendRenewalError(const String& uid, const String& message);

  // search_us measures device-side lookup only.
  void sendFindResult(bool found, const String& uid, const String& name,
                      const String& registered, double validDays,
                      uint32_t searchUs);

  // Device-side name scan.
  void sendFindNameResult(class DatabaseManager& db,
                          const String& queryLower);

  void sendTime(time_t epoch, const String& formatted);
  void sendTimezoneResult(bool applied, long gmtOffsetSec,
                          int daylightOffsetSec,
                          const String& message);

  // Reads exactly len bytes. Short read indicates stream desynchronization.
  size_t readRawExact(uint8_t* buf, size_t len,
                      unsigned long overallTimeoutMs);

  void writeRaw(const uint8_t* buf, size_t len);

  // Announces an upcoming raw binary export.
  void sendExportBinHeader(size_t totalBytes, size_t count);

  // Final import summary.
  void sendImportBinResult(size_t added, size_t errors);

  void sendSyncBeginResult(class DatabaseManager& db);
  // Announces the upcoming raw manifest transfer (same header shape as
  // export_bin, distinct "type" so the host can tell them apart).
  void sendSyncManifestHeader(size_t totalBytes, size_t count);
  // Sync profiling is included in the response (mirrors sendImportResult).
  void sendSyncResult(bool ok, const String& errorMessage, size_t removed,
                      size_t added, size_t replaced, size_t errors,
                      uint32_t dbCrc32, const struct SyncProfile& prof);

private:
  char lineBuf_[kLineBufCapacity];
  size_t lineLen_ = 0;
  SerialMessageHandler handler_;

  void handleLine_(char* line);
};