//! Serial transport: port discovery, line framing, request/retry semantics
//! and raw binary transfers.
//!
//! * [`Device`] talks to a [`Link`] trait object, not a concrete port, so the
//!   full protocol stack runs in CI against an in-memory firmware simulator
//!   (`tests/device_sim.rs`).
//! * Bytes are read in 50 ms slices; a timeout means *inactivity* (no byte
//!   for the timeout) rather than a fixed overall budget. Raw transfers use
//!   the same stall detector as the firmware's `readRawExact()`.
//! * Every read slice checks [`crate::interrupt`].
//! * Unsolicited `uid_detected` / `renewal_result` messages are queued
//!   ([`Device::next_message`]) and never mistaken for a command reply.
//! * Non-JSON boot/log lines are skipped instead of triggering a retry.
//! * Retries only happen for idempotent requests; after a retried request
//!   succeeds, a short quiet-drain absorbs a possibly-late duplicate reply so
//!   it cannot be mistaken for the reply to the next request.

use std::collections::VecDeque;
use std::io;
use std::thread;
use std::time::{Duration, Instant};

use indicatif::ProgressBar;
use serialport::{SerialPort, SerialPortType};

use crate::error::{Error, Result};
use crate::interrupt;
use crate::protocol::{Request, Response};

pub const DEFAULT_BAUD: u32 = 2_000_000;
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(2);
pub const DEFAULT_RETRIES: u32 = 3;
/// Granularity of blocking reads (also the Ctrl+C reaction time).
const SLICE: Duration = Duration::from_millis(50);
/// Stall detector for raw transfers (firmware uses 5 s per chunk).
pub const RAW_STALL_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_LINE: usize = 64 * 1024 * 1024;

/// (VID, PID) pairs of common ESP32-S3 USB bridges / native USB.
/// Run `gatecore-cli list-ports` to see what your OS reports.
pub const KNOWN_ESP32_VID_PID: &[(u16, u16)] = &[
    (0x303A, 0x1001), // Espressif native USB-Serial/JTAG
    (0x303A, 0x0002), // Espressif USB CDC
    (0x10C4, 0xEA60), // Silicon Labs CP210x
    (0x1A86, 0x7523), // WCH CH340
    (0x1A86, 0x55D4), // WCH CH343 / CH9102
];

const FALLBACK_TAGS: &[&str] = &["cp210", "ch340", "ch343", "ch910", "esp32", "usb-serial", "usb serial", "uart"];

// Port discovery

#[derive(Clone, Debug)]
pub struct PortInfo {
    pub device: String,
    pub vid: Option<u16>,
    pub pid: Option<u16>,
    pub manufacturer: String,
    pub description: String,
}

pub fn list_ports() -> Vec<PortInfo> {
    let mut ports: Vec<PortInfo> = serialport::available_ports()
        .unwrap_or_default()
        .into_iter()
        .map(|p| match p.port_type {
            SerialPortType::UsbPort(u) => PortInfo {
                device: p.port_name,
                vid: Some(u.vid),
                pid: Some(u.pid),
                manufacturer: u.manufacturer.unwrap_or_default(),
                description: u.product.unwrap_or_default(),
            },
            other => PortInfo {
                device: p.port_name,
                vid: None,
                pid: None,
                manufacturer: String::new(),
                description: match other {
                    SerialPortType::BluetoothPort => "Bluetooth".into(),
                    SerialPortType::PciPort => "PCI".into(),
                    _ => String::new(),
                },
            },
        })
        .collect();
    ports.sort_by(|a, b| a.device.cmp(&b.device));
    ports
}

/// Every plausible ESP32 port: VID:PID matches first, then a name heuristic.
pub fn find_esp32_ports() -> Vec<String> {
    let ports = list_ports();
    let mut exact = Vec::new();
    let mut fallback = Vec::new();
    for p in ports {
        if let (Some(v), Some(pid)) = (p.vid, p.pid) {
            if KNOWN_ESP32_VID_PID.contains(&(v, pid)) {
                exact.push(p.device);
                continue;
            }
        }
        let hay = format!("{} {}", p.description, p.manufacturer).to_lowercase();
        if FALLBACK_TAGS.iter().any(|t| hay.contains(t)) {
            fallback.push(p.device);
        }
    }
    exact.extend(fallback);
    exact
}

// Link abstraction

/// Minimal byte transport. Implementations block for at most ~50 ms (the read slice).
pub trait Link: Send {
    /// Read available bytes; `Ok(0)` when nothing arrived within the slice.
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize>;
    /// Write some bytes; `Ok(0)` when no progress could be made in the slice.
    fn write(&mut self, buf: &[u8]) -> io::Result<usize>;
    fn flush(&mut self) -> io::Result<()>;
    /// Bytes ready to read without blocking.
    fn available(&mut self) -> io::Result<usize>;
    /// Drop anything pending in the OS input buffer.
    fn discard_input(&mut self) -> io::Result<()>;
}

