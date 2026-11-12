//! Reading and writing user files: JSON arrays, CSV, and raw `users.bin`.
//!
//! One loader serves `import`, `sync` and `convert`, so every entry point
//! applies exactly the same validation.
//!
//! Format is chosen by extension: `.json`, `.bin`, anything else is CSV.
//!
//! Field semantics (JSON and CSV alike):
//! * `uid`, `name` -- required.
//! * `valid_days` absent / null / empty -> **admin** badge (never expires).
//! * `registered` absent / null / empty with a `valid_days` -> "floating"
//!   date (see [`crate::database::Validity`]).

use std::fs;
use std::path::Path;

use serde_json::{json, Value};

use crate::convert::{self, RECORD_SIZE};
use crate::database::{format_days, parse_date, validate_name, validate_valid_days, Uid, UserEntry, Validity};
use crate::error::{Error, Result};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileKind {
    Json,
    Csv,
    Bin,
}

impl FileKind {
    pub fn from_path(path: &Path) -> FileKind {
        match path.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase).as_deref() {
            Some("json") => FileKind::Json,
            Some("bin") => FileKind::Bin,
            _ => FileKind::Csv,
        }
    }
}

/// A row/record that was skipped, with the reason.
#[derive(Clone, Debug)]
pub struct Issue {
    pub location: String,
    pub message: String,
}

impl std::fmt::Display for Issue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.location, self.message)
    }
}

#[derive(Debug, Default)]
pub struct LoadedFile {
    pub entries: Vec<UserEntry>,
    pub issues: Vec<Issue>,
    /// For `.bin` input: the valid records exactly as stored in the file,
    /// so `import` can stream them without re-encoding.
    pub raw_bin: Option<Vec<u8>>,
    /// Informational notes (encoding fallback, torn trailing bytes, ...).
    pub notes: Vec<String>,
}

impl LoadedFile {
    pub fn floating_count(&self) -> usize {
        self.entries.iter().filter(|e| e.is_floating()).count()
    }
}

pub fn load(path: &Path) -> Result<LoadedFile> {
    let data = fs::read(path).map_err(|e| Error::file(path, e))?;
    match FileKind::from_path(path) {
        FileKind::Bin => load_bin(&data),
        kind => {
            let (text, note) = decode_text(&data);
            let mut loaded = if kind == FileKind::Json { load_json(&text)? } else { load_csv(&text)? };
            if let Some(n) = note {
                loaded.notes.insert(0, n);
            }
            Ok(loaded)
        }
    }
}

// Text decoding: UTF-8 (with or without BOM), falling back to Windows-1252
// (what Excel writes for "CSV" on most Western-European Windows installs).

const CP1252_HIGH: [char; 32] = [
    '\u{20AC}', '\u{0081}', '\u{201A}', '\u{0192}', '\u{201E}', '\u{2026}', '\u{2020}', '\u{2021}', '\u{02C6}', '\u{2030}',
    '\u{0160}', '\u{2039}', '\u{0152}', '\u{008D}', '\u{017D}', '\u{008F}', '\u{0090}', '\u{2018}', '\u{2019}', '\u{201C}',
    '\u{201D}', '\u{2022}', '\u{2013}', '\u{2014}', '\u{02DC}', '\u{2122}', '\u{0161}', '\u{203A}', '\u{0153}', '\u{009D}',
    '\u{017E}', '\u{0178}',
];

fn decode_text(data: &[u8]) -> (String, Option<String>) {
    let body = data.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(data);
    match std::str::from_utf8(body) {
        Ok(s) => (s.to_string(), None),
        Err(_) => {
            let s = body
                .iter()
                .map(|&b| if (0x80..0xA0).contains(&b) { CP1252_HIGH[(b - 0x80) as usize] } else { char::from(b) })
                .collect();
            (s, Some("file isn't UTF-8 -- read as Windows-1252. Re-save as UTF-8 if any names look garbled.".into()))
        }
    }
}

