//! End-to-end tests: real `Device` + real command handlers against an
//! in-memory simulation of the firmware's serial protocol
//! (`firmware/SystemController.cpp` + `SerialProtocol.cpp`).
//!
//! The simulator reuses the crate's record codec for storage; byte-level
//! compatibility of that codec with the original implementation is proven
//! separately by the golden-vector tests in `src/convert.rs`.

use std::collections::{BTreeMap, VecDeque};
use std::io;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};

use rfid_cli::cli::{BackupFormat, Command};
use rfid_cli::commands::{self, Ctx};
use rfid_cli::convert::{self, RECORD_SIZE, UID_ENTRY_SIZE};
use rfid_cli::database::{parse_date, Uid, UserEntry, Validity};
use rfid_cli::protocol::{Request, WireUser};
use rfid_cli::serial::{Device, Link, RequestOpts};
use rfid_cli::Error;

// Simulator

enum Rx {
    Line,
    ImportBin { left: usize, buf: Vec<u8> },
    SyncApply { remove: usize, add: usize, replace: usize, buf: Vec<u8> },
}

#[derive(Default)]
struct Knobs {
    /// Silently drop the next N replies (simulates lost replies).
    drop_replies: usize,
    /// Prefix every reply with a non-JSON log line.
    noise: bool,
    /// Card UIDs to report after `enter_scan_mode`.
    cards: VecDeque<String>,
    /// Make `import_bin` stall after the ack (simulates a broken transfer).
    stall_import_bin: bool,
    /// Every request `type` received, in order.
    log: Vec<String>,
}

struct Sim {
    users: BTreeMap<Uid, UserEntry>,
    import_snapshot: Option<BTreeMap<Uid, UserEntry>>,
    rx_mode: Rx,
    line: Vec<u8>,
    tx: VecDeque<u8>,
    knobs: Knobs,
}

type Shared = Arc<Mutex<Sim>>;

fn sim_today() -> chrono::NaiveDate {
    parse_date("2000-01-01").unwrap() // never used: sim only stores concrete records
}

impl Sim {
    fn new() -> Shared {
        Arc::new(Mutex::new(Sim {
            users: BTreeMap::new(),
            import_snapshot: None,
            rx_mode: Rx::Line,
            line: Vec::new(),
            tx: VecDeque::new(),
            knobs: Knobs::default(),
        }))
    }

    fn reply(&mut self, v: Value) {
        if self.knobs.drop_replies > 0 {
            self.knobs.drop_replies -= 1;
            return;
        }
        if self.knobs.noise {
            self.tx.extend(b"[log] something happened\r\n");
        }
        self.tx.extend(serde_json::to_vec(&v).unwrap());
        self.tx.push_back(b'\n');
    }

    fn ok(&mut self) {
        self.reply(json!({"status": "ok"}));
    }

    fn err(&mut self, msg: &str) {
        self.reply(json!({"status": "error", "message": msg}));
    }

    fn crc(&self) -> u32 {
        let v: Vec<UserEntry> = self.users.values().cloned().collect();
        convert::canonical_crc32_of(&v, sim_today())
    }

    fn sorted_records(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for e in self.users.values() {
            out.extend_from_slice(&convert::encode_record(e, sim_today()));
        }
        out
    }

    fn user_json(e: &UserEntry) -> Value {
        match e.validity {
            Validity::Admin => json!({"uid": e.uid, "name": e.name, "registered": "", "valid_days": -1.0}),
            Validity::Expiring { registered, valid_days } => {
                json!({"uid": e.uid, "name": e.name, "registered": registered.unwrap().to_string(), "valid_days": valid_days})
            }
        }
    }

    fn wire_to_entry(u: &Value) -> Result<UserEntry, String> {
        let uid = Uid::parse(u["uid"].as_str().ok_or("Missing 'uid' or 'name'")?).map_err(|_| "Invalid UID format")?;
        let name = u["name"].as_str().ok_or("Missing 'uid' or 'name'")?.to_string();
        rfid_cli::database::validate_name(&name).map_err(|_| "Invalid or empty name")?;
        let validity = match (u.get("registered"), u.get("valid_days")) {
            (Some(r), Some(v)) => Validity::Expiring {
                registered: Some(parse_date(r.as_str().unwrap()).map_err(|_| "Invalid 'registered' date")?),
                valid_days: v.as_f64().unwrap(),
            },
            _ => Validity::Admin,
        };
        Ok(UserEntry { uid, name, validity })
    }

