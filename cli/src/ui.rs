//! Terminal output: coloured status lines, tables, progress bars.
//!
//! Status lines (`info`/`success`) go to stdout, problems (`warning`/`error`)
//! to stderr, so `rfid-cli export ... 2>/dev/null` stays clean. Colours honour
//! `NO_COLOR`, `CLICOLOR_FORCE`, `--no-color` and non-TTY output via the
//! `colored` crate.

use std::io::IsTerminal;

use colored::Colorize;
use comfy_table::presets::UTF8_FULL_CONDENSED;
use comfy_table::{Attribute, Cell, Color, ContentArrangement, Table};
use indicatif::{ProgressBar, ProgressStyle};

use crate::database::UserEntry;
use crate::protocol::{DeviceStatus, NetStatus};
use crate::serial::PortInfo;
use crate::sync_stats::SyncStats;

pub fn info(msg: &str) {
    println!("{} {msg}", "i".cyan().bold());
}

pub fn success(msg: &str) {
    println!("{} {msg}", "OK".green().bold());
}

pub fn warning(msg: &str) {
    eprintln!("{} {msg}", "WARN".yellow().bold());
}

pub fn error(msg: &str) {
    eprintln!("{} {msg}", "ERROR".red().bold());
}

pub fn debug(msg: &str) {
    eprintln!("{}", msg.dimmed());
}

pub fn set_color(enabled: bool) {
    colored::control::set_override(enabled);
}

fn colors_on() -> bool {
    colored::control::SHOULD_COLORIZE.should_colorize()
}

fn new_table(title_cols: &[&str]) -> Table {
    let mut t = Table::new();
    t.load_preset(UTF8_FULL_CONDENSED).set_content_arrangement(ContentArrangement::Dynamic);
    if !colors_on() {
        t.force_no_tty();
    }
    t.set_header(title_cols.iter().map(|c| Cell::new(c).add_attribute(Attribute::Bold)));
    t
}

pub fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}

/// Bytes per second, human readable.
pub fn rate(bytes: usize, secs: f64) -> String {
    if secs <= 0.0 {
        return "n/a".into();
    }
    format!("{}/s", human_bytes((bytes as f64 / secs) as u64))
}

pub fn print_users(users: &[UserEntry], title: &str) {
    if users.is_empty() {
        info("No users registered.");
        return;
    }
    println!("{}", format!("{title} ({})", users.len()).bold());
    let mut t = new_table(&["UID", "Name", "Registered", "Valid days", "Expires (approx.)"]);
    for u in users {
        let name = if u.is_admin() {
            Cell::new(format!("(ADMIN) {}", u.name)).fg(Color::Yellow).add_attribute(Attribute::Bold)
        } else {
            Cell::new(&u.name)
        };
        t.add_row(vec![
            Cell::new(u.uid.as_str()).fg(Color::Magenta),
            name,
            Cell::new(u.registered_label()),
            Cell::new(u.valid_days_label()),
            Cell::new(u.expiration_label()).fg(Color::Yellow),
        ]);
    }
    println!("{t}");
}

pub fn print_status(s: &DeviceStatus) {
    let used_pct = if s.fs_total_bytes > 0 { s.fs_used_bytes as f64 / s.fs_total_bytes as f64 * 100.0 } else { 0.0 };
    let mut t = new_table(&["Field", "Value"]);
    t.add_row(vec![Cell::new("Database file").fg(Color::Cyan), Cell::new(&s.db_path)]);
    t.add_row(vec![Cell::new("Registered users").fg(Color::Cyan), Cell::new(s.user_count)]);
    t.add_row(vec![
        Cell::new("Storage used").fg(Color::Cyan),
        Cell::new(format!("{} / {} ({used_pct:.1}%)", human_bytes(s.fs_used_bytes), human_bytes(s.fs_total_bytes))),
    ]);
    t.add_row(vec![Cell::new("Storage free").fg(Color::Cyan), Cell::new(human_bytes(s.fs_free_bytes))]);
    if let Some(crc) = s.db_crc32 {
        t.add_row(vec![Cell::new("DB checksum").fg(Color::Cyan), Cell::new(format!("0x{crc:08x}"))]);
    }
    if let Some(fw) = &s.firmware_version {
        t.add_row(vec![Cell::new("Firmware").fg(Color::Cyan), Cell::new(fw)]);
    }
    if let Some(p) = &s.protocol_version {
        t.add_row(vec![Cell::new("Protocol").fg(Color::Cyan), Cell::new(p)]);
    }
    println!("{}", "Device Status".bold());
    println!("{t}");

    const WIDTH: usize = 40;
    let filled = ((WIDTH as f64) * used_pct / 100.0).round().clamp(0.0, WIDTH as f64) as usize;
    let bar = "█".repeat(filled);
    let bar = if used_pct < 75.0 {
        bar.green()
    } else if used_pct < 90.0 {
        bar.yellow()
    } else {
        bar.red()
    };
    println!("{bar}{}", "░".repeat(WIDTH - filled).bright_black());
}

