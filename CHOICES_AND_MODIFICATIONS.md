# Choices and Modifications

This release moves the host tool from Python to Rust, reorganises the
repository around `gatecore-firmware/` and `gatecore-cli/`, and updates the
firmware to 1.2.0. These notes record the layout, the main Rust decisions, the
bugs fixed during the port, and the parts that still need hardware testing.

## 1. Repository layout

```
RFID_Access_Control/
├── gatecore-firmware/     Arduino sources, partition table, upload helpers
├── gatecore-cli/          Rust crate (gatecore-cli)
│   ├── src/               library and thin binary
│   ├── tests/             simulator tests and golden vectors
│   ├── samples/           example JSON and CSV user files
│   ├── usersdata/         local user files (git-ignored)
│   └── backups/           automatic backups (git-ignored)
├── docs/                  gatecore-firmware 1.1.0 → 1.2.0 diff
├── .github/workflows/     CLI tests, firmware compile, release builds
├── Wiring.png
├── README.md
├── CHOICES_AND_MODIFICATIONS.md
└── LICENSE.md
```

The sketch moved to `gatecore-firmware/gatecore-firmware.ino`. The Arduino IDE
requires the main `.ino` file to have the same name as its folder, so keeping
`RFID_Access_Control.ino` inside `gatecore-firmware/` would not build. The
`Config.h`, `RFIDManager`, `SerialProtocol`, `SyncProfiler` and
`SystemController` files moved with it.

`python_cli/` and the old Python tests were removed. Their functionality now
lives in `gatecore-cli/src`; the test intent carried over into the Rust unit and
simulator tests, though the suites are not one-for-one. The wiki is a separate
Git repository. Its current pages
describe `gatecore-cli` 2.0.1 and firmware 1.2.0; the old Python CLI page is
kept only as historical documentation.

The `.gitignore` excludes `gatecore-cli/target/`, `gatecore-cli/usersdata/`
and `gatecore-cli/backups/`. The last two contain badge holders' names, so they
must stay out of version control.

## 2. Rust CLI

### Dependencies

| Task | Crate | Replaces |
|---|---|---|
| Argument parsing | `clap` 4 | argparse |
| Serial port | `serialport` 4 | pyserial |
| Interactive shell | `rustyline` 15 | prompt_toolkit |
| Tables and colours | `comfy-table`, `colored` | rich |
| Progress bars | `indicatif` | none |
| JSON and CSV | `serde`, `serde_json`, `csv` | json, csv |
| CRC32 | `crc32fast` | zlib.crc32 |
| Errors | `thiserror` | `SystemExit` and exceptions |
| Ctrl+C | `ctrlc` | signal module |
| Password input | `rpassword` | none; the old CLI echoed it |

The serial layer uses blocking `serialport`, not `tokio-serial`. The protocol
is request/response over one port; an async runtime would add complexity without
improving throughput.

### Structure

The crate is a library with a thin `main.rs` binary. Commands return
`Result<(), Error>`, and the binary maps those errors to exit codes. Offline
commands never open the serial port.

| Module | Responsibility |
|---|---|
| `error.rs` | One error type, exit codes and link-failure detection |
| `interrupt.rs` | Ctrl+C as a polled flag |
| `convert.rs` | `users.bin` records, CRCs, sync manifest entries |
| `database.rs` | UID, name and date validation; `UserEntry` |
| `userfile.rs` | JSON, CSV and `.bin` loading and writing |
| `protocol.rs` | Typed requests and response views |
| `serial.rs` | Discovery, framing, retries and raw transfers |
| `ui.rs` | Messages, tables, prompts and progress bars |
| `sync_stats.rs` | Timing output for `sync --stats` |
| `cli.rs` | clap definitions shared by one-shot mode and the shell |
| `commands/` | Implementations for users, device, scan, bulk and sync commands |
| `repl.rs` | Interactive shell |

A few decisions shaped the implementation:

