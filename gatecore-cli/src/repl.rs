//! Interactive shell (`gatecore-cli` with no subcommand).
//!
//! Lines are parsed by the same clap definitions as one-shot mode
//! ([`ShellLine`]) and executed by the same [`commands::run`], against one
//! persistent connection. Shell-only meta commands: `help`, `history`,
//! `version`, `clear`, `connect <port>`, `disconnect`, `reconnect`,
//! `exit`/`quit` (or Ctrl+D). Ctrl+C clears the current line at the prompt
//! and interrupts a running command.

use std::borrow::Cow;
use std::path::PathBuf;
use std::time::Instant;

use clap::{CommandFactory, Parser};
use colored::Colorize;
use rustyline::completion::{Completer, FilenameCompleter, Pair};
use rustyline::error::ReadlineError;
use rustyline::highlight::Highlighter;
use rustyline::hint::Hinter;
use rustyline::history::FileHistory;
use rustyline::validate::Validator;
use rustyline::{Context, Editor, Helper};

use crate::cli::{Cli, ShellLine};
use crate::commands::{self, Ctx};
use crate::error::{Error, Result};
use crate::protocol::Request;
use crate::serial::{find_esp32_ports, Device};
use crate::{interrupt, ui};

const META: &[(&str, &str)] = &[
    ("connect", "Connect to a device: connect <port>"),
    ("disconnect", "Disconnect without leaving the shell"),
    ("reconnect", "Reconnect to the last device"),
    ("help", "Show this help (or: <command> --help)"),
    ("history", "Show command history"),
    ("version", "Show CLI, firmware and protocol versions"),
    ("clear", "Clear the screen"),
    ("exit", "Leave the shell (also: quit, Ctrl+D)"),
];

const GROUPS: &[(&str, &[&str])] = &[
    ("Database", &["list", "find", "add", "remove", "rename", "import", "export", "sync"]),
    ("Device", &["status", "netstatus", "scan", "tag-renew", "configure"]),
    ("Time", &["ntp-time", "ntp-sync", "timezone"]),
    ("Offline", &["convert", "list-ports"]),
    ("Shell", &["connect", "disconnect", "reconnect", "help", "history", "version", "clear", "exit"]),
];

const FILE_COMMANDS: &[&str] = &["import", "export", "sync", "convert"];

// Connection state

struct Conn {
    dev: Option<Device>,
    last_port: Option<String>,
    baud: u32,
    verbose: bool,
}

impl Conn {
    fn label(&self) -> String {
        match &self.dev {
            Some(d) => d.port_name().to_string(),
            None => "disconnected".into(),
        }
    }

    fn connect(&mut self, port: &str) -> bool {
        self.dev = None; // close any previous port first (Windows can't open it twice)
        ui::info(&format!("Connecting to {port}..."));
        match Device::open(port, self.baud, self.verbose) {
            Ok(d) => {
                self.dev = Some(d);
                self.last_port = Some(port.to_string());
                true
            }
            Err(e) => {
                interrupt::clear();
                ui::error(&e.to_string());
                false
            }
        }
    }

    fn disconnect(&mut self) {
        if let Some(d) = self.dev.take() {
            self.last_port = Some(d.port_name().to_string());
        }
    }
}

fn select_port() -> Result<Option<String>> {
    let candidates = find_esp32_ports();
    match candidates.len() {
        1 => return Ok(Some(candidates[0].clone())),
        0 => ui::info("No ESP32 detected automatically."),
        _ => {
            println!("Available serial ports:\n");
            for (i, p) in candidates.iter().enumerate() {
                println!("  [{}] {p}", i + 1);
            }
            println!();
            loop {
                let choice = ui::prompt_line("Select port (number, or a port name; blank to skip): ")?;
                if choice.is_empty() {
                    return Ok(None);
                }
                match choice.parse::<usize>() {
                    Ok(n) if (1..=candidates.len()).contains(&n) => return Ok(Some(candidates[n - 1].clone())),
                    Ok(_) => ui::error("Invalid selection."),
                    Err(_) => return Ok(Some(choice)),
                }
            }
        }
    }
    let port = ui::prompt_line("Enter serial port (COM5, /dev/ttyUSB0, ...) or blank to continue disconnected: ")?;
    Ok((!port.is_empty()).then_some(port))
}

// rustyline helper: completion + coloured prompt

struct ShellHelper {
    words: Vec<String>,
    flags: Vec<(String, Vec<String>)>,
    files: FilenameCompleter,
}