    fn handle(&mut self, v: Value) {
        let t = v["type"].as_str().unwrap_or("").to_string();
        self.knobs.log.push(t.clone());
        match t.as_str() {
            "status" => {
                let crc = self.crc();
                self.reply(json!({"status":"ok","type":"status","firmware_version":"sim","protocol_version":"sim/v1",
                    "db_path":"/users.bin","fs_total_bytes":1000000,"fs_used_bytes":1000,"fs_free_bytes":999000,
                    "user_count": self.users.len(), "db_crc32": crc}));
            }
            "list" | "find_name" => {
                let q = v["query"].as_str().unwrap_or("").to_lowercase();
                let users: Vec<Value> =
                    self.users.values().filter(|u| u.name.to_lowercase().contains(&q)).map(Sim::user_json).collect();
                self.reply(json!({"status":"ok","users":users}));
            }
            "find" => {
                let uid = Uid::from_device(v["uid"].as_str().unwrap());
                match self.users.get(&uid).cloned() {
                    Some(u) => {
                        let mut j = Sim::user_json(&u);
                        j["status"] = "ok".into();
                        j["type"] = "find_result".into();
                        self.reply(j)
                    }
                    None => self.reply(json!({"status":"error","type":"find_result","message":"UID not found"})),
                }
            }
            "add" => match Sim::wire_to_entry(&v) {
                Ok(e) if self.users.contains_key(&e.uid) => self.err("Duplicate UID"),
                Ok(e) => {
                    self.users.insert(e.uid.clone(), e);
                    self.ok()
                }
                Err(m) => self.err(&m),
            },
            "remove" => {
                if self.users.remove(&Uid::from_device(v["uid"].as_str().unwrap())).is_some() {
                    self.ok()
                } else {
                    self.err("UID not found")
                }
            }
            "rename" => {
                let uid = Uid::from_device(v["uid"].as_str().unwrap());
                match self.users.get_mut(&uid) {
                    Some(u) => {
                        u.name = v["name"].as_str().unwrap().into();
                        self.ok()
                    }
                    None => self.err("UID not found"),
                }
            }
            "clear_all" => {
                self.users.clear();
                self.ok()
            }
            "remove_all_except" => {
                let keep: Vec<Uid> = v["uids"].as_array().unwrap().iter().map(|u| Uid::from_device(u.as_str().unwrap())).collect();
                let before = self.users.len();
                self.users.retain(|k, _| keep.contains(k));
                let removed = before - self.users.len();
                self.reply(json!({"status":"ok","type":"remove_all_except","removed_count":removed}));
            }
            "export_bin" | "sync_manifest" => {
                let recs = self.sorted_records();
                let payload: Vec<u8> = if t == "export_bin" {
                    recs
                } else {
                    recs.chunks(RECORD_SIZE).flat_map(|r| [&r[..1 + 10], &r[RECORD_SIZE - 4..]].concat()).collect()
                };
                self.reply(json!({"status":"ok","type": t, "bytes": payload.len(), "count": self.users.len()}));
                self.tx.extend(payload);
            }
            "import_begin" => {
                self.import_snapshot = Some(self.users.clone());
                self.ok()
            }
            "import_abort" => {
                if let Some(s) = self.import_snapshot.take() {
                    self.users = s;
                }
                self.ok()
            }
            "import_end" => {
                self.import_snapshot = None;
                self.reply(json!({"status":"ok","type":"import_result","added": self.users.len(),"errors":0,"save_ms":5}));
            }
            "import_bin" => {
                let n = v["bytes"].as_u64().unwrap() as usize;
                self.ok();
                self.rx_mode = Rx::ImportBin { left: n, buf: Vec::new() };
            }
            "batch_add" => {
                let (mut added, mut failed) = (0, Vec::new());
                for u in v["users"].as_array().unwrap() {
                    match Sim::wire_to_entry(u) {
                        Ok(e) if self.users.contains_key(&e.uid) => failed.push(json!({"uid": e.uid, "message":"Duplicate UID"})),
                        Ok(e) => {
                            self.users.insert(e.uid.clone(), e);
                            added += 1
                        }
                        Err(m) => failed.push(json!({"uid": u["uid"], "message": m})),
                    }
                }
                let errors = failed.len();
                self.reply(json!({"status":"ok","type":"batch_add_result","added":added,"errors":errors,"failed":failed}));
            }
            "sync_begin" => {
                let crc = self.crc();
                self.reply(json!({"status":"ok","type":"sync_begin","db_crc32":crc,"count":self.users.len()}));
            }
            "sync_apply" => {
                let g = |k: &str| v[k].as_u64().unwrap() as usize;
                self.ok();
                self.rx_mode = Rx::SyncApply { remove: g("remove"), add: g("add"), replace: g("replace"), buf: Vec::new() };
            }
            "enter_scan_mode" => {
                self.ok();
                if let Some(uid) = self.knobs.cards.pop_front() {
                    self.tx.extend(serde_json::to_vec(&json!({"status":"ok","type":"uid_detected","uid":uid})).unwrap());
                    self.tx.push_back(b'\n');
                }
            }
            "exit_scan_mode" | "exit_renewal_mode" | "enter_renewal_mode" => self.ok(),
            _ => self.err("Unknown command type"),
        }
    }

