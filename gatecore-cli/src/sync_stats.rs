//! Timing / throughput data for one `sync` run.
//!
//! Always populated (the `Instant` reads are negligible) but only rendered
//! with `sync --stats` -- see [`crate::ui::print_sync_stats`].

#[derive(Clone, Debug, Default)]
pub struct SyncStats {
    /// Device already matched (CRC fast path) -- most stages skipped.
    pub fast_path: bool,
    pub dry_run: bool,
    /// Diff came out empty, nothing was sent.
    pub nothing_to_apply: bool,

    /// `sync_begin` round trip.
    pub crc_fastpath_s: f64,
    /// `None` when the fast path can't apply (undated users in the file).
    pub crc_match: Option<bool>,

    pub manifest_s: f64,
    pub manifest_count: usize,
    pub manifest_bytes: usize,

    /// Looking up the device's registration dates for undated file entries.
    pub resolve_s: f64,
    pub resolved_dates: usize,

    pub diff_s: f64,
    pub remove_n: usize,
    pub add_n: usize,
    pub replace_n: usize,
    pub unchanged_n: usize,

    pub transfer_s: f64,
    pub transfer_bytes: usize,

    /// Device-side profiling (from `sync_result`), milliseconds.
    pub apply_remove_ms: u64,
    pub apply_add_ms: u64,
    pub apply_replace_ms: u64,
    pub save_encode_ms: u64,
    pub save_write_ms: u64,
    pub save_verify_ms: u64,
    pub save_finalize_ms: u64,
    pub save_total_ms: u64,

    pub final_crc_match: bool,
    pub total_s: f64,
}
