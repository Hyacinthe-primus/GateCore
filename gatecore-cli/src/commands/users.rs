//! list / find / add / remove / rename

use std::time::Duration;

use super::{bulk, Ctx};
use crate::cli::BackupFormat;
use crate::database::{self, parse_uid_list, validate_name, Uid, UserEntry, Validity};
use crate::error::{Error, Result};
use crate::protocol::{parse_user_list, DeviceUser, Request, WireUser};
use crate::serial::RequestOpts;
use crate::ui;

/// Fetch every user via the JSON `list` transport (display order).
pub(crate) fn fetch_all(ctx: &mut Ctx<'_>) -> Result<Vec<UserEntry>> {
    let dev = ctx.device()?;
    // The reply is one (possibly multi-MB) line streamed as it's produced;
    // the timeout is an inactivity timeout, so size doesn't matter.
    let resp = dev.request_with(&Request::List, RequestOpts::timeout(Duration::from_secs(5)))?.require_ok()?;
    let (mut users, bad) = parse_user_list(&resp)?;
    if bad > 0 {
        ui::warning(&format!("{bad} malformed entr{} in the device listing were skipped", if bad == 1 { "y" } else { "ies" }));
    }
    database::sort_for_display(&mut users);
    Ok(users)
}

pub fn list(ctx: &mut Ctx<'_>) -> Result<()> {
    let users = fetch_all(ctx)?;
    ui::print_users(&users, "Registered Users");
    Ok(())
}

pub fn find(ctx: &mut Ctx<'_>, uid: Option<&str>, name: Option<&str>) -> Result<()> {
    if let Some(raw) = uid {
        let uid = Uid::parse(raw).map_err(Error::Usage)?;
        let resp = ctx.device()?.request(&Request::Find { uid: uid.to_string() })?;
        if !resp.is_ok() {
            let msg = resp.message().unwrap_or("UID not found");
            return Err(Error::NotFound(format!("{uid}: {msg}")));
        }
        let user: DeviceUser = resp.parse()?;
        ui::print_users(&[user.into_entry()], "Found");
        return Ok(());
    }

    let query = name.unwrap_or_default().trim().to_string();
    if query.is_empty() {
        return Err(Error::usage("--name needs a non-empty search string"));
    }
    // Device-side substring scan: only matches cross the wire.
    let dev = ctx.device()?;
    let resp = dev.request_with(&Request::FindName { query: query.clone() }, RequestOpts::timeout(Duration::from_secs(5)))?;
    let mut matches = if resp.is_ok() {
        parse_user_list(&resp)?.0
    } else if resp.message().is_some_and(|m| m.to_lowercase().contains("unknown command")) {
        // Very old firmware: fall back to a full listing.
        let q = query.to_lowercase();
        fetch_all(ctx)?.into_iter().filter(|u| u.name.to_lowercase().contains(&q)).collect()
    } else {
        return Err(Error::Device(resp.message().unwrap_or("find_name failed").to_string()));
    };
    if matches.is_empty() {
        return Err(Error::NotFound(format!("no user found matching '{query}'")));
    }
    database::sort_for_display(&mut matches);
    ui::print_users(&matches, &format!("Matches for '{query}'"));
    Ok(())
}

fn ask_if_missing(value: Option<String>, label: &str) -> Result<String> {
    match value {
        Some(v) => Ok(v.trim().to_string()),
        None => ui::prompt_line(label),
    }
}

pub fn add(ctx: &mut Ctx<'_>, uid: Option<String>, name: Option<String>, valid_days: Option<f64>) -> Result<()> {
    let uid = Uid::parse(&ask_if_missing(uid, "UID: ")?).map_err(Error::Usage)?;
    let name = ask_if_missing(name, "Name: ")?;
    validate_name(&name).map_err(Error::Usage)?;

    let today = database::today();
    let entry = UserEntry {
        uid: uid.clone(),
        name: name.clone(),
        validity: match valid_days {
            None => Validity::Admin,
            Some(v) => Validity::Expiring { registered: Some(today), valid_days: v },
        },
    };
    // A single add re-writes the file suffix on flash; give it room.
    let resp = ctx
        .device()?
        .request_with(&Request::Add(WireUser::from_entry(&entry, today)), RequestOpts::timeout(Duration::from_secs(10)))?
        .require_ok()?;
    if let Some(note) = resp.str_field("note") {
        ui::info(note);
    }
    match valid_days {
        None => ui::success(&format!("Added ADMIN user '{name}' with UID {uid} (never expires).")),
        Some(v) => ui::success(&format!(
            "Added user '{name}' with UID {uid} (registered {today}, valid {} day(s), expires ~{}).",
            database::format_days(v),
            entry.expiration_label()
        )),
    }
    Ok(())
}