    fn feed(&mut self, data: &[u8]) {
        for &b in data {
            match &mut self.rx_mode {
                Rx::Line => {
                    if b == b'\n' {
                        let line = std::mem::take(&mut self.line);
                        match serde_json::from_slice::<Value>(&line) {
                            Ok(v) => self.handle(v),
                            Err(_) => self.err("Malformed JSON"),
                        }
                    } else {
                        self.line.push(b);
                    }
                }
                Rx::ImportBin { left, buf } => {
                    buf.push(b);
                    *left -= 1;
                    if *left == 0 {
                        let buf = std::mem::take(buf);
                        self.rx_mode = Rx::Line;
                        if self.knobs.stall_import_bin {
                            continue; // never answer
                        }
                        let (mut added, mut errors) = (0, 0);
                        for rec in buf.chunks(RECORD_SIZE) {
                            match convert::decode_record(rec) {
                                Ok(e) if !self.users.contains_key(&e.uid) => {
                                    self.users.insert(e.uid.clone(), e);
                                    added += 1
                                }
                                _ => errors += 1,
                            }
                        }
                        self.reply(json!({"status":"ok","type":"import_bin_result","added":added,"errors":errors}));
                    }
                }
                Rx::SyncApply { remove, add, replace, buf } => {
                    buf.push(b);
                    let need = *remove * UID_ENTRY_SIZE + (*add + *replace) * RECORD_SIZE;
                    if buf.len() == need {
                        let (r, a, buf) = (*remove, *add, std::mem::take(buf));
                        self.rx_mode = Rx::Line;
                        let (rem, rest) = buf.split_at(r * UID_ENTRY_SIZE);
                        let (adds, reps) = rest.split_at(a * RECORD_SIZE);
                        let (mut nr, mut na, mut np, mut errors) = (0, 0, 0, 0);
                        for e in rem.chunks(UID_ENTRY_SIZE) {
                            let uid = Uid::from_bytes(&e[1..1 + e[0] as usize]);
                            if self.users.remove(&uid).is_some() { nr += 1 } else { errors += 1 }
                        }
                        for rec in adds.chunks(RECORD_SIZE) {
                            let e = convert::decode_record(rec).unwrap();
                            if self.users.insert(e.uid.clone(), e).is_none() { na += 1 } else { errors += 1 }
                        }
                        for rec in reps.chunks(RECORD_SIZE) {
                            let e = convert::decode_record(rec).unwrap();
                            if self.users.insert(e.uid.clone(), e).is_some() { np += 1 } else { errors += 1 }
                        }
                        let crc = self.crc();
                        self.reply(json!({"status":"ok","type":"sync_result","removed":nr,"added":na,"replaced":np,
                            "errors":errors,"db_crc32":crc,"save_ms":3}));
                    }
                }
            }
        }
    }
}

struct SimLink(Shared);

impl Link for SimLink {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let mut s = self.0.lock().unwrap();
        if s.tx.is_empty() {
            drop(s);
            std::thread::sleep(Duration::from_millis(1));
            return Ok(0);
        }
        let n = buf.len().min(s.tx.len());
        for (i, b) in s.tx.drain(..n).enumerate() {
            buf[i] = b;
        }
        Ok(n)
    }
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().feed(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
    fn available(&mut self) -> io::Result<usize> {
        Ok(self.0.lock().unwrap().tx.len())
    }
    fn discard_input(&mut self) -> io::Result<()> {
        self.0.lock().unwrap().tx.clear();
        Ok(())
    }
}

// Helpers

fn setup() -> (Shared, Device) {
    let sim = Sim::new();
    let dev = Device::with_link(Box::new(SimLink(sim.clone())), "sim");
    (sim, dev)
}