impl ShellHelper {
    fn new() -> ShellHelper {
        let cmd = Cli::command();
        let mut words: Vec<String> = META.iter().map(|(n, _)| n.to_string()).collect();
        words.push("quit".into());
        let mut flags = Vec::new();
        for sub in cmd.get_subcommands() {
            let longs: Vec<String> = sub.get_arguments().filter_map(|a| a.get_long()).map(|l| format!("--{l}")).collect();
            for name in std::iter::once(sub.get_name()).chain(sub.get_visible_aliases()) {
                words.push(name.to_string());
                flags.push((name.to_string(), longs.clone()));
            }
        }
        words.sort();
        words.dedup();
        ShellHelper { words, flags, files: FilenameCompleter::new() }
    }
}

impl Completer for ShellHelper {
    type Candidate = Pair;

    fn complete(&self, line: &str, pos: usize, ctx: &Context<'_>) -> rustyline::Result<(usize, Vec<Pair>)> {
        let head = &line[..pos];
        let start = head.rfind(char::is_whitespace).map(|i| i + 1).unwrap_or(0);
        let word = &head[start..];
        let pair = |s: &String| Pair { display: s.clone(), replacement: s.clone() };
        if start == 0 {
            return Ok((0, self.words.iter().filter(|w| w.starts_with(word)).map(pair).collect()));
        }
        let cmd = head.split_whitespace().next().unwrap_or("");
        if word.starts_with('-') {
            let list = self.flags.iter().find(|(n, _)| n == cmd).map(|(_, f)| f.as_slice()).unwrap_or(&[]);
            return Ok((start, list.iter().filter(|f| f.starts_with(word)).map(pair).collect()));
        }
        if FILE_COMMANDS.contains(&cmd) {
            return self.files.complete(line, pos, ctx);
        }
        Ok((pos, Vec::new()))
    }
}

impl Hinter for ShellHelper {
    type Hint = String;
}

impl Highlighter for ShellHelper {
    fn highlight_prompt<'b, 's: 'b, 'p: 'b>(&'s self, prompt: &'p str, _default: bool) -> Cow<'b, str> {
        Cow::Owned(prompt.green().bold().to_string())
    }
}

impl Validator for ShellHelper {}
impl Helper for ShellHelper {}

// Output helpers

fn print_banner(conn: &mut Conn) {
    let line = "═".repeat(50);
    println!("{}", format!("╔{line}╗").cyan());
    println!("{}", format!("║{:^50}║", "GateCore").cyan().bold());
    println!("{}", format!("║{:^50}║", format!("CLI v{}", env!("CARGO_PKG_VERSION"))).cyan());
    println!("{}", format!("╚{line}╝").cyan());
    match &mut conn.dev {
        Some(dev) => {
            println!("Connected to {} (ESP32-S3)", dev.port_name());
            if let Ok(r) = dev.request(&Request::Status) {
                println!("  {} {}", "Firmware:".cyan(), r.str_field("firmware_version").unwrap_or("unknown"));
                println!("  {} {}", "Protocol:".cyan(), r.str_field("protocol_version").unwrap_or("unknown"));
                if let Some(n) = r.u64_field("user_count") {
                    println!("  {} {n}", "Users:   ".cyan());
                }
            }
        }
        None => println!("Not connected. Use 'connect <port>' to attach a device."),
    }
    println!("\nType 'help' for available commands, '<command> --help' for details.");
    println!("Press Ctrl+D or type 'exit' to quit.\n");
}

fn print_help() {
    let cmd = Cli::command();
    for (group, names) in GROUPS {
        println!("{}", group.bold());
        for name in *names {
            let about = cmd
                .find_subcommand(name)
                .and_then(|s| s.get_about().map(ToString::to_string))
                .or_else(|| META.iter().find(|(n, _)| n == name).map(|(_, d)| d.to_string()))
                .unwrap_or_default();
            println!("  {:<12} {about}", name.cyan());
        }
        println!();
    }
    println!("{}: ls -> list, rm -> remove, ? -> help\n", "Aliases".bold());
}

fn print_version(conn: &mut Conn) {
    let (fw, proto) = match &mut conn.dev {
        Some(d) => match d.request(&Request::Status) {
            Ok(r) => (
                r.str_field("firmware_version").unwrap_or("unknown").to_string(),
                r.str_field("protocol_version").unwrap_or("unknown").to_string(),
            ),
            Err(_) => ("unknown".into(), "unknown".into()),
        },
        None => ("(not connected)".into(), "(not connected)".into()),
    };
    println!("GateCore CLI  {}", env!("CARGO_PKG_VERSION"));
    println!("Firmware  {fw}");
    println!("Protocol  {proto}");
    println!("Platform  {}-{}", std::env::consts::OS, std::env::consts::ARCH);
}

/// shlex treats `\` as an escape; keep Windows paths (`C:\data\u.json`) intact.
fn split_line(line: &str) -> Option<Vec<String>> {
    if cfg!(windows) {
        shlex::split(&line.replace('\\', "\\\\"))
    } else {
        shlex::split(line)
    }
}

fn history_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".gatecore_cli_history"))
}

// Main loop

