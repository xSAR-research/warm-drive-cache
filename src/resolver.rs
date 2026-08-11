//! Safe mapping from a mount path to rclone's on-disk VFS cache layout.
//!
//! rclone stores content below `<cache-dir>/vfs/<remote-name>/<remote-path>`.
//! The configured cache must therefore contain exactly one remote directory unless callers use
//! [`resolve_with_remote`]. This tool never assumes that mount-relative files live directly below
//! `--cache-dir`.
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub struct ResolutionError {
    pub mount_path: PathBuf,
    pub cache_root: PathBuf,
    pub relative_path: Option<PathBuf>,
    pub attempted_destination: Option<PathBuf>,
    pub message: String,
}
impl std::fmt::Display for ResolutionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} (mount={}, cache={}, relative={}, destination={})",
            self.message,
            self.mount_path.display(),
            self.cache_root.display(),
            self.relative_path
                .as_deref()
                .map_or_else(|| "<unresolved>".into(), |p| p.display().to_string()),
            self.attempted_destination
                .as_deref()
                .map_or_else(|| "<unresolved>".into(), |p| p.display().to_string())
        )
    }
}
impl std::error::Error for ResolutionError {}

pub fn resolve(
    sync_root: &Path,
    cache_root: &Path,
    mount_path: &Path,
) -> Result<PathBuf, ResolutionError> {
    let relative = mount_path
        .strip_prefix(sync_root)
        .map(Path::to_path_buf)
        .map_err(|_| {
            err(
                mount_path,
                cache_root,
                None,
                None,
                "mount path is outside configured sync root",
            )
        })?;
    let vfs = cache_root.join("vfs");
    let remotes = std::fs::read_dir(&vfs)
        .map_err(|e| {
            err(
                mount_path,
                cache_root,
                Some(relative.clone()),
                Some(vfs.clone()),
                &format!("cannot inspect rclone VFS layout: {e}"),
            )
        })?
        .filter_map(Result::ok)
        .filter(|e| e.path().is_dir())
        .collect::<Vec<_>>();
    if remotes.len() != 1 {
        return Err(err(
            mount_path,
            cache_root,
            Some(relative),
            Some(vfs),
            "expected exactly one <cache-dir>/vfs/<remote-name> directory; configure an unambiguous cache",
        ));
    }
    resolve_with_remote(
        sync_root,
        cache_root,
        remotes[0].file_name().as_ref(),
        mount_path,
    )
}
pub fn resolve_with_remote(
    sync_root: &Path,
    cache_root: &Path,
    remote: &std::ffi::OsStr,
    mount_path: &Path,
) -> Result<PathBuf, ResolutionError> {
    let rel = mount_path
        .strip_prefix(sync_root)
        .map(Path::to_path_buf)
        .map_err(|_| {
            err(
                mount_path,
                cache_root,
                None,
                None,
                "mount path is outside configured sync root",
            )
        })?;
    if rel.components().any(|c| {
        matches!(
            c,
            std::path::Component::ParentDir
                | std::path::Component::RootDir
                | std::path::Component::Prefix(_)
        )
    }) {
        return Err(err(
            mount_path,
            cache_root,
            Some(rel),
            None,
            "relative path contains traversal",
        ));
    }
    let base = cache_root.join("vfs").join(remote);
    let dest = base.join(&rel);
    let canonical_root = cache_root.canonicalize().map_err(|e| {
        err(
            mount_path,
            cache_root,
            Some(rel.clone()),
            Some(dest.clone()),
            &format!("cannot canonicalize cache root: {e}"),
        )
    })?;
    let existing = dest
        .ancestors()
        .find(|p| p.exists())
        .unwrap_or(cache_root)
        .canonicalize()
        .map_err(|e| {
            err(
                mount_path,
                cache_root,
                Some(rel.clone()),
                Some(dest.clone()),
                &format!("cannot canonicalize destination parent: {e}"),
            )
        })?;
    if !existing.starts_with(&canonical_root) {
        return Err(err(
            mount_path,
            cache_root,
            Some(rel),
            Some(dest),
            "destination escapes configured cache root",
        ));
    }
    Ok(dest)
}
fn err(m: &Path, c: &Path, r: Option<PathBuf>, d: Option<PathBuf>, s: &str) -> ResolutionError {
    ResolutionError {
        mount_path: m.into(),
        cache_root: c.into(),
        relative_path: r,
        attempted_destination: d,
        message: s.into(),
    }
}
