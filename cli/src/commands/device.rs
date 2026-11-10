//! status / netstatus / ntp-time / ntp-sync / configure / timezone / list-ports

use std::time::Duration;

use super::Ctx;
use crate::error::{Error, Result};
use crate::protocol::{DeviceStatus, NetStatus, Request};
use crate::serial::{find_esp32_ports, list_ports, RequestOpts};
use crate::ui;

pub fn fetch_status(ctx: &mut Ctx<'_>) -> Result<DeviceStatus> {
    // computeCrc32() walks the whole DB on the device: allow a few seconds.
    ctx.device()?.request_with(&Request::Status, RequestOpts::timeout(Duration::from_secs(5)))?.require_ok()?.parse()
}

pub fn status(ctx: &mut Ctx<'_>) -> Result<()> {
    let s = fetch_status(ctx)?;
    ui::print_status(&s);
    Ok(())
}

pub fn netstatus(ctx: &mut Ctx<'_>) -> Result<()> {
    let s: NetStatus = ctx.device()?.request(&Request::NetStatus)?.require_ok()?.parse()?;
    ui::print_net_status(&s);
    Ok(())
}

pub fn ntp_time(ctx: &mut Ctx<'_>) -> Result<()> {
    let r = ctx.device()?.request(&Request::GetTime)?.require_ok()?;
    let formatted = r.str_field("formatted").unwrap_or("?");
    let epoch = r.u64_field("epoch").unwrap_or(0);
    let utc = chrono::DateTime::from_timestamp(epoch as i64, 0)
        .map(|t| t.format("%Y-%m-%d %H:%M:%S UTC").to_string())
        .unwrap_or_else(|| "?".into());
    ui::success(&format!("Device time: {formatted} (local) = {utc}, epoch {epoch}"));
    let drift = epoch as i64 - chrono::Utc::now().timestamp();
    if drift.abs() > 5 {
        ui::warning(&format!("Device clock differs from this machine by {drift} s."));
    }
    Ok(())
}

pub fn ntp_sync(ctx: &mut Ctx<'_>) -> Result<()> {
    // syncTimeBlocking_() can block up to NTP_SYNC_TIMEOUT_MS (8 s).
    let r = ctx.device()?.request_with(&Request::NtpSync, RequestOpts::timeout(Duration::from_secs(15)))?;
    if r.bool_field("synced") {
        ui::success(&format!("NTP synced: {}", r.message().unwrap_or("")));
        Ok(())
    } else {
        Err(Error::Device(r.message().unwrap_or("NTP sync failed").to_string()))
    }
}

pub fn configure_wifi(ctx: &mut Ctx<'_>, ssid: Option<String>, password: Option<String>) -> Result<()> {
    let ssid = match ssid {
        Some(s) => s,
        None => ui::prompt_line("Wi-Fi SSID: ")?,
    };
    if ssid.trim().is_empty() {
        return Err(Error::usage("SSID cannot be empty"));
    }
    // Prompt without echo; never display the password.
    let password = match password {
        Some(p) => p,
        None => ui::prompt_secret("Wi-Fi password (hidden): ")?,
    };
    ui::info(&format!("Connecting the device to '{ssid}' (up to ~20 s)..."));
    // connect (10 s) + NTP (8 s) + 1 s LCD pause on the device side.
    let r = ctx.device()?.request_with(
        &Request::ConfigureWifi { ssid: ssid.clone(), password },
        RequestOpts::once(Duration::from_secs(30)),
    )?;
    if r.bool_field("connected") {
        ui::success(&format!("Device connected to '{ssid}' and synced its clock. Credentials saved."));
        Ok(())
    } else {
        Err(Error::Device(r.message().unwrap_or("device failed to connect").to_string()))
    }
}

/// `+01:00`, `-04:30`, ...
pub fn format_utc_offset(total_sec: i64) -> String {
    let sign = if total_sec < 0 { '-' } else { '+' };
    let a = total_sec.unsigned_abs();
    format!("UTC{sign}{:02}:{:02}", a / 3600, (a % 3600) / 60)
}

pub fn timezone(ctx: &mut Ctx<'_>, gmt: i64, dst: i64) -> Result<()> {
    let local = chrono::Local::now().offset().local_minus_utc() as i64;
    let r = ctx.device()?.request_with(
        &Request::ConfigureTimezone { gmt_offset_sec: gmt, daylight_offset_sec: dst },
        RequestOpts::timeout(Duration::from_secs(15)),
    )?;
    if !r.bool_field("applied") {
        return Err(Error::Device(r.message().unwrap_or("device failed to apply the timezone").to_string()));
    }
    let g = r.0.get("gmt_offset_sec").and_then(|v| v.as_i64()).unwrap_or(gmt);
    let d = r.0.get("daylight_offset_sec").and_then(|v| v.as_i64()).unwrap_or(dst);
    // Show a fractional offset as "-04:30", not "-5h".
    ui::success(&format!("Timezone set to {} (GMT {g} s + DST {d} s). Persisted.", format_utc_offset(g + d)));
    if g + d != local {
        ui::warning(&format!(
            "This machine is at {} -- registration dates are stamped with this machine's date, \
             so the device should normally use the same offset.",
            format_utc_offset(local)
        ));
    }
    Ok(())
}

pub fn list_ports_cmd() -> Result<()> {
    ui::print_ports(&list_ports(), &find_esp32_ports());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::format_utc_offset;

    #[test]
    fn offsets() {
        assert_eq!(format_utc_offset(3600), "UTC+01:00");
        assert_eq!(format_utc_offset(-16200), "UTC-04:30");
        assert_eq!(format_utc_offset(0), "UTC+00:00");
        assert_eq!(format_utc_offset(20700), "UTC+05:45");
    }
}
