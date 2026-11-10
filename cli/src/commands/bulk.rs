//! import / export / convert, plus the automatic pre-wipe backup.

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::{report_issues, users, Ctx};
use crate::cli::BackupFormat;
use crate::convert::{self, RECORD_SIZE};
use crate::database::{self, UserEntry, MAX_USERS};
use crate::error::{Error, Result};
use crate::protocol::{BatchAddResult, RawHeader, Request, WireUser};
use crate::serial::{scaled_timeout, Device, RequestOpts, RAW_STALL_TIMEOUT};
use crate::userfile::{self, FileKind};
use crate::ui;

/// Users per `batch_add` (legacy JSON transport). Sized against the
/// firmware's 16 KB line buffer with ~2.5x headroom.
const IMPORT_BATCH_SIZE: usize = 100;

pub struct ImportOptions {
    pub dry_run: bool,
    pub clear: bool,
    pub no_backup: bool,
    pub backup_format: BackupFormat,
    pub json_transport: bool,
}

/// Raw `export_bin`: returns `(records, count)` exactly as stored on flash.
pub fn export_raw(dev: &mut Device) -> Result<(Vec<u8>, usize)> {
    let hdr: RawHeader = dev
        .request_with(&Request::ExportBin, RequestOpts::once(Duration::from_secs(10)))?
        .expect_kind("export_bin")?
        .parse()?;
    if hdr.bytes == 0 {
        return Ok((Vec::new(), 0));
    }
    let t0 = Instant::now();
    let pb = ui::progress_bytes(hdr.bytes, "export");
    let raw = dev.read_raw(hdr.bytes, RAW_STALL_TIMEOUT, Some(&pb));
    pb.finish_and_clear();
    let raw = raw?;
    if hdr.bytes != hdr.count * RECORD_SIZE {
        return Err(Error::protocol(format!(
            "export_bin announced {} bytes for {} records (record size {RECORD_SIZE}) -- firmware/CLI record layout mismatch",
            hdr.bytes, hdr.count
        )));
    }
    let secs = t0.elapsed().as_secs_f64();
    ui::info(&format!("  Binary transfer: {} in {secs:.2}s ({})", ui::human_bytes(hdr.bytes as u64), ui::rate(hdr.bytes, secs)));
    Ok((raw, hdr.count))
}

/// Back up the device before a destructive operation.
///
/// A failed backup **aborts** the wipe: proceeding without a copy would
/// defeat the point of taking one. `--no-backup` is the explicit way to wipe
/// without a backup.
pub fn backup_before_wipe(dev: &mut Device, reason: &str, format: BackupFormat) -> Result<Option<PathBuf>> {
    ui::info("Taking an automatic backup first (use --no-backup to skip)...");
    let (raw, count) = export_raw(dev).map_err(|e| {
        Error::Device(format!("automatic backup failed ({e}) -- nothing was deleted. Re-run with --no-backup to wipe anyway."))
    })?;
    if count == 0 {
        ui::info("Device is already empty -- no backup needed.");
        return Ok(None);
    }
    let as_json = match format {
        BackupFormat::Json => true,
        BackupFormat::Bin => false,
        BackupFormat::Ask if std::io::stdin().is_terminal() => ui::confirm(
            &format!("Save the backup of {count} user(s) as readable JSON? (No = raw .bin, re-importable byte-for-byte)"),
            false,
        )?,
        BackupFormat::Ask => false,
    };
    let stamp = chrono::Local::now().format("%Y%m%d_%H%M%S");
    let path = PathBuf::from("backups").join(format!("pre_{reason}_{stamp}.{}", if as_json { "json" } else { "bin" }));
    if as_json {
        let decoded = convert::decode_records(&raw);
        for (i, why) in &decoded.corrupt {
            ui::warning(&format!("  backup: skipping corrupt record {i}: {why}"));
        }
        userfile::write_atomic(&path, userfile::to_json_string(&decoded.entries).as_bytes())?;
        ui::info(&format!("Backup: {} user(s) saved to {}", decoded.entries.len(), path.display()));
    } else {
        userfile::write_bin(&path, &raw)?;
        ui::info(&format!(
            "Backup: {count} user(s) saved to {} (re-import it directly, or 'rfid-cli convert {} out.json')",
            path.display(),
            path.display()
        ));
    }
    Ok(Some(path))
}

