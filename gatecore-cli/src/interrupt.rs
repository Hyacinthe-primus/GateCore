//! Ctrl+C handling as a cooperative cancellation flag.
//!
//! A single process-wide handler raises a flag; every blocking device loop
//! in [`crate::serial`] polls it between short (50 ms) read slices and
//! returns [`crate::Error::Interrupted`], so Ctrl+C is responsive everywhere
//! and the command gets a chance to restore the device (e.g. leave scan
//! mode).
//!
//! A second Ctrl+C before the first is consumed force-quits, so a wedged
//! operation cannot trap the user.
//!
//! Interactive prompts go through rustyline, which puts the terminal in raw
//! mode and receives Ctrl+C as a key press rather than a signal, so this
//! handler never interferes with line editing.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Once;

static INTERRUPTED: AtomicBool = AtomicBool::new(false);
static INSTALL: Once = Once::new();

/// Install the process-wide handler (idempotent).
pub fn install() {
    INSTALL.call_once(|| {
        let _ = ctrlc::set_handler(|| {
            if INTERRUPTED.swap(true, Ordering::SeqCst) {
                // Second press while the first is still pending: bail out.
                eprintln!("\nForce quit.");
                std::process::exit(130);
            }
        });
    });
}

/// Was Ctrl+C pressed since the last [`clear`]?
pub fn is_set() -> bool {
    INTERRUPTED.load(Ordering::SeqCst)
}

/// Reset the flag (called before each command and after handling).
pub fn clear() {
    INTERRUPTED.store(false, Ordering::SeqCst);
}

/// Return `Err(Interrupted)` if the flag is set (and consume it).
pub fn check() -> crate::Result<()> {
    if INTERRUPTED.swap(false, Ordering::SeqCst) {
        Err(crate::Error::Interrupted)
    } else {
        Ok(())
    }
}

/// Raise the flag programmatically (used by tests).
pub fn trigger() {
    INTERRUPTED.store(true, Ordering::SeqCst);
}
