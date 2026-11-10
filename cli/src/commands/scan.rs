//! scan / tag-renew -- the two commands driven by card events.

use std::time::{Duration, Instant};

use super::Ctx;
use crate::cli::Quota;
use crate::database::format_days;
use crate::error::{Error, Result};
use crate::protocol::Request;
use crate::serial::Device;
use crate::{interrupt, ui};

/// Wait for the next `uid_detected` event (other messages are ignored).
fn wait_for_uid(dev: &mut Device, overall: Duration) -> Result<String> {
    let deadline = Instant::now() + overall;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(Error::Timeout("waiting for a card to be presented".into()));
        }
        if let Some(msg) = dev.next_message(left.min(Duration::from_secs(1)))? {
            if msg.kind() == Some("uid_detected") {
                return Ok(msg.str_field("uid").unwrap_or("?").to_string());
            }
        }
    }
}

/// Best-effort: put the device back on its idle screen.
fn leave(dev: &mut Device, req: Request) {
    interrupt::clear();
    if let Err(e) = dev.request(&req).and_then(|r| r.require_ok()) {
        ui::error(&format!("Could not return the device to idle: {e}"));
    }
}

pub fn scan(ctx: &mut Ctx<'_>, timeout: f64, infinite: bool) -> Result<()> {
    if !timeout.is_finite() || timeout <= 0.0 {
        return Err(Error::usage("--timeout must be a positive number of seconds"));
    }
    let dev = ctx.device()?;
    // `continuous` keeps firmware >= 1.2 in scan mode between cards, closing
    // the window where a card presented during re-arming was treated as an
    // access attempt (and could count toward the lockout). Older firmware
    // ignores the key, so we still re-arm after each card.
    dev.request(&Request::EnterScanMode { continuous: infinite })?.require_ok()?;

    if !infinite {
        ui::info("Scan mode active -- present a card to the reader now... (Ctrl+C to cancel)");
        return match wait_for_uid(dev, Duration::from_secs_f64(timeout)) {
            Ok(uid) => {
                ui::success(&format!("Detected UID: {uid}"));
                Ok(()) // firmware leaves scan mode by itself after one card
            }
            Err(Error::Interrupted) => {
                ui::info("Scan cancelled.");
                leave(dev, Request::ExitScanMode);
                Ok(())
            }
            Err(e) => {
                leave(dev, Request::ExitScanMode);
                Err(e)
            }
        };
    }

    ui::info("Continuous scan mode -- present cards to the reader. Press Ctrl+C to stop.");
    let mut count = 0u64;
    let outcome = loop {
        match wait_for_uid(dev, Duration::from_secs(24 * 3600)) {
            Ok(uid) => {
                count += 1;
                ui::success(&format!("#{count}  Detected UID: {uid}"));
                if let Err(e) = dev.request(&Request::EnterScanMode { continuous: true }).and_then(|r| r.require_ok()) {
                    break Err(e);
                }
            }
            Err(Error::Interrupted) => break Ok(()),
            Err(e) => break Err(e),
        }
    };
    ui::info(&format!("Scan stopped. Total cards scanned: {count}"));
    leave(dev, Request::ExitScanMode);
    outcome
}

pub fn tag_renew(ctx: &mut Ctx<'_>, valid_days: f64, quota: Quota) -> Result<()> {
    let dev = ctx.device()?;
    dev.request(&Request::EnterRenewalMode { valid_days })?.require_ok()?;
    ui::info(&format!(
        "Renewal mode active (valid_days={}). Present cards to the reader; Ctrl+C to stop.",
        format_days(valid_days)
    ));

    let mut renewed = 0u32;
    let mut failed = 0u32;
    let outcome: Result<()> = loop {
        if let Quota::Max(max) = quota {
            if renewed >= max {
                ui::info(&format!("Quota reached ({max})."));
                break Ok(());
            }
        }
        let msg = match dev.next_message(Duration::from_secs(1)) {
            Ok(Some(m)) => m,
            Ok(None) => continue,
            Err(Error::Interrupted) => break Ok(()),
            Err(e) => break Err(e),
        };
        let is_renewal = msg.kind() == Some("renewal_result");
        if is_renewal && msg.is_ok() {
            renewed += 1;
            ui::success(&format!(
                "#{renewed}  {} ({}) -> registered={}, valid_days={}",
                msg.str_field("uid").unwrap_or("?"),
                msg.str_field("name").unwrap_or("?"),
                msg.str_field("registered").unwrap_or("?"),
                msg.0.get("valid_days").map(ToString::to_string).unwrap_or_else(|| "?".into()),
            ));
        } else if !msg.is_ok() && (is_renewal || msg.kind().is_none()) {
            // Firmware >= 1.2 tags renewal failures with type renewal_result
            // and the UID; older firmware sends a bare error line.
            failed += 1;
            let who = msg.str_field("uid").map(|u| format!("{u}: ")).unwrap_or_default();
            ui::error(&format!("  {who}{}", msg.message().unwrap_or("renewal failed")));
        }
    };
    ui::info(&format!("Renewal stopped. Renewed: {renewed}, rejected: {failed}."));
    leave(dev, Request::ExitRenewalMode);
    outcome
}
