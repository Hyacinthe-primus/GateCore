//! The newline-delimited JSON control protocol (`SystemController::
//! handleSerialMessage_` on the firmware side).
//!
//! Requests are a single serde enum tagged by `"type"`, so a message cannot
//! have a misspelled key or a missing field. Responses stay as JSON objects
//! ([`Response`]) with typed views, because profiling fields are optional and
//! older firmware omits them.

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::database::UserEntry;
use crate::error::{Error, Result};

/// One user inside a `batch_add` (legacy JSON import transport) or `add`.
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct WireUser {
    pub uid: String,
    pub name: String,
    /// Absent (not null) for admin badges -- the firmware checks
    /// `containsKey`, so the keys must be omitted, not set to null.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub registered: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub valid_days: Option<f64>,
}

impl WireUser {
    pub fn from_entry(e: &UserEntry, today: chrono::NaiveDate) -> WireUser {
        use crate::database::Validity;
        match e.validity {
            Validity::Admin => WireUser { uid: e.uid.to_string(), name: e.name.clone(), registered: None, valid_days: None },
            Validity::Expiring { registered, valid_days } => WireUser {
                uid: e.uid.to_string(),
                name: e.name.clone(),
                registered: Some(registered.unwrap_or(today).to_string()),
                valid_days: Some(valid_days),
            },
        }
    }
}

#[derive(Clone, Debug, Serialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    Status,
    NetStatus,
    List,
    Find { uid: String },
    FindName { query: String },
    Add(WireUser),
    Remove { uid: String },
    Rename { uid: String, name: String },
    ClearAll,
    RemoveAllExcept { uids: Vec<String> },
    /// `continuous` is honoured by firmware >= 1.2 (stay in scan mode after
    /// each card); older firmware ignores the unknown key.
    EnterScanMode {
        #[serde(skip_serializing_if = "std::ops::Not::not")]
        continuous: bool,
    },
    ExitScanMode,
    EnterRenewalMode { valid_days: f64 },
    ExitRenewalMode,
    ConfigureWifi { ssid: String, password: String },
    ConfigureTimezone { gmt_offset_sec: i64, daylight_offset_sec: i64 },
    GetTime,
    NtpSync,
    ImportBegin,
    ImportEnd,
    /// Firmware >= 1.2: discard an unfinished import and reload from flash.
    ImportAbort,
    BatchAdd { users: Vec<WireUser> },
    ImportBin { bytes: usize },
    ExportBin,
    SyncBegin,
    SyncManifest,
    SyncApply { remove: usize, add: usize, replace: usize },
}