fn run(dev: &mut Device, cmd: Command) -> rfid_cli::Result<()> {
    commands::run(&cmd, &mut Ctx::shared(dev))
}

fn user(uid: &str, name: &str, reg: Option<&str>, days: Option<f64>) -> UserEntry {
    UserEntry {
        uid: Uid::parse(uid).unwrap(),
        name: name.into(),
        validity: match days {
            None => Validity::Admin,
            Some(v) => Validity::Expiring { registered: reg.map(|r| parse_date(r).unwrap()), valid_days: v },
        },
    }
}

fn seed(sim: &Shared, users: &[UserEntry]) {
    let mut s = sim.lock().unwrap();
    for u in users {
        s.users.insert(u.uid.clone(), u.clone());
    }
}

fn write(dir: &std::path::Path, name: &str, body: &str) -> std::path::PathBuf {
    let p = dir.join(name);
    std::fs::write(&p, body).unwrap();
    p
}

fn import_cmd(file: std::path::PathBuf, json_transport: bool) -> Command {
    Command::Import { file, dry_run: false, clear: false, no_backup: false, backup_format: BackupFormat::Bin, json_transport }
}

fn sync_cmd(file: std::path::PathBuf, restamp: bool) -> Command {
    Command::Sync { file, dry_run: false, stats: false, restamp }
}

const SAMPLE_JSON: &str = r#"[
  {"uid": "04A1B2C3", "name": "Alice Martin", "registered": "2026-01-15", "valid_days": 365},
  {"uid": "04D4E5F6", "name": "Bruno Petit", "registered": "2026-02-01", "valid_days": 180},
  {"uid": "12345678", "name": "Admin Garage"}
]"#;

// Tests

#[test]
fn import_binary_then_export_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let (sim, mut dev) = setup();
    run(&mut dev, import_cmd(write(dir.path(), "u.json", SAMPLE_JSON), false)).unwrap();
    assert_eq!(sim.lock().unwrap().users.len(), 3);
    assert!(sim.lock().unwrap().knobs.log.contains(&"import_bin".to_string()));

    let out = dir.path().join("out.json");
    run(&mut dev, Command::Export { file: out.clone(), json_transport: false }).unwrap();
    let back = rfid_cli::userfile::load(&out).unwrap().entries;
    let orig = rfid_cli::userfile::load(&dir.path().join("u.json")).unwrap().entries;
    let mut orig = orig;
    orig.sort_by(|a, b| a.uid.cmp(&b.uid));
    assert_eq!(back, orig);

    // .bin export is byte-exact with the device records and re-imports.
    let bin = dir.path().join("out.bin");
    run(&mut dev, Command::Export { file: bin.clone(), json_transport: false }).unwrap();
    assert_eq!(std::fs::read(&bin).unwrap().len(), convert::HEADER_SIZE + 3 * RECORD_SIZE);
    sim.lock().unwrap().users.clear();
    run(&mut dev, import_cmd(bin, false)).unwrap();
    assert_eq!(sim.lock().unwrap().users.len(), 3);
}

#[test]
fn import_json_transport_and_crc_skip() {
    let dir = tempfile::tempdir().unwrap();
    let (sim, mut dev) = setup();
    let f = write(dir.path(), "u.json", SAMPLE_JSON);
    run(&mut dev, import_cmd(f.clone(), true)).unwrap();
    assert_eq!(sim.lock().unwrap().users.len(), 3);
    assert!(sim.lock().unwrap().knobs.log.contains(&"batch_add".to_string()));
    // Second import of the same file: db_crc32 matches -> no import_begin.
    sim.lock().unwrap().knobs.log.clear();
    run(&mut dev, import_cmd(f, false)).unwrap();
    assert!(!sim.lock().unwrap().knobs.log.contains(&"import_begin".to_string()));
}

#[test]
fn failed_import_is_aborted_on_device() {
    let dir = tempfile::tempdir().unwrap();
    let (sim, mut dev) = setup();
    seed(&sim, &[user("AABBCCDD", "Existing", None, None)]);
    sim.lock().unwrap().knobs.stall_import_bin = true;
    // Shorten nothing: the stall surfaces as a timeout of the result read.
    let err = run(&mut dev, import_cmd(write(dir.path(), "u.json", SAMPLE_JSON), false)).unwrap_err();
    assert!(matches!(err, Error::Timeout(_)), "{err}");
    let s = sim.lock().unwrap();
    assert!(s.knobs.log.contains(&"import_abort".to_string()));
    assert_eq!(s.users.len(), 1, "device must be back to its pre-import state");
}

