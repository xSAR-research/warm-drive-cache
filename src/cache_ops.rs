//! Cache directory maintenance: size reporting and content deletion.
//!
//! Operates only on rclone `--cache-dir` paths — never on sync/mount trees.

use std::fs;
use std::path::Path;

/// Compute on-disk size in bytes of a directory tree using `std::fs` only.
pub fn dir_size(path: &Path) -> u64 {
    let mut size = 0u64;
    if let Ok(entries) = fs::read_dir(path) {
        for entry in entries.flatten() {
            let p = entry.path();
            // The application owns this file until all cache work and reporting have finished.
            if p.file_name()
                .is_some_and(|name| name == crate::cache_lock::LOCK_NAME)
            {
                continue;
            }
            if let Ok(meta) = fs::symlink_metadata(&p) {
                if meta.is_file() {
                    size += meta.len();
                } else if meta.is_dir() {
                    size += dir_size(&p);
                }
            }
        }
    }
    size
}

/// Format a non-negative byte count for screen display (IEC binary units).
///
/// Single shared formatter for cache sizes, walk limits, and reports:
/// - `< 1024` → `N Bytes`
/// - otherwise → `X[unit] (N Bytes)` with unit `KiB` / `MiB` / `GiB` / `TiB` / `PiB`
///
/// Whole multiples print without a fractional part (`64KiB (65536 Bytes)`);
/// other values use two decimal places (`482.49KiB (494072 Bytes)`).
pub fn format_bytes(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const UNITS: &[&str] = &["KiB", "MiB", "GiB", "TiB", "PiB"];

    if bytes < KIB {
        return format!("{bytes} Bytes");
    }

    let mut value = bytes as f64;
    let mut unit_idx = 0usize;
    value /= KIB as f64;
    while value >= KIB as f64 && unit_idx + 1 < UNITS.len() {
        value /= KIB as f64;
        unit_idx += 1;
    }
    let unit = UNITS[unit_idx];
    if (value - value.round()).abs() < 1e-9 {
        format!("{:.0}{unit} ({bytes} Bytes)", value.round())
    } else {
        format!("{value:.2}{unit} ({bytes} Bytes)")
    }
}

/// Human description of `walk.max_file_size_bytes` special cases and limits.
///
/// - `-1` — metadata only (no File contents read)
/// - `0` — File contents read for every file
/// - `N > 0` — File contents read when size ≤ N (and ≥ min when set)
pub fn format_max_file_size_limit(max: i64) -> String {
    match max {
        -1 => "metadata only (−1: no File contents read)".into(),
        0 => "all files (0: File contents read for every size)".into(),
        n if n > 0 => format_bytes(n as u64),
        n => format!("invalid ({n})"),
    }
}

/// Delete all files and subdirectories inside `path` (keep the dir itself).
/// If `dry_run`, only print what would be deleted and return estimated size.
/// Returns bytes deleted (or would-be-deleted).
pub fn delete_dir_contents(path: &Path, dry_run: bool) -> u64 {
    let mut deleted = 0u64;
    if let Ok(entries) = fs::read_dir(path) {
        for entry in entries.flatten() {
            let p = entry.path();
            if p.file_name()
                .is_some_and(|name| name == crate::cache_lock::LOCK_NAME)
            {
                continue;
            }
            if let Ok(meta) = fs::symlink_metadata(&p) {
                let sz = if meta.is_file() { meta.len() } else { 0 };
                if dry_run {
                    println!("   Would delete: {}", p.display());
                } else if meta.is_dir() {
                    deleted += delete_dir_contents(&p, dry_run);
                } else if let Err(e) = fs::remove_file(&p) {
                    eprintln!("   Warning: failed to remove file {}: {}", p.display(), e);
                }
                deleted += sz;
            }
        }
    }
    deleted
}

#[cfg(test)]
mod tests {
    use super::{format_bytes, format_max_file_size_limit};

    #[test]
    fn format_bytes_under_1_kib_is_bytes_only() {
        assert_eq!(format_bytes(0), "0 Bytes");
        assert_eq!(format_bytes(1), "1 Bytes");
        assert_eq!(format_bytes(1023), "1023 Bytes");
    }

    #[test]
    fn format_bytes_exact_kib() {
        // 65536 = 64 * 1024
        assert_eq!(format_bytes(65_536), "64KiB (65536 Bytes)");
        assert_eq!(format_bytes(1024), "1KiB (1024 Bytes)");
    }

    #[test]
    fn format_bytes_fractional_kib_and_mib() {
        assert_eq!(format_bytes(494_072), "482.49KiB (494072 Bytes)");
        assert_eq!(format_bytes(2_724_922), "2.60MiB (2724922 Bytes)");
        assert_eq!(format_bytes(1024 * 1024), "1MiB (1048576 Bytes)");
    }

    #[test]
    fn format_bytes_gib_and_tib() {
        let gib = 1024u64 * 1024 * 1024;
        assert_eq!(format_bytes(gib), "1GiB (1073741824 Bytes)");
        let tib = gib * 1024;
        assert_eq!(format_bytes(tib), "1TiB (1099511627776 Bytes)");
    }

    #[test]
    fn format_max_file_size_specials() {
        assert!(format_max_file_size_limit(-1).contains("metadata only"));
        assert!(format_max_file_size_limit(0).contains("all files"));
        assert_eq!(format_max_file_size_limit(65_536), "64KiB (65536 Bytes)");
    }
}
