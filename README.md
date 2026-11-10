# RFID Access Control (ESP32-S3)

[![CLI](https://img.shields.io/github/actions/workflow/status/Hyacinthe-primus/RFID_Access_Control/cli.yml?label=CLI&logo=githubactions&logoColor=white&style=for-the-badge)](https://github.com/Hyacinthe-primus/RFID_Access_Control/actions/workflows/cli.yml)
[![Firmware](https://img.shields.io/github/actions/workflow/status/Hyacinthe-primus/RFID_Access_Control/firmware.yml?label=Firmware&logo=githubactions&logoColor=white&style=for-the-badge)](https://github.com/Hyacinthe-primus/RFID_Access_Control/actions/workflows/firmware.yml)
![Rust](https://img.shields.io/badge/CLI-Rust-000000?logo=rust&logoColor=white&style=for-the-badge)
![C++](https://img.shields.io/badge/Firmware-C%2B%2B-00599C?logo=cplusplus&logoColor=white&style=for-the-badge)
![ESP32-S3](https://img.shields.io/badge/ESP32-S3-E7352C?logo=espressif&logoColor=white&style=for-the-badge)
![Platforms](https://img.shields.io/badge/CLI-Windows%20%7C%20Linux%20%7C%20macOS-555?style=for-the-badge)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg?style=for-the-badge)](LICENSE.md)

A standalone RFID access controller built around an ESP32-S3 and a PN532
reader. The board keeps its own user database on LittleFS, checks badge expiry
against an NTP-synced clock, and runs without a computer attached. The LCD and
passive buzzer provide the local feedback; admin badges never expire.

`rfid-cli` is the host-side tool. It is a single Rust binary for Windows, Linux
and macOS, and works either as one-shot commands for scripts or as an
interactive shell. Imports and syncs use a raw binary transfer rather than
sending one JSON message per user.

Current versions: firmware **1.2.0**, `rfid-cli` **2.0.0**.

## Repository layout

```
RFID_Access_Control/
├── firmware/            Arduino sketch and upload helpers
│   ├── firmware.ino     setup() and loop()
│   ├── *.h, *.cpp       controller, database, serial protocol, display, ...
│   ├── partitions.csv   16 MB flash layout with a 12 MB LittleFS partition
│   ├── uploader.ps1     build, upload and monitor (Windows)
│   └── uploader.sh      build, upload and monitor (Linux, macOS)
├── cli/                 rfid-cli (Rust)
│   ├── src/             library and thin binary
│   ├── tests/           golden vectors and an in-memory firmware simulator
│   └── samples/         example CSV and JSON user files
├── docs/                firmware 1.1.0 → 1.2.0 diff
├── .github/workflows/   CLI tests, firmware compile, release builds
├── CHOICES_AND_MODIFICATIONS.md
├── Wiring.png
├── README.md
└── LICENSE.md
```

## Install rfid-cli

Release archives for Windows x64, Linux x64 and macOS (Apple silicon and Intel)
are on the [Releases](https://github.com/Hyacinthe-primus/RFID_Access_Control/releases)
page. To build it yourself, install Rust 1.90 or newer from
[rustup.rs](https://rustup.rs), then:

```bash
git clone https://github.com/Hyacinthe-primus/RFID_Access_Control.git
cd RFID_Access_Control/cli
cargo build --release --locked    # target/release/rfid-cli
cargo install --path . --locked   # optional: install it on PATH
```

| | Windows 10/11 | Linux | macOS |
|---|---|---|---|
| Build deps | MSVC build tools (rustup offers to install them) | `libudev-dev` and `pkg-config` (Debian/Ubuntu), `systemd-devel` (Fedora) | Xcode command line tools |
| Serial port | `COM5` | `/dev/ttyUSB0`, `/dev/ttyACM0` | `/dev/cu.usbserial-*`, `/dev/cu.usbmodem*` |
| Setup | CH34x or CP210x driver if no port appears | Add your user to `dialout` (`uucp` on Arch) | CH34x driver on older macOS versions |

On Windows, run the built binary from the crate directory:

```powershell
.\target\release\rfid-cli.exe list-ports
.\target\release\rfid-cli.exe --port COM5 status
```

On Linux and macOS, `rfid-cli` can be used directly once it is on `PATH`:

```bash
rfid-cli list-ports
rfid-cli --port /dev/ttyUSB0 status
```

## Quick start

```bash
rfid-cli                                  # no subcommand: interactive shell
rfid-cli status
rfid-cli add --uid 04A1B2C3 --name "Alice Martin" --valid-days 365
rfid-cli add --uid 5AF73581 --name "Admin"          # no --valid-days: admin badge
rfid-cli import users.csv --dry-run                 # validate without a device
rfid-cli sync users.json --dry-run                  # show what would change
rfid-cli export backup.bin                          # device binary format
rfid-cli convert users.json users.bin               # offline conversion
rfid-cli --help                                     # all commands
rfid-cli <command> --help                           # command details
```

The port is auto-detected. Use `--port` or `RFID_PORT` to override it. The baud
rate defaults to 2000000 and can be changed with `--baud` or `RFID_BAUD`; it
must still match the firmware's `SERIAL_BAUD`. `--verbose` logs protocol lines
and masks the password in the outgoing Wi-Fi setup command. Exit codes are `0`
for success, `1` for a failed command, `2` for a usage error, and `130` for a
forced interrupt.

## Documentation

The wiki contains the [installation guide](https://github.com/Hyacinthe-primus/RFID_Access_Control/wiki/Installation),
the [full CLI reference](https://github.com/Hyacinthe-primus/RFID_Access_Control/wiki/CLI-Guide),
the [serial protocol](https://github.com/Hyacinthe-primus/RFID_Access_Control/wiki/Serial-Protocol-Specification),
[firmware setup](https://github.com/Hyacinthe-primus/RFID_Access_Control/wiki/Firmware-Setup),
[hardware and wiring](https://github.com/Hyacinthe-primus/RFID_Access_Control/wiki/Hardware-and-Wiring),
[troubleshooting](https://github.com/Hyacinthe-primus/RFID_Access_Control/wiki/Troubleshooting),
and notes on the [project structure](https://github.com/Hyacinthe-primus/RFID_Access_Control/wiki/Project-Structure)
and [development workflow](https://github.com/Hyacinthe-primus/RFID_Access_Control/wiki/Development).
The [changelog](https://github.com/Hyacinthe-primus/RFID_Access_Control/wiki/CHANGELOG)
and [`CHOICES_AND_MODIFICATIONS.md`](CHOICES_AND_MODIFICATIONS.md) cover release
history and the reasoning behind the current design.

## Flash the firmware

Install the ESP32 board package (3.x), `Adafruit PN532`, `LiquidCrystal I2C`
and `ArduinoJson` 7.x from the Arduino IDE Library Manager.

With the Arduino IDE:

1. Open `firmware/firmware.ino`. Keep the folder name `firmware`; the IDE
   requires it to match the `.ino` file.
2. Select **ESP32S3 Dev Module**.
3. Set **USB CDC On Boot: Disabled**, **Flash Size: 16MB (128Mb)**,
   **Partition Scheme: Custom**, and **PSRAM: OPI PSRAM**.
4. Upload the sketch.

With `arduino-cli`, the included helpers use the same core options as CI:

```powershell
# Windows
.\firmware\uploader.ps1 -Port COM5 -File firmware\firmware.ino -Monitor
```

```bash
# Linux and macOS
./firmware/uploader.sh --setup                 # once
./firmware/uploader.sh --port /dev/ttyUSB0 --monitor
```

If PowerShell blocks the script, run it with
`powershell -ExecutionPolicy Bypass -File .\firmware\uploader.ps1 ...`.
Both helpers open the serial monitor at 2000000 baud. That is the protocol rate, not the ESP32 USB upload speed.

## Tests and CI

```bash
cd cli
cargo test --locked
```

The suite has 51 tests: 38 unit tests and 13 end-to-end tests. The end-to-end
tests drive the real command handlers through an in-memory firmware simulator,
so no hardware is needed. Golden vectors pin the `users.bin` encoding against
the original Python implementation.

CI runs the tests on Windows, Linux and macOS with stable Rust, runs them again
on Rust 1.90 on Linux, compiles the firmware for the ESP32-S3, and builds
release archives for `v*` tags.

## License

MIT. See [LICENSE.md](LICENSE.md).