#[test]
fn sync_applies_minimal_diff_and_verifies() {
    let dir = tempfile::tempdir().unwrap();
    let (sim, mut dev) = setup();
    seed(
        &sim,
        &[
            user("04A1B2C3", "Alice", Some("2026-01-15"), Some(365.0)),
            user("04D4E5F6", "Old Bruno", Some("2026-02-01"), Some(180.0)),
            user("0E4F5061", "Gone", Some("2026-01-20"), Some(30.0)),
        ],
    );
    let file = write(
        dir.path(),
        "target.csv",
        "uid,name,registered,valid_days\n04A1B2C3,Alice,2026-01-15,365\n04D4E5F6,Bruno,2026-02-01,180\n12345678,Admin,,\n",
    );
    run(&mut dev, sync_cmd(file.clone(), false)).unwrap();
    {
        let s = sim.lock().unwrap();
        let names: Vec<&str> = s.users.values().map(|u| u.name.as_str()).collect();
        assert_eq!(names, ["Alice", "Bruno", "Admin"]);
    }
    // Second run: CRC fast path, no manifest.
    sim.lock().unwrap().knobs.log.clear();
    run(&mut dev, sync_cmd(file, false)).unwrap();
    assert_eq!(sim.lock().unwrap().knobs.log, ["sync_begin"]);
}

#[test]
fn sync_preserves_device_dates_for_undated_entries() {
    let dir = tempfile::tempdir().unwrap();
    let (sim, mut dev) = setup();
    seed(&sim, &[user("04A1B2C3", "Alice", Some("2020-03-01"), Some(30.0))]);
    // Same user, no 'registered' in the file, plus a brand-new undated user.
    let file = write(dir.path(), "f.json", r#"[{"uid":"04A1B2C3","name":"Alice","valid_days":30},
                                             {"uid":"0A1B2C3D","name":"New","valid_days":10}]"#);
    run(&mut dev, sync_cmd(file.clone(), false)).unwrap();
    {
        let s = sim.lock().unwrap();
        let alice = &s.users[&Uid::parse("04A1B2C3").unwrap()];
        assert_eq!(alice.validity, Validity::Expiring { registered: Some(parse_date("2020-03-01").unwrap()), valid_days: 30.0 });
        let new = &s.users[&Uid::parse("0A1B2C3D").unwrap()];
        assert_eq!(
            new.validity,
            Validity::Expiring { registered: Some(rfid_cli::database::today()), valid_days: 10.0 }
        );
    }
    // Re-running is a no-op: the device's date is preserved.
    sim.lock().unwrap().knobs.log.clear();
    run(&mut dev, sync_cmd(file.clone(), false)).unwrap();
    assert!(!sim.lock().unwrap().knobs.log.contains(&"sync_apply".to_string()));
    // --restamp opts into restamping explicitly.
    run(&mut dev, sync_cmd(file, true)).unwrap();
    let s = sim.lock().unwrap();
    assert_eq!(
        s.users[&Uid::parse("04A1B2C3").unwrap()].validity,
        Validity::Expiring { registered: Some(rfid_cli::database::today()), valid_days: 30.0 }
    );
}

#[test]
fn remove_except_refuses_when_no_keep_uid_exists() {
    let (sim, mut dev) = setup();
    seed(&sim, &[user("04A1B2C3", "A", None, None), user("04D4E5F6", "B", None, None)]);
    let cmd = |except: &str| Command::Remove {
        uid: None,
        force: false,
        no_backup: false,
        backup_format: BackupFormat::Bin,
        except: Some(except.into()),
    };
    assert!(matches!(run(&mut dev, cmd("DEADBEEF")), Err(Error::Usage(_))));
    assert_eq!(sim.lock().unwrap().users.len(), 2, "typo must not wipe the DB");
    run(&mut dev, cmd("04A1B2C3,DEADBEEF")).unwrap();
    assert_eq!(sim.lock().unwrap().users.len(), 1);
}

#[test]
fn remove_force_takes_a_backup_first() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = std::env::current_dir().unwrap();
    std::env::set_current_dir(dir.path()).unwrap();
    let (sim, mut dev) = setup();
    seed(&sim, &[user("04A1B2C3", "A", None, None)]);
    let r = run(
        &mut dev,
        Command::Remove { uid: None, force: true, no_backup: false, backup_format: BackupFormat::Bin, except: None },
    );
    std::env::set_current_dir(cwd).unwrap();
    r.unwrap();
    assert!(sim.lock().unwrap().users.is_empty());
    let backups: Vec<_> = std::fs::read_dir(dir.path().join("backups")).unwrap().collect();
    assert_eq!(backups.len(), 1);
}

