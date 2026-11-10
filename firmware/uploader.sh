#!/usr/bin/env bash
# Compile, upload and monitor the firmware with arduino-cli on Linux and macOS.
# Counterpart of uploader.ps1. Works with the bash 3.2 that ships with macOS.

set -u

ESP32_INDEX_URL="https://espressif.github.io/arduino-esp32/package_esp32_index.json"
FQBN="esp32:esp32:esp32s3:FlashSize=16M,PSRAM=opi,PartitionScheme=custom"
MONITOR_FQBN="esp32:esp32:esp32s3"
LIBS=("Adafruit PN532" "LiquidCrystal I2C" "ArduinoJson@7.2.1")

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
port=""
file="$script_dir/firmware.ino"
baud=2000000
monitor=0
setup=0
list=0

if [ -t 1 ]; then
  red=$'\033[31m'; green=$'\033[32m'; cyan=$'\033[36m'; reset=$'\033[0m'
else
  red=""; green=""; cyan=""; reset=""
fi

say()  { printf '\n%s%s%s\n' "$cyan" "$1" "$reset"; }
ok()   { printf '%s%s%s\n' "$green" "$1" "$reset"; }
fail() { printf '%s%s%s\n' "$red" "$1" "$reset" >&2; exit "${2:-1}"; }

usage() {
  cat <<EOF
Usage: $(basename "$0") [options]

  -p, --port PORT    serial port, e.g. /dev/ttyUSB0 or /dev/cu.usbserial-0001
                     (auto-picked when exactly one candidate is plugged in)
  -f, --file FILE    sketch to build (default: firmware.ino next to this script)
  -m, --monitor      open the serial monitor after a successful upload
      --monitor-only open the serial monitor and skip compile and upload
  -b, --baud N       monitor baud rate (default: $baud, the firmware's SERIAL_BAUD)
      --setup        install the ESP32 core and the libraries, then exit
      --list         list the boards arduino-cli can see, then exit
  -h, --help         show this help

Examples:
  $(basename "$0") --setup
  $(basename "$0") --port /dev/ttyUSB0 --monitor
  $(basename "$0") --port /dev/cu.usbserial-0001 --monitor-only
EOF
}

monitor_only=0
while [ $# -gt 0 ]; do
  case "$1" in
    -p|--port)  [ $# -ge 2 ] || fail "$1 needs a value" 2; port="$2"; shift 2 ;;
    -f|--file)  [ $# -ge 2 ] || fail "$1 needs a value" 2; file="$2"; shift 2 ;;
    -b|--baud)  [ $# -ge 2 ] || fail "$1 needs a value" 2; baud="$2"; shift 2 ;;
    -m|--monitor)  monitor=1; shift ;;
    --monitor-only) monitor=1; monitor_only=1; shift ;;
    --setup)    setup=1; shift ;;
    --list)     list=1; shift ;;
    -h|--help)  usage; exit 0 ;;
    *)          usage >&2; fail "Unknown option: $1" 2 ;;
  esac
done

command -v arduino-cli >/dev/null 2>&1 || fail \
"arduino-cli not found. Install it from https://arduino.github.io/arduino-cli/latest/installation/
  macOS:  brew install arduino-cli
  Linux:  curl -fsSL https://raw.githubusercontent.com/arduino/arduino-cli/master/install.sh | sh"

if [ "$setup" -eq 1 ]; then
  say "Registering the ESP32 board index..."
  arduino-cli config init >/dev/null 2>&1 || true
  # config add appends, so skip it when the URL is already there.
  if ! arduino-cli config dump 2>/dev/null | grep -q "package_esp32_index.json"; then
    arduino-cli config add board_manager.additional_urls "$ESP32_INDEX_URL" || fail "Could not edit the arduino-cli config."
  fi
  arduino-cli core update-index || fail "core update-index failed."
  say "Installing the ESP32 core (large download, be patient)..."
  arduino-cli core install esp32:esp32 || fail "ESP32 core install failed."
  say "Installing libraries..."
  arduino-cli lib install "${LIBS[@]}" || fail "Library install failed."
  ok "Setup done."
  exit 0
fi

if [ "$list" -eq 1 ]; then
  arduino-cli board list
  exit $?
fi

if [ -z "$port" ]; then
  candidates=()
  for p in /dev/ttyUSB* /dev/ttyACM* /dev/cu.usbserial* /dev/cu.usbmodem* /dev/cu.wchusbserial* /dev/cu.SLAB_USBtoUART*; do
    [ -e "$p" ] && candidates+=("$p")
  done
  if [ "${#candidates[@]}" -eq 1 ]; then
    port="${candidates[0]}"
    printf 'Using %s\n' "$port"
  elif [ "${#candidates[@]}" -eq 0 ]; then
    fail "No serial port found. Plug the board in, or pass --port. (--list shows what arduino-cli sees.)"
  else
    printf 'Several serial ports found:\n' >&2
    printf '  %s\n' "${candidates[@]}" >&2
    fail "Pick one with --port."
  fi
fi

run_monitor() {
  say "Opening the serial monitor (Ctrl+C to quit)..."
  printf 'Port     : %s\nBaudrate : %s\n\n' "$port" "$baud"
  arduino-cli monitor -p "$port" -b "$MONITOR_FQBN" --config "baudrate=$baud"
}

if [ "$monitor_only" -eq 1 ]; then
  run_monitor
  exit $?
fi

[ -f "$file" ] || fail "File not found: $file"

if [ ! -r "$port" ] || [ ! -w "$port" ]; then
  case "$(uname -s)" in
    Linux) fail "No read/write access to $port. Add yourself to the serial group (sudo usermod -aG dialout \$USER, 'uucp' on Arch), then log out and back in." ;;
    *)     fail "No read/write access to $port." ;;
  esac
fi

say "Compiling $file..."
arduino-cli compile --fqbn "$FQBN" "$file" || fail "Compilation failed. If the ESP32 core or a library is missing, run: $0 --setup" $?
ok "Compilation successful."

say "Uploading to $port..."
arduino-cli upload -p "$port" --fqbn "$FQBN" "$file" || fail "Upload failed. Some boards need the BOOT button held while the upload starts." $?
ok "Upload successful."

if [ "$monitor" -eq 1 ]; then
  run_monitor
fi

ok "Done."
