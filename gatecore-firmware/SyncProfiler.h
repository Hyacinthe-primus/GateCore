#pragma once
/*
 * SyncProfiler.h
 * Timing counters for sync_apply's remove/add/replace phases and final save.
 * Kept separate from ImportProfile: the two paths measure different work.
 * Not part of the stable protocol.
 */

#include <Arduino.h>
#include "MicroTimer.h"

struct SyncProfile {
  // sync_apply's three merge-diff phases.
  unsigned long removePhaseUs = 0;
  unsigned long addPhaseUs = 0;
  unsigned long replacePhaseUs = 0;

  unsigned long saveUs = 0;

  // save() breakdown (sums to ~saveUs minus open/header overhead).
  unsigned long saveEncodeUs = 0;
  unsigned long saveWriteUs = 0;
  unsigned long saveVerifyUs = 0;
  unsigned long saveFinalizeUs = 0;

  void reset() { *this = SyncProfile(); }
};

extern SyncProfile g_syncProfile;