pub fn remove(
    ctx: &mut Ctx<'_>,
    uid: Option<String>,
    force: bool,
    no_backup: bool,
    backup_format: BackupFormat,
    except: Option<&str>,
) -> Result<()> {
    if force {
        let dev = ctx.device()?;
        if no_backup {
            ui::info("Skipping the automatic backup (--no-backup).");
        } else {
            bulk::backup_before_wipe(dev, "remove_force", backup_format)?;
        }
        dev.request_with(&Request::ClearAll, RequestOpts::timeout(Duration::from_secs(30)))?.require_ok()?;
        ui::success("Removed ALL users from the device database.");
        return Ok(());
    }

    if let Some(raw) = except {
        let keep = parse_uid_list(raw).map_err(Error::Usage)?;
        let dev = ctx.device()?;
        // The firmware only checks the *format* of keep-UIDs, so a typo in
        // every one of them would wipe the whole database. Verify at least
        // one exists.
        let mut missing = Vec::new();
        for u in &keep {
            if !dev.request(&Request::Find { uid: u.to_string() })?.is_ok() {
                missing.push(u.to_string());
            }
        }
        if missing.len() == keep.len() {
            return Err(Error::usage(format!(
                "none of the --except UIDs exist on the device ({}) -- refusing, this would delete everyone. \
                 Use 'remove --force' if that is really what you want.",
                missing.join(", ")
            )));
        }
        if !missing.is_empty() {
            ui::warning(&format!("Not on the device (ignored): {}", missing.join(", ")));
        }
        let resp = dev
            .request_with(
                &Request::RemoveAllExcept { uids: keep.iter().map(ToString::to_string).collect() },
                RequestOpts::timeout(Duration::from_secs(30)),
            )?
            .require_ok()?;
        let removed = resp.u64_field("removed_count").map(|n| n.to_string()).unwrap_or_else(|| "?".into());
        let kept: Vec<&str> = keep.iter().map(Uid::as_str).filter(|u| !missing.iter().any(|m| m == u)).collect();
        ui::success(&format!("Removed {removed} user(s), kept: {}.", kept.join(", ")));
        return Ok(());
    }

    let uid = Uid::parse(&ask_if_missing(uid, "UID to remove: ")?).map_err(Error::Usage)?;
    // Removal rewrites the whole on-flash DB (LittleFS cannot truncate), up
    // to 5 MB, hence the long timeout. Retrying is safe: "UID not found" on
    // a retry is reported as success.
    let resp = ctx
        .device()?
        .request_with(&Request::Remove { uid: uid.to_string() }, RequestOpts { timeout: Duration::from_secs(45), attempts: 2 })?;
    if !resp.is_ok() {
        let msg = resp.message().unwrap_or("unknown error");
        return Err(if msg.eq_ignore_ascii_case("uid not found") {
            Error::NotFound(format!("{uid} is not registered"))
        } else {
            Error::Device(msg.to_string())
        });
    }
    if let Some(note) = resp.str_field("note") {
        ui::info(note);
    }
    ui::success(&format!("Removed UID {uid}."));
    Ok(())
}

pub fn rename(ctx: &mut Ctx<'_>, uid: Option<String>, name: Option<String>) -> Result<()> {
    let uid = Uid::parse(&ask_if_missing(uid, "UID to rename: ")?).map_err(Error::Usage)?;
    let name = ask_if_missing(name, "New name: ")?;
    // Apply the same name validation as `add`.
    validate_name(&name).map_err(Error::Usage)?;
    let resp = ctx.device()?.request_with(
        &Request::Rename { uid: uid.to_string(), name: name.clone() },
        RequestOpts { timeout: Duration::from_secs(10), attempts: 2 },
    )?;
    if !resp.is_ok() {
        let msg = resp.message().unwrap_or("unknown error");
        return Err(if msg.eq_ignore_ascii_case("uid not found") {
            Error::NotFound(format!("{uid} is not registered"))
        } else {
            Error::Device(msg.to_string())
        });
    }
    ui::success(&format!("Renamed UID {uid} to '{name}'."));
    Ok(())
}
