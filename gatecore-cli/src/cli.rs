//! Command-line definitions (clap derive).
//!
//! The same [`Command`] enum is parsed in one-shot mode (`gatecore-cli status`)
//! and inside the interactive shell, so flags, help text and validation
//! cannot drift between the two.

use std::path::PathBuf;
use std::str::FromStr;

use clap::{ArgGroup, Parser, Subcommand, ValueEnum};

use crate::database::parse_valid_days;
use crate::serial::DEFAULT_BAUD;

#[derive(Debug, Parser)]
#[command(
    name = "gatecore-cli",
    version,
    about = "Manage the GateCore user database on the ESP32-S3 over serial.",
    long_about = "Manage the GateCore user database on the ESP32-S3 over serial.\n\n\
                  Run without a subcommand to enter the interactive shell; run with a \
                  subcommand for one-shot, scriptable use. The serial port is auto-detected \
                  (override with --port or RFID_PORT).",
    after_help = "Examples:\n  gatecore-cli                         # interactive shell\n  \
                  gatecore-cli status\n  gatecore-cli add --uid 04A1B2C3 --name \"Alice\" --valid-days 365\n  \
                  gatecore-cli sync users.json --dry-run\n  gatecore-cli export backup.bin\n  \
                  gatecore-cli convert users.json users.bin"
)]
pub struct Cli {
    /// Serial port (default: auto-detect). Also used by the shell to skip port selection.
    #[arg(long, global = true, env = "RFID_PORT")]
    pub port: Option<String>,

    /// Baud rate (must match SERIAL_BAUD in gatecore-firmware/Config.h).
    #[arg(long, global = true, env = "RFID_BAUD", default_value_t = DEFAULT_BAUD)]
    pub baud: u32,

    /// Trace every protocol line sent/received (passwords are masked).
    #[arg(short, long, global = true)]
    pub verbose: bool,

    /// Disable coloured output (NO_COLOR is honoured too).
    #[arg(long, global = true)]
    pub no_color: bool,

    #[command(subcommand)]
    pub command: Option<Command>,
}

/// Parser used for each line typed in the interactive shell.
#[derive(Debug, Parser)]
#[command(name = "", no_binary_name = true, disable_version_flag = true)]
pub struct ShellLine {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ValueEnum)]
pub enum BackupFormat {
    /// Ask interactively (falls back to `bin` when stdin is not a terminal).
    #[default]
    Ask,
    /// Human-readable JSON (decoded on this machine).
    Json,
    /// Raw users.bin, byte-for-byte as exported (re-importable as is).
    Bin,
}

/// `--quota N` or `--quota none`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Quota {
    #[default]
    Unlimited,
    Max(u32),
}

impl FromStr for Quota {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.eq_ignore_ascii_case("none") {
            return Ok(Quota::Unlimited);
        }
        match s.parse::<u32>() {
            Ok(n) if n > 0 => Ok(Quota::Max(n)),
            _ => Err(format!("invalid quota '{s}' (a positive number, or 'none')")),
        }
    }
}

fn days_arg(s: &str) -> Result<f64, String> {
    parse_valid_days(s)
}

fn gmt_offset_arg(s: &str) -> Result<i64, String> {
    let v: i64 = s.parse().map_err(|_| format!("'{s}' is not an integer number of seconds"))?;
    if !(-12 * 3600..=14 * 3600).contains(&v) {
        return Err(format!("{v} s is outside UTC-12:00 .. UTC+14:00"));
    }
    Ok(v)
}

fn dst_offset_arg(s: &str) -> Result<i64, String> {
    let v: i64 = s.parse().map_err(|_| format!("'{s}' is not an integer number of seconds"))?;
    if !(-3600..=7200).contains(&v) {
        return Err(format!("{v} s is outside the plausible DST range (-3600 .. 7200)"));
    }
    Ok(v)
}

#[derive(Clone, Debug, Subcommand)]
pub enum Command {
    /// Show all registered users
    #[command(visible_alias = "ls")]
    List,

    /// Find a user by exact UID (device-side binary search) or by name (partial match)
    #[command(group(ArgGroup::new("by").required(true).args(["uid", "name"])))]
    Find {
        /// Exact UID to look up
        #[arg(long)]
        uid: Option<String>,
        /// Case-insensitive substring of the name
        #[arg(long)]
        name: Option<String>,
    },

