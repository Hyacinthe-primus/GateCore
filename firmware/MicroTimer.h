#pragma once
/*
 * MicroTimer.h
 * Adds elapsed micros() to a caller-owned accumulator on scope exit.
 */

#include <Arduino.h>

class ScopedMicroTimer {
public:
  explicit ScopedMicroTimer(unsigned long& accumulator)
      : accumulator_(accumulator), start_(micros()) {}
  ~ScopedMicroTimer() { accumulator_ += (micros() - start_); }

private:
  unsigned long& accumulator_;
  unsigned long start_;
};