- `serial::Link` is a trait, not a concrete port. The tests plug in an
  in-memory firmware simulator and run the real `Device` and command handlers.
- Requests are a single `#[serde(tag = "type")]` enum. A misspelled field is a
  compile error rather than a runtime error. Response fields that older
  firmware may omit have defaults.
- Shell lines and one-shot commands share the same clap `Command` definitions,
  so command names, flags and validation stay in sync. Global options such as
  `--port`, `--baud` and `--verbose` are set when the shell starts; inside the
  shell, use `connect`, `disconnect` and `version` for those operations.
- `Ctx::device()` opens the port lazily. `convert`, `list-ports` and
  `import --dry-run` work without a board.
- A request timeout is an idle timeout: it means no byte arrived for the
  configured period. Raw transfers use a separate stall timer. This matches the
  way the firmware behaves and avoids guessing a total transfer time.
- Reads block in short slices and check the interrupt flag. Commands can leave
  scan or renewal mode, or abort an import, before returning. A second Ctrl+C
  exits the process with 130.
- Exports and backups are written to `<file>.part` and renamed on success, so
  an interrupted write does not leave a truncated file under the final name.

### Compatibility with the Python command line

Most subcommands kept their names and flags, but this is not a drop-in
replacement for every script:

| Command | Difference |
|---|---|
| `list`, `find`, `add`, `rename` | Same names. `find --name` now filters on the device; it falls back to `list` on very old firmware. |
| `remove` | Adds `--backup-format`; `--except` refuses if none of the keep UIDs exist. |
| `status` | Also reports checksum, firmware and protocol versions. |
| `ntp-time` | Also prints UTC and warns when the host clock differs by more than 5 seconds. |
| `configure` | Asks for the Wi-Fi password without echo when stdin is a terminal. |
| `scan` | Uses continuous mode on firmware 1.2 and later. |
| `tag-renew` | Reports per-card failures; firmware 1.2 includes the UID in the error. |
| `import` | Defaults to the binary transport. `--json-transport` keeps the old batch path. |
| `export` | Chooses JSON, CSV or binary by extension. The old CLI treated every non-`.bin` file as JSON. |
| `sync` | Adds `--restamp` and `--stats`. Undated entries keep device dates where possible. |
| `convert` | New subcommand. Replaces the old standalone `convert.py`. |

Global options are `--port` (`RFID_PORT`), `--baud` (`RFID_BAUD`),
`-v/--verbose` and `--no-color` (`NO_COLOR` is also respected). `--verbose`
masks the outgoing Wi-Fi password; it does not filter every possible password
occurrence from all protocol traffic.

## 3. Bugs fixed in the Python CLI

### Data correctness

`sync` used to stamp every file row that had `valid_days` but no `registered`
date with today's date. The date is part of the record CRC, so running the same
sync on a later day replaced those records and quietly renewed the badges.
Undated rows now keep the date already stored on the device. A date is looked
up with `find` for up to 64 users, or with a bulk export above that. If the
device has no date to keep, including for an existing admin badge, the row
becomes an expiring record dated today. `--restamp` deliberately restores the
old behaviour.

### Parsing and validation

- Invalid dates such as `2024-02-30` or `2024/01/01` used to fail inside the
  encoder with an uncaught exception. They are now rejected while parsing, and
  the row is skipped with a message.
- `registered: null` was converted to the string `"None"` and later crashed.
- Odd-length UIDs were accepted by the host. The firmware did not accept them.
- `convert.py` stripped every non-hex character, so `zz04aabbcc` looked like a
  valid UID. Only `:`, `-` and whitespace are treated as separators now.
- Names were only checked for length. Control characters and whitespace-only
  names are rejected now, matching the firmware.
- `rename` did not check the name length at all.
- `nan` could be stored as `valid_days`, producing invalid JSON on the wire.
- Dates before 1970 were silently clamped. They are now rejected.

The firmware still only validates the shape of dates in a binary record. The
strict date range is a CLI rule; a hand-built `.bin` file should be treated with
the same care as any other untrusted input.

