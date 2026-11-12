# GateCore v2.0.1

This is the GateCore rename. The firmware remains **1.2.0**, the serial
protocol is unchanged, and existing `users.bin` databases work as before.

## What changed

- Firmware sources moved to `gatecore-firmware/`.
- The sketch is now `gatecore-firmware/gatecore-firmware.ino`.
- The Rust crate and executable are now `gatecore-cli` (`gatecore_cli` for the
  library import).
- CLI release assets are now named `gatecore-cli-...`.
- Workflows, badges, documentation and shell prompts use the new names.
- Shell history moved to `~/.gatecore_cli_history`.
- The Windows uploader now matches the shell uploader more closely:
  `-Setup`, `-List`, `-MonitorOnly`, a default sketch path, port
  auto-detection, and the `-p`/`-f`/`-m`/`-b` aliases.
- The firmware diff is now
  `docs/gatecore-firmware-1.1.0-to-1.2.0.diff`.

## Breaking changes

The executable name changed from `rfid-cli` to **`gatecore-cli`**. Update
scripts, aliases, shortcuts and service definitions that use the old name.

The firmware sketch path also changed. Open
`gatecore-firmware/gatecore-firmware.ino` in the Arduino IDE; the folder and
`.ino` name must stay `gatecore-firmware`.

If your board already runs firmware 1.2.0, you do not need to reflash it just
for this rename. Rebuild only if you want the new source layout on the board.

## Downloads

| Platform | Archive |
|---|---|
| Windows 10/11 x64 | `gatecore-cli-2.0.1-x86_64-pc-windows-msvc.zip` |
| Linux x64 | `gatecore-cli-2.0.1-x86_64-unknown-linux-gnu.tar.gz` |
| macOS Apple silicon | `gatecore-cli-2.0.1-aarch64-apple-darwin.tar.gz` |
| macOS Intel | `gatecore-cli-2.0.1-x86_64-apple-darwin.tar.gz` |

Each archive has a matching `.sha256` file. The binaries are not code-signed.

## Upgrade

```bash
# Replace the old executable with the new one.
gatecore-cli --version
gatecore-cli list-ports
```

Existing command names and flags are unchanged apart from the executable name.
`RFID_PORT` and `RFID_BAUD` are still accepted.

## Verification

- `cargo +1.90.0 test --locked`: 51 passed.
- `cargo +stable test --locked`: 51 passed.
- `cargo +1.90.0 build --release --locked`: passed.
- CI runs stable Rust on Windows, Linux and macOS, plus Rust 1.90 on Linux.
- The firmware source is unchanged; CI compiles it for the ESP32-S3.

## Documentation

- [Installation](https://github.com/Hyacinthe-primus/RFID_Access_Control/wiki/Installation)
- [CLI Guide](https://github.com/Hyacinthe-primus/RFID_Access_Control/wiki/CLI-Guide)
- [Serial Protocol](https://github.com/Hyacinthe-primus/RFID_Access_Control/wiki/Serial-Protocol-Specification)
- [Firmware Setup](https://github.com/Hyacinthe-primus/RFID_Access_Control/wiki/Firmware-Setup)
- [Troubleshooting](https://github.com/Hyacinthe-primus/RFID_Access_Control/wiki/Troubleshooting)
- [Changelog](https://github.com/Hyacinthe-primus/RFID_Access_Control/wiki/CHANGELOG)

**Full Changelog:** https://github.com/Hyacinthe-primus/RFID_Access_Control/compare/v2.0.0...v2.0.1
