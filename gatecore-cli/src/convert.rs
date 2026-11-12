//! `users.bin` codec, shared byte-for-byte with `gatecore-firmware/DatabaseManager.cpp`.
//!
//! ```text
//! file   = header(7) record*
//! header = "RUD1" version:u8 record_size:u16le
//! record = uid_len:u8 uid:[u8;10] name_len:u8 name:[u8;48]
//!          reg_days:u16le valid_days:f64le crc32:u32le      (74 bytes)
//! ```
//!
//! * `reg_days` = days since 1970-01-01, `0xFFFF` for admin badges.
//! * `valid_days` = `-1.0` for admin badges.
//! * `crc32` (IEEE, zlib-compatible) covers the first 70 bytes of the record.
//!
//! The whole-database checksum (`db_crc32`) is the CRC32 chained over the
//! first 70 bytes of every record in UID order -- the per-record trailing
//! CRC is deliberately excluded (chaining CRC32 over codewords collapses to
//! a value that only depends on the record count).
//!
//! Sync wire entries: a manifest entry is `uid_len uid[10] crc32` (15 bytes)
//! and a remove-list entry is `uid_len uid[10]` (11 bytes).

use std::collections::HashSet;

use chrono::NaiveDate;

use crate::database::{
    date_to_days, days_to_date, Uid, UserEntry, Validity, ADMIN_VALID_DAYS, MAX_NAME_LEN, MAX_UID_HEX_LEN,
};
use crate::error::{Error, Result};

pub const MAGIC: &[u8; 4] = b"RUD1";
pub const VERSION: u8 = 1;
pub const UID_BYTES: usize = MAX_UID_HEX_LEN / 2;
pub const HEADER_SIZE: usize = 4 + 1 + 2;
pub const RECORD_SIZE: usize = 1 + UID_BYTES + 1 + MAX_NAME_LEN + 2 + 8 + 4;
/// Bytes covered by the per-record CRC (and by the canonical db CRC).
pub const PAYLOAD_SIZE: usize = RECORD_SIZE - 4;
pub const ADMIN_DAYS_SENTINEL: u16 = 0xFFFF;
pub const MANIFEST_ENTRY_SIZE: usize = 1 + UID_BYTES + 4;
pub const UID_ENTRY_SIZE: usize = 1 + UID_BYTES;

const OFF_UID_LEN: usize = 0;
const OFF_UID: usize = 1;
const OFF_NAME_LEN: usize = OFF_UID + UID_BYTES;
const OFF_NAME: usize = OFF_NAME_LEN + 1;
const OFF_REG: usize = OFF_NAME + MAX_NAME_LEN;
const OFF_VALID: usize = OFF_REG + 2;
const OFF_CRC: usize = OFF_VALID + 8;

const _: () = assert!(RECORD_SIZE == 74);
const _: () = assert!(OFF_CRC == PAYLOAD_SIZE);

pub type RawRecord = [u8; RECORD_SIZE];

/// CRC32 (IEEE 802.3), zlib-compatible chaining: `crc32(b, crc32(a)) == crc32(a ++ b)`.
pub fn crc32(data: &[u8], init: u32) -> u32 {
    let mut h = crc32fast::Hasher::new_with_initial(init);
    h.update(data);
    h.finalize()
}

/// The 7-byte `.bin` file header.
pub fn header_bytes() -> [u8; HEADER_SIZE] {
    let rs = (RECORD_SIZE as u16).to_le_bytes();
    [MAGIC[0], MAGIC[1], MAGIC[2], MAGIC[3], VERSION, rs[0], rs[1]]
}

/// Encode one user. Floating registration dates are stamped with `today`.
///
/// Callers are expected to pass validated entries (see [`crate::database`]);
/// the name is still truncated on a UTF-8 boundary as a last line of
/// defence so the encoder can never emit a split multi-byte character.
pub fn encode_record(entry: &UserEntry, today: NaiveDate) -> RawRecord {
    let mut rec = [0u8; RECORD_SIZE];

    let uid = entry.uid.to_bytes();
    let uid_len = uid.len().min(UID_BYTES);
    rec[OFF_UID_LEN] = uid_len as u8;
    rec[OFF_UID..OFF_UID + uid_len].copy_from_slice(&uid[..uid_len]);

    let name = utf8_truncate(&entry.name, MAX_NAME_LEN);
    rec[OFF_NAME_LEN] = name.len() as u8;
    rec[OFF_NAME..OFF_NAME + name.len()].copy_from_slice(name.as_bytes());

    let (reg_days, valid_days) = match entry.validity {
        Validity::Admin => (ADMIN_DAYS_SENTINEL, ADMIN_VALID_DAYS),
        Validity::Expiring { registered, valid_days } => (date_to_days(registered.unwrap_or(today)), valid_days),
    };
    rec[OFF_REG..OFF_REG + 2].copy_from_slice(&reg_days.to_le_bytes());
    rec[OFF_VALID..OFF_VALID + 8].copy_from_slice(&valid_days.to_le_bytes());

    let crc = crc32(&rec[..PAYLOAD_SIZE], 0);
    rec[OFF_CRC..].copy_from_slice(&crc.to_le_bytes());
    rec
}

