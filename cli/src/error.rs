//! Single error type for the whole crate.
//!
//! Every failure is a typed [`Error`] that bubbles up to exactly one place
//! (the one-shot entry point or the REPL loop), which decides how to render
//! it and whether the link needs a health check ([`Error::is_link_failure`]).

use std::path::PathBuf;

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Invalid user input (bad UID, empty name, conflicting flags, ...).
    #[error("{0}")]
    Usage(String),

    /// The firmware answered with `{"status":"error","message":...}`.
    #[error("device error: {0}")]
    Device(String),

    /// No (valid) answer arrived in time.
    #[error("timed out {0}")]
    Timeout(String),

    /// The byte stream no longer lines up with the protocol (bad JSON,
    /// unexpected reply type, short raw transfer, ...).
    #[error("protocol error: {0}")]
    Protocol(String),

    /// Could not find or open a serial port.
    #[error("{0}")]
    NoDevice(String),

    #[error("serial port error: {0}")]
    Serial(#[from] serialport::Error),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// I/O error tied to a specific file (gives the user the path).
    #[error("{}: {source}", path.display())]
    File {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// A user file (JSON / CSV / .bin) is malformed as a whole.
    #[error("{0}")]
    Format(String),

    /// Ctrl+C while a device operation was in flight.
    #[error("interrupted by user")]
    Interrupted,

    /// A lookup found nothing (unknown UID, no name match, ...).
    #[error("{0}")]
    NotFound(String),

    /// The user declined a confirmation or aborted a prompt.
    #[error("cancelled")]
    Cancelled,
}

impl Error {
    pub fn usage(msg: impl Into<String>) -> Self {
        Error::Usage(msg.into())
    }

    pub fn protocol(msg: impl Into<String>) -> Self {
        Error::Protocol(msg.into())
    }

    pub fn format(msg: impl Into<String>) -> Self {
        Error::Format(msg.into())
    }

    pub fn file(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Error::File { path: path.into(), source }
    }

    /// True when the error may mean the serial link itself is gone (as
    /// opposed to the device politely rejecting a request, or bad input).
    /// The shell only pings/reconnects in that case.
    pub fn is_link_failure(&self) -> bool {
        matches!(
            self,
            Error::Timeout(_) | Error::Protocol(_) | Error::Serial(_) | Error::Io(_) | Error::NoDevice(_)
        )
    }

    /// Process exit code for one-shot mode (130 = conventional SIGINT code).
    pub fn exit_code(&self) -> i32 {
        match self {
            Error::Interrupted | Error::Cancelled => 130,
            Error::Usage(_) => 2,
            _ => 1,
        }
    }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Error::Protocol(e.to_string())
    }
}