#[test]
fn lost_reply_on_add_is_recovered_by_retry() {
    let (sim, mut dev) = setup();
    sim.lock().unwrap().knobs.drop_replies = 1;
    let req = Request::Add(WireUser { uid: "04A1B2C3".into(), name: "A".into(), registered: None, valid_days: None });
    let r = dev.request_with(&req, RequestOpts { timeout: Duration::from_millis(200), attempts: 3 }).unwrap();
    assert!(r.is_ok(), "Duplicate UID on retry must count as success");
    assert!(r.str_field("note").is_some());
    assert_eq!(sim.lock().unwrap().users.len(), 1);
}

#[test]
fn rename_unknown_uid_is_not_reported_as_success_on_retry() {
    let (sim, mut dev) = setup();
    sim.lock().unwrap().knobs.drop_replies = 1;
    let req = Request::Rename { uid: "04A1B2C3".into(), name: "X".into() };
    let r = dev.request_with(&req, RequestOpts { timeout: Duration::from_millis(200), attempts: 3 }).unwrap();
    assert!(!r.is_ok());
}

#[test]
fn non_json_noise_is_skipped_not_retried() {
    let (sim, mut dev) = setup();
    sim.lock().unwrap().knobs.noise = true;
    run(&mut dev, Command::Status).unwrap();
    run(&mut dev, Command::List).unwrap();
    // Exactly one 'status' and one 'list' reached the device: no resends.
    assert_eq!(sim.lock().unwrap().knobs.log, ["status", "list"]);
}

#[test]
fn scan_receives_card_event() {
    let (sim, mut dev) = setup();
    sim.lock().unwrap().knobs.cards.push_back("04AABBCC".into());
    run(&mut dev, Command::Scan { timeout: 2.0, infinite: false }).unwrap();
    // Timeout path sends exit_scan_mode.
    let err = run(&mut dev, Command::Scan { timeout: 0.3, infinite: false }).unwrap_err();
    assert!(matches!(err, Error::Timeout(_)));
    assert_eq!(sim.lock().unwrap().knobs.log.last().unwrap(), "exit_scan_mode");
}

#[test]
fn find_add_rename_remove_flow() {
    let (sim, mut dev) = setup();
    run(&mut dev, Command::Add { uid: Some("04:a1:b2:c3".into()), name: Some("Zoë".into()), valid_days: Some(0.5) }).unwrap();
    run(&mut dev, Command::Find { uid: Some("04A1B2C3".into()), name: None }).unwrap();
    run(&mut dev, Command::Find { uid: None, name: Some("zo".into()) }).unwrap();
    assert!(matches!(run(&mut dev, Command::Find { uid: None, name: Some("nobody".into()) }), Err(Error::NotFound(_))));
    run(&mut dev, Command::Rename { uid: Some("04A1B2C3".into()), name: Some("Zed".into()) }).unwrap();
    assert!(matches!(
        run(&mut dev, Command::Rename { uid: Some("04A1B2C3".into()), name: Some("x".repeat(49)) }),
        Err(Error::Usage(_))
    ));
    assert_eq!(sim.lock().unwrap().users.values().next().unwrap().name, "Zed");
    run(&mut dev, Command::Remove { uid: Some("04A1B2C3".into()), force: false, no_backup: false, backup_format: BackupFormat::Bin, except: None })
        .unwrap();
    assert!(sim.lock().unwrap().users.is_empty());
    assert!(matches!(
        run(&mut dev, Command::Remove { uid: Some("04A1B2C3".into()), force: false, no_backup: false, backup_format: BackupFormat::Bin, except: None }),
        Err(Error::NotFound(_))
    ));
}

#[test]
fn offline_commands_need_no_device() {
    let dir = tempfile::tempdir().unwrap();
    let f = write(dir.path(), "u.json", SAMPLE_JSON);
    let mut ctx = Ctx::offline();
    commands::run(&Command::Import { file: f.clone(), dry_run: true, clear: false, no_backup: false, backup_format: BackupFormat::Ask, json_transport: false }, &mut ctx).unwrap();
    commands::run(&Command::Convert { src: f, dst: dir.path().join("u.bin") }, &mut ctx).unwrap();
    assert!(matches!(commands::run(&Command::Status, &mut ctx), Err(Error::NoDevice(_))));
}