/// Longest prefix of `s` that fits in `max` bytes without splitting a char.
pub fn utf8_truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// The per-record CRC stored in the last 4 bytes.
pub fn record_crc(rec: &[u8]) -> u32 {
    u32::from_le_bytes(rec[OFF_CRC..OFF_CRC + 4].try_into().expect("4 bytes"))
}

/// Decode one record, verifying its CRC and field lengths.
pub fn decode_record(rec: &[u8]) -> Result<UserEntry, String> {
    if rec.len() != RECORD_SIZE {
        return Err(format!("truncated record ({} of {RECORD_SIZE} bytes)", rec.len()));
    }
    let stored = record_crc(rec);
    let actual = crc32(&rec[..PAYLOAD_SIZE], 0);
    if stored != actual {
        return Err(format!("CRC32 mismatch (record corrupt): stored 0x{stored:08x}, computed 0x{actual:08x}"));
    }
    let uid_len = rec[OFF_UID_LEN] as usize;
    let name_len = rec[OFF_NAME_LEN] as usize;
    if !(4..=UID_BYTES).contains(&uid_len) || name_len > MAX_NAME_LEN {
        return Err(format!("implausible uid_len/name_len ({uid_len}/{name_len})"));
    }
    let uid = Uid::from_bytes(&rec[OFF_UID..OFF_UID + uid_len]);
    let name = String::from_utf8_lossy(&rec[OFF_NAME..OFF_NAME + name_len]).into_owned();
    let reg_days = u16::from_le_bytes([rec[OFF_REG], rec[OFF_REG + 1]]);
    let valid_days = f64::from_le_bytes(rec[OFF_VALID..OFF_VALID + 8].try_into().expect("8 bytes"));

    let validity = if reg_days == ADMIN_DAYS_SENTINEL || valid_days == ADMIN_VALID_DAYS {
        Validity::Admin
    } else {
        Validity::Expiring { registered: Some(days_to_date(reg_days)), valid_days }
    };
    Ok(UserEntry { uid, name, validity })
}

/// Result of decoding a stream of records: good entries plus per-record errors.
#[derive(Debug, Default)]
pub struct DecodedRecords {
    pub entries: Vec<UserEntry>,
    /// `(record index, reason)` for every record that failed to decode.
    pub corrupt: Vec<(usize, String)>,
}

/// Decode a contiguous run of records (no header). Trailing partial bytes
/// are ignored, same as the firmware loader.
pub fn decode_records(payload: &[u8]) -> DecodedRecords {
    let mut out = DecodedRecords { entries: Vec::with_capacity(payload.len() / RECORD_SIZE), corrupt: Vec::new() };
    for (i, rec) in payload.chunks_exact(RECORD_SIZE).enumerate() {
        match decode_record(rec) {
            Ok(e) => out.entries.push(e),
            Err(why) => out.corrupt.push((i, why)),
        }
    }
    out
}

/// A parsed `.bin` file: the record payload (header stripped, trailing
/// partial record dropped) plus how many trailing bytes were dropped.
#[derive(Debug)]
pub struct BinFile<'a> {
    pub payload: &'a [u8],
    pub trailing_bytes: usize,
}

/// Validate a `.bin` header and split off the payload.
pub fn parse_bin(data: &[u8]) -> Result<BinFile<'_>> {
    if data.len() < HEADER_SIZE {
        return Err(Error::format("file too small to contain a users.bin header"));
    }
    if &data[0..4] != MAGIC {
        return Err(Error::format("bad magic bytes (not a users.bin file)"));
    }
    let version = data[4];
    if version != VERSION {
        return Err(Error::format(format!("unsupported format version {version} (this tool knows version {VERSION})")));
    }
    let record_size = u16::from_le_bytes([data[5], data[6]]) as usize;
    if record_size != RECORD_SIZE {
        return Err(Error::format(format!(
            "record size {record_size} doesn't match this tool's {RECORD_SIZE} -- built against a different Config.h \
             (MAX_NAME_LEN / MAX_UID_HEX_LEN mismatch)"
        )));
    }
    let body = &data[HEADER_SIZE..];
    let full = body.len() / RECORD_SIZE * RECORD_SIZE;
    Ok(BinFile { payload: &body[..full], trailing_bytes: body.len() - full })
}