pub fn import(ctx: &mut Ctx<'_>, file: &Path, opts: &ImportOptions) -> Result<()> {
    let loaded = userfile::load(file)?;
    for n in &loaded.notes {
        ui::info(n);
    }
    report_issues(&loaded.issues);
    if loaded.entries.is_empty() {
        return Err(Error::format(format!("no valid entries found in {}", file.display())));
    }
    let dups = convert::duplicate_uids(&loaded.entries);
    if !dups.is_empty() {
        ui::warning(&format!(
            "{} UID(s) appear more than once in the file; the device keeps the first and reports the rest as duplicates.",
            dups.len()
        ));
    }
    let n = loaded.entries.len();
    ui::info(&format!("Parsed {n} user(s) ({} row(s) skipped).", loaded.issues.len()));
    if n > MAX_USERS {
        // Checked before anything touches the device (and before --clear wipes it).
        return Err(Error::usage(format!("the file holds {n} users; the device stores at most {MAX_USERS}")));
    }

    if opts.dry_run {
        const SHOWN: usize = 50;
        let mut preview: Vec<UserEntry> = loaded.entries.iter().take(SHOWN).cloned().collect();
        database::sort_for_display(&mut preview);
        ui::print_users(&preview, "Would import");
        if n > SHOWN {
            ui::info(&format!("(showing the first {SHOWN} of {n} users)"));
        }
        ui::info("Dry run -- the device was not contacted.");
        return Ok(());
    }

    let today = database::today();
    let dev = ctx.device()?;

    if opts.clear {
        if opts.no_backup {
            ui::info("Skipping the automatic backup (--no-backup).");
        } else {
            backup_before_wipe(dev, "import_clear", opts.backup_format)?;
        }
        ui::info("Clearing existing users...");
        dev.request_with(&Request::ClearAll, RequestOpts::timeout(Duration::from_secs(30)))?.require_ok()?;
        ui::success("All users removed.");
    } else {
        let status = dev.request_with(&Request::Status, RequestOpts::timeout(Duration::from_secs(5)))?.require_ok()?;
        let current = status.u64_field("user_count").unwrap_or(0) as usize;
        if current + n > MAX_USERS {
            return Err(Error::usage(format!(
                "device has {current} users; importing {n} more would exceed the {MAX_USERS} limit. \
                 Use --clear to wipe first, or trim the file."
            )));
        }
        if let Some(device_crc) = status.u64_field("db_crc32") {
            if dups.is_empty() && convert::canonical_crc32_of(&loaded.entries, today) as u64 == device_crc {
                ui::success(&format!(
                    "Device already holds exactly these {n} user(s) (db_crc32=0x{device_crc:08x}) -- nothing to import."
                ));
                return Ok(());
            }
        }
    }

    ui::info("Starting batch import...");
    let start = Instant::now();
    dev.request(&Request::ImportBegin)?.require_ok()?;

    // From here on the device is in import mode with its reader paused; any
    // failure must release it.
    let transfer = if opts.json_transport {
        send_json_batches(dev, &loaded.entries, today)
    } else {
        send_binary(dev, loaded.raw_bin.as_deref(), &loaded.entries, today)
    };
    let (added, errors) = match transfer {
        Ok(v) => v,
        Err(e) => {
            abort_import(dev);
            return Err(e);
        }
    };

    // import_end performs one blocking full-DB flash write.
    let end_timeout = Duration::from_secs_f64((10.0 + n as f64 * 0.01).max(15.0));
    let resp = match dev.request_with(&Request::ImportEnd, RequestOpts::once(end_timeout)).and_then(|r| r.require_ok()) {
        Ok(r) => r,
        Err(e) => {
            abort_import(dev);
            return Err(e);
        }
    };
    let total = resp.u64_field("added").map(|v| v.to_string()).unwrap_or_else(|| "?".into());
    ui::success(&format!(
        "Import complete: {added} added, {errors} rejected, {total} user(s) now on the device ({:.1}s).",
        start.elapsed().as_secs_f64()
    ));
    if resp.has("save_ms") {
        let f = |k: &str| resp.u64_field(k).unwrap_or(0);
        ui::info(&format!(
            "  Device profile: json_parse={}ms batch_loop={}ms save={}ms (encode={}ms write={}ms verify={}ms finalize={}ms)",
            f("json_parse_ms"),
            f("batch_loop_ms"),
            f("save_ms"),
            f("save_encode_ms"),
            f("save_write_ms"),
            f("save_verify_ms"),
            f("save_finalize_ms")
        ));
    }
    if errors > 0 {
        ui::warning(&format!("{errors} user(s) were rejected by the device (duplicates or invalid records)."));
    }
    Ok(())
}

/// Release import mode after a failure (firmware >= 1.2; older firmware
/// answers "Unknown command type" and needs a reboot).
fn abort_import(dev: &mut Device) {
    crate::interrupt::clear();
    match dev.request_with(&Request::ImportAbort, RequestOpts::timeout(Duration::from_secs(10))) {
        Ok(r) if r.is_ok() => ui::warning("Import aborted -- the device reloaded its previous database from flash."),
        _ => ui::warning(
            "Could not abort the import on the device (firmware < 1.2?). It reverts by itself after 2 minutes \
             of inactivity on firmware >= 1.2; otherwise reboot it to discard the partial import.",
        ),
    }
}

