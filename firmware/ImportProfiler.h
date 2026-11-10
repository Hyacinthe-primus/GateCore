#pragma once
/*
 * ImportProfiler.h
 * Timing counters for one import, folded into the import_result response.
 * Not part of the stable protocol.
 */

#include <Arduino.h>
#include "MicroTimer.h"

struct ImportProfile {
  unsigned long jsonParseUs = 0;
  unsigned long jsonParseCount = 0;

  unsigned long batchLoopUs = 0;
  unsigned long batchCount = 0;
  unsigned long userCount = 0;

  unsigned long ackSerializeUs = 0;

  unsigned long saveUs = 0;

  // save() breakdown (sums to ~saveUs minus open/header overhead).
  unsigned long saveEncodeUs = 0;
  unsigned long saveWriteUs = 0;
  unsigned long saveVerifyUs = 0;
  unsigned long saveFinalizeUs = 0;

  // Gap between last response sent and next line received (pure wire/host
  // wait, no firmware CPU time during import).
  unsigned long transportWaitUs = 0;
  unsigned long transportWaitCount = 0;
  unsigned long lastResponseSentUs = 0;

  void reset() { *this = ImportProfile(); }
};

extern ImportProfile g_importProfile;
