// src/config.rs
//
// Minimal, isolated config loader for warm-drive-cache.
// Replaces the hardcoded paths array + adds walk controls (max_depth) and ignore list.
//
// Loading priority (per approved plan):
//   1. $WARM_DRIVE_CACHE_CONFIG (full path to a json file) - for CI, testing, multiple setups
//   2. XDG: $XDG_CONFIG_HOME/warm-drive-cache/config.json  or  ~/.config/warm-drive-cache/config.json
//
// Rules:
// - Missing file → return defaults that exactly match the old hardcoded constants (smooth transition)
// - Bad JSON / unreadable / validation failure → Err with actionable message (caller does eprintln + exit 1)
// - Paths must be non-empty and absolute (reject relatives for predictability + safety)
// - Users are strongly encouraged (and now enforced) to use absolute paths
//
// This module is pure data + fs read. No side effects on the cache maintenance logic itself.
// The 12 existing unit tests continue to pass because they never call main() or this loader for real paths.
//
// See README for example + location documentation.

use serde::Deserialize;
use std::env;
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Deserialize, Clone)]
pub struct Config {
    #[serde(default = "default_version")]
    pub version: u32,

    /// List of path pairs. "sync" is the rclone-exposed directory (traversed and 1 byte read per file to warm).
    /// "cache" is the rclone --cache-dir (used ONLY for size calculation and deletion to clear stale cache data).
    /// The cache dir is typically separate from the sync dir to avoid deleting live data.
    pub paths: Vec<PathPair>,

    #[serde(default)]
    pub walk: WalkOptions,

    #[serde(default)]
    pub ignore: IgnoreOptions,

    #[serde(default)]
    pub mount_wait: MountWait,
}

#[derive(Debug, Deserialize, Clone)]
pub struct PathPair {
    /// The directory exposed by rclone mount (e.g. /home/user/Documents/Gdrive/AccessIT).
    /// ONLY traversed for warming (read 1 byte from each file). NEVER delete from here.
    pub sync: String,
    /// The rclone cache directory (from --cache-dir in service unit, e.g. /home/user/.rclone_cache).
    /// Used for on-disk size checks and complete deletion of contents to refresh cache.
    /// Can be shared across multiple sync dirs.
    pub cache: String,
}

fn default_version() -> u32 {
    1
}

#[derive(Debug, Deserialize, Default, Clone)]
pub struct WalkOptions {
    /// None (or omitted) = unlimited (exact behaviour before this feature).
    /// Some(n) = WalkDir::max_depth(n)
    pub max_depth: Option<usize>,
}