    /// Register a new user (prompts for missing UID/name)
    Add {
        #[arg(long)]
        uid: Option<String>,
        #[arg(long)]
        name: Option<String>,
        /// Days the badge stays valid from today (decimals allowed, e.g. 0.5).
        /// Omit to register an ADMIN badge that never expires.
        #[arg(long, value_parser = days_arg)]
        valid_days: Option<f64>,
    },

    /// Delete one user, every user (--force), or everyone except some (--except)
    #[command(visible_alias = "rm")]
    Remove {
        /// UID to delete
        #[arg(long, conflicts_with_all = ["force", "except"])]
        uid: Option<String>,
        /// Delete ALL users (a backup is taken first unless --no-backup)
        #[arg(long, conflicts_with = "except")]
        force: bool,
        /// Skip the automatic pre-wipe backup (only with --force)
        #[arg(long, requires = "force")]
        no_backup: bool,
        /// Format of the automatic backup
        #[arg(long, value_enum, default_value_t = BackupFormat::Ask)]
        backup_format: BackupFormat,
        /// Keep only these UIDs (comma-separated) and delete everyone else
        #[arg(long, value_name = "UID[,UID...]")]
        except: Option<String>,
    },

    /// Rename an existing user (prompts for missing UID/name)
    Rename {
        #[arg(long)]
        uid: Option<String>,
        #[arg(long)]
        name: Option<String>,
    },

    /// Show DB file, user count, checksum and LittleFS usage
    Status,

    /// Show Wi-Fi state (connected? SSID? IP? signal?)
    Netstatus,

    /// Show the device's current local time (after NTP sync)
    NtpTime,

    /// Force an NTP resync on the device
    NtpSync,

    /// Read the UID of the next presented card
    Scan {
        /// Seconds to wait for a card
        #[arg(long, default_value_t = 30.0)]
        timeout: f64,
        /// Keep scanning cards until Ctrl+C
        #[arg(long)]
        infinite: bool,
    },

    /// Renew existing badges: each presented card gets registered=today, valid_days=N
    TagRenew {
        /// New validity period in days
        #[arg(value_parser = days_arg)]
        valid_days: f64,
        /// Stop after this many renewals ('none' = until Ctrl+C)
        #[arg(long, default_value = "none")]
        quota: Quota,
    },

    /// List serial ports with VID/PID (to debug auto-detection)
    ListPorts,

    /// Provision the device's Wi-Fi credentials (password is prompted without echo)
    Configure {
        /// Wi-Fi SSID
        #[arg(short = 'w', long = "wifi")]
        ssid: Option<String>,
        /// Wi-Fi password (prefer the hidden prompt: command-line args end up in shell history)
        #[arg(short, long)]
        password: Option<String>,
    },

    /// Set and persist the device's timezone (no reflash needed)
    Timezone {
        /// GMT offset in seconds, e.g. 3600 for UTC+1, -18000 for UTC-5
        #[arg(long = "offset", allow_negative_numbers = true, value_parser = gmt_offset_arg)]
        gmt_offset_sec: i64,
        /// Daylight-saving offset in seconds, added on top of --offset
        #[arg(long = "dst", allow_negative_numbers = true, default_value_t = 0, value_parser = dst_offset_arg)]
        daylight_offset_sec: i64,
    },

    /// Import users from JSON, CSV or .bin (one flash write at the end)
    Import {
        /// JSON, CSV or .bin file
        file: PathBuf,
        /// Parse and validate only; no device needed
        #[arg(long)]
        dry_run: bool,
        /// Wipe all existing users first (backup taken unless --no-backup)
        #[arg(long)]
        clear: bool,
        /// Skip the automatic pre-wipe backup (only with --clear)
        #[arg(long, requires = "clear")]
        no_backup: bool,
        /// Format of the automatic backup
        #[arg(long, value_enum, default_value_t = BackupFormat::Ask)]
        backup_format: BackupFormat,
        /// Use the legacy per-batch JSON transport (older firmware / debugging)
        #[arg(long)]
        json_transport: bool,
    },

    /// Export every user to a JSON, CSV or .bin file
    Export {
        /// Output path; the extension picks the format (.json, .csv, .bin)
        file: PathBuf,
        /// Use the legacy JSON 'list' transport (older firmware / debugging)
        #[arg(long)]
        json_transport: bool,
    },

