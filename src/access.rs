//! Central access error context and narrow rate-limit classification.
use std::{
    io,
    path::{Path, PathBuf},
};
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessOperation {
    DirectoryTraversal,
    MountMetadata,
    MountOpen,
    MountRead,
    CacheMetadataPoll,
    CacheOpen,
    CacheRead,
}
#[derive(Debug)]
pub struct AccessError {
    pub operation: AccessOperation,
    pub path: PathBuf,
    pub source: io::Error,
    pub rate_limited: bool,
}
impl std::fmt::Display for AccessError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{:?} {}: {}",
            self.operation,
            self.path.display(),
            self.source
        )
    }
}
impl std::error::Error for AccessError {}
pub fn classify(operation: AccessOperation, path: &Path, error: io::Error) -> AccessError {
    let s = error.to_string().to_ascii_lowercase();
    let rate_limited = matches!(error.raw_os_error(), Some(403 | 429))
        || [
            "http 403",
            "http 429",
            "quota exceeded",
            "user-rate-limit exceeded",
            "user rate limit exceeded",
            "rate-limit exceeded",
            "rate limit exceeded",
        ]
        .iter()
        .any(|needle| s.contains(needle));
    AccessError {
        operation,
        path: path.into(),
        source: error,
        rate_limited,
    }
}
