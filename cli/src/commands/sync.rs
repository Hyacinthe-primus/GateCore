//! sync -- make the device DB exactly match a local file with a merge-diff.
//!
//! 1. `sync_begin`: device `db_crc32`. If the file fully pins every record
//!    (no undated users) and the canonical CRCs match, stop: nothing to do.
//! 2. `sync_manifest`: `(uid, record_crc32)` for every device record.
//! 3. Resolve undated ("floating") file entries -- see below.
//! 4. Diff: remove (device-only), add (file-only), replace (CRC differs).
//! 5. `sync_apply` + raw stream (remove UIDs, add records, replace records).
//! 6. Verify the returned `db_crc32` against the expected canonical CRC.
//!
//! ## Floating registration dates
//!
//! A file entry with `valid_days` but no `registered` date is "floating".
//! Stamping it with today on every run would change its record CRC daily, so
//! repeated syncs would silently extend the badge. An undated entry for an
//! existing device user keeps the device's date; only new users get today's
//! date. `--restamp` opts into stamping every run explicitly.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::{Duration, Instant};

use chrono::NaiveDate;

use super::{bulk, report_issues, Ctx};
use crate::convert::{self, RawRecord, MANIFEST_ENTRY_SIZE};
use crate::database::{self, Uid, UserEntry, Validity};
use crate::error::{Error, Result};
use crate::protocol::{DeviceUser, RawHeader, Request, SyncResult};
use crate::serial::{scaled_timeout, Device, RequestOpts, RAW_STALL_TIMEOUT};
use crate::sync_stats::SyncStats;
use crate::{ui, userfile};

/// Up to this many date lookups use per-UID `find`; beyond, one bulk export.
const FIND_LOOKUP_LIMIT: usize = 64;

/// Last occurrence of each UID wins (keeps first-seen order).
pub fn dedupe_last_wins(entries: Vec<UserEntry>) -> (Vec<UserEntry>, usize) {
    let n = entries.len();
    let mut pos: HashMap<Uid, usize> = HashMap::with_capacity(n);
    let mut out: Vec<UserEntry> = Vec::with_capacity(n);
    for e in entries {
        match pos.get(&e.uid) {
            Some(&i) => out[i] = e,
            None => {
                pos.insert(e.uid.clone(), out.len());
                out.push(e);
            }
        }
    }
    let dups = n - out.len();
    (out, dups)
}

/// Registration date currently stored on the device for each UID
/// (`None` = admin badge or not found).
fn device_dates(dev: &mut Device, uids: &[Uid]) -> Result<HashMap<Uid, Option<NaiveDate>>> {
    let pick = |e: &UserEntry| match e.validity {
        Validity::Expiring { registered, .. } => registered,
        Validity::Admin => None,
    };
    let mut out = HashMap::with_capacity(uids.len());
    if uids.len() <= FIND_LOOKUP_LIMIT {
        for uid in uids {
            let r = dev.request(&Request::Find { uid: uid.to_string() })?;
            let date = if r.is_ok() { r.parse::<DeviceUser>().ok().map(DeviceUser::into_entry).and_then(|e| pick(&e)) } else { None };
            out.insert(uid.clone(), date);
        }
    } else {
        let wanted: HashSet<&Uid> = uids.iter().collect();
        let (raw, _) = bulk::export_raw(dev)?;
        for e in convert::decode_records(&raw).entries {
            if wanted.contains(&e.uid) {
                out.insert(e.uid.clone(), pick(&e));
            }
        }
    }
    Ok(out)
}

fn show_uids(label: &str, uids: &[&Uid]) {
    if uids.is_empty() {
        return;
    }
    const SHOWN: usize = 20;
    let list: Vec<&str> = uids.iter().take(SHOWN).map(|u| u.as_str()).collect();
    let more = if uids.len() > SHOWN { format!(" ... (+{})", uids.len() - SHOWN) } else { String::new() };
    println!("    {label:<8} {}{more}", list.join(", "));
}

