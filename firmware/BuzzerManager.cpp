#include "BuzzerManager.h"
#include "Config.h"

// Ascending, pleasant major arpeggio
const BuzzerManager::Note BuzzerManager::kGrantedSeq_[] = {
  {523, 375},  // C5
  {659, 375},  // E5
  {784, 375},  // G5
  {1047, 375}, // C6
};
const uint8_t BuzzerManager::kGrantedLen_ = sizeof(kGrantedSeq_) / sizeof(kGrantedSeq_[0]);

// Descending, clearly different "error" tone
const BuzzerManager::Note BuzzerManager::kDeniedSeq_[] = {
  {440, 375}, // A4
  {349, 375}, // F4
  {294, 375}, // D4
  {220, 375}, // A3
};
const uint8_t BuzzerManager::kDeniedLen_ = sizeof(kDeniedSeq_) / sizeof(kDeniedSeq_[0]);

void BuzzerManager::begin() {
  pinMode(BUZZER_PIN, OUTPUT);
  noTone(BUZZER_PIN);
}

void BuzzerManager::startNote_(uint8_t idx) {
  index_ = idx;
  noteStartMs_ = millis();
  const Note& n = seq_[idx];
  if (n.freqHz > 0) {
    tone(BUZZER_PIN, n.freqHz);
  } else {
    noTone(BUZZER_PIN);
  }
}

void BuzzerManager::startSequence_(const Note* seq, uint8_t len) {
  seq_ = seq;
  seqLen_ = len;
  playing_ = true;
  startNote_(0);
}

void BuzzerManager::playGranted() {
  startSequence_(kGrantedSeq_, kGrantedLen_);
}

void BuzzerManager::playDenied() {
  startSequence_(kDeniedSeq_, kDeniedLen_);
}

void BuzzerManager::update() {
  if (!playing_) return;

  if (millis() - noteStartMs_ >= seq_[index_].durationMs) {
    uint8_t next = index_ + 1;
    if (next >= seqLen_) {
      noTone(BUZZER_PIN);
      playing_ = false;
      return;
    }
    startNote_(next);
  }
}