fn rssi_bars(rssi: i64) -> &'static str {
    match rssi {
        0 => "",
        r if r >= -55 => "████",
        r if r >= -65 => "███░",
        r if r >= -75 => "██░░",
        r if r >= -85 => "█░░░",
        _ => "░░░░",
    }
}

pub fn print_net_status(s: &NetStatus) {
    let mut t = new_table(&["Field", "Value"]);
    let field = |n: &str| Cell::new(n).fg(Color::Cyan);
    if s.connected {
        t.add_row(vec![field("Wi-Fi"), Cell::new("Connected").fg(Color::Green)]);
        t.add_row(vec![field("SSID"), Cell::new(if s.ssid.is_empty() { "(unknown)" } else { &s.ssid })]);
        t.add_row(vec![field("IP address"), Cell::new(if s.ip.is_empty() { "(unknown)" } else { &s.ip })]);
        t.add_row(vec![field("Signal (RSSI)"), Cell::new(format!("{} dBm  {}", s.rssi, rssi_bars(s.rssi)))]);
    } else {
        t.add_row(vec![field("Wi-Fi"), Cell::new("Not connected").fg(Color::Red)]);
        t.add_row(vec![field("SSID"), Cell::new("(none)")]);
        t.add_row(vec![field("IP address"), Cell::new("(none)")]);
    }
    t.add_row(vec![
        field("NTP time"),
        if s.time_synced { Cell::new("Synced").fg(Color::Green) } else { Cell::new("Not synced").fg(Color::Red) },
    ]);
    println!("{}", "Network Status".bold());
    println!("{t}");
}

pub fn print_ports(ports: &[PortInfo], esp32: &[String]) {
    if ports.is_empty() {
        info("No serial ports detected.");
        return;
    }
    let id = |v: Option<u16>| v.map(|x| format!("0x{x:04X}")).unwrap_or_else(|| "-".into());
    let mut t = new_table(&["Device", "VID", "PID", "Manufacturer", "Description", "ESP32?"]);
    for p in ports {
        let guess = esp32.iter().position(|d| d == &p.device);
        t.add_row(vec![
            Cell::new(&p.device).fg(Color::Magenta),
            Cell::new(id(p.vid)).fg(Color::Yellow),
            Cell::new(id(p.pid)).fg(Color::Yellow),
            Cell::new(&p.manufacturer),
            Cell::new(&p.description),
            match guess {
                Some(0) => Cell::new("yes (default)").fg(Color::Green),
                Some(_) => Cell::new("candidate").fg(Color::Green),
                None => Cell::new(""),
            },
        ]);
    }
    println!("{}", format!("Serial Ports ({})", ports.len()).bold());
    println!("{t}");
}

/// Byte-count progress bar on stderr; hidden when stderr isn't a terminal
/// or the transfer is too small to be worth one.
pub fn progress_bytes(total: usize, label: &str) -> ProgressBar {
    if total < 64 * 1024 || !std::io::stderr().is_terminal() {
        return ProgressBar::hidden();
    }
    let pb = ProgressBar::new(total as u64);
    pb.set_style(
        ProgressStyle::with_template("{msg:>10} [{bar:32.cyan/blue}] {bytes}/{total_bytes} {bytes_per_sec} eta {eta}")
            .expect("valid template")
            .progress_chars("█▉▊▋▌▍▎▏ "),
    );
    pb.set_message(label.to_string());
    pb
}

// sync --stats report

fn section(icon: &str, title: &str, detail: &str, timing: &str) {
    println!("\n {}", format!("{icon}  {title}").bold());
    println!("    {}", detail.dimmed());
    if !timing.is_empty() {
        println!("    {timing}");
    }
}