/// Binary transport: one `import_bin` header, then the raw records.
fn send_binary(dev: &mut Device, raw: Option<&[u8]>, entries: &[UserEntry], today: chrono::NaiveDate) -> Result<(u64, u64)> {
    let encoded;
    let payload: &[u8] = match raw {
        Some(r) => {
            ui::info(&format!("  Sending {} straight from the .bin file (no re-encoding)...", ui::human_bytes(r.len() as u64)));
            r
        }
        None => {
            let mut buf = Vec::with_capacity(entries.len() * RECORD_SIZE);
            for e in entries {
                buf.extend_from_slice(&convert::encode_record(e, today));
            }
            encoded = buf;
            ui::info(&format!("  Encoded {} user(s) into {}...", entries.len(), ui::human_bytes(encoded.len() as u64)));
            &encoded
        }
    };
    if payload.is_empty() {
        return Ok((0, 0));
    }
    let budget = scaled_timeout(payload.len());
    dev.request_with(&Request::ImportBin { bytes: payload.len() }, RequestOpts::once(budget))?.require_ok()?;
    let t0 = Instant::now();
    let pb = ui::progress_bytes(payload.len(), "import");
    let w = dev.write_raw(payload, RAW_STALL_TIMEOUT, Some(&pb));
    pb.finish_and_clear();
    w?;
    let secs = t0.elapsed().as_secs_f64();
    let resp = dev.recv(budget)?.expect_kind("import_bin_result")?;
    ui::info(&format!("  Binary transfer: {secs:.2}s ({})", ui::rate(payload.len(), secs)));
    Ok((resp.u64_field("added").unwrap_or(0), resp.u64_field("errors").unwrap_or(0)))
}

/// Legacy JSON transport: `batch_add` of 100 users, one round trip each.
/// A timed-out batch is resent once; on that resend, "Duplicate UID"
/// failures are the first attempt's own users and count as added.
fn send_json_batches(dev: &mut Device, entries: &[UserEntry], today: chrono::NaiveDate) -> Result<(u64, u64)> {
    let (mut added, mut errors) = (0u64, 0u64);
    let total = entries.len();
    let mut done = 0usize;
    let mut last_report = Instant::now();
    for (bi, chunk) in entries.chunks(IMPORT_BATCH_SIZE).enumerate() {
        let req = Request::BatchAdd { users: chunk.iter().map(|e| WireUser::from_entry(e, today)).collect() };
        let opts = RequestOpts::once(Duration::from_secs(10));
        let (res, retried) = match dev.request_with(&req, opts) {
            Ok(r) => (r, false),
            Err(Error::Timeout(_)) => (dev.request_with(&req, opts)?, true),
            Err(e) => return Err(e),
        };
        let mut r: BatchAddResult = res.require_ok()?.parse()?;
        if retried {
            let before = r.failed.len();
            r.failed.retain(|f| !f.message.eq_ignore_ascii_case("duplicate uid"));
            let reclaimed = (before - r.failed.len()) as u64;
            r.added += reclaimed;
            r.errors = r.errors.saturating_sub(reclaimed);
        }
        for f in &r.failed {
            ui::error(&format!("  rejected {}: {}", f.uid, f.message));
        }
        added += r.added;
        errors += r.errors;
        done += chunk.len();
        if last_report.elapsed() >= Duration::from_secs(2) || done == total {
            ui::info(&format!("  ...{done}/{total} sent (batch {})", bi + 1));
            last_report = Instant::now();
        }
    }
    Ok((added, errors))
}

pub fn export(ctx: &mut Ctx<'_>, file: &Path, json_transport: bool) -> Result<()> {
    let kind = FileKind::from_path(file);
    let today = database::today();
    let count = if json_transport {
        let mut entries = users::fetch_all(ctx)?;
        entries.sort_by(|a, b| a.uid.cmp(&b.uid));
        userfile::save(file, &entries, today)?;
        entries.len()
    } else {
        let (raw, count) = export_raw(ctx.device()?)?;
        if kind == FileKind::Bin {
            userfile::write_bin(file, &raw)?; // byte-exact copy of the device DB
            count
        } else {
            let decoded = convert::decode_records(&raw);
            for (i, why) in &decoded.corrupt {
                ui::warning(&format!("  skipping corrupt record {i}: {why}"));
            }
            userfile::save(file, &decoded.entries, today)?;
            decoded.entries.len()
        }
    };
    ui::success(&format!("Exported {count} user(s) to {}.", file.display()));
    Ok(())
}

pub fn convert(src: &Path, dst: &Path) -> Result<()> {
    let report = userfile::convert_file(src, dst, database::today())?;
    for n in &report.notes {
        ui::info(n);
    }
    report_issues(&report.skipped);
    ui::success(&format!("Wrote {} user(s) to {}.", report.written, dst.display()));
    Ok(())
}