    /// Make the device DB exactly match a file (merge-diff: only sends what differs)
    Sync {
        /// JSON, CSV or .bin file
        file: PathBuf,
        /// Show the diff without changing the device
        #[arg(long)]
        dry_run: bool,
        /// Print a timing breakdown of every stage
        #[arg(long)]
        stats: bool,
        /// Stamp today's date on users whose 'registered' is missing in the file,
        /// even if they already exist on the device (legacy behaviour: extends their validity)
        #[arg(long)]
        restamp: bool,
    },

    /// Convert offline between JSON, CSV and .bin (replaces convert.py)
    Convert {
        /// Source file (.json, .csv or .bin)
        src: PathBuf,
        /// Destination file (.json, .csv or .bin)
        dst: PathBuf,
    },
}

impl Command {
    /// Name as typed by the user (for the shell's grouping/help).
    pub fn name(&self) -> &'static str {
        match self {
            Command::List => "list",
            Command::Find { .. } => "find",
            Command::Add { .. } => "add",
            Command::Remove { .. } => "remove",
            Command::Rename { .. } => "rename",
            Command::Status => "status",
            Command::Netstatus => "netstatus",
            Command::NtpTime => "ntp-time",
            Command::NtpSync => "ntp-sync",
            Command::Scan { .. } => "scan",
            Command::TagRenew { .. } => "tag-renew",
            Command::ListPorts => "list-ports",
            Command::Configure { .. } => "configure",
            Command::Timezone { .. } => "timezone",
            Command::Import { .. } => "import",
            Command::Export { .. } => "export",
            Command::Sync { .. } => "sync",
            Command::Convert { .. } => "convert",
        }
    }

    /// Whether this invocation will talk to the device at all.
    pub fn needs_device(&self) -> bool {
        !matches!(self, Command::ListPorts | Command::Convert { .. } | Command::Import { dry_run: true, .. })
    }

    /// Destructive invocations the shell asks to confirm.
    pub fn destructive_warning(&self) -> Option<&'static str> {
        match self {
            Command::Remove { force: true, .. } => Some("This will delete ALL users from the device."),
            Command::Remove { except: Some(_), .. } => Some("This will delete every user NOT in the --except list."),
            Command::Import { clear: true, dry_run: false, .. } => Some("This will wipe existing users before importing."),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn clap_definition_is_consistent() {
        Cli::command().debug_assert();
        ShellLine::command().debug_assert();
    }

    #[test]
    fn names_match_clap() {
        let cmd = Cli::command();
        let parse = |args: &[&str]| ShellLine::try_parse_from(args).unwrap().command;
        for sub in cmd.get_subcommands() {
            let name = sub.get_name();
            let args: Vec<&str> = match name {
                "find" => vec![name, "--uid", "04AABBCC"],
                "tag-renew" => vec![name, "1"],
                "timezone" => vec![name, "--offset", "0"],
                "import" | "export" | "sync" => vec![name, "x.json"],
                "convert" => vec![name, "a.json", "b.bin"],
                _ => vec![name],
            };
            assert_eq!(parse(&args).name(), name);
        }
    }

    #[test]
    fn flag_rules() {
        assert!(ShellLine::try_parse_from(["remove", "--uid", "04AABBCC", "--force"]).is_err());
        assert!(ShellLine::try_parse_from(["remove", "--no-backup"]).is_err());
        assert!(ShellLine::try_parse_from(["find"]).is_err());
        assert!(ShellLine::try_parse_from(["add", "--valid-days", "-1"]).is_err());
        assert!(ShellLine::try_parse_from(["add", "--valid-days", "nan"]).is_err());
        assert!(ShellLine::try_parse_from(["timezone", "--offset", "-18000"]).is_ok());
        assert!(ShellLine::try_parse_from(["timezone", "--offset", "99999"]).is_err());
        assert!(ShellLine::try_parse_from(["tag-renew", "5", "--quota", "0"]).is_err());
        assert!(ShellLine::try_parse_from(["ls"]).is_ok());
        assert!(ShellLine::try_parse_from(["rm", "--uid", "04AABBCC"]).is_ok());
        let c = ShellLine::try_parse_from(["import", "f.csv", "--dry-run"]).unwrap().command;
        assert!(!c.needs_device());
    }
}
