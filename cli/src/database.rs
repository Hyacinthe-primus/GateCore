//! Validated domain model: UIDs, names, registration dates and validity.
//!
//! Every limit mirrors the firmware (`firmware/Config.h` and
//! `DatabaseManager::isValid*`), so invalid input is rejected on the host
//! with a clear message instead of being half-accepted or truncated by the
//! device:
//!
//! * UIDs strip only separators (`:`, `-`, whitespace); the result must be
//!   8-20 hex digits with even length.
//! * Names are limited to 48 UTF-8 bytes and reject control characters and
//!   whitespace-only input.
//! * Registration dates are validated at parse time, including the `u16`
//!   day range the binary format can represent.

use std::fmt;

use chrono::{Duration, NaiveDate};
use serde::{Serialize, Serializer};

/// Max name length in UTF-8 bytes (`MAX_NAME_LEN` in Config.h).
pub const MAX_NAME_LEN: usize = 48;
/// Shortest UID: 4 bytes (MIFARE Classic).
pub const MIN_UID_HEX_LEN: usize = 8;
/// Longest UID: 10 bytes (triple-size UID).
pub const MAX_UID_HEX_LEN: usize = 20;
/// Device capacity (`MAX_USERS` in Config.h).
pub const MAX_USERS: usize = 70_000;
/// `valid_days` sentinel the firmware uses for admin badges.
pub const ADMIN_VALID_DAYS: f64 = -1.0;
/// Largest day offset the binary format stores (0xFFFF is the admin sentinel).
pub const MAX_STORABLE_DAYS: i64 = 65_534;

pub fn epoch() -> NaiveDate {
    NaiveDate::from_ymd_opt(1970, 1, 1).expect("valid epoch")
}

/// Today's local calendar date. Registration dates are calendar dates, so keep
/// the device timezone aligned (see the `timezone` command).
pub fn today() -> NaiveDate {
    chrono::Local::now().date_naive()
}

// UID

/// A normalized card UID: uppercase hex, even length, 8..=20 digits.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Uid(String);

impl Uid {
    /// Parse user input. Accepts common separators (`04:AA:BB:CC`,
    /// `04-aa-bb-cc`, `04 AA BB CC`) but nothing else.
    pub fn parse(raw: &str) -> Result<Uid, String> {
        let cleaned: String = raw
            .chars()
            .filter(|c| !(c.is_whitespace() || *c == ':' || *c == '-'))
            .collect();
        if cleaned.is_empty() {
            return Err("UID is empty".into());
        }
        if !cleaned.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(format!("'{raw}' is not a valid UID (hex digits only)"));
        }
        let n = cleaned.len();
        if !(MIN_UID_HEX_LEN..=MAX_UID_HEX_LEN).contains(&n) {
            return Err(format!(
                "'{raw}' is not a valid UID ({n} hex digits; expected {MIN_UID_HEX_LEN}-{MAX_UID_HEX_LEN})"
            ));
        }
        if n % 2 != 0 {
            return Err(format!("'{raw}' is not a valid UID (odd number of hex digits)"));
        }
        Ok(Uid(cleaned.to_ascii_uppercase()))
    }

    /// Build from raw UID bytes (binary records / manifests).
    pub fn from_bytes(bytes: &[u8]) -> Uid {
        let mut s = String::with_capacity(bytes.len() * 2);
        for b in bytes {
            s.push(char::from_digit(u32::from(b >> 4), 16).unwrap_or('0'));
            s.push(char::from_digit(u32::from(b & 0x0F), 16).unwrap_or('0'));
        }
        Uid(s.to_ascii_uppercase())
    }

    /// Wrap a UID string reported by the device without re-validating it
    /// (the device is the authority on what it stores).
    pub fn from_device(s: &str) -> Uid {
        Uid(s.trim().to_ascii_uppercase())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Raw bytes (pairs of hex digits). Invalid pairs (only possible for
    /// `from_device` values) decode as 0.
    pub fn to_bytes(&self) -> Vec<u8> {
        self.0
            .as_bytes()
            .chunks(2)
            .map(|pair| {
                std::str::from_utf8(pair)
                    .ok()
                    .and_then(|p| u8::from_str_radix(p, 16).ok())
                    .unwrap_or(0)
            })
            .collect()
    }
}

impl fmt::Display for Uid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Serialize for Uid {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.0)
    }
}