### Transport

- Non-JSON boot lines used to count as failed replies and caused command
  resends. They are skipped.
- A card event could be discarded or mistaken for the reply to the next
  request. Events are now queued separately from replies.
- A late reply to a retried request could leave the link one reply out of sync.
  A short drain follows retried requests.
- `rename` treated "UID not found" on a retry as success. That exception only
  makes sense for `remove`.
- `export_bin` and `sync_manifest` could be retried after a lost header, which
  could make the device stream the database twice. Raw requests are now
  single-attempt.
- A failed import could leave the board in import mode with the reader paused.
  The CLI sends `import_abort`; firmware 1.2 also recovers after two minutes of
  serial silence.

### Safety

- A failed automatic backup used to be ignored before a wipe. It now aborts
  `remove --force` and `import --clear`; `--no-backup` is the explicit opt-out.
- The backup prompt blocked non-interactive runs. Without a terminal it
  defaults to `.bin`, and `--backup-format` selects the format explicitly.
- `remove --except` could delete everyone if every keep UID was misspelled.
  It now refuses when none of the supplied UIDs exist.
- `import --dry-run` no longer requires a device.
- The Wi-Fi password is no longer echoed when the prompt is interactive.
- Timezone offsets with a 30-minute component are formatted correctly.
- The import capacity check now runs before `--clear` wipes the database.

The test suite covers the main regressions: sync date handling, retries,
noise lines, scan events, `--except`, backup-before-wipe, import abort, and the
offline commands. Some edge cases, such as a failed backup caused by a full
disk, do not have a dedicated automated test.

## 4. Firmware 1.2.0

The full diff is in `docs/gatecore-firmware-1.1.0-to-1.2.0.diff`. It applies
cleanly to the 1.1.0 sources after they are moved into `gatecore-firmware/`.

| # | Area | Problem | Fix |
|---|---|---|---|
| F1 | `SystemController.cpp` | Renewal added the GMT/DST offset before calling `localtime()`, which applied the offset again. With no NTP sync, renewals were stamped 1970-01-01. Admin badges presented in renewal mode became expiring badges. | Use `localtime_r` on raw UTC, refuse renewal when time is not synced, skip admin badges, and return a typed `renewal_result` error with the UID. |
| F2 | `SystemController.cpp` | `import_bin` and `sync_apply` sent `ok` before allocating their buffers. A failed allocation produced an error after the host had already started sending raw bytes. | Allocate before acknowledging. |
| F3 | `SystemController.cpp`, `Config.h` | A host that disappeared mid-import left the board in `DB_BUSY` until reboot. | Add `import_abort` and reload the flash copy after two minutes without serial traffic. |
| F4 | `SerialProtocol.cpp` | `list` and `find_name` printed `valid_days` with two decimals, corrupting values such as `0.333` during a JSON export. Large values printed as `ovf`. | Add `printJsonDouble()` with a shortest round-trippable representation. |
| F5 | `DatabaseManager.cpp` | `removeAllExcept` was O(n·k) and could trip the task watchdog on a full database. | Sort the keep list and use binary search, with periodic watchdog resets. |
| F6 | `NetworkManager.cpp` | A timezone change made while offline was persisted but not applied to `localtime()`. | Apply the new offset through `configTime()` immediately. |
| F7 | `RFIDManager.cpp` | The UID buffer was 7 bytes, but the PN532 library can copy a 10-byte triple-size UID. | Use a 10-byte buffer and check the reported length. |
| F8 | `SystemController.cpp` | During `scan --infinite`, a card presented while the host re-armed scan mode could be evaluated as an access attempt and count toward the lockout. | Add the optional `continuous` flag to `enter_scan_mode`. |

`kFirmwareVersion` is `1.2.0`. The protocol version remains
`hybrid-json-ctrl+bin-bulk/v1`; the new fields and commands are additive.

## 5. Behaviour changes