/// Encode records sorted by UID (the firmware's on-disk invariant).
pub fn encode_sorted(entries: &[UserEntry], today: NaiveDate) -> Vec<u8> {
    let mut idx: Vec<usize> = (0..entries.len()).collect();
    idx.sort_by(|&a, &b| entries[a].uid.cmp(&entries[b].uid));
    let mut out = Vec::with_capacity(entries.len() * RECORD_SIZE);
    for i in idx {
        out.extend_from_slice(&encode_record(&entries[i], today));
    }
    out
}

/// Canonical whole-database CRC32; must equal `DatabaseManager::computeCrc32()`.
pub fn canonical_crc32<'a, I>(records: I) -> u32
where
    I: IntoIterator<Item = (&'a Uid, &'a RawRecord)>,
{
    let mut sorted: Vec<(&Uid, &RawRecord)> = records.into_iter().collect();
    sorted.sort_unstable_by(|a, b| a.0.cmp(b.0));
    let mut h = crc32fast::Hasher::new();
    for (_, rec) in sorted {
        h.update(&rec[..PAYLOAD_SIZE]);
    }
    h.finalize()
}

/// Convenience: canonical CRC of a list of entries (floating dates -> `today`).
pub fn canonical_crc32_of(entries: &[UserEntry], today: NaiveDate) -> u32 {
    let recs: Vec<(Uid, RawRecord)> = entries.iter().map(|e| (e.uid.clone(), encode_record(e, today))).collect();
    canonical_crc32(recs.iter().map(|(u, r)| (u, r)))
}

/// Remove-list entry for `sync_apply`.
pub fn encode_uid_entry(uid: &Uid) -> [u8; UID_ENTRY_SIZE] {
    let mut out = [0u8; UID_ENTRY_SIZE];
    let bytes = uid.to_bytes();
    let n = bytes.len().min(UID_BYTES);
    out[0] = n as u8;
    out[1..1 + n].copy_from_slice(&bytes[..n]);
    out
}

/// Decode one manifest entry into `(uid, record_crc32)`.
pub fn decode_manifest_entry(entry: &[u8]) -> Result<(Uid, u32), String> {
    if entry.len() != MANIFEST_ENTRY_SIZE {
        return Err(format!("truncated manifest entry ({} of {MANIFEST_ENTRY_SIZE} bytes)", entry.len()));
    }
    let uid_len = entry[0] as usize;
    if uid_len > UID_BYTES {
        return Err(format!("implausible uid_len ({uid_len})"));
    }
    let uid = Uid::from_bytes(&entry[1..1 + uid_len]);
    let crc = u32::from_le_bytes(entry[1 + UID_BYTES..].try_into().expect("4 bytes"));
    Ok((uid, crc))
}