impl Request {
    /// The wire `type` string (for messages and retry policy).
    pub fn kind(&self) -> &'static str {
        match self {
            Request::Status => "status",
            Request::NetStatus => "net_status",
            Request::List => "list",
            Request::Find { .. } => "find",
            Request::FindName { .. } => "find_name",
            Request::Add(_) => "add",
            Request::Remove { .. } => "remove",
            Request::Rename { .. } => "rename",
            Request::ClearAll => "clear_all",
            Request::RemoveAllExcept { .. } => "remove_all_except",
            Request::EnterScanMode { .. } => "enter_scan_mode",
            Request::ExitScanMode => "exit_scan_mode",
            Request::EnterRenewalMode { .. } => "enter_renewal_mode",
            Request::ExitRenewalMode => "exit_renewal_mode",
            Request::ConfigureWifi { .. } => "configure_wifi",
            Request::ConfigureTimezone { .. } => "configure_timezone",
            Request::GetTime => "get_time",
            Request::NtpSync => "ntp_sync",
            Request::ImportBegin => "import_begin",
            Request::ImportEnd => "import_end",
            Request::ImportAbort => "import_abort",
            Request::BatchAdd { .. } => "batch_add",
            Request::ImportBin { .. } => "import_bin",
            Request::ExportBin => "export_bin",
            Request::SyncBegin => "sync_begin",
            Request::SyncManifest => "sync_manifest",
            Request::SyncApply { .. } => "sync_apply",
        }
    }

    /// Serialize to one `\n`-terminated line.
    pub fn encode(&self) -> Vec<u8> {
        let mut line = serde_json::to_vec(self).expect("requests always serialize");
        line.push(b'\n');
        line
    }

    /// Device error messages that, *on a retry*, prove the first attempt
    /// already succeeded and only its reply was lost.
    ///
    /// `rename` is deliberately absent: it never deletes the user, so "UID
    /// not found" on a retry means the UID never existed. A normal rename
    /// retry is harmless and needs no special case.
    pub fn retry_safe_errors(&self) -> &'static [&'static str] {
        match self {
            Request::Add(_) => &["duplicate uid"],
            Request::Remove { .. } => &["uid not found"],
            _ => &[],
        }
    }

    /// Whether resending after a lost reply is harmless. Requests whose
    /// reply is followed by a raw byte stream are excluded too: a resend
    /// would make the device stream twice and desynchronize the link.
    pub fn is_idempotent(&self) -> bool {
        !matches!(
            self,
            Request::ImportBin { .. }
                | Request::SyncApply { .. }
                | Request::ImportEnd
                | Request::ConfigureWifi { .. }
                | Request::ExportBin
                | Request::SyncManifest
                | Request::BatchAdd { .. }
        )
    }
}

/// Unsolicited message types the firmware emits on its own (card events).
pub const EVENT_TYPES: &[&str] = &["uid_detected", "renewal_result"];

/// A decoded JSON object from the device.
#[derive(Clone, Debug, PartialEq)]
pub struct Response(pub Map<String, Value>);

impl Response {
    pub fn decode(line: &[u8]) -> Result<Response> {
        let text = String::from_utf8_lossy(line);
        let text = text.trim();
        if text.is_empty() {
            return Err(Error::protocol("empty line"));
        }
        match serde_json::from_str::<Value>(text) {
            Ok(Value::Object(m)) => Ok(Response(m)),
            Ok(_) => Err(Error::protocol(format!("expected a JSON object from device, got: {text}"))),
            Err(e) => Err(Error::protocol(format!("malformed JSON from device: {text:?} ({e})"))),
        }
    }

    pub fn status(&self) -> &str {
        self.0.get("status").and_then(Value::as_str).unwrap_or("")
    }

    pub fn is_ok(&self) -> bool {
        self.status() == "ok"
    }

    pub fn kind(&self) -> Option<&str> {
        self.0.get("type").and_then(Value::as_str)
    }

    pub fn is_event(&self) -> bool {
        self.kind().is_some_and(|k| EVENT_TYPES.contains(&k))
    }

    pub fn message(&self) -> Option<&str> {
        self.0.get("message").and_then(Value::as_str)
    }

    pub fn str_field(&self, key: &str) -> Option<&str> {
        self.0.get(key).and_then(Value::as_str)
    }

    pub fn bool_field(&self, key: &str) -> bool {
        self.0.get(key).and_then(Value::as_bool).unwrap_or(false)
    }

    pub fn u64_field(&self, key: &str) -> Option<u64> {
        self.0.get(key).and_then(|v| v.as_u64().or_else(|| v.as_f64().filter(|f| *f >= 0.0).map(|f| f as u64)))
    }

    pub fn has(&self, key: &str) -> bool {
        self.0.contains_key(key)
    }

    /// `Err(Device(message))` unless `status == "ok"`.
    pub fn require_ok(self) -> Result<Response> {
        if self.is_ok() {
            Ok(self)
        } else {
            Err(Error::Device(self.message().unwrap_or("unknown error (no message)").to_string()))
        }
    }

    /// Like [`require_ok`](Self::require_ok) but also checks `type`.
    pub fn expect_kind(self, kind: &str) -> Result<Response> {
        let r = self.require_ok()?;
        match r.kind() {
            Some(k) if k == kind => Ok(r),
            other => Err(Error::protocol(format!(
                "expected a '{kind}' reply, got '{}'",
                other.unwrap_or("(untyped)")
            ))),
        }
    }