/// Parse a comma-separated UID list (`--except A,B,C`).
pub fn parse_uid_list(raw: &str) -> Result<Vec<Uid>, String> {
    let uids = raw
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(Uid::parse)
        .collect::<Result<Vec<_>, _>>()?;
    if uids.is_empty() {
        return Err("expected at least one UID".into());
    }
    Ok(uids)
}

// Names, dates, validity

/// Mirror of `DatabaseManager::isValidName` + the byte limit.
pub fn validate_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("name is empty".into());
    }
    let len = name.len();
    if len > MAX_NAME_LEN {
        return Err(format!("name '{name}' is {len} UTF-8 bytes (max {MAX_NAME_LEN})"));
    }
    if name.bytes().any(|b| b < 0x20) {
        return Err(format!("name '{}' contains control characters", name.escape_debug()));
    }
    if name.bytes().all(|b| b.is_ascii_whitespace()) {
        return Err("name is only whitespace".into());
    }
    Ok(())
}

/// Strict `YYYY-MM-DD` parse, restricted to the range the binary DB format
/// can represent (1970-01-01 .. 2149-06-05).
pub fn parse_date(raw: &str) -> Result<NaiveDate, String> {
    let s = raw.trim();
    let shape_ok = s.len() == 10
        && s.bytes().enumerate().all(|(i, b)| if i == 4 || i == 7 { b == b'-' } else { b.is_ascii_digit() });
    if !shape_ok {
        return Err(format!("invalid date '{s}' (expected YYYY-MM-DD)"));
    }
    let date = NaiveDate::parse_from_str(s, "%Y-%m-%d").map_err(|_| format!("'{s}' is not a real calendar date"))?;
    let days = (date - epoch()).num_days();
    if !(0..=MAX_STORABLE_DAYS).contains(&days) {
        return Err(format!("date '{s}' is outside the storable range (1970-01-01 .. {})", days_to_date(MAX_STORABLE_DAYS as u16)));
    }
    Ok(date)
}

pub fn date_to_days(date: NaiveDate) -> u16 {
    // parse_date() guarantees the range; clamp defensively anyway.
    (date - epoch()).num_days().clamp(0, MAX_STORABLE_DAYS) as u16
}

pub fn days_to_date(days: u16) -> NaiveDate {
    epoch() + Duration::days(i64::from(days))
}

/// `valid_days` must be finite and non-negative; NaN/inf would serialize as
/// invalid JSON tokens.
pub fn validate_valid_days(v: f64) -> Result<f64, String> {
    if !v.is_finite() {
        return Err(format!("valid_days '{v}' is not a finite number"));
    }
    if v < 0.0 {
        return Err("valid_days cannot be negative (omit it for an admin badge)".into());
    }
    // ~27 million years; keeps the value far below anything that could
    // overflow the firmware's epoch arithmetic.
    if v > 1.0e10 {
        return Err(format!("valid_days {v} is unreasonably large"));
    }
    Ok(v)
}

pub fn parse_valid_days(raw: &str) -> Result<f64, String> {
    let v: f64 = raw.trim().parse().map_err(|_| format!("'{}' is not a valid number of days", raw.trim()))?;
    validate_valid_days(v)
}

/// How long a badge is valid.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Validity {
    /// Never expires (no registration date, `valid_days = -1` on device).
    Admin,
    /// Expires `valid_days` after `registered`. `registered: None` means the
    /// source file did not specify a date ("floating"): it is stamped with
    /// today's date when written -- except during `sync`, which preserves
    /// the date already on the device for existing users.
    Expiring { registered: Option<NaiveDate>, valid_days: f64 },
}