// JSON

fn load_json(text: &str) -> Result<LoadedFile> {
    let value: Value = serde_json::from_str(text).map_err(|e| Error::format(format!("invalid JSON: {e}")))?;
    let Value::Array(items) = value else {
        return Err(Error::format("JSON must be a top-level array of user objects, e.g. [{\"uid\": ..., \"name\": ...}]"));
    };
    let mut out = LoadedFile { entries: Vec::with_capacity(items.len()), ..Default::default() };
    for (i, item) in items.iter().enumerate() {
        let location = format!("entry {}", i + 1);
        let Some(obj) = item.as_object() else {
            out.issues.push(Issue { location, message: "not a JSON object".into() });
            continue;
        };
        let field = |k: &str| -> Option<String> {
            match obj.get(k) {
                None | Some(Value::Null) => None,
                Some(Value::String(s)) => Some(s.trim().to_string()),
                Some(Value::Number(n)) => Some(n.to_string()),
                Some(other) => Some(other.to_string()),
            }
            .filter(|s| !s.is_empty())
        };
        match build_entry(field("uid"), field("name"), field("registered"), field("valid_days")) {
            Ok(e) => out.entries.push(e),
            Err(message) => out.issues.push(Issue { location, message }),
        }
    }
    Ok(out)
}

// CSV

fn load_csv(text: &str) -> Result<LoadedFile> {
    let mut rdr = csv::ReaderBuilder::new().flexible(true).trim(csv::Trim::All).from_reader(text.as_bytes());
    let headers: Vec<String> = rdr
        .headers()
        .map_err(|e| Error::format(format!("CSV parse error: {e}")))?
        .iter()
        .map(|h| h.trim().to_ascii_lowercase())
        .collect();
    if headers.iter().all(String::is_empty) {
        return Err(Error::format("CSV file is empty or has no header row"));
    }
    let col = |name: &str| headers.iter().position(|h| h == name);
    let (Some(uid_col), Some(name_col)) = (col("uid"), col("name")) else {
        return Err(Error::format(format!("CSV must have 'uid' and 'name' columns (found: {})", headers.join(", "))));
    };
    let reg_col = col("registered");
    let days_col = col("valid_days");

    let mut out = LoadedFile::default();
    for row in rdr.records() {
        // Physical line number (the csv crate skips blank lines, so a
        // running counter would drift).
        let line = match &row {
            Ok(r) => r.position().map(|p| p.line()).unwrap_or(0),
            Err(e) => e.position().map(|p| p.line()).unwrap_or(0),
        };
        let location = format!("row {line}");
        let row = match row {
            Ok(r) => r,
            Err(e) => {
                out.issues.push(Issue { location, message: format!("CSV parse error: {e}") });
                continue;
            }
        };
        if row.iter().all(str::is_empty) {
            continue; // blank line
        }
        let get = |c: Option<usize>| c.and_then(|c| row.get(c)).map(str::to_string).filter(|s| !s.is_empty());
        match build_entry(get(Some(uid_col)), get(Some(name_col)), get(reg_col), get(days_col)) {
            Ok(e) => out.entries.push(e),
            Err(message) => out.issues.push(Issue { location, message }),
        }
    }
    Ok(out)
}

/// Shared field validation for JSON and CSV rows.
fn build_entry(
    uid: Option<String>,
    name: Option<String>,
    registered: Option<String>,
    valid_days: Option<String>,
) -> std::result::Result<UserEntry, String> {
    let uid = Uid::parse(&uid.ok_or("missing 'uid'")?)?;
    let name = name.ok_or("missing 'name'")?;
    validate_name(&name)?;
    let validity = match valid_days {
        None => Validity::Admin,
        Some(raw) => {
            let v: f64 = raw.parse().map_err(|_| format!("invalid valid_days '{raw}'"))?;
            let valid_days = validate_valid_days(v)?;
            let registered = registered.map(|r| parse_date(&r)).transpose()?;
            Validity::Expiring { registered, valid_days }
        }
    };
    Ok(UserEntry { uid, name, validity })
}