pub struct SerialLink {
    port: Box<dyn SerialPort>,
}

impl SerialLink {
    /// Open with DTR/RTS low so boards that wire them to EN/GPIO0 (auto-reset
    /// circuits) are not rebooted or put into the bootloader.
    pub fn open(path: &str, baud: u32) -> Result<SerialLink> {
        let mut port = serialport::new(path, baud)
            .timeout(SLICE)
            .dtr_on_open(false)
            .open()
            .map_err(|e| Error::NoDevice(format!("failed to open {path}: {e}")))?;
        let _ = port.write_request_to_send(false);
        let _ = port.write_data_terminal_ready(false);
        Ok(SerialLink { port })
    }
}

fn timeout_as_zero(r: io::Result<usize>) -> io::Result<usize> {
    match r {
        Err(e) if matches!(e.kind(), io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted) => {
            Ok(0)
        }
        other => other,
    }
}

impl Link for SerialLink {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        timeout_as_zero(self.port.read(buf))
    }
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        timeout_as_zero(self.port.write(buf))
    }
    fn flush(&mut self) -> io::Result<()> {
        match self.port.flush() {
            Err(e) if e.kind() == io::ErrorKind::TimedOut => Ok(()),
            other => other,
        }
    }
    fn available(&mut self) -> io::Result<usize> {
        self.port.bytes_to_read().map(|n| n as usize).map_err(io::Error::from)
    }
    fn discard_input(&mut self) -> io::Result<()> {
        self.port.clear(serialport::ClearBuffer::Input).map_err(io::Error::from)
    }
}

// Device

/// Per-request overrides.
#[derive(Clone, Copy, Debug)]
pub struct RequestOpts {
    /// Max silence while waiting for the reply.
    pub timeout: Duration,
    /// Total attempts (>= 1). Forced to 1 for non-idempotent requests.
    pub attempts: u32,
}

impl Default for RequestOpts {
    fn default() -> Self {
        RequestOpts { timeout: DEFAULT_TIMEOUT, attempts: DEFAULT_RETRIES }
    }
}

impl RequestOpts {
    pub fn timeout(t: Duration) -> Self {
        RequestOpts { timeout: t, ..Default::default() }
    }
    pub fn once(t: Duration) -> Self {
        RequestOpts { timeout: t, attempts: 1 }
    }
}

/// An open, protocol-level connection to the firmware.
pub struct Device {
    link: Box<dyn Link>,
    port_name: String,
    rx: Vec<u8>,
    events: VecDeque<Response>,
    verbose: bool,
}

impl Device {
    /// Open a real serial port and wait for the firmware to answer.
    pub fn open(port: &str, baud: u32, verbose: bool) -> Result<Device> {
        let link = SerialLink::open(port, baud)?;
        let mut dev = Device::with_link(Box::new(link), port);
        dev.verbose = verbose;
        if !dev.handshake(Duration::from_millis(300), Duration::from_secs(45))? {
            crate::ui::warning(&format!(
                "{port}: firmware did not answer 'status' within 45 s -- continuing anyway \
                 (wrong port? firmware still formatting LittleFS?)"
            ));
        }
        Ok(dev)
    }

    /// Wrap an already-open link (no handshake). Used by tests.
    pub fn with_link(link: Box<dyn Link>, name: &str) -> Device {
        Device { link, port_name: name.to_string(), rx: Vec::with_capacity(8192), events: VecDeque::new(), verbose: false }
    }

    pub fn set_verbose(&mut self, v: bool) {
        self.verbose = v;
    }

    pub fn port_name(&self) -> &str {
        &self.port_name
    }

    /// Poll `status` until the firmware answers. The first boot can block for
    /// a long time in `LittleFS.format()`, and boot traces are not JSON.
    pub fn handshake(&mut self, settle: Duration, ready_timeout: Duration) -> Result<bool> {
        sleep_interruptible(settle)?;
        let deadline = Instant::now() + ready_timeout;
        while Instant::now() < deadline {
            self.rx.clear();
            let _ = self.link.discard_input();
            self.send(&Request::Status)?;
            match self.recv(Duration::from_secs(1)) {
                Ok(_) => {
                    // Several polls may have been answered late; swallow them.
                    self.drain_quiet(Duration::from_millis(300))?;
                    return Ok(true);
                }
                Err(Error::Interrupted) => return Err(Error::Interrupted),
                Err(_) => sleep_interruptible(Duration::from_millis(300))?,
            }
        }
        Ok(false)
    }

