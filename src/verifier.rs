//! Complete streaming and stable local-cache verification.
use blake3::Hasher;
use std::{
    fs,
    io::{self, Read},
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::{Duration, Instant, SystemTime},
};
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerifyOutcome {
    Verified,
    ChecksumDisabled,
    AttributesOnly,
    CacheFileTimeout,
    SourceChanged,
    DestinationChanged,
    SizeMismatch,
    ChecksumMismatch,
    RateLimited,
    IoError(String),
    Cancelled,
}
#[derive(Debug, Clone, Copy)]
pub struct WaitOptions {
    pub interval: Duration,
    pub timeout: Duration,
    pub stable_observations: u8,
}
impl Default for WaitOptions {
    fn default() -> Self {
        Self {
            interval: Duration::from_millis(500),
            timeout: Duration::from_secs(30),
            stable_observations: 2,
        }
    }
}
fn signature(m: &fs::Metadata) -> (u64, Option<SystemTime>) {
    (m.len(), m.modified().ok())
}
fn digest(path: &Path, cancel: &AtomicBool) -> io::Result<(u64, blake3::Hash)> {
    let mut f = fs::File::open(path)?;
    let mut h = Hasher::new();
    let mut n = 0u64;
    let mut b = [0u8; 128 * 1024];
    loop {
        if cancel.load(Ordering::SeqCst) {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
        }
        let got = f.read(&mut b)?;
        if got == 0 {
            break;
        }
        h.update(&b[..got]);
        n = n
            .checked_add(got as u64)
            .ok_or_else(|| io::Error::other("byte count overflow"))?;
    }
    Ok((n, h.finalize()))
}
pub fn verify(
    source: &Path,
    dest: &Path,
    checksum: bool,
    content: bool,
    wait: WaitOptions,
    cancel: &AtomicBool,
) -> VerifyOutcome {
    if !content {
        return VerifyOutcome::AttributesOnly;
    }
    let before = match fs::metadata(source) {
        Ok(m) => m,
        Err(e) => {
            return VerifyOutcome::IoError(format!("mount metadata {}: {e}", source.display()));
        }
    };
    let (read, src_hash) = match digest(source, cancel) {
        Ok(v) => v,
        Err(e) if e.kind() == io::ErrorKind::Interrupted => return VerifyOutcome::Cancelled,
        Err(e) => return VerifyOutcome::IoError(format!("mount read {}: {e}", source.display())),
    };
    let after = match fs::metadata(source) {
        Ok(m) => m,
        Err(e) => {
            return VerifyOutcome::IoError(format!("mount metadata {}: {e}", source.display()));
        }
    };
    if before.len() != read {
        return VerifyOutcome::SizeMismatch;
    }
    if signature(&before) != signature(&after) {
        return VerifyOutcome::SourceChanged;
    }
    let start = Instant::now();
    let mut last = None;
    let mut stable = 0;
    let dm = loop {
        if cancel.load(Ordering::SeqCst) {
            return VerifyOutcome::Cancelled;
        }
        match fs::metadata(dest) {
            Ok(m) if m.is_file() && m.len() == before.len() => {
                let s = signature(&m);
                if Some(s) == last {
                    stable += 1
                } else {
                    last = Some(s);
                    stable = 1
                }
                if stable >= wait.stable_observations.max(1) {
                    break m;
                }
            }
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => {
                return VerifyOutcome::IoError(format!("cache metadata {}: {e}", dest.display()));
            }
        }
        if start.elapsed() >= wait.timeout {
            return VerifyOutcome::CacheFileTimeout;
        }
        thread::sleep(wait.interval)
    };
    if !checksum {
        return VerifyOutcome::ChecksumDisabled;
    }
    let (n, dh) = match digest(dest, cancel) {
        Ok(v) => v,
        Err(e) if e.kind() == io::ErrorKind::Interrupted => return VerifyOutcome::Cancelled,
        Err(e) => return VerifyOutcome::IoError(format!("cache read {}: {e}", dest.display())),
    };
    let da = match fs::metadata(dest) {
        Ok(m) => m,
        Err(e) => return VerifyOutcome::IoError(format!("cache metadata {}: {e}", dest.display())),
    };
    if dm.len() != n || n != before.len() {
        return VerifyOutcome::SizeMismatch;
    }
    if signature(&dm) != signature(&da) {
        return VerifyOutcome::DestinationChanged;
    }
    if src_hash != dh {
        return VerifyOutcome::ChecksumMismatch;
    }
    VerifyOutcome::Verified
}