// BIN

fn load_bin(data: &[u8]) -> Result<LoadedFile> {
    let bin = convert::parse_bin(data)?;
    let mut out = LoadedFile::default();
    if bin.trailing_bytes > 0 {
        out.notes.push(format!(
            "{} trailing byte(s) after the last full record (likely a torn write) -- ignored, same as the firmware.",
            bin.trailing_bytes
        ));
    }
    let mut raw = Vec::with_capacity(bin.payload.len());
    for (i, rec) in bin.payload.chunks_exact(RECORD_SIZE).enumerate() {
        match convert::decode_record(rec) {
            Ok(e) => {
                raw.extend_from_slice(rec);
                out.entries.push(e);
            }
            Err(message) => out.issues.push(Issue { location: format!("record {i}"), message }),
        }
    }
    out.raw_bin = Some(raw);
    Ok(out)
}

// Writers

/// JSON representation of one user (admin -> `registered`/`valid_days` null).
pub fn entry_to_json(e: &UserEntry) -> Value {
    match e.validity {
        Validity::Admin => json!({ "uid": e.uid, "name": e.name, "registered": null, "valid_days": null }),
        Validity::Expiring { registered, valid_days } => json!({
            "uid": e.uid,
            "name": e.name,
            "registered": registered.map(|d| d.to_string()),
            "valid_days": valid_days,
        }),
    }
}

pub fn to_json_string(entries: &[UserEntry]) -> String {
    let arr: Vec<Value> = entries.iter().map(entry_to_json).collect();
    serde_json::to_string_pretty(&arr).expect("JSON values always serialize") + "\n"
}

pub fn to_csv_string(entries: &[UserEntry]) -> Result<String> {
    let mut w = csv::Writer::from_writer(Vec::new());
    let ioerr = |e: csv::Error| Error::format(format!("CSV write error: {e}"));
    w.write_record(["uid", "name", "registered", "valid_days"]).map_err(ioerr)?;
    for e in entries {
        let (reg, days) = match e.validity {
            Validity::Admin => (String::new(), String::new()),
            Validity::Expiring { registered, valid_days } => {
                (registered.map(|d| d.to_string()).unwrap_or_default(), format_days(valid_days))
            }
        };
        w.write_record([e.uid.as_str(), &e.name, &reg, &days]).map_err(ioerr)?;
    }
    let bytes = w.into_inner().map_err(|e| Error::format(format!("CSV write error: {e}")))?;
    Ok(String::from_utf8(bytes).expect("CSV of UTF-8 strings is UTF-8"))
}

/// Write `.bin` (header + raw records).
pub fn write_bin(path: &Path, records: &[u8]) -> Result<()> {
    let mut data = Vec::with_capacity(convert::HEADER_SIZE + records.len());
    data.extend_from_slice(&convert::header_bytes());
    data.extend_from_slice(records);
    write_atomic(path, &data)
}

/// Write entries in the format implied by `path`'s extension.
pub fn save(path: &Path, entries: &[UserEntry], today: chrono::NaiveDate) -> Result<()> {
    match FileKind::from_path(path) {
        FileKind::Json => write_atomic(path, to_json_string(entries).as_bytes()),
        FileKind::Csv => write_atomic(path, to_csv_string(entries)?.as_bytes()),
        FileKind::Bin => write_bin(path, &convert::encode_sorted(entries, today)),
    }
}

/// Write via a temp file + rename so an interrupted export/backup never
/// leaves a truncated file behind under the final name.
pub fn write_atomic(path: &Path, data: &[u8]) -> Result<()> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        fs::create_dir_all(dir).map_err(|e| Error::file(dir, e))?;
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".part");
    let tmp = std::path::PathBuf::from(tmp);
    fs::write(&tmp, data).map_err(|e| Error::file(&tmp, e))?;
    fs::rename(&tmp, path).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        Error::file(path, e)
    })
}

