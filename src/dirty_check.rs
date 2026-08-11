//! Protect dirty rclone VFS entries before cache removal.
use serde_json::Value;
use std::{
    collections::HashMap,
    fs::{self, OpenOptions},
    io::Read,
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::{Duration, Instant},
};
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirtyEntry {
    pub metadata_path: PathBuf,
    pub content_size: u64,
}
fn field<'a>(value: &'a Value, name: &str) -> Option<&'a Value> {
    value
        .as_object()?
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v)
}
fn true_value(v: &Value) -> bool {
    v.as_bool()
        .unwrap_or_else(|| v.as_str().is_some_and(|s| s.eq_ignore_ascii_case("true")))
}
fn number(v: Option<&Value>) -> Option<u64> {
    let v = v?;
    v.as_u64()
        .or_else(|| v.as_i64().and_then(|n| u64::try_from(n).ok()))
        .or_else(|| v.as_str()?.parse().ok())
}
pub fn parse_dirty_metadata(path: &Path) -> Result<Option<DirtyEntry>, String> {
    // Open a new read-only handle for every observation and close it before returning. We never
    // request an advisory/exclusive lock and never retain a handle between polls, so rclone can
    // replace, truncate, or rewrite the metadata file. The kernel's coherent page cache makes
    // writes visible to subsequent opens; an atomic rename is also found by the next directory scan.
    let mut file = OpenOptions::new().read(true).open(path).map_err(|e| {
        format!(
            "cannot open rclone metadata read-only {}: {e}",
            path.display()
        )
    })?;
    let mut text = String::new();
    file.read_to_string(&mut text)
        .map_err(|e| format!("cannot read rclone metadata {}: {e}", path.display()))?;
    drop(file);
    let value: Value = serde_json::from_str(&text)
        .map_err(|e| format!("invalid rclone metadata JSON {}: {e}", path.display()))?;
    if !field(&value, "Dirty").is_some_and(true_value) {
        return Ok(None);
    };
    let content_size = number(field(&value, "Size"))
        .unwrap_or_else(|| fs::metadata(path).map(|m| m.len()).unwrap_or(0));
    Ok(Some(DirtyEntry {
        metadata_path: path.into(),
        content_size,
    }))
}
pub fn scan(cache: &Path) -> Result<Vec<DirtyEntry>, String> {
    let root = cache.join("vfsMeta");
    if !root.exists() {
        return Ok(Vec::new());
    }
    if !root.is_dir() {
        return Err(format!(
            "rclone metadata path is not a directory: {}",
            root.display()
        ));
    }
    let mut dirty = Vec::new();
    for entry in walkdir::WalkDir::new(&root).follow_links(false) {
        let entry = entry
            .map_err(|e| format!("cannot traverse rclone metadata {}: {e}", root.display()))?;
        if entry.file_type().is_file()
            && let Some(item) = parse_dirty_metadata(entry.path())?
        {
            dirty.push(item)
        }
    }
    Ok(dirty)
}
pub fn calculated_wait_secs(size: u64, configured_max: u64) -> u64 {
    let by_size = size.saturating_add(4095) / 4096;
    let by_size = by_size.max(1);
    if configured_max == 0 {
        by_size
    } else {
        by_size.min(configured_max)
    }
}
pub fn wait_until_clean(
    cache: &Path,
    configured_max: u64,
    cancel: &AtomicBool,
) -> Result<(), String> {
    wait_until_clean_with_interval(cache, configured_max, cancel, Duration::from_millis(1000))
}

/// Implementation with an injectable interval so synthetic tests can prove that a replacement
/// metadata file is observed without sleeping for a full production polling interval.
pub fn wait_until_clean_with_interval(
    cache: &Path,
    configured_max: u64,
    cancel: &AtomicBool,
    poll_interval: Duration,
) -> Result<(), String> {
    let mut first_seen = HashMap::<PathBuf, Instant>::new();
    loop {
        if cancel.load(Ordering::SeqCst) {
            return Err("cancelled while waiting for dirty rclone metadata".into());
        }
        let dirty = scan(cache)?;
        if dirty.is_empty() {
            return Ok(());
        }
        first_seen.retain(|path, _| dirty.iter().any(|item| &item.metadata_path == path));
        for item in &dirty {
            let started = *first_seen
                .entry(item.metadata_path.clone())
                .or_insert_with(Instant::now);
            let limit = calculated_wait_secs(item.content_size, configured_max);
            let elapsed = started.elapsed().as_secs();
            println!(
                "   ⏳ Waiting for rclone to save {} (Dirty=true) — {}s of {}s",
                item.metadata_path.display(),
                elapsed,
                limit
            );
            if elapsed >= limit {
                return Err(format!(
                    "rclone cache metadata {} remained Dirty=true for {elapsed}s; the cache was not removed because the file has not been saved to the rclone source",
                    item.metadata_path.display()
                ));
            }
        }
        thread::sleep(poll_interval)
    }
}