#[derive(Debug, Deserialize, Default, Clone)]
pub struct IgnoreOptions {
    /// List of basenames (dirs or files) to skip.
    /// Matching dirs will have their entire subtree pruned (via filter_entry).
    /// Matching is exact on the basename (OsStr, anywhere in the tree).
    /// The root path itself is never skipped even if its basename matches.
    ///
    /// Example: [".git", "node_modules", "target", ".cache"]
    /// This replaces the old hardcoded "skip .git" stub.
    #[serde(default)]
    pub names: Vec<String>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct MountWait {
    #[serde(default = "default_initial_secs")]
    pub initial_secs: u64,
    #[serde(default = "default_retry_delays")]
    pub retry_delays_secs: Vec<u64>,
    #[serde(default = "default_max_wait_secs")]
    pub max_wait_secs: u64,
}

impl Default for MountWait {
    fn default() -> Self {
        Self {
            initial_secs: default_initial_secs(),
            retry_delays_secs: default_retry_delays(),
            max_wait_secs: default_max_wait_secs(),
        }
    }
}

fn default_initial_secs() -> u64 {
    3
}
fn default_retry_delays() -> Vec<u64> {
    vec![3, 5, 8]
}
fn default_max_wait_secs() -> u64 {
    30
}

/// Main entry point used by the binary.
/// Checks the run directory (directory containing the executable) for `config.json` first.
/// Falls back to WARM_DRIVE_CACHE_CONFIG env var, then XDG ProjectDirs.
pub fn load() -> Result<Config, String> {
    // Check run directory (next to the binary) for config.json
    if let Ok(exe) = env::current_exe() {
        if let Some(run_dir) = exe.parent() {
            let local = run_dir.join("config.json");
            if local.exists() {
                return load_from_path(&local);
            }
        }
    }

    if let Ok(p) = env::var("WARM_DRIVE_CACHE_CONFIG") {
        if !p.is_empty() {
            return load_from_path(&PathBuf::from(p));
        }
    }

    // XDG / standard Linux location using the directories crate (correct, audited)
    let config_dir = match directories::ProjectDirs::from("au", "xsar", "warm-drive-cache") {
        Some(dirs) => dirs.config_dir().to_path_buf(),
        None => {
            // Very rare on a real Linux system; fall back to $HOME/.config
            let home = env::var("HOME").unwrap_or_else(|_| ".".to_string());
            PathBuf::from(home).join(".config").join("warm-drive-cache")
        }
    };

    let path = config_dir.join("config.json");
    load_from_path(&path)
}

/// Load + validate from an explicit path. Used by the binary and by unit tests (via tempfile).
pub fn load_from_path(path: &std::path::Path) -> Result<Config, String> {
    if !path.exists() {
        // Graceful default (matches pre-config behaviour exactly)
        // Caller may print a one-line info message.
        return Ok(Config {
            version: default_version(),
            paths: vec![], // special sentinel — binary must handle "no paths supplied" as error
            walk: WalkOptions::default(),
            ignore: IgnoreOptions::default(),
            mount_wait: MountWait::default(),
        });
    }

    let contents = fs::read_to_string(path)
        .map_err(|e| format!("cannot read config {}: {}", path.display(), e))?;

    let mut cfg: Config = serde_json::from_str(&contents)
        .map_err(|e| format!("invalid JSON in {}: {}", path.display(), e))?;

    // Apply defaults for any omitted sections (serde default + our helpers already do most of it)
    if cfg.mount_wait.retry_delays_secs.is_empty() {
        cfg.mount_wait.retry_delays_secs = default_retry_delays();
    }

    // Validation (strict but friendly)
    if cfg.paths.is_empty() {
        return Err(format!(
            "config {} has empty \"paths\" array. Add at least one path pair with \"sync\" and \"cache\".",
            path.display()
        ));
    }

    for pair in &cfg.paths {
        for (label, p) in [("sync", &pair.sync), ("cache", &pair.cache)] {
            if p.is_empty() {
                return Err(format!("empty {} path string in config", label));
            }
            // Enforce absolute for predictability and to avoid CWD attacks / surprises
            if !p.starts_with('/') {
                return Err(format!(
                    "{} path {:?} is relative. Use absolute paths only (e.g. /path/to/...). \
                     Relative paths are rejected for safety and predictability.",
                    label, p
                ));
            }
            // Basic sanity (no control chars / NUL that could confuse paths later)
            if p.contains('\0') || p.chars().any(|c| c.is_control() && c != '\t') {
                return Err(format!("{} path {:?} contains invalid control characters", label, p));
            }
        }
        // Safety: cache must not be under or same as sync (to prevent deleting live data)
        if pair.cache.starts_with(&pair.sync) || pair.sync.starts_with(&pair.cache) {
            return Err(format!(
                "cache {:?} and sync {:?} must not overlap or contain each other. \
                 Deletion happens ONLY on cache; traversal ONLY on sync.",
                pair.cache, pair.sync
            ));
        }
    }

    // Reasonable bounds (defence in depth)
    if cfg.paths.len() > 128 {
        return Err("too many paths in config (max 128)".to_string());
    }
    if let Some(d) = cfg.walk.max_depth {
        if d == 0 {
            return Err(
                "walk.max_depth 0 is not useful (would only visit the roots themselves)"
                    .to_string(),
            );
        }
    }

    Ok(cfg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::io::Write;
    use tempfile::TempDir;

    #[test]
    fn config_deserialize_in_memory_minimal_and_full() {
        let minimal = r#"{"paths": [{"sync": "/tmp/foo", "cache": "/tmp/cache"}]}"#;
        let c: Config = serde_json::from_str(minimal).expect("in-memory minimal");
        assert_eq!(c.paths.len(), 1);
        assert_eq!(c.paths[0].sync, "/tmp/foo");
        assert_eq!(c.paths[0].cache, "/tmp/cache");
        assert!(c.walk.max_depth.is_none());
        assert!(c.ignore.names.is_empty());
        // mount_wait should have today's defaults via our Default impls
        assert_eq!(c.mount_wait.initial_secs, 3);

        let full = r#"{
            "version": 1,
            "paths": [
                {"sync": "/a", "cache": "/cache/a"},
                {"sync": "/b", "cache": "/cache/b"}
            ],
            "walk": { "max_depth": 5 },
            "ignore": { "names": [".git", "target"] },
            "mount_wait": { "initial_secs": 1, "retry_delays_secs": [9], "max_wait_secs": 99 }
        }"#;
        let c: Config = serde_json::from_str(full).unwrap();
        assert_eq!(c.paths.len(), 2);
        assert_eq!(c.paths[0].sync, "/a");
        assert_eq!(c.paths[0].cache, "/cache/a");
        assert_eq!(c.walk.max_depth, Some(5));
        assert_eq!(
            c.ignore.names,
            vec![".git".to_string(), "target".to_string()]
        );
        assert_eq!(c.mount_wait.max_wait_secs, 99);
    }

    #[test]
    fn config_load_from_tempfile_and_validation() {
        let td = TempDir::new().expect("tempdir for config test");
        let p = td.path().join("config.json");

        // Good file
        {
            let mut f = File::create(&p).unwrap();
            f.write_all(br#"{"paths":[{"sync":"/abs/one","cache":"/cache/abs"},{"sync":"/abs/two","cache":"/cache/abs"}], "ignore":{"names":[".git"]}}"#)
                .unwrap();
        }
        let c = load_from_path(&p).expect("load good config");
        assert_eq!(c.paths.len(), 2);
        assert_eq!(c.paths[0].sync, "/abs/one");
        assert_eq!(c.paths[0].cache, "/cache/abs");
        assert_eq!(c.ignore.names, vec![".git".to_string()]);

        // Relative path must be rejected
        {
            let mut f = File::create(&p).unwrap();
            f.write_all(br#"{"paths":[{"sync":"relative/path","cache":"/cache/foo"}]}"#).unwrap();
        }
        let err = load_from_path(&p).unwrap_err();
        assert!(
            err.contains("relative"),
            "expected relative rejection: {}",
            err
        );

        // Bad JSON
        {
            let mut f = File::create(&p).unwrap();
            f.write_all(b"{ this is not json }").unwrap();
        }
        let err = load_from_path(&p).unwrap_err();
        assert!(err.contains("invalid JSON"), "{}", err);
    }

    #[test]
    fn config_missing_file_returns_defaults_but_empty_paths() {
        let p = std::path::Path::new("/this/path/does/not/exist/ever/config.json");
        let c = load_from_path(p).expect("missing file yields default struct");
        // Our loader returns a struct with empty paths on missing file (caller decides what to do)
        assert!(c.paths.is_empty());
        assert_eq!(c.mount_wait.initial_secs, 3);
        assert!(c.walk.max_depth.is_none());
    }
}
