//! One handler per subcommand, all returning `Result<()>`.
//!
//! Handlers never print-and-exit; they return an [`Error`] and let the caller
//! (one-shot `main` or the shell) decide what to do with it. The device
//! connection is obtained through [`Ctx::device`], which opens the port
//! lazily -- so offline commands (`convert`, `list-ports`,
//! `import --dry-run`) never require hardware.

mod bulk;
mod device;
mod scan;
mod sync;
mod users;

use crate::cli::Command;
use crate::error::{Error, Result};
use crate::serial::{find_esp32_ports, list_ports, Device};
use crate::ui;

pub use bulk::{export_raw, ImportOptions};

enum Conn<'a> {
    /// One-shot mode: open on first use, close when the context drops.
    Lazy { port: Option<String>, baud: u32, dev: Option<Device> },
    /// Shell mode: borrow the shell's persistent connection.
    Shared(&'a mut Device),
    /// Shell mode while disconnected.
    Offline,
}

/// Execution context handed to every command.
pub struct Ctx<'a> {
    conn: Conn<'a>,
    verbose: bool,
}

impl<'a> Ctx<'a> {
    pub fn one_shot(port: Option<String>, baud: u32, verbose: bool) -> Ctx<'static> {
        Ctx { conn: Conn::Lazy { port, baud, dev: None }, verbose }
    }

    pub fn shared(dev: &'a mut Device) -> Ctx<'a> {
        Ctx { conn: Conn::Shared(dev), verbose: false }
    }

    pub fn offline() -> Ctx<'static> {
        Ctx { conn: Conn::Offline, verbose: false }
    }

    /// The device connection, opening it if needed.
    pub fn device(&mut self) -> Result<&mut Device> {
        let verbose = self.verbose;
        match &mut self.conn {
            Conn::Shared(d) => Ok(d),
            Conn::Offline => Err(Error::NoDevice("not connected (use 'connect <port>' or 'reconnect')".into())),
            Conn::Lazy { port, baud, dev } => {
                if dev.is_none() {
                    let name = match port.clone() {
                        Some(p) => p,
                        None => auto_detect_port()?,
                    };
                    *dev = Some(Device::open(&name, *baud, verbose)?);
                }
                Ok(dev.as_mut().expect("just opened"))
            }
        }
    }
}

/// Pick the ESP32 port automatically, explaining what was seen on failure.
pub fn auto_detect_port() -> Result<String> {
    let candidates = find_esp32_ports();
    match candidates.as_slice() {
        [] => {
            let seen: Vec<String> = list_ports().into_iter().map(|p| p.device).collect();
            Err(Error::NoDevice(format!(
                "could not auto-detect the ESP32 (ports seen: {}). Plug it in or pass --port.",
                if seen.is_empty() { "none".to_string() } else { seen.join(", ") }
            )))
        }
        [only] => Ok(only.clone()),
        [first, rest @ ..] => {
            ui::info(&format!(
                "Several candidate ports ({first}, {}); using {first}. Pass --port to choose.",
                rest.join(", ")
            ));
            Ok(first.clone())
        }
    }
}

/// Run one parsed command.
pub fn run(cmd: &Command, ctx: &mut Ctx<'_>) -> Result<()> {
    match cmd {
        Command::List => users::list(ctx),
        Command::Find { uid, name } => users::find(ctx, uid.as_deref(), name.as_deref()),
        Command::Add { uid, name, valid_days } => users::add(ctx, uid.clone(), name.clone(), *valid_days),
        Command::Remove { uid, force, no_backup, backup_format, except } => {
            users::remove(ctx, uid.clone(), *force, *no_backup, *backup_format, except.as_deref())
        }
        Command::Rename { uid, name } => users::rename(ctx, uid.clone(), name.clone()),
        Command::Status => device::status(ctx),
        Command::Netstatus => device::netstatus(ctx),
        Command::NtpTime => device::ntp_time(ctx),
        Command::NtpSync => device::ntp_sync(ctx),
        Command::Configure { ssid, password } => device::configure_wifi(ctx, ssid.clone(), password.clone()),
        Command::Timezone { gmt_offset_sec, daylight_offset_sec } => {
            device::timezone(ctx, *gmt_offset_sec, *daylight_offset_sec)
        }
        Command::ListPorts => device::list_ports_cmd(),
        Command::Scan { timeout, infinite } => scan::scan(ctx, *timeout, *infinite),
        Command::TagRenew { valid_days, quota } => scan::tag_renew(ctx, *valid_days, *quota),
        Command::Import { file, dry_run, clear, no_backup, backup_format, json_transport } => bulk::import(
            ctx,
            file,
            &ImportOptions {
                dry_run: *dry_run,
                clear: *clear,
                no_backup: *no_backup,
                backup_format: *backup_format,
                json_transport: *json_transport,
            },
        ),
        Command::Export { file, json_transport } => bulk::export(ctx, file, *json_transport),
        Command::Sync { file, dry_run, stats, restamp } => sync::sync(ctx, file, *dry_run, *stats, *restamp),
        Command::Convert { src, dst } => bulk::convert(src, dst),
    }
}

/// Print per-row problems from a user file, capped so a 100k-row file with
/// a systematic mistake doesn't flood the terminal.
pub(crate) fn report_issues(issues: &[crate::userfile::Issue]) {
    const SHOWN: usize = 20;
    for i in issues.iter().take(SHOWN) {
        ui::warning(&format!("  skipping {i}"));
    }
    if issues.len() > SHOWN {
        ui::warning(&format!("  ... and {} more skipped row(s)", issues.len() - SHOWN));
    }
}