    // Low-level reading

    /// One read slice into `rx`. Returns bytes read (0 on a quiet slice).
    fn fill(&mut self) -> Result<usize> {
        interrupt::check()?;
        let mut buf = [0u8; 16 * 1024];
        let n = self.link.read(&mut buf)?;
        self.rx.extend_from_slice(&buf[..n]);
        Ok(n)
    }

    /// Next `\n`-terminated line (without the terminator), waiting at most
    /// `idle` of silence.
    fn read_line(&mut self, idle: Duration) -> Result<Vec<u8>> {
        let mut scanned = 0;
        let mut last_progress = Instant::now();
        loop {
            if let Some(pos) = self.rx[scanned..].iter().position(|&b| b == b'\n') {
                let end = scanned + pos;
                let mut line: Vec<u8> = self.rx.drain(..=end).collect();
                line.pop();
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                return Ok(line);
            }
            scanned = self.rx.len();
            if scanned > MAX_LINE {
                self.rx.clear();
                return Err(Error::protocol("line exceeds 64 MiB without a newline"));
            }
            if self.fill()? > 0 {
                last_progress = Instant::now();
            } else if last_progress.elapsed() >= idle {
                return Err(Error::Timeout("waiting for the device".into()));
            }
        }
    }

    /// Next JSON object, skipping non-JSON noise (boot traces, log lines).
    fn read_message(&mut self, idle: Duration) -> Result<Response> {
        loop {
            let line = self.read_line(idle)?;
            if line.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            match Response::decode(&line) {
                Ok(r) => {
                    if self.verbose {
                        crate::ui::debug(&format!("<- {}", truncate_for_log(&line)));
                    }
                    return Ok(r);
                }
                Err(_) => {
                    if self.verbose {
                        crate::ui::debug(&format!("<- (non-JSON, ignored) {}", truncate_for_log(&line)));
                    }
                }
            }
        }
    }

    /// Next reply that is not an async card event (events are queued).
    pub fn recv(&mut self, idle: Duration) -> Result<Response> {
        loop {
            let msg = self.read_message(idle)?;
            if msg.is_event() {
                self.events.push_back(msg);
                continue;
            }
            return Ok(msg);
        }
    }