/// One user as read from a file or the device.
#[derive(Clone, Debug, PartialEq)]
pub struct UserEntry {
    pub uid: Uid,
    pub name: String,
    pub validity: Validity,
}

impl UserEntry {
    pub fn is_admin(&self) -> bool {
        matches!(self.validity, Validity::Admin)
    }

    pub fn is_floating(&self) -> bool {
        matches!(self.validity, Validity::Expiring { registered: None, .. })
    }

    /// Build from the fields the firmware reports in `list` / `find`.
    /// Admin if `registered` is empty or `valid_days == -1` (both sentinels,
    /// exactly like `UserRecord::isAdmin()`).
    pub fn from_device(uid: &str, name: &str, registered: &str, valid_days: f64) -> UserEntry {
        let validity = if registered.is_empty() || valid_days == ADMIN_VALID_DAYS {
            Validity::Admin
        } else {
            Validity::Expiring { registered: parse_date(registered).ok(), valid_days }
        };
        UserEntry { uid: Uid::from_device(uid), name: name.to_string(), validity }
    }

    /// Pin a floating registration date to `date` (no-op otherwise).
    pub fn with_registered_if_floating(mut self, date: NaiveDate) -> UserEntry {
        if let Validity::Expiring { registered: registered @ None, .. } = &mut self.validity {
            *registered = Some(date);
        }
        self
    }

    pub fn registered_label(&self) -> String {
        match self.validity {
            Validity::Admin => "-".into(),
            Validity::Expiring { registered: Some(d), .. } => d.to_string(),
            Validity::Expiring { registered: None, .. } => "(today)".into(),
        }
    }

    pub fn valid_days_label(&self) -> String {
        match self.validity {
            Validity::Admin => "-".into(),
            Validity::Expiring { valid_days, .. } => format_days(valid_days),
        }
    }

    /// Approximate expiration for display (the device is the authority).
    /// Fractional validity is shown with a time of day.
    pub fn expiration_label(&self) -> String {
        match self.validity {
            Validity::Admin => "ADMIN (no expiry)".into(),
            Validity::Expiring { registered: None, .. } => "?".into(),
            Validity::Expiring { registered: Some(d), valid_days } => {
                let secs = (valid_days * 86_400.0).round();
                if !secs.is_finite() || secs > 1.0e15 {
                    return "never (practically)".into();
                }
                let start = d.and_hms_opt(0, 0, 0).expect("midnight");
                match start.checked_add_signed(Duration::seconds(secs as i64)) {
                    Some(t) if valid_days.fract() == 0.0 => t.date().to_string(),
                    Some(t) => t.format("%Y-%m-%d %H:%M").to_string(),
                    None => "never (practically)".into(),
                }
            }
        }
    }
}

/// Print `30` instead of `30.0`, but keep real fractions.
pub fn format_days(v: f64) -> String {
    if v.fract() == 0.0 && v.abs() < 1e15 {
        format!("{}", v as i64)
    } else {
        format!("{v}")
    }
}