/// Outcome of an offline conversion.
#[derive(Debug)]
pub struct ConvertReport {
    pub written: usize,
    pub skipped: Vec<Issue>,
    pub notes: Vec<String>,
}

/// Convert between any two of JSON / CSV / BIN.
///
/// Text sources are strict: any invalid row or duplicate UID aborts the
/// conversion (a `.bin` is meant to be imported verbatim, so it must never
/// silently contain fewer users than its source). Corrupt records in a
/// `.bin` source are skipped with a warning, like the firmware loader.
pub fn convert_file(src: &Path, dst: &Path, today: chrono::NaiveDate) -> Result<ConvertReport> {
    let (sk, dk) = (FileKind::from_path(src), FileKind::from_path(dst));
    if sk == dk {
        return Err(Error::usage("source and destination have the same format (use .json, .csv or .bin)"));
    }
    let loaded = load(src)?;
    if sk != FileKind::Bin {
        if !loaded.issues.is_empty() {
            let list: Vec<String> = loaded.issues.iter().take(10).map(ToString::to_string).collect();
            let more = loaded.issues.len().saturating_sub(10);
            return Err(Error::format(format!(
                "{} invalid row(s) in {}:\n  {}{}",
                loaded.issues.len(),
                src.display(),
                list.join("\n  "),
                if more > 0 { format!("\n  ... and {more} more") } else { String::new() }
            )));
        }
        let dups = convert::duplicate_uids(&loaded.entries);
        if !dups.is_empty() {
            let shown: Vec<&str> = dups.iter().take(10).map(Uid::as_str).collect();
            return Err(Error::format(format!("duplicate UID(s) in {}: {}", src.display(), shown.join(", "))));
        }
    }
    let mut entries = loaded.entries;
    if dk == FileKind::Bin {
        // Already sorted by encode_sorted(); nothing else to do.
    } else {
        entries.sort_by(|a, b| a.uid.cmp(&b.uid));
    }
    save(dst, &entries, today)?;
    Ok(ConvertReport { written: entries.len(), skipped: loaded.issues, notes: loaded.notes })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::parse_date;

    fn today() -> chrono::NaiveDate {
        parse_date("2026-07-01").unwrap()
    }

    #[test]
    fn json_parsing_rules() {
        let text = r#"[
            {"uid": "04A1B2C3", "name": "Alice", "registered": "2026-01-15", "valid_days": 365},
            {"uid": "12345678", "name": "Admin"},
            {"uid": "12345679", "name": "Admin2", "registered": null, "valid_days": null},
            {"uid": "AABBCCDD", "name": "Float", "valid_days": "0.5"},
            {"uid": 12345670, "name": "Numeric uid", "valid_days": 1},
            {"uid": "bad", "name": "x"},
            {"uid": "AABBCCDE", "name": "Bad date", "registered": "2024-02-30", "valid_days": 1},
            {"uid": "AABBCCDF", "name": "Neg", "valid_days": -3},
            {"uid": "AABBCCE0"},
            "not an object"
        ]"#;
        let f = load_json(text).unwrap();
        assert_eq!(f.entries.len(), 5);
        assert_eq!(f.issues.len(), 5);
        assert!(f.entries[1].is_admin() && f.entries[2].is_admin());
        assert!(f.entries[3].is_floating());
        assert_eq!(f.entries[4].uid.as_str(), "12345670");
        assert!(f.issues[1].message.contains("calendar"));
    }

    #[test]
    fn json_top_level_must_be_array() {
        assert!(load_json(r#"{"uid": "x"}"#).is_err());
        assert!(load_json("not json").is_err());
    }

    #[test]
    fn csv_parsing_rules() {
        let text = "UID , Name,registered,VALID_DAYS\n\
                    04A1B2C3,Alice Martin,2026-01-15,365\n\
                    12345678,Admin Garage,,\n\
                    \n\
                    0A1B2C3D,Floating,,90\n\
                    0A1B2C3E,,2026-01-01,5\n\
                    0A1B2C3F,Nan days,,nan\n";
        let f = load_csv(text).unwrap();
        assert_eq!(f.entries.len(), 3);
        assert_eq!(f.issues.len(), 2);
        assert_eq!(f.issues[0].location, "row 6");
        assert!(f.entries[1].is_admin());
        assert!(f.entries[2].is_floating());
    }

    #[test]
    fn csv_requires_columns() {
        assert!(load_csv("id,name\n1,x\n").is_err());
        assert!(load_csv("").is_err());
    }

    #[test]
    fn cp1252_fallback() {
        let (s, note) = decode_text(b"uid,name\n04A1B2C3,Ren\xe9 \x80\n");
        assert!(note.is_some());
        assert!(s.contains("René €"));
        let (s, note) = decode_text(b"\xEF\xBB\xBFuid");
        assert_eq!(s, "uid");
        assert!(note.is_none());
    }

    #[test]
    fn conversions_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let json = dir.path().join("a.json");
        fs::write(
            &json,
            r#"[{"uid":"04A1B2C3","name":"Zoë","registered":"2026-01-15","valid_days":365},{"uid":"12345678","name":"Admin"}]"#,
        )
        .unwrap();
        let bin = dir.path().join("a.bin");
        let csv = dir.path().join("a.csv");
        let back = dir.path().join("b.json");
        assert_eq!(convert_file(&json, &bin, today()).unwrap().written, 2);
        assert_eq!(convert_file(&bin, &csv, today()).unwrap().written, 2);
        assert_eq!(convert_file(&csv, &back, today()).unwrap().written, 2);
        let a = load(&json).unwrap().entries;
        let mut b = load(&back).unwrap().entries;
        b.sort_by(|x, y| x.uid.cmp(&y.uid));
        let mut a = a;
        a.sort_by(|x, y| x.uid.cmp(&y.uid));
        assert_eq!(a, b);
        let raw = load(&bin).unwrap().raw_bin.unwrap();
        assert_eq!(raw.len(), 2 * RECORD_SIZE);
    }

    #[test]
    fn convert_is_strict_for_text_sources() {
        let dir = tempfile::tempdir().unwrap();
        let json = dir.path().join("a.json");
        fs::write(&json, r#"[{"uid":"04A1B2C3","name":"A"},{"uid":"04a1b2c3","name":"B"}]"#).unwrap();
        let err = convert_file(&json, &dir.path().join("a.bin"), today()).unwrap_err();
        assert!(err.to_string().contains("duplicate"));
        fs::write(&json, format!(r#"[{{"uid":"04A1B2C3","name":"{}"}}]"#, "A".repeat(49))).unwrap();
        assert!(convert_file(&json, &dir.path().join("a.bin"), today()).is_err());
        assert!(convert_file(&json, &dir.path().join("b.json"), today()).is_err(), "same format");
    }

    #[test]
    fn bin_skips_corrupt_records() {
        let e = UserEntry { uid: Uid::parse("04AABBCC").unwrap(), name: "A".into(), validity: Validity::Admin };
        let mut data = convert::header_bytes().to_vec();
        data.extend_from_slice(&convert::encode_record(&e, today()));
        let mut bad = convert::encode_record(&e, today());
        bad[30] ^= 0xFF;
        data.extend_from_slice(&bad);
        let f = load_bin(&data).unwrap();
        assert_eq!(f.entries.len(), 1);
        assert_eq!(f.issues.len(), 1);
        assert_eq!(f.raw_bin.unwrap().len(), RECORD_SIZE);
    }
}
