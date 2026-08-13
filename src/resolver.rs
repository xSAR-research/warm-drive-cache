//! Safe mapping from a mount path to rclone's on-disk VFS cache layout.
//!
//! rclone stores content below `<cache-dir>/vfs/<remote-name>/<remote-path>`.
//!
//! A shared `--cache-dir` often has several remote directories. Callers should pass
//! the rclone remote for this mount ([`resolve`] `remote` argument, unit `ExecStart`,
//! or `/proc/mounts`). This tool never assumes that mount-relative files live
//! directly below `--cache-dir`.
use std::ffi::OsStr;
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

/// Take the rclone remote name from a `remote:` / `remote:path` spec.
/// Ignores flags, absolute paths, and `http:` / `https:`.
pub fn remote_name_from_spec(tok: &str) -> Option<String> {
    let tok = tok.trim().trim_matches(|c| c == '"' || c == '\'');
    if tok.starts_with('-') || tok.starts_with('/') {
        return None;
    }
    let (name, _rest) = tok.split_once(':')?;
    let name = name.strip_prefix("rclone#").unwrap_or(name);
    if name.is_empty() || name.contains('/') {
        return None;
    }
    if name.eq_ignore_ascii_case("http") || name.eq_ignore_ascii_case("https") {
        return None;
    }
    Some(name.to_string())
}

/// Find the first rclone `remote:` token in unit / ExecStart text.
pub fn parse_rclone_remote_name(text: &str) -> Option<String> {
    for raw in text.split_whitespace() {
        let tok = raw
            .trim_matches(|c| c == '"' || c == '\'')
            .trim_end_matches('\\');
        if tok.is_empty() || tok == "\\" {
            continue;
        }
        if let Some(name) = remote_name_from_spec(tok) {
            return Some(name);
        }
    }
    None
}

/// Content tree: `<cache-dir>/vfs/<remote>`.
pub fn vfs_content_dir(cache_root: &Path, remote: &OsStr) -> PathBuf {
    cache_root.join("vfs").join(remote)
}

/// Metadata tree: `<cache-dir>/vfsMeta/<remote>`.
pub fn vfs_meta_dir(cache_root: &Path, remote: &OsStr) -> PathBuf {
    cache_root.join("vfsMeta").join(remote)
}

/// Infer the rclone remote that is mounted on `sync_root` from `/proc/self/mounts`.
pub fn remote_from_mounts(sync_root: &Path) -> Option<String> {
    let text = std::fs::read_to_string("/proc/self/mounts").ok()?;
    let sync = sync_root
        .canonicalize()
        .unwrap_or_else(|_| sync_root.to_path_buf());
    let mut best: Option<(usize, String)> = None;
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        let src = parts.next()?;
        let dest = PathBuf::from(parts.next()?);
        let dest_c = dest.canonicalize().unwrap_or(dest);
        if sync != dest_c && !sync.starts_with(&dest_c) {
            continue;
        }
        let name = remote_name_from_spec(src)?;
        let score = dest_c.as_os_str().len();
        if best.as_ref().is_none_or(|(s, _)| score >= *s) {
            best = Some((score, name));
        }
    }
    best.map(|(_, n)| n)
}

fn list_vfs_remotes(vfs: &Path) -> std::io::Result<Vec<std::ffi::OsString>> {
    if !vfs.is_dir() {
        return Ok(Vec::new());
    }
    let mut names = Vec::new();
    for entry in std::fs::read_dir(vfs)? {
        let entry = entry?;
        if entry.path().is_dir() {
            names.push(entry.file_name());
        }
    }
    Ok(names)
}

#[allow(dead_code)] // public lib helper; the binary uses resolve_for_remote
pub fn resolve(
    sync_root: &Path,
    cache_root: &Path,
    mount_path: &Path,
) -> Result<PathBuf, ResolutionError> {
    resolve_for_remote(sync_root, cache_root, mount_path, None)
}

/// Resolve a mount file to its VFS cache path. `remote` is the rclone remote
/// name (`gdrive` in `gdrive:folder`). When omitted, a single `vfs/*` directory
/// is used, otherwise `/proc/self/mounts` is consulted.
pub fn resolve_for_remote(
    sync_root: &Path,
    cache_root: &Path,
    mount_path: &Path,
    remote: Option<&OsStr>,
) -> Result<PathBuf, ResolutionError> {
    if let Some(name) = remote {
        return resolve_with_remote(sync_root, cache_root, name, mount_path);
    }
    if let Some(name) = remote_from_mounts(sync_root) {
        return resolve_with_remote(sync_root, cache_root, OsStr::new(&name), mount_path);
    }

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
    let remotes = list_vfs_remotes(&vfs).map_err(|e| {
        err(
            mount_path,
            cache_root,
            Some(relative.clone()),
            Some(vfs.clone()),
            &format!("cannot inspect rclone VFS layout: {e}"),
        )
    })?;
    match remotes.as_slice() {
        [only] => resolve_with_remote(sync_root, cache_root, only.as_os_str(), mount_path),
        [] => Err(err(
            mount_path,
            cache_root,
            Some(relative),
            Some(vfs),
            "no <cache-dir>/vfs/<remote-name> directory yet and rclone remote could not be inferred",
        )),
        many => {
            let names: Vec<String> = many
                .iter()
                .map(|n| n.to_string_lossy().into_owned())
                .collect();
            Err(err(
                mount_path,
                cache_root,
                Some(relative),
                Some(vfs),
                &format!(
                    "shared cache has multiple vfs remotes ({}); set paths[].service to the mount unit or use a dedicated --cache-dir",
                    names.join(", ")
                ),
            ))
        }
    }
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
    let base = vfs_content_dir(cache_root, remote);
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