/// Display order used by `list`: admins first, then by name (case-insensitive).
pub fn sort_for_display(users: &mut [UserEntry]) {
    users.sort_by(|a, b| {
        (!a.is_admin(), a.name.to_lowercase(), a.uid.as_str()).cmp(&(!b.is_admin(), b.name.to_lowercase(), b.uid.as_str()))
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uid_normalization() {
        assert_eq!(Uid::parse("04:aa:bb:cc").unwrap().as_str(), "04AABBCC");
        assert_eq!(Uid::parse(" 04-AA-BB-CC-DD ").unwrap().as_str(), "04AABBCCDD");
        assert_eq!(Uid::parse("04 aa bb cc").unwrap().as_str(), "04AABBCC");
    }

    #[test]
    fn uid_rejections() {
        assert!(Uid::parse("04AABBC").is_err(), "too short");
        assert!(Uid::parse("04AABBCCD").is_err(), "odd length");
        assert!(Uid::parse("zz04aabbcc").is_err(), "non-hex garbage");
        assert!(Uid::parse(&"A".repeat(22)).is_err(), "too long");
        assert!(Uid::parse("").is_err());
    }

    #[test]
    fn uid_bytes_roundtrip() {
        let uid = Uid::parse("04AABBCCDD").unwrap();
        assert_eq!(uid.to_bytes(), vec![0x04, 0xAA, 0xBB, 0xCC, 0xDD]);
        assert_eq!(Uid::from_bytes(&uid.to_bytes()), uid);
    }

    #[test]
    fn uid_list() {
        let l = parse_uid_list("04AABBCC, 5AF73581,").unwrap();
        assert_eq!(l.len(), 2);
        assert!(parse_uid_list(" , ").is_err());
        assert!(parse_uid_list("04AABBCC,nope").is_err());
    }

    #[test]
    fn name_policy() {
        assert!(validate_name("Alice").is_ok());
        assert!(validate_name(&"A".repeat(48)).is_ok());
        assert!(validate_name(&"A".repeat(49)).is_err());
        // 47 ASCII + 'é' (2 bytes) = 49 bytes -> rejected (matches Usersdata/test.json)
        assert!(validate_name(&format!("{}é", "A".repeat(47))).is_err());
        assert!(validate_name(&format!("{}é", "A".repeat(46))).is_ok());
        assert!(validate_name("").is_err());
        assert!(validate_name("   ").is_err());
        assert!(validate_name("Tab\there").is_err());
        assert!(validate_name("O'Brien \"Bob\" \\").is_ok());
    }

    #[test]
    fn dates() {
        assert_eq!(parse_date("2026-01-15").unwrap(), NaiveDate::from_ymd_opt(2026, 1, 15).unwrap());
        assert!(parse_date("2024-02-30").is_err());
        assert!(parse_date("2024/01/01").is_err());
        assert!(parse_date("1969-12-31").is_err());
        assert!(parse_date("2149-06-05").is_ok());
        assert!(parse_date("2149-06-06").is_err());
        assert_eq!(date_to_days(parse_date("1970-01-02").unwrap()), 1);
        assert_eq!(days_to_date(20_000).to_string(), "2024-10-04");
    }

    #[test]
    fn valid_days_policy() {
        assert_eq!(parse_valid_days("0.5").unwrap(), 0.5);
        assert!(parse_valid_days("-1").is_err());
        assert!(parse_valid_days("nan").is_err());
        assert!(parse_valid_days("inf").is_err());
        assert!(parse_valid_days("abc").is_err());
    }

    #[test]
    fn expiration_labels() {
        let d = parse_date("2026-01-01").unwrap();
        let mk = |v| UserEntry {
            uid: Uid::parse("04AABBCC").unwrap(),
            name: "x".into(),
            validity: Validity::Expiring { registered: Some(d), valid_days: v },
        };
        assert_eq!(mk(30.0).expiration_label(), "2026-01-31");
        assert_eq!(mk(0.5).expiration_label(), "2026-01-01 12:00");
        let admin = UserEntry::from_device("04AABBCC", "boss", "", -1.0);
        assert!(admin.is_admin());
        assert_eq!(admin.expiration_label(), "ADMIN (no expiry)");
    }

    #[test]
    fn display_sort() {
        let mut v = vec![
            UserEntry::from_device("04AABBC1", "bob", "2026-01-01", 1.0),
            UserEntry::from_device("04AABBC2", "Zed", "", -1.0),
            UserEntry::from_device("04AABBC3", "alice", "2026-01-01", 1.0),
        ];
        sort_for_display(&mut v);
        let names: Vec<_> = v.iter().map(|u| u.name.as_str()).collect();
        assert_eq!(names, ["Zed", "alice", "bob"]);
    }
}