fn ms(secs: f64) -> String {
    format!("{:.0} ms", secs * 1000.0)
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

pub fn print_sync_stats(s: &SyncStats) {
    let skip = if s.fast_path {
        Some("skipped (device already matched -- nothing to sync)")
    } else if s.dry_run {
        Some("skipped (dry run)")
    } else if s.nothing_to_apply {
        Some("skipped (diff was empty)")
    } else {
        None
    };

    let crc = match s.crc_match {
        Some(true) => "CRC match".green().to_string(),
        Some(false) => "CRC mismatch".yellow().to_string(),
        None => "not applicable (file has undated users -- diff required)".to_string(),
    };
    section("◆", "CRC fast-path", &crc, &format!("({})", ms(s.crc_fastpath_s)));

    if s.fast_path {
        section("◆", "Manifest transfer", skip.unwrap_or_default(), "");
        section("◆", "Diff computation", skip.unwrap_or_default(), "");
    } else {
        section(
            "◆",
            "Manifest transfer",
            &format!("{} • {}", plural(s.manifest_count, "entry", "entries"), human_bytes(s.manifest_bytes as u64)),
            &format!("{} ({})", ms(s.manifest_s), rate(s.manifest_bytes, s.manifest_s)),
        );
        if s.resolved_dates > 0 || s.resolve_s > 0.0 {
            section(
                "◆",
                "Registration-date lookup",
                &format!("{} preserved from the device", plural(s.resolved_dates, "date", "dates")),
                &ms(s.resolve_s),
            );
        }
        section(
            "◆",
            "Diff computation",
            &format!("{} remove • {} add • {} replace • {} unchanged", s.remove_n, s.add_n, s.replace_n, s.unchanged_n),
            &ms(s.diff_s),
        );
    }

    if let Some(reason) = skip {
        for t in ["Sync transfer", "Apply changes", "Save database", "Final CRC verification"] {
            section("◆", t, reason, "");
        }
    } else {
        let n = s.remove_n + s.add_n + s.replace_n;
        section(
            "◆",
            "Sync transfer",
            &format!("{} • {}", plural(n, "record", "records"), human_bytes(s.transfer_bytes as u64)),
            &format!("{} ({})", ms(s.transfer_s), rate(s.transfer_bytes, s.transfer_s)),
        );
        section(
            "◆",
            "Apply changes",
            &format!("remove={}ms • add={}ms • replace={}ms", s.apply_remove_ms, s.apply_add_ms, s.apply_replace_ms),
            &format!("{} ms", s.apply_remove_ms + s.apply_add_ms + s.apply_replace_ms),
        );
        section(
            "◆",
            "Save database",
            &format!(
                "encode={}ms • write={}ms • verify={}ms • finalize={}ms",
                s.save_encode_ms, s.save_write_ms, s.save_verify_ms, s.save_finalize_ms
            ),
            &format!("{} ms", s.save_total_ms),
        );
        let verdict = if s.final_crc_match {
            "host == device".green().to_string()
        } else {
            "host != device (MISMATCH)".red().to_string()
        };
        section("◆", "Final CRC verification", &verdict, "");
    }
    println!("\n {}", "◆  Total time elapsed".bold());
    println!("    {}", ms(s.total_s));
}

/// Ask a yes/no question (default: no). Non-interactive stdin -> `default`.
pub fn confirm(question: &str, default: bool) -> crate::Result<bool> {
    if !std::io::stdin().is_terminal() {
        return Ok(default);
    }
    let suffix = if default { "[Y/n]" } else { "[y/N]" };
    let answer = prompt_line(&format!("{question} {suffix} "))?;
    let a = answer.trim().to_lowercase();
    Ok(if a.is_empty() { default } else { a.starts_with('y') })
}

/// Read one line from the user via rustyline (Ctrl+C / Ctrl+D -> Cancelled).
pub fn prompt_line(label: &str) -> crate::Result<String> {
    let mut rl = rustyline::DefaultEditor::new().map_err(|e| crate::Error::usage(e.to_string()))?;
    match rl.readline(label) {
        Ok(s) => Ok(s.trim().to_string()),
        Err(rustyline::error::ReadlineError::Interrupted | rustyline::error::ReadlineError::Eof) => {
            Err(crate::Error::Cancelled)
        }
        Err(e) => Err(crate::Error::usage(e.to_string())),
    }
}

/// Read a secret without echo (falls back to a visible prompt if no TTY).
pub fn prompt_secret(label: &str) -> crate::Result<String> {
    if std::io::stdin().is_terminal() {
        rpassword::prompt_password(label).map_err(crate::Error::Io)
    } else {
        prompt_line(label)
    }
}
