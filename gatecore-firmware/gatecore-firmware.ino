/*
 * gatecore-firmware.ino
 *
 * ESP32-S3 standalone access controller.
 * PN532 (hardware SPI) + 16x2 I2C LCD + LittleFS user database.
 * Serial link to the host CLI for admin operations; the controller runs
 * standalone with no PC attached.
 *
 * Board: ESP32-S3 Dev Module (QFN56, 8MB Octal PSRAM, 40MHz XTAL)
 * Arduino IDE settings that matter for THIS project:
 *   Tools > USB CDC On Boot        : Disabled
 *   Tools > Flash Size             : 16MB
 *   Tools > Partition Scheme       : Custom (uses partitions.csv)
 *   Tools > PSRAM                  : OPI PSRAM
 *
 * See README.md for wiring and setup instructions.
 */

#include "SystemController.h"
#include <esp_task_wdt.h>

SystemController controller;

void setup() {
  // Register Arduino's loopTask with the Task Watchdog.
  // Required before controller.begin(), which may temporarily manage
  // TWDT membership during long LittleFS operations.
  esp_task_wdt_add(NULL);

  controller.begin();
}

void loop() {
  // Feed the Task Watchdog once per loop iteration.
  esp_task_wdt_reset();

  controller.update();

  // No delay(): RFIDManager already limits the polling interval.
}