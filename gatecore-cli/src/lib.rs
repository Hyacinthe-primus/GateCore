//! `gatecore_cli` -- host-side management library for GateCore on the
//! ESP32-S3 firmware.
//!
//! The binary (`src/main.rs`) is a thin shell around this library so every
//! layer below can be unit- and integration-tested without hardware (see
//! `tests/device_sim.rs`, which drives the real [`serial::Device`] against an
//! in-memory firmware simulator through the [`serial::Link`] trait).
//!
//! Layering (each module only depends on the ones above it):
//!
//! | module         | responsibility                                            |
//! |----------------|-----------------------------------------------------------|
//! | [`error`]      | one error type, exit codes, "is this a link failure?"     |
//! | [`interrupt`]  | Ctrl+C as a polled cancellation flag                      |
//! | [`convert`]    | `users.bin` codec, CRC32, manifest/uid wire entries       |
//! | [`database`]   | validated domain model (UID, names, dates, validity)      |
//! | [`userfile`]   | JSON / CSV / .bin user files (load, save, convert)        |
//! | [`protocol`]   | typed requests and responses of the newline-JSON protocol |
//! | [`serial`]     | port discovery, framing, retries, raw binary transfers    |
//! | [`ui`]         | coloured messages, tables, progress bars                  |
//! | [`sync_stats`] | timing model for `sync --stats`                           |
//! | [`cli`]        | clap definitions (shared by one-shot mode and the shell)  |
//! | [`commands`]   | one handler per subcommand                                |
//! | [`repl`]       | interactive shell                                         |

pub mod cli;
pub mod commands;
pub mod convert;
pub mod database;
pub mod error;
pub mod interrupt;
pub mod protocol;
pub mod repl;
pub mod serial;
pub mod sync_stats;
pub mod ui;
pub mod userfile;

pub use error::{Error, Result};