/// UIDs that appear more than once (first occurrence order).
pub fn duplicate_uids(entries: &[UserEntry]) -> Vec<Uid> {
    let mut seen = HashSet::with_capacity(entries.len());
    let mut dups = Vec::new();
    for e in entries {
        if !seen.insert(&e.uid) && !dups.contains(&e.uid) {
            dups.push(e.uid.clone());
        }
    }
    dups
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::parse_date;

    fn entry(uid: &str, name: &str, reg: Option<&str>, vd: Option<f64>) -> UserEntry {
        let validity = match vd {
            None => Validity::Admin,
            Some(v) => Validity::Expiring { registered: reg.map(|r| parse_date(r).unwrap()), valid_days: v },
        };
        UserEntry { uid: Uid::parse(uid).unwrap(), name: name.into(), validity }
    }

    fn today() -> NaiveDate {
        parse_date("2026-07-01").unwrap()
    }

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    // Legacy golden vectors: these prove byte compatibility with existing
    // .bin files and with the firmware's db_crc32.
    #[test]
    fn golden_records_match_python() {
        let cases = [
            (entry("04A1B2C3", "Alice Martin", Some("2026-01-15"), Some(365.0)), include_str!("../tests/golden/rec_alice.hex")),
            (entry("12345678", "Admin Garage", None, None), include_str!("../tests/golden/rec_admin.hex")),
            (entry("04AABBCCDDEEFF001122", "Zoë Ünïcødé", Some("2030-12-31"), Some(0.5)), include_str!("../tests/golden/rec_unicode.hex")),
        ];
        for (e, want) in cases {
            assert_eq!(hex(&encode_record(&e, today())), want.trim(), "record for {}", e.uid);
        }
    }

    #[test]
    fn golden_canonical_crc_matches_python() {
        let entries = vec![
            entry("0E4F5061", "David Leroy", Some("2026-01-20"), Some(30.0)),
            entry("04A1B2C3", "Alice Martin", Some("2026-01-15"), Some(365.0)),
            entry("12345678", "Admin Garage", None, None),
            entry("0A1B2C3D", "Chloe Dubois", Some("2026-03-10"), Some(90.0)),
            entry("04D4E5F6", "Bruno Petit", None, Some(180.0)), // floating -> today
        ];
        let want = u32::from_str_radix(include_str!("../tests/golden/samples_crc.hex").trim(), 16).unwrap();
        assert_eq!(canonical_crc32_of(&entries, today()), want);
    }

    #[test]
    fn roundtrip() {
        let e = entry("04AABBCCDD", "Bob", Some("2026-02-03"), Some(12.25));
        let rec = encode_record(&e, today());
        assert_eq!(decode_record(&rec).unwrap(), e);
        let a = entry("04AABBCC", "Root", None, None);
        assert_eq!(decode_record(&encode_record(&a, today())).unwrap(), a);
    }

    #[test]
    fn floating_date_is_stamped() {
        let e = entry("04AABBCC", "Bob", None, Some(1.0));
        let d = decode_record(&encode_record(&e, today())).unwrap();
        assert_eq!(d.validity, Validity::Expiring { registered: Some(today()), valid_days: 1.0 });
    }

    #[test]
    fn corruption_detected() {
        let mut rec = encode_record(&entry("04AABBCC", "Bob", Some("2026-02-03"), Some(1.0)), today());
        rec[20] ^= 0x01;
        assert!(decode_record(&rec).unwrap_err().contains("CRC32 mismatch"));
        assert!(decode_record(&rec[..10]).is_err());
    }

    #[test]
    fn crc_is_content_sensitive() {
        // Regression for the "codeword collapse" bug the firmware comments
        // describe: two different DBs with the same record count must not
        // share a db_crc32.
        let a = vec![entry("04AABBCC", "A", None, None), entry("04AABBCD", "B", None, None)];
        let b = vec![entry("04AABBCC", "A", None, None), entry("04AABBCD", "C", None, None)];
        assert_ne!(canonical_crc32_of(&a, today()), canonical_crc32_of(&b, today()));
        // Order-independent (sorted by UID internally).
        let mut a2 = a.clone();
        a2.reverse();
        assert_eq!(canonical_crc32_of(&a, today()), canonical_crc32_of(&a2, today()));
        // Empty DB.
        assert_eq!(canonical_crc32_of(&[], today()), 0);
    }

    #[test]
    fn utf8_truncation_never_splits() {
        let s = format!("{}é", "A".repeat(47));
        assert_eq!(utf8_truncate(&s, 48), "A".repeat(47));
        assert_eq!(utf8_truncate("short", 48), "short");
    }

    #[test]
    fn bin_header_validation() {
        let mut data = header_bytes().to_vec();
        data.extend_from_slice(&encode_record(&entry("04AABBCC", "A", None, None), today()));
        data.extend_from_slice(&[1, 2, 3]);
        let bin = parse_bin(&data).unwrap();
        assert_eq!(bin.payload.len(), RECORD_SIZE);
        assert_eq!(bin.trailing_bytes, 3);
        assert!(parse_bin(b"RUD").is_err());
        assert!(parse_bin(b"XXXX\x01\x4a\x00").is_err());
        assert!(parse_bin(b"RUD1\x02\x4a\x00").is_err());
        assert!(parse_bin(b"RUD1\x01\x40\x00").is_err());
    }

    #[test]
    fn manifest_and_uid_entries() {
        let e = entry("04AABBCCDD", "A", None, None);
        let rec = encode_record(&e, today());
        let mut m = Vec::new();
        m.extend_from_slice(&rec[..1 + UID_BYTES]);
        m.extend_from_slice(&rec[PAYLOAD_SIZE..]);
        let (uid, crc) = decode_manifest_entry(&m).unwrap();
        assert_eq!(uid, e.uid);
        assert_eq!(crc, record_crc(&rec));
        let ue = encode_uid_entry(&e.uid);
        assert_eq!(&ue[..], &rec[..UID_ENTRY_SIZE]);
    }

    #[test]
    fn duplicates() {
        let v = vec![
            entry("04AABBCC", "A", None, None),
            entry("04AABBCD", "B", None, None),
            entry("04AABBCC", "C", None, None),
            entry("04AABBCC", "D", None, None),
        ];
        assert_eq!(duplicate_uids(&v), vec![Uid::parse("04AABBCC").unwrap()]);
    }
}