- `sync` keeps existing device dates for undated file entries. Existing admin
  badges have no date, so an undated entry that matches one becomes an expiring
  badge dated today. Use explicit dates if that is not what you want.
- `--restamp` restores the old "today for every undated row" behaviour.
- A failed automatic backup now stops `remove --force` and `import --clear`.
  Use `--no-backup` when there is no backup destination.
- JSON exports represent admin badges as `"registered": null,
  "valid_days": null`. The importer also accepts rows with both fields omitted.
- `sync` can delete users and does not take a backup. Use `--dry-run` first;
  one-shot commands do not ask for confirmation.
- Exit code 130 means the process was forced to stop. The first Ctrl+C normally
  lets the active command clean up and exit on its own.
- A `.bin` export is the device's binary file format, not a dump of raw flash
  bytes. The CLI adds the 7-byte header and the device's record encoder writes
  each record.
- `import` with `--clear` performs two saves: one for the clear and one at
  `import_end`. Without `--clear`, the database is written once at the end.

Compatibility:

| Host | Firmware 1.1.0 | Firmware 1.2.0 |
|---|---|---|
| `gatecore-cli` 2.0.x | Works. `import_abort` is unavailable, so a failed import needs a reboot. | Full functionality. |
| Python CLI 1.0.1 | Works as before. | Protocol additions are backwards-compatible, but the old CLI does not use the 1.2 commands or fields. It is unmaintained and was not tested for this release. |

## 6. Build and run

Prerequisites:

- Rust 1.90 or newer. `Cargo.lock` is committed and should be used with
  `--locked`.
- Linux: `libudev-dev` and `pkg-config` for the build, plus membership in the
  `dialout` group (`uucp` on Arch) for serial access.
- Windows and macOS: the normal Rust toolchain; macOS needs the Xcode command
  line tools.

```bash
cd gatecore-cli
cargo build --release --locked
cargo test --locked
cargo install --path . --locked
```

A few commands to check the installation:

```bash
gatecore-cli list-ports
gatecore-cli --port COM5 status
gatecore-cli add --uid 04A1B2C3 --name "Alice" --valid-days 365
gatecore-cli import samples/users_example.csv --dry-run
gatecore-cli export backup.bin
gatecore-cli convert backup.bin backup.json
```

For the firmware, open `gatecore-firmware/gatecore-firmware.ino` with the board settings in the
README, or use `gatecore-firmware/uploader.ps1` on Windows and
`gatecore-firmware/uploader.sh` on Linux and macOS.

## 7. Verification

The Rust side was verified with both toolchains used by CI:

- `cargo +1.90.0 test --locked`: 51 passed.
- `cargo +stable test --locked`: 51 passed.
- `cargo +1.90.0 build --release --locked`: passes.
- `cargo +1.90.0 clippy --locked --all-targets`: completes with one advisory
  style warning (`manual_is_multiple_of` in `database.rs`).
- `cargo +1.90.0 doc --locked --no-deps`: clean.
- The firmware diff applies with `git apply --check` to the 1.1.0 sources once
  they are placed under `gatecore-firmware/`.

The tests include 38 unit tests and 13 end-to-end tests. They cover the binary
codec and golden vectors, validation, retries, noise handling, scan events,
import over both transports, export, sync and the pre-wipe backup path. The
serial layer is tested through the simulator, not a physical port.

The firmware is compiled in CI for the ESP32-S3 with the same FQBN as the
upload helpers. I have not run this revision on a board as part of this
documentation pass. Before deploying it, smoke-test card reads, renewal, scan,
import and sync on the target hardware.

## 8. Known limitations

- Replies do not carry a request ID. Retry safety depends on idempotency rules
  and the post-retry drain; it is mitigation, not a guarantee.
- `import --json-transport` is retained for old firmware and debugging. It is
  much slower than the binary path.
- The firmware validates binary record shape and CRC, but not calendar dates.
  Use the CLI or validate generated files before importing them.
- The Python CLI is no longer maintained. The historical guide is kept in the
  wiki for users of old releases.
