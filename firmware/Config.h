#pragma once
/*
 * Config.h
 * Hardware pin map and system constants.
 *
 * Board tested: ESP32-S3 (QFN56, rev v0.2), 8MB Octal PSRAM, 40MHz XTAL.
 * Some S3 dev boards route PSRAM over GPIO 33-37/26-32; check Tools > PSRAM
 * before assuming the pins below are free on a different module.
 */

#include <Arduino.h>

// Using ESP32-S3's default FSPI pins.
#define PN532_SCK   12
#define PN532_MISO  13
#define PN532_MOSI  11
#define PN532_SS    10

// I2C (16x2 LCD)
#define LCD_SDA     8
#define LCD_SCL     9
#define LCD_I2C_ADDR 0x27      // Common default; 0x3F is the other common one.
#define LCD_COLS    16
#define LCD_ROWS    2

// Filesystem
// USERS_DB_PATH: binary DB (see DatabaseManager.h). Legacy JSON is
// auto-migrated on first boot, kept as .bak for recovery.
#define USERS_DB_PATH             "/users.bin"
#define USERS_DB_LEGACY_JSON_PATH "/users.json"
#define USERS_DB_TMP_PATH         "/users.tmp"

// Timing (all non-blocking, millis()-based)
#define BOOT_SCREEN_MIN_MS     2500
#define RESULT_DISPLAY_MS      2000
#define UID_DISPLAY_MS         2000
#define CARD_COOLDOWN_MS       2500
#define RFID_POLL_TIMEOUT_MS   50
#define SERIAL_BAUD            2000000

// Validation
#define MAX_NAME_LEN   48
#define MAX_USERS      70000   // PSRAM-backed; SRAM-only limit was ~2000
#define MIN_UID_HEX_LEN 8      // 4 bytes minimum (MIFARE Classic UID)
#define MAX_UID_HEX_LEN 20     // 10 bytes maximum (triple-size UID)
#define REGISTERED_DATE_LEN 10 // strlen("YYYY-MM-DD")

// Wi-Fi / NTP -- credentials provisioned at runtime via 'configure_wifi'.
#define WIFI_CONNECT_TIMEOUT_MS   10000
#define NTP_SERVER_1              "pool.ntp.org"
#define NTP_SERVER_2              "time.nist.gov"
// Timezone: first-boot defaults, overridden by 'configure_timezone' (NVS).
// Device timezone must match the CLI machine's timezone.
#define NTP_GMT_OFFSET_SEC        0   // UTC+0
#define NTP_DAYLIGHT_OFFSET_SEC   0
#define NTP_SYNC_TIMEOUT_MS       8000    // how long to wait for time() to become sane
#define NTP_RESYNC_INTERVAL_MS    (6UL * 60UL * 60UL * 1000UL)  // re-sync every 6h to correct drift

// Buzzer (passive, driven via ledc/tone -- never a delay()-blocking tone)
#define BUZZER_PIN   6

// Anti-brute-force: consecutive denied badges → lockout.
// Serial/CLI still works during lockout.
#define MAX_CONSECUTIVE_DENIALS   5
#define LOCKOUT_DURATION_MS       (30UL * 1000UL)   // 30s

// Import mode pauses the RFID reader. If the host vanishes mid-import
// (cable pulled, CLI killed), revert to the flash copy after this much
// serial silence instead of staying offline until reboot.
#define IMPORT_IDLE_TIMEOUT_MS    (120UL * 1000UL)  // 2 min
