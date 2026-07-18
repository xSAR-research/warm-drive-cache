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

/// Format size in suitable units (MiB preferred when >= 1 MiB).
pub fn format_bytes(bytes: u64) -> String {
    let mib = bytes as f64 / (1024.0 * 1024.0);
    if mib >= 1.0 {
        format!("{:.2} MiB ({} bytes)", mib, bytes)
    } else {
        format!("{} bytes", bytes)
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
