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
    /// Reserved for access-layer classification; not produced by [`verify`] yet.
    #[allow(dead_code)]
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
fn digest(
    path: &Path,
    cancel: &AtomicBool,
    on_progress: Option<&dyn Fn(u64)>,
) -> io::Result<(u64, blake3::Hash)> {
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
        if let Some(cb) = on_progress {
            cb(n);
        }
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
    on_progress: Option<&dyn Fn(u64)>,
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
    let (read, src_hash) = match digest(source, cancel, on_progress) {
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
    // Google Docs/Sheets and similar Drive objects often stat as 0 bytes. A
    // read may also yield 0 (nothing exported) — rclone then never creates a
    // VFS cache file. Waiting for dest.len()==0 times out after 30s.
    if read == 0 {
        return VerifyOutcome::ChecksumDisabled;
    }
    if after.len() != read {
        return VerifyOutcome::SizeMismatch;
    }
    if before.len() != 0 && before.len() != read {
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
            Ok(m) if m.is_file() && m.len() == read => {
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
    let (n, dh) = match digest(dest, cancel, None) {
        Ok(v) => v,
        Err(e) if e.kind() == io::ErrorKind::Interrupted => return VerifyOutcome::Cancelled,
        Err(e) => return VerifyOutcome::IoError(format!("cache read {}: {e}", dest.display())),
    };
    let da = match fs::metadata(dest) {
        Ok(m) => m,
        Err(e) => return VerifyOutcome::IoError(format!("cache metadata {}: {e}", dest.display())),
    };
    if dm.len() != n || n != read {
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
