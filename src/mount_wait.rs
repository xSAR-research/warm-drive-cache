//! Optional FUSE mount settle helpers (wait for listings to populate).
//!
//! Retained for config-driven use / tests; current main path may omit an explicit wait.

use crate::config::MountWait;
use std::fs;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

/// Pause after the path exists, before we check for real mount content.
#[allow(dead_code)]
pub const INITIAL_WAIT_SECS: u64 = 3;

/// Retry delays when the directory still looks empty (mount not populated yet).
#[allow(dead_code)]
pub const RETRY_DELAYS_SECS: &[u64] = &[3, 5, 8];

/// Hard ceiling on total wait time per path — never block longer than this.
#[allow(dead_code)]
pub const MAX_WAIT_SECS: u64 = 30;

/// Returns true if the directory has at least one entry (not just an empty mount point).
#[allow(dead_code)]
pub fn directory_has_content(path: &Path) -> bool {
    match fs::read_dir(path) {
        Ok(mut entries) => entries.next().is_some(),
        Err(_) => false,
    }
}

/// Sleep up to `requested` seconds, but never beyond `MAX_WAIT_SECS` total elapsed.
#[allow(dead_code)]
pub fn sleep_capped(start: &Instant, requested: u64) -> bool {
    let elapsed = start.elapsed().as_secs();
    if elapsed >= MAX_WAIT_SECS {
        return false;
    }
    let remaining = MAX_WAIT_SECS - elapsed;
    let actual = requested.min(remaining);
    thread::sleep(Duration::from_secs(actual));
    true
}

/// Budget-aware sleep used by `wait_for_mount_content`.
#[allow(dead_code)]
pub fn sleep_capped_with_budget(start: &Instant, requested: u64, max_wait: u64) -> bool {
    let elapsed = start.elapsed().as_secs();
    if elapsed >= max_wait {
        return false;
    }
    let remaining = max_wait - elapsed;
    let actual = requested.min(remaining);
    thread::sleep(Duration::from_secs(actual));
    true
}

/// Wait for an rclone FUSE mount to populate before walking.
///
/// When `verbose` is false, progress lines are suppressed; warnings still print
/// if the wait times out or the directory stays empty.
pub fn wait_for_mount_content(path: &Path, wait: &MountWait, verbose: bool) -> bool {
    let start = Instant::now();

    if verbose {
        println!(
            "   ⏳ Path exists — waiting {}s for mount to settle (max {}s total)...",
            wait.initial_secs, wait.max_wait_secs
        );
    }
    if !sleep_capped_with_budget(&start, wait.initial_secs, wait.max_wait_secs) {
        eprintln!(
            "   ⚠️  Max wait timeout ({}s) reached — proceeding anyway.",
            wait.max_wait_secs
        );
        return directory_has_content(path);
    }

    if directory_has_content(path) {
        if verbose {
            println!("   ✓ Directory has content, starting walk.");
        }
        return true;
    }

    if verbose {
        println!("   ⏳ Directory looks empty (mount may still be populating)...");
    }

    for (attempt, &delay) in wait.retry_delays_secs.iter().enumerate() {
        if start.elapsed().as_secs() >= wait.max_wait_secs {
            eprintln!(
                "   ⚠️  Max wait timeout ({}s) reached — proceeding anyway.",
                wait.max_wait_secs
            );
            return directory_has_content(path);
        }

        if verbose {
            println!(
                "   ⏳ Retry {}/{}: waiting {}s...",
                attempt + 1,
                wait.retry_delays_secs.len(),
                delay
            );
        }
        if !sleep_capped_with_budget(&start, delay, wait.max_wait_secs) {
            eprintln!(
                "   ⚠️  Max wait timeout ({}s) reached — proceeding anyway.",
                wait.max_wait_secs
            );
            return directory_has_content(path);
        }

        if directory_has_content(path) {
            if verbose {
                println!("   ✓ Directory now has content, starting walk.");
            }
            return true;
        }
    }

    eprintln!("   ⚠️  Still empty after retries — proceeding anyway (may be legitimately empty).");
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::io::Write;
    use std::time::{Duration, Instant};
    use tempfile::TempDir;

    #[test]
    fn directory_has_content_empty_dir_false() {
        let tmp = TempDir::new().expect("tempdir");
        assert!(!directory_has_content(tmp.path()));
    }

    #[test]
    fn directory_has_content_populated_dir_true() {
        let tmp = TempDir::new().expect("tempdir");
        File::create(tmp.path().join("file.txt"))
            .unwrap()
            .write_all(b"hi")
            .unwrap();
        assert!(directory_has_content(tmp.path()));
    }

    #[test]
    fn directory_has_content_error_cases_false() {
        assert!(!directory_has_content(Path::new(
            "/definitely/not/a/real/path/abc123xyz"
        )));
        let tmp = TempDir::new().expect("tempdir");
        let file_path = tmp.path().join("notadir");
        File::create(&file_path).unwrap();
        assert!(!directory_has_content(&file_path));
    }

    #[test]
    fn sleep_capped_budget_exhausted_immediate_false() {
        let past = Instant::now() - Duration::from_secs(MAX_WAIT_SECS + 5);
        let start = Instant::now();
        let result = sleep_capped(&past, 10);
        assert!(!result);
        assert!(start.elapsed() < Duration::from_millis(50));
    }

    #[test]
    fn sleep_capped_within_budget_zero_request_true() {
        let start = Instant::now();
        assert!(sleep_capped(&start, 0));
    }
}