    /// Deserialize into a typed view.
    pub fn parse<T: DeserializeOwned>(&self) -> Result<T> {
        T::deserialize(Value::Object(self.0.clone()))
            .map_err(|e| Error::protocol(format!("malformed '{}' reply: {e}", self.kind().unwrap_or("?"))))
    }
}

// Typed views

#[derive(Clone, Debug, Deserialize)]
pub struct DeviceStatus {
    pub db_path: String,
    pub fs_total_bytes: u64,
    pub fs_used_bytes: u64,
    pub fs_free_bytes: u64,
    pub user_count: u64,
    #[serde(default)]
    pub db_crc32: Option<u32>,
    #[serde(default)]
    pub firmware_version: Option<String>,
    #[serde(default)]
    pub protocol_version: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct NetStatus {
    #[serde(default)]
    pub connected: bool,
    #[serde(default)]
    pub ssid: String,
    #[serde(default)]
    pub ip: String,
    #[serde(default)]
    pub rssi: i64,
    #[serde(default)]
    pub time_synced: bool,
}

/// A user as streamed by `list` / `find_name` / `find`.
#[derive(Clone, Debug, Deserialize)]
pub struct DeviceUser {
    pub uid: String,
    pub name: String,
    #[serde(default)]
    pub registered: String,
    #[serde(default)]
    pub valid_days: f64,
}

impl DeviceUser {
    pub fn into_entry(self) -> UserEntry {
        UserEntry::from_device(&self.uid, &self.name, &self.registered, self.valid_days)
    }
}

/// `list` / `find_name` reply. Malformed entries are skipped (and counted)
/// rather than failing the whole listing.
pub fn parse_user_list(resp: &Response) -> Result<(Vec<UserEntry>, usize)> {
    let users = resp
        .0
        .get("users")
        .and_then(Value::as_array)
        .ok_or_else(|| Error::protocol("reply is missing the 'users' array"))?;
    let mut out = Vec::with_capacity(users.len());
    let mut bad = 0;
    for u in users {
        match DeviceUser::deserialize(u) {
            Ok(du) => out.push(du.into_entry()),
            Err(_) => bad += 1,
        }
    }
    Ok((out, bad))
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct BatchFailure {
    #[serde(default)]
    pub uid: String,
    #[serde(default)]
    pub message: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct BatchAddResult {
    pub added: u64,
    pub errors: u64,
    #[serde(default)]
    pub failed: Vec<BatchFailure>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct SyncResult {
    pub removed: u64,
    pub added: u64,
    pub replaced: u64,
    pub errors: u64,
    pub db_crc32: u32,
    #[serde(default)]
    pub remove_phase_ms: u64,
    #[serde(default)]
    pub add_phase_ms: u64,
    #[serde(default)]
    pub replace_phase_ms: u64,
    #[serde(default)]
    pub save_ms: u64,
    #[serde(default)]
    pub save_encode_ms: u64,
    #[serde(default)]
    pub save_write_ms: u64,
    #[serde(default)]
    pub save_verify_ms: u64,
    #[serde(default)]
    pub save_finalize_ms: u64,
}

/// Header of a raw transfer (`export_bin` / `sync_manifest`).
#[derive(Clone, Copy, Debug, Deserialize)]
pub struct RawHeader {
    pub bytes: usize,
    pub count: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_wire_format() {
        let s = |r: Request| String::from_utf8(r.encode()).unwrap();
        assert_eq!(s(Request::Status), "{\"type\":\"status\"}\n");
        assert_eq!(s(Request::Find { uid: "04AABBCC".into() }), "{\"type\":\"find\",\"uid\":\"04AABBCC\"}\n");
        assert_eq!(
            s(Request::SyncApply { remove: 1, add: 2, replace: 3 }),
            "{\"type\":\"sync_apply\",\"remove\":1,\"add\":2,\"replace\":3}\n"
        );
        assert_eq!(s(Request::EnterScanMode { continuous: false }), "{\"type\":\"enter_scan_mode\"}\n");
        assert_eq!(
            s(Request::EnterScanMode { continuous: true }),
            "{\"type\":\"enter_scan_mode\",\"continuous\":true}\n"
        );
    }

    #[test]
    fn admin_add_omits_keys() {
        let r = Request::Add(WireUser { uid: "04AABBCC".into(), name: "Zoë".into(), registered: None, valid_days: None });
        let line = String::from_utf8(r.encode()).unwrap();
        assert_eq!(line, "{\"type\":\"add\",\"uid\":\"04AABBCC\",\"name\":\"Zoë\"}\n");
        let r = Request::Add(WireUser {
            uid: "04AABBCC".into(),
            name: "A".into(),
            registered: Some("2026-01-01".into()),
            valid_days: Some(0.5),
        });
        assert!(String::from_utf8(r.encode()).unwrap().contains("\"registered\":\"2026-01-01\",\"valid_days\":0.5"));
    }

    #[test]
    fn every_kind_matches_serde_tag() {
        let all = [
            Request::Status,
            Request::NetStatus,
            Request::List,
            Request::ClearAll,
            Request::ExitScanMode,
            Request::ExitRenewalMode,
            Request::GetTime,
            Request::NtpSync,
            Request::ImportBegin,
            Request::ImportEnd,
            Request::ImportAbort,
            Request::ExportBin,
            Request::SyncBegin,
            Request::SyncManifest,
            Request::ImportBin { bytes: 0 },
            Request::RemoveAllExcept { uids: vec![] },
            Request::FindName { query: String::new() },
            Request::ConfigureTimezone { gmt_offset_sec: 0, daylight_offset_sec: 0 },
            Request::EnterRenewalMode { valid_days: 1.0 },
        ];
        for r in all {
            let v: Value = serde_json::from_slice(&r.encode()).unwrap();
            assert_eq!(v["type"], r.kind());
        }
    }

    #[test]
    fn response_helpers() {
        let r = Response::decode(br#"{"status":"error","message":"UID not found"}"#).unwrap();
        assert!(!r.is_ok());
        assert_eq!(r.clone().require_ok().unwrap_err().to_string(), "device error: UID not found");
        assert!(Response::decode(b"[boot] db_.begin() ...").is_err());
        assert!(Response::decode(b"[1,2]").is_err());
        let ev = Response::decode(br#"{"status":"ok","type":"uid_detected","uid":"04AABBCC"}"#).unwrap();
        assert!(ev.is_event());
        assert!(ev.clone().expect_kind("uid_detected").is_ok());
        assert!(ev.expect_kind("status").is_err());
    }

    #[test]
    fn user_list_parsing_tolerates_bad_entries() {
        let r = Response::decode(
            br#"{"status":"ok","users":[{"uid":"04AABBCC","name":"A","registered":"","valid_days":-1.00},
                {"uid":"04AABBCD","name":"B","registered":"2026-01-01","valid_days":30.00},{"oops":1}]}"#,
        )
        .unwrap();
        let (users, bad) = parse_user_list(&r).unwrap();
        assert_eq!(users.len(), 2);
        assert_eq!(bad, 1);
        assert!(users[0].is_admin());
        assert!(!users[1].is_admin());
    }

    #[test]
    fn status_parsing_is_backward_compatible() {
        let r = Response::decode(
            br#"{"status":"ok","type":"status","db_path":"/users.bin","fs_total_bytes":100,"fs_used_bytes":10,"fs_free_bytes":90,"user_count":3}"#,
        )
        .unwrap();
        let s: DeviceStatus = r.parse().unwrap();
        assert_eq!(s.user_count, 3);
        assert!(s.db_crc32.is_none());
    }
}