    /// Next message of any kind (queued events first). `Ok(None)` when
    /// `wait` elapses without anything arriving.
    pub fn next_message(&mut self, wait: Duration) -> Result<Option<Response>> {
        if let Some(ev) = self.events.pop_front() {
            return Ok(Some(ev));
        }
        match self.read_message(wait) {
            Ok(m) => Ok(Some(m)),
            Err(Error::Timeout(_)) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Discard stale input before a request, keeping complete event lines.
    fn drain_pending(&mut self) -> Result<()> {
        let mut guard = 0;
        while self.link.available().unwrap_or(0) > 0 && guard < 256 {
            self.fill()?;
            guard += 1;
        }
        self.harvest_events_and_clear();
        Ok(())
    }

    /// Read until the line has been quiet for `window`, then discard (keeping events).
    fn drain_quiet(&mut self, window: Duration) -> Result<()> {
        let start = Instant::now();
        let mut last = Instant::now();
        while last.elapsed() < window && start.elapsed() < Duration::from_secs(5) {
            if self.fill()? > 0 {
                last = Instant::now();
            }
        }
        self.harvest_events_and_clear();
        Ok(())
    }

    fn harvest_events_and_clear(&mut self) {
        for line in self.rx.split(|&b| b == b'\n') {
            if let Ok(r) = Response::decode(line) {
                if r.is_event() {
                    self.events.push_back(r);
                } else if self.verbose {
                    crate::ui::debug(&format!("<- (stale, discarded) {}", truncate_for_log(line)));
                }
            }
        }
        self.rx.clear();
    }

    // Requests

    /// Write one request without waiting for the reply.
    pub fn send(&mut self, req: &Request) -> Result<()> {
        let line = req.encode();
        if self.verbose {
            let shown = match req {
                Request::ConfigureWifi { ssid, .. } => format!("{{\"type\":\"configure_wifi\",\"ssid\":{ssid:?},\"password\":\"***\"}}"),
                _ => truncate_for_log(&line[..line.len() - 1]),
            };
            crate::ui::debug(&format!("-> {shown}"));
        }
        self.write_all(&line, DEFAULT_TIMEOUT, None)
    }

    /// Send with default timeout/retries and return the reply.
    pub fn request(&mut self, req: &Request) -> Result<Response> {
        self.request_with(req, RequestOpts::default())
    }

    /// Send, wait for the reply, retry on timeout/garbage (idempotent
    /// requests only). An "already happened" error on a retry (e.g.
    /// `Duplicate UID` for `add`) is reported as success.
    pub fn request_with(&mut self, req: &Request, opts: RequestOpts) -> Result<Response> {
        let attempts = if req.is_idempotent() { opts.attempts.max(1) } else { 1 };
        let mut last_err = None;
        for attempt in 1..=attempts {
            self.drain_pending()?;
            self.send(req)?;
            match self.recv(opts.timeout) {
                Ok(resp) => {
                    if attempt > 1 {
                        self.drain_quiet(Duration::from_millis(200))?;
                        let msg = resp.message().unwrap_or("").to_lowercase();
                        if !resp.is_ok() && req.retry_safe_errors().contains(&msg.as_str()) {
                            let mut ok = serde_json::Map::new();
                            ok.insert("status".into(), "ok".into());
                            ok.insert(
                                "note".into(),
                                format!(
                                    "'{}' on a retry of '{}' means the first attempt went through; only its reply was lost",
                                    resp.message().unwrap_or(""),
                                    req.kind()
                                )
                                .into(),
                            );
                            return Ok(Response(ok));
                        }
                    }
                    return Ok(resp);
                }
                Err(e @ (Error::Timeout(_) | Error::Protocol(_))) => {
                    last_err = Some(e);
                    if attempt < attempts {
                        sleep_interruptible(Duration::from_millis(300))?;
                    }
                }
                Err(e) => return Err(e),
            }
        }
        let why = last_err.map(|e| e.to_string()).unwrap_or_default();
        Err(Error::Timeout(format!(
            "waiting for a reply to '{}' ({} attempt{}; last error: {why})",
            req.kind(),
            attempts,
            if attempts == 1 { "" } else { "s" }
        )))
    }

    /// Liveness probe used by the shell after a failure.
    pub fn ping(&mut self) -> bool {
        self.request_with(&Request::Status, RequestOpts { timeout: Duration::from_secs(1), attempts: 2 }).is_ok()
    }

    // Raw binary transfers

    /// Write raw bytes, failing if no progress is made for `stall`.
    pub fn write_raw(&mut self, data: &[u8], stall: Duration, progress: Option<&ProgressBar>) -> Result<()> {
        self.write_all(data, stall, progress)
    }

    fn write_all(&mut self, data: &[u8], stall: Duration, progress: Option<&ProgressBar>) -> Result<()> {
        let mut off = 0;
        let mut last_progress = Instant::now();
        while off < data.len() {
            interrupt::check()?;
            let end = (off + 4096).min(data.len());
            let n = self.link.write(&data[off..end])?;
            if n > 0 {
                off += n;
                last_progress = Instant::now();
                if let Some(pb) = progress {
                    pb.set_position(off as u64);
                }
            } else if last_progress.elapsed() >= stall {
                return Err(Error::Timeout(format!("writing to the device (stalled at {off}/{} bytes)", data.len())));
            }
        }
        self.link.flush()?;
        Ok(())
    }

    /// Read exactly `n` raw bytes, failing if the stream stalls for `stall`.
    pub fn read_raw(&mut self, n: usize, stall: Duration, progress: Option<&ProgressBar>) -> Result<Vec<u8>> {
        let mut last_progress = Instant::now();
        while self.rx.len() < n {
            if self.fill()? > 0 {
                last_progress = Instant::now();
                if let Some(pb) = progress {
                    pb.set_position(self.rx.len().min(n) as u64);
                }
            } else if last_progress.elapsed() >= stall {
                let got = self.rx.len();
                self.rx.clear();
                return Err(Error::Protocol(format!(
                    "raw transfer stalled: expected {n} bytes, got {got} -- reconnect and retry"
                )));
            }
        }
        if let Some(pb) = progress {
            pb.set_position(n as u64);
        }
        Ok(self.rx.drain(..n).collect())
    }
}

fn truncate_for_log(line: &[u8]) -> String {
    let s = String::from_utf8_lossy(line);
    if s.chars().count() > 300 {
        format!("{}... ({} bytes)", s.chars().take(300).collect::<String>(), line.len())
    } else {
        s.into_owned()
    }
}

/// Sleep in slices so Ctrl+C stays responsive.
pub fn sleep_interruptible(d: Duration) -> Result<()> {
    let end = Instant::now() + d;
    while Instant::now() < end {
        interrupt::check()?;
        thread::sleep(SLICE.min(end.saturating_duration_since(Instant::now())));
    }
    Ok(())
}

/// Stall-tolerant budget for a device-side operation proportional to size:
/// 15 s floor, plus 1 s per 50 KB.
pub fn scaled_timeout(bytes: usize) -> Duration {
    Duration::from_secs_f64((5.0 + bytes as f64 / 50_000.0).max(15.0))
}