pub fn run(initial_port: Option<String>, baud: u32, verbose: bool) -> Result<()> {
    let mut conn = Conn { dev: None, last_port: None, baud, verbose };
    let mut port = initial_port;
    loop {
        let p = match port.take() {
            Some(p) => p,
            None => match select_port() {
                Ok(Some(p)) => p,
                Ok(None) | Err(Error::Cancelled) => break,
                Err(e) => return Err(e),
            },
        };
        if conn.connect(&p) {
            break;
        }
    }
    print_banner(&mut conn);

    let mut rl: Editor<ShellHelper, FileHistory> = Editor::new().map_err(|e| Error::usage(e.to_string()))?;
    rl.set_helper(Some(ShellHelper::new()));
    let hist = history_path();
    if let Some(h) = &hist {
        let _ = rl.load_history(h);
    }

    loop {
        interrupt::clear();
        let prompt = format!("gatecore({})> ", conn.label());
        let line = match rl.readline(&prompt) {
            Ok(l) => l,
            Err(ReadlineError::Interrupted) => continue,
            Err(ReadlineError::Eof) => break,
            Err(e) => {
                ui::error(&e.to_string());
                break;
            }
        };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let _ = rl.add_history_entry(line);
        let Some(tokens) = split_line(line) else {
            ui::error("Could not parse the line (unbalanced quotes?)");
            continue;
        };
        let Some(first) = tokens.first().map(String::as_str) else { continue };

        match first {
            "exit" | "quit" => break,
            "help" | "?" => {
                print_help();
                continue;
            }
            "clear" => {
                let _ = rl.clear_screen();
                continue;
            }
            "version" => {
                print_version(&mut conn);
                continue;
            }
            "history" => {
                for (i, h) in rl.history().iter().enumerate() {
                    println!("{:>4}  {h}", (i + 1).to_string().cyan());
                }
                continue;
            }
            "connect" => {
                match tokens.get(1) {
                    Some(p) if tokens.len() == 2 => {
                        let p = p.clone();
                        if conn.connect(&p) {
                            ui::success(&format!("Connected to {p}."));
                        }
                    }
                    _ => ui::error("Usage: connect <port>"),
                }
                continue;
            }
            "disconnect" => {
                if conn.dev.is_some() {
                    let name = conn.label();
                    conn.disconnect();
                    ui::success(&format!("Disconnected from {name}."));
                } else {
                    ui::info("Already disconnected.");
                }
                continue;
            }
            "reconnect" => {
                match conn.last_port.clone().or_else(|| conn.dev.as_ref().map(|d| d.port_name().to_string())) {
                    Some(p) => {
                        if conn.connect(&p) {
                            ui::success(&format!("Reconnected to {p}."));
                        }
                    }
                    None => ui::error("No previous device to reconnect to."),
                }
                continue;
            }
            _ => {}
        }

        let cmd = match ShellLine::try_parse_from(&tokens) {
            Ok(s) => s.command,
            Err(e) => {
                let _ = e.print(); // clap renders usage errors and --help itself
                continue;
            }
        };
        if cmd.needs_device() && conn.dev.is_none() {
            ui::warning("Not connected. Use 'connect <port>' or 'reconnect'.");
            continue;
        }
        if let Some(warn) = cmd.destructive_warning() {
            ui::warning(warn);
            match ui::confirm("Continue?", false) {
                Ok(true) => {}
                _ => {
                    ui::info("Cancelled.");
                    continue;
                }
            }
        }

        let started = Instant::now();
        let result = match conn.dev.as_mut() {
            Some(dev) => commands::run(&cmd, &mut Ctx::shared(dev)),
            None => commands::run(&cmd, &mut Ctx::offline()),
        };
        match result {
            Ok(()) => {}
            Err(Error::Interrupted) => ui::warning("Interrupted."),
            Err(Error::Cancelled) => ui::info("Cancelled."),
            Err(e) => {
                ui::error(&e.to_string());
                // Only health-check the link for transport-level failures;
                // a device-side "UID not found" needs no extra round trip.
                if e.is_link_failure() {
                    interrupt::clear();
                    if let Some(dev) = conn.dev.as_mut() {
                        if !dev.ping() {
                            let p = dev.port_name().to_string();
                            ui::error("Connection lost. Attempting to reconnect...");
                            conn.disconnect();
                            if conn.connect(&p) {
                                ui::success(&format!("Reconnected to {p}."));
                            } else {
                                ui::error("Device unavailable -- continuing disconnected.");
                            }
                        }
                    }
                }
            }
        }
        let secs = started.elapsed().as_secs_f64();
        if secs > 1.0 {
            ui::info(&format!("Elapsed: {secs:.2} s"));
        }
    }

    if let Some(h) = &hist {
        let _ = rl.save_history(h);
    }
    conn.disconnect();
    Ok(())
}