pub fn sync(ctx: &mut Ctx<'_>, file: &Path, dry_run: bool, show_stats: bool, restamp: bool) -> Result<()> {
    let loaded = userfile::load(file)?;
    for n in &loaded.notes {
        ui::info(n);
    }
    if !loaded.issues.is_empty() {
        ui::warning(&format!("{} row(s)/record(s) of {} are invalid:", loaded.issues.len(), file.display()));
        report_issues(&loaded.issues);
    }
    let (entries, dups) = dedupe_last_wins(loaded.entries);
    if dups > 0 {
        ui::info(&format!("{dups} duplicate UID(s) in {} -- kept the last occurrence of each.", file.display()));
    }
    if entries.is_empty() {
        return Err(Error::format(
            "no valid entries in the file -- refusing to sync against an empty set \
             (use 'remove --force' to really wipe the device)",
        ));
    }
    if entries.len() > database::MAX_USERS {
        return Err(Error::usage(format!("the file holds {} users; the device stores at most {}", entries.len(), database::MAX_USERS)));
    }

    let today = database::today();
    let mut entries = entries;
    if restamp {
        entries = entries.into_iter().map(|e| e.with_registered_if_floating(today)).collect();
    }
    let floating = entries.iter().filter(|e| e.is_floating()).count();

    let mut stats = SyncStats { dry_run, ..Default::default() };
    let dev = ctx.device()?;
    let total_t0 = Instant::now();

    // 1. CRC fast path.
    let t0 = Instant::now();
    let begin = dev.request_with(&Request::SyncBegin, RequestOpts::timeout(Duration::from_secs(5)))?.expect_kind("sync_begin")?;
    stats.crc_fastpath_s = t0.elapsed().as_secs_f64();
    let device_crc = begin.u64_field("db_crc32").ok_or_else(|| Error::protocol("sync_begin reply lacks db_crc32"))? as u32;
    let device_count = begin.u64_field("count").unwrap_or(0);

    if floating == 0 {
        let local_crc = convert::canonical_crc32_of(&entries, today);
        stats.crc_match = Some(local_crc == device_crc);
        if local_crc == device_crc {
            stats.fast_path = true;
            stats.total_s = total_t0.elapsed().as_secs_f64();
            ui::success(&format!(
                "Device already matches {} exactly ({} user(s), db_crc32=0x{device_crc:08x}) -- nothing to do.",
                file.display(),
                entries.len()
            ));
            if show_stats {
                ui::print_sync_stats(&stats);
            }
            return Ok(());
        }
    }
    ui::info(&format!(
        "Device has {device_count} user(s) (db_crc32=0x{device_crc:08x}); file has {} -- computing the diff...",
        entries.len()
    ));

    // 2. Manifest.
    let t0 = Instant::now();
    let hdr: RawHeader = dev
        .request_with(&Request::SyncManifest, RequestOpts::once(Duration::from_secs(10)))?
        .expect_kind("sync_manifest")?
        .parse()?;
    let mut device_crcs: HashMap<Uid, u32> = HashMap::with_capacity(hdr.count);
    if hdr.bytes > 0 {
        let pb = ui::progress_bytes(hdr.bytes, "manifest");
        let raw = dev.read_raw(hdr.bytes, RAW_STALL_TIMEOUT, Some(&pb));
        pb.finish_and_clear();
        let raw = raw?;
        if hdr.bytes != hdr.count * MANIFEST_ENTRY_SIZE {
            return Err(Error::protocol(format!("manifest announced {} bytes for {} entries", hdr.bytes, hdr.count)));
        }
        for (i, chunk) in raw.chunks_exact(MANIFEST_ENTRY_SIZE).enumerate() {
            let (uid, crc) = convert::decode_manifest_entry(chunk).map_err(|e| Error::protocol(format!("manifest entry {i}: {e}")))?;
            device_crcs.insert(uid, crc);
        }
    }
    stats.manifest_s = t0.elapsed().as_secs_f64();
    stats.manifest_count = hdr.count;
    stats.manifest_bytes = hdr.bytes;

    // 3. Floating dates: keep the device's date for existing users.
    if floating > 0 {
        let t0 = Instant::now();
        let existing: Vec<Uid> =
            entries.iter().filter(|e| e.is_floating() && device_crcs.contains_key(&e.uid)).map(|e| e.uid.clone()).collect();
        if !existing.is_empty() {
            let dates = device_dates(dev, &existing)?;
            for e in entries.iter_mut().filter(|e| e.is_floating()) {
                if let Some(Some(d)) = dates.get(&e.uid) {
                    *e = e.clone().with_registered_if_floating(*d);
                    stats.resolved_dates += 1;
                }
            }
        }
        // Whatever is still undated is new on the device (or was an admin badge): today.
        entries = entries.into_iter().map(|e| e.with_registered_if_floating(today)).collect();
        stats.resolve_s = t0.elapsed().as_secs_f64();
        if stats.resolved_dates > 0 && !show_stats {
            ui::info(&format!(
                "  Kept the device's registration date for {} undated user(s) in the file (use --restamp to reset them to today).",
                stats.resolved_dates
            ));
        }
    }

    // 4. Diff.
    let t0 = Instant::now();
    let local: Vec<(Uid, RawRecord)> = entries.iter().map(|e| (e.uid.clone(), convert::encode_record(e, today))).collect();
    let local_index: HashMap<&Uid, &RawRecord> = local.iter().map(|(u, r)| (u, r)).collect();
    let mut remove: Vec<&Uid> = device_crcs.keys().filter(|u| !local_index.contains_key(u)).collect();
    let mut add: Vec<&Uid> = Vec::new();
    let mut replace: Vec<&Uid> = Vec::new();
    for (uid, rec) in &local {
        match device_crcs.get(uid) {
            None => add.push(uid),
            Some(&crc) if crc != convert::record_crc(rec) => replace.push(uid),
            Some(_) => {}
        }
    }
    remove.sort();
    add.sort();
    replace.sort();
    let expected_crc = convert::canonical_crc32(local.iter().map(|(u, r)| (u, r)));
    stats.diff_s = t0.elapsed().as_secs_f64();
    stats.remove_n = remove.len();
    stats.add_n = add.len();
    stats.replace_n = replace.len();
    stats.unchanged_n = local.len() - add.len() - replace.len();

    if !show_stats {
        ui::info(&format!(
            "  Diff: {} to remove, {} to add, {} to replace, {} unchanged.",
            remove.len(),
            add.len(),
            replace.len(),
            stats.unchanged_n
        ));
    }

    if dry_run {
        show_uids("remove:", &remove);
        show_uids("add:", &add);
        show_uids("replace:", &replace);
        stats.total_s = total_t0.elapsed().as_secs_f64();
        ui::info("Dry run -- no changes made to the device.");
        if show_stats {
            ui::print_sync_stats(&stats);
        }
        return Ok(());
    }

    if remove.is_empty() && add.is_empty() && replace.is_empty() {
        stats.nothing_to_apply = true;
        stats.final_crc_match = expected_crc == device_crc;
        stats.total_s = total_t0.elapsed().as_secs_f64();
        ui::success(&format!("Device already matches {} -- nothing to do.", file.display()));
        if show_stats {
            ui::print_sync_stats(&stats);
        }
        return Ok(());
    }

    // 5. Apply.
    let mut payload = Vec::with_capacity(remove.len() * convert::UID_ENTRY_SIZE + (add.len() + replace.len()) * convert::RECORD_SIZE);
    for u in &remove {
        payload.extend_from_slice(&convert::encode_uid_entry(u));
    }
    for u in add.iter().chain(replace.iter()) {
        payload.extend_from_slice(local_index[u]);
    }
    let budget = scaled_timeout(payload.len());
    dev.request_with(
        &Request::SyncApply { remove: remove.len(), add: add.len(), replace: replace.len() },
        RequestOpts::once(budget),
    )?
    .require_ok()?;
    let t0 = Instant::now();
    let pb = ui::progress_bytes(payload.len(), "sync");
    let w = dev.write_raw(&payload, RAW_STALL_TIMEOUT, Some(&pb));
    pb.finish_and_clear();
    w?;
    stats.transfer_s = t0.elapsed().as_secs_f64();
    stats.transfer_bytes = payload.len();
    if !show_stats {
        ui::info(&format!("  Sync transfer: {:.2}s ({})", stats.transfer_s, ui::rate(payload.len(), stats.transfer_s)));
    }

    // The device applies, then rewrites the whole DB on flash (verify by
    // re-read included) before answering: give it a generous silence budget.
    let result_resp = dev.recv(budget.max(Duration::from_secs(60)))?;
    if result_resp.kind() != Some("sync_result") {
        if !result_resp.is_ok() {
            // e.g. "sync_apply: transfer stalled/incomplete" (device reloads from flash).
            return Err(Error::Device(result_resp.message().unwrap_or("sync_apply failed").to_string()));
        }
        return Err(Error::protocol(format!("expected 'sync_result', got '{}'", result_resp.kind().unwrap_or("(untyped)"))));
    }
    let failed_persist = !result_resp.is_ok();
    let result: SyncResult = result_resp.parse()?;
    stats.apply_remove_ms = result.remove_phase_ms;
    stats.apply_add_ms = result.add_phase_ms;
    stats.apply_replace_ms = result.replace_phase_ms;
    stats.save_encode_ms = result.save_encode_ms;
    stats.save_write_ms = result.save_write_ms;
    stats.save_verify_ms = result.save_verify_ms;
    stats.save_finalize_ms = result.save_finalize_ms;
    stats.save_total_ms = result.save_ms;
    if failed_persist {
        return Err(Error::Device(format!(
            "{} -- the device reverted to its previous database",
            result_resp.message().unwrap_or("sync failed")
        )));
    }
    if result.errors > 0 {
        ui::warning(&format!("{} operation(s) failed on the device; see the checksum verdict below.", result.errors));
    }

    // 6. Verify.
    stats.final_crc_match = result.db_crc32 == expected_crc;
    stats.total_s = total_t0.elapsed().as_secs_f64();
    if show_stats {
        ui::print_sync_stats(&stats);
    }
    if stats.final_crc_match {
        ui::success(&format!(
            "Sync complete and verified: {} removed, {} added, {} replaced (db_crc32=0x{:08x}).",
            result.removed, result.added, result.replaced, result.db_crc32
        ));
        Ok(())
    } else {
        Err(Error::Device(format!(
            "sync finished but db_crc32 MISMATCH: device=0x{:08x}, expected=0x{expected_crc:08x}. Re-run 'sync' to retry.",
            result.db_crc32
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dedupe_keeps_last_in_first_position() {
        let e = |uid: &str, name: &str| UserEntry { uid: Uid::parse(uid).unwrap(), name: name.into(), validity: Validity::Admin };
        let (v, d) = dedupe_last_wins(vec![e("04AABBCC", "a"), e("04AABBCD", "b"), e("04AABBCC", "c")]);
        assert_eq!(d, 1);
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].name, "c");
        assert_eq!(v[1].name, "b");
    }
}
