//! JSON configuration loader and validation for warm-drive-cache.
//!
//! Load order: run-dir `warm-drive-cache.json` → `WARM_DRIVE_CACHE_CONFIG` → XDG config path.
//! Public sample is tracked as `warm-drive-cache-example.json`; local `warm-drive-cache.json` is gitignored.
//!
//! Size fields accept whole JSON integers **or** whole-number strings with
//! optional units (`64KiB`, `64K`, `1MB`, …). Fractional values are rejected.
//! See [`parse_size_expr`].
//!
//! See README for schema and examples. More xSAR tools: https://xSAR.com.au

use serde::Deserialize;
use serde::de::{self, Deserializer, Visitor};
use std::env;
use std::fmt;
use std::fs;
use std::path::PathBuf;

/// Parse the boolean vocabulary shared by JSON configuration and CLI overrides.
pub fn parse_bool(input: &str) -> Result<bool, String> {
    match input.trim().to_ascii_lowercase().as_str() {
        "true" | "yes" | "y" => Ok(true),
        "false" | "no" | "n" => Ok(false),
        _ => Err(format!(
            "invalid boolean {input:?}; use TRUE/YES/Y or FALSE/NO/N"
        )),
    }
}

fn deserialize_bool<'de, D: Deserializer<'de>>(d: D) -> Result<bool, D::Error> {
    struct BoolVisitor;
    impl<'de> Visitor<'de> for BoolVisitor {
        type Value = bool;
        fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
            f.write_str("a boolean or TRUE/YES/Y/FALSE/NO/N string")
        }
        fn visit_bool<E: de::Error>(self, v: bool) -> Result<bool, E> {
            Ok(v)
        }
        fn visit_str<E: de::Error>(self, v: &str) -> Result<bool, E> {
            parse_bool(v).map_err(E::custom)
        }
        fn visit_string<E: de::Error>(self, v: String) -> Result<bool, E> {
            self.visit_str(&v)
        }
    }
    d.deserialize_any(BoolVisitor)
}

// ── Human-readable size parsing ─────────────────────────────────────────────

/// Multiplier for a unit suffix (case-insensitive). Binary powers of 1024.
///
/// Accepted (with or without spaces before the unit):
/// - no unit / `B` / `b` → bytes
/// - `K` / `KB` / `KiB` → 1024
/// - `M` / `MB` / `MiB` → 1024²
/// - `G` / `GB` / `GiB` → 1024³
/// - `T` / `TB` / `TiB` → 1024⁴
/// - `P` / `PB` / `PiB` → 1024⁵
///
/// `B` and `b` are treated the same (bytes). Single-letter `K`/`M`/… omit the `B`.
fn unit_multiplier(unit: &str) -> Result<u64, String> {
    let u = unit.trim().to_ascii_lowercase();
    // Strip a trailing "ib" / "b" ambiguity by matching longest known forms first.
    let mult = match u.as_str() {
        "" | "b" => 1,
        "k" | "kb" | "kib" => 1024,
        "m" | "mb" | "mib" => 1024 * 1024,
        "g" | "gb" | "gib" => 1024 * 1024 * 1024,
        "t" | "tb" | "tib" => 1024 * 1024 * 1024 * 1024,
        "p" | "pb" | "pib" => 1024 * 1024 * 1024 * 1024 * 1024,
        _ => {
            return Err(format!(
                "unknown size unit {unit:?} (use B, K/KB/KiB, M/MB/MiB, G/GB/GiB, T/TB/TiB, P/PB/PiB; \
                 case-insensitive; bare K means KiB)"
            ));
        }
    };
    Ok(mult)
}

/// Parse a size expression into a signed byte count.
///
/// Accepts:
/// - bare whole integers: `65536`, `-1`
/// - whole coefficients with units: `64k`, `1MiB`
/// - optional whitespace: `64 KiB`
///
/// Fractional values (`12.5`, `"1.5KiB"`) are rejected. `-1` with no unit is
/// the only allowed negative (metadata-only policy for max).
pub fn parse_size_expr(input: &str) -> Result<i64, String> {
    let s = input.trim();
    if s.is_empty() {
        return Err("size string is empty".into());
    }

    let (negative, body) = if let Some(rest) = s.strip_prefix('-') {
        (true, rest.trim_start())
    } else if let Some(rest) = s.strip_prefix('+') {
        (false, rest.trim_start())
    } else {
        (false, s)
    };

    if body.is_empty() {
        return Err(format!("invalid size expression {input:?}"));
    }

    // Coefficient: whole digits only (no decimal point).
    let bytes = body.as_bytes();
    let mut i = 0usize;
    let mut saw_digit = false;
    while i < bytes.len() {
        let c = bytes[i];
        if c.is_ascii_digit() {
            saw_digit = true;
            i += 1;
        } else {
            break;
        }
    }
    if i < bytes.len() && bytes[i] == b'.' {
        return Err(format!(
            "configuration error: size {input:?} must be a whole number of bytes \
             (fractions are not allowed; use 1536 or \"2KiB\", not \"1.5KiB\")"
        ));
    }
    if !saw_digit {
        return Err(format!(
            "configuration error: size {input:?} must start with a number \
             (e.g. 65536, \"64KiB\", \"64K\", \"1MB\")"
        ));
    }

    let num_str = &body[..i];
    let unit = body[i..].trim();
    let coef: u64 = num_str
        .parse()
        .map_err(|_| format!("configuration error: cannot parse numeric part of size {input:?}"))?;

    if negative {
        // Only exact -1 (no unit) is meaningful for max_file_size_bytes.
        if coef == 1 && unit.is_empty() {
            return Ok(-1);
        }
        return Err(format!(
            "configuration error: negative size {input:?} is invalid. \
             Only -1 (metadata only) is allowed as a negative value for max_file_size_bytes"
        ));
    }

    let mult = unit_multiplier(unit)?;
    let product = coef.checked_mul(mult).ok_or_else(|| {
        format!("configuration error: size {input:?} exceeds maximum representable bytes")
    })?;
    if product > i64::MAX as u64 {
        return Err(format!(
            "configuration error: size {input:?} exceeds maximum representable bytes"
        ));
    }
    Ok(product as i64)
}

/// Parse a non-negative size (for `min_file_size_bytes`).
pub fn parse_non_negative_size(input: &str) -> Result<u64, String> {
    let v = parse_size_expr(input)?;
    if v < 0 {
        return Err(format!(
            "configuration error: min_file_size_bytes cannot be negative (got {input:?})"
        ));
    }
    Ok(v as u64)
}

fn deserialize_min_file_size<'de, D>(deserializer: D) -> Result<u64, D::Error>
where
    D: Deserializer<'de>,
{
    struct MinVisitor;
    impl<'de> Visitor<'de> for MinVisitor {
        type Value = u64;

        fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
            f.write_str("a non-negative size as integer or string (e.g. 1024, \"64KiB\", \"64K\")")
        }

        fn visit_u64<E: de::Error>(self, v: u64) -> Result<u64, E> {
            Ok(v)
        }

        fn visit_i64<E: de::Error>(self, v: i64) -> Result<u64, E> {
            if v < 0 {
                return Err(E::custom(format!(
                    "configuration error: walk.min_file_size_bytes cannot be negative (got {v})"
                )));
            }
            Ok(v as u64)
        }

        fn visit_f64<E: de::Error>(self, v: f64) -> Result<u64, E> {
            if !v.is_finite() || v < 0.0 {
                return Err(E::custom(format!(
                    "configuration error: walk.min_file_size_bytes invalid number {v}"
                )));
            }
            if (v - v.round()).abs() > 1e-9 {
                return Err(E::custom(format!(
                    "configuration error: walk.min_file_size_bytes must be a whole byte count \
                     (got {v}); fractional values are not allowed"
                )));
            }
            Ok(v.round() as u64)
        }

        fn visit_str<E: de::Error>(self, v: &str) -> Result<u64, E> {
            parse_non_negative_size(v).map_err(E::custom)
        }

        fn visit_string<E: de::Error>(self, v: String) -> Result<u64, E> {
            self.visit_str(&v)
        }
    }
    deserializer.deserialize_any(MinVisitor)
}

fn deserialize_max_file_size<'de, D>(deserializer: D) -> Result<i64, D::Error>
where
    D: Deserializer<'de>,
{
    struct MaxVisitor;
    impl<'de> Visitor<'de> for MaxVisitor {
        type Value = i64;

        fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
            f.write_str(
                "a size as integer or string: -1, 0, N, or \"64KiB\" / \"64K\" / \"1MB\" / …",
            )
        }

        fn visit_i64<E: de::Error>(self, v: i64) -> Result<i64, E> {
            Ok(v)
        }

        fn visit_u64<E: de::Error>(self, v: u64) -> Result<i64, E> {
            if v > i64::MAX as u64 {
                return Err(E::custom(
                    "configuration error: walk.max_file_size_bytes exceeds i64 range",
                ));
            }
            Ok(v as i64)
        }

        fn visit_f64<E: de::Error>(self, v: f64) -> Result<i64, E> {
            if !v.is_finite() {
                return Err(E::custom(
                    "configuration error: walk.max_file_size_bytes is not a finite number",
                ));
            }
            if (v - v.round()).abs() > 1e-9 {
                return Err(E::custom(format!(
                    "configuration error: walk.max_file_size_bytes must be a whole byte count \
                     (got {v}); fractional values are not allowed"
                )));
            }
            Ok(v.round() as i64)
        }

        fn visit_str<E: de::Error>(self, v: &str) -> Result<i64, E> {
            parse_size_expr(v).map_err(E::custom)
        }

        fn visit_string<E: de::Error>(self, v: String) -> Result<i64, E> {
            self.visit_str(&v)
        }
    }
    deserializer.deserialize_any(MaxVisitor)
}

#[derive(Debug, Deserialize, Clone)]
pub struct Config {
    #[serde(default = "default_version")]
    pub version: u32,

    /// List of path pairs. "sync" is the rclone-exposed directory (traversed; size-gated
    /// File contents read or metadata only). "cache" is the rclone --cache-dir (used ONLY for
    /// size calculation and deletion to clear stale cache data).
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
    /// The directory exposed by rclone mount (e.g. /home/user/mounts/project-a).
    /// ONLY traversed for warming (File contents read when size is in range, else attributes only).
    /// NEVER delete from here.
    pub sync: String,
    /// The rclone cache directory (from --cache-dir in service unit, e.g. /home/user/.rclone_cache).
    /// Used for on-disk size checks and complete deletion of contents to refresh cache.
    /// Can be shared across multiple sync dirs.
    pub cache: String,
    /// Optional systemd unit that mounts this sync path (e.g. `gdrive-project-a.service`).
    /// Accepts **system** units (`/etc/systemd/system/`) or **user** units (`systemctl --user`).
    /// When set, startup detects the scope via LoadState, checks `is-active`, and may offer to start.
    #[serde(default)]
    pub service: Option<String>,
}

fn default_version() -> u32 {
    1
}

#[derive(Debug, Deserialize, Clone)]
pub struct WalkOptions {
    /// Verify mount and local VFS cache contents with BLAKE3. Enabled by default.
    #[serde(default = "default_checksum", deserialize_with = "deserialize_bool")]
    pub checksum: bool,
    /// None (or omitted) = unlimited (exact behaviour before this feature).
    /// Some(n) = WalkDir::max_depth(n)
    pub max_depth: Option<usize>,

    /// Minimum file size (bytes) eligible for a File contents read when `max_file_size_bytes > 0`.
    /// `0` means no minimum (any size is eligible on the low side).
    ///
    /// JSON: integer **or** string with unit (`"64KiB"`, `"64K"`, `"1MB"`, …). Stored as bytes.
    #[serde(
        default = "default_min_file_size_bytes",
        deserialize_with = "deserialize_min_file_size"
    )]
    pub min_file_size_bytes: u64,

    /// Maximum file size limit for File contents read (bytes after unit expansion).
    ///
    /// JSON: integer **or** string with unit (`"64KiB"`, `"64K"`, `"1.5MiB"`, …).
    ///
    /// Special values:
    /// - `-1` — no File contents read; metadata only for every file
    /// - `0` — File contents read for **all** files (any size; ignores min)
    /// - `N > 0` — File contents read when size is within min..N
    ///
    /// Other negative values are rejected.
    #[serde(
        default = "default_max_file_size_bytes",
        deserialize_with = "deserialize_max_file_size"
    )]
    pub max_file_size_bytes: i64,

    /// Maximum worker threads for concurrent file warm operations (open/read or metadata).
    /// Default 8. Must be in 1..=64. Spinner shows active/max during the walk.
    #[serde(default = "default_max_threads")]
    pub max_threads: usize,

    /// Nominal threads-display width in characters. Default 80; values below 80
    /// become 80 and values above 200 become 200. Extra columns above 80
    /// lengthen only the Source filename field.
    #[serde(default = "default_width")]
    pub width: usize,
}

impl Default for WalkOptions {
    fn default() -> Self {
        Self {
            checksum: default_checksum(),
            max_depth: None,
            min_file_size_bytes: default_min_file_size_bytes(),
            max_file_size_bytes: default_max_file_size_bytes(),
            max_threads: default_max_threads(),
            width: default_width(),
        }
    }
}

fn default_min_file_size_bytes() -> u64 {
    0
}
fn default_max_file_size_bytes() -> i64 {
    0
}
fn default_max_threads() -> usize {
    8
}
fn default_checksum() -> bool {
    true
}

pub const DISPLAY_WIDTH_DEFAULT: usize = 80;
pub const DISPLAY_WIDTH_MIN: usize = 80;
pub const DISPLAY_WIDTH_MAX: usize = 200;

fn default_width() -> usize {
    DISPLAY_WIDTH_DEFAULT
}

/// Clamp a requested threads-display width into `80..=200`.
pub fn clamp_display_width(n: usize) -> usize {
    n.clamp(DISPLAY_WIDTH_MIN, DISPLAY_WIDTH_MAX)
}

/// Source-filename columns at a threads-display width.
/// Width 80 uses 40 characters; each extra column above 80 adds one (160 at 200).
pub const SOURCE_FILENAME_BASE_CHARS: usize = 40;

pub fn source_filename_max_chars(width: usize) -> usize {
    SOURCE_FILENAME_BASE_CHARS + clamp_display_width(width).saturating_sub(DISPLAY_WIDTH_DEFAULT)
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
/// Checks the run directory (directory containing the executable) for `warm-drive-cache.json` first.
/// Falls back to WARM_DRIVE_CACHE_CONFIG env var, then XDG ProjectDirs.
pub fn load() -> Result<Config, String> {
    // Check run directory (next to the binary) for warm-drive-cache.json
    if let Ok(exe) = env::current_exe()
        && let Some(run_dir) = exe.parent()
    {
        let local = run_dir.join("warm-drive-cache.json");
        if local.exists() {
            return load_from_path(&local);
        }
    }

    if let Ok(p) = env::var("WARM_DRIVE_CACHE_CONFIG")
        && !p.is_empty()
    {
        return load_from_path(&PathBuf::from(p));
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

    let path = config_dir.join("warm-drive-cache.json");
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

    cfg.walk.width = clamp_display_width(cfg.walk.width);

    if cfg.mount_wait.retry_delays_secs.is_empty() {
        return Err(
            "mount_wait.retry_delays_secs must not be empty (omit the field to use [3, 5, 8])"
                .into(),
        );
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
                return Err(format!(
                    "{} path {:?} contains invalid control characters",
                    label, p
                ));
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
        if let Some(svc) = &pair.service {
            let name = svc.trim();
            if name.is_empty() {
                return Err("paths[].service is empty; omit the field or set a unit name".into());
            }
            // Unit names: letters, digits, @_.- and must end with .service for mounts we manage
            if name.contains('/') || name.contains('\0') || name.chars().any(|c| c.is_control()) {
                return Err(format!(
                    "service name {:?} looks invalid (no path separators or control characters)",
                    svc
                ));
            }
        }
    }

    // Reasonable bounds (defence in depth)
    if cfg.paths.len() > 128 {
        return Err("too many paths in config (max 128)".to_string());
    }
    if let Some(d) = cfg.walk.max_depth
        && d == 0
    {
        return Err(
            "walk.max_depth 0 is not useful (would only visit the roots themselves)".to_string(),
        );
    }

    if cfg.walk.max_threads == 0 || cfg.walk.max_threads > 64 {
        return Err(format!(
            "walk.max_threads must be between 1 and 64 (got {})",
            cfg.walk.max_threads
        ));
    }

    let min = cfg.walk.min_file_size_bytes;
    let max = cfg.walk.max_file_size_bytes;
    // max_file_size_bytes: only -1, 0, or positive byte counts are allowed.
    if max < -1 {
        return Err(format!(
            "configuration error: walk.max_file_size_bytes ({max}) is invalid. \
             Allowed: -1 (metadata only), 0 (File contents read for all files), \
             or a size limit as bytes or a unit string (e.g. 65536, \"64KiB\", \"64K\"). \
             Other negative values are rejected."
        ));
    }
    // When max is a positive upper bound, min must not exceed it.
    if max > 0 && min != 0 && min > max as u64 {
        return Err(format!(
            "walk.min_file_size_bytes ({min}) cannot be greater than walk.max_file_size_bytes ({max})"
        ));
    }

    Ok(cfg)
}

/// Re-run effective configuration validation after command-line overrides.
pub fn validate_effective(cfg: &Config) -> Result<(), String> {
    if !(1..=64).contains(&cfg.walk.max_threads) {
        return Err(format!(
            "walk.max_threads must be between 1 and 64 (got {})",
            cfg.walk.max_threads
        ));
    }
    let max = cfg.walk.max_file_size_bytes;
    if max < -1 {
        return Err(format!(
            "walk.max_file_size_bytes ({max}) is invalid; use -1, 0, or a positive size"
        ));
    }
    if max > 0 && cfg.walk.min_file_size_bytes > max as u64 {
        return Err(format!(
            "walk.min_file_size_bytes ({}) cannot be greater than walk.max_file_size_bytes ({max})",
            cfg.walk.min_file_size_bytes
        ));
    }
    Ok(())
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
        assert!(c.paths[0].service.is_none());
        assert!(c.walk.max_depth.is_none());
        assert_eq!(c.walk.min_file_size_bytes, 0);
        assert_eq!(c.walk.max_file_size_bytes, 0);
        assert_eq!(c.walk.max_threads, 8);
        assert_eq!(c.walk.width, DISPLAY_WIDTH_DEFAULT);
        assert!(c.ignore.names.is_empty());
        // mount_wait should have today's defaults via our Default impls
        assert_eq!(c.mount_wait.initial_secs, 3);

        let full = r#"{
            "version": 1,
            "paths": [
                {"sync": "/a", "cache": "/cache/a", "service": "rclone-a.service"},
                {"sync": "/b", "cache": "/cache/b"}
            ],
            "walk": {
                "max_depth": 5,
                "min_file_size_bytes": 0,
                "max_file_size_bytes": 5120,
                "max_threads": 4
            },
            "ignore": { "names": [".git", "target"] },
            "mount_wait": { "initial_secs": 1, "retry_delays_secs": [9], "max_wait_secs": 99 }
        }"#;
        let c: Config = serde_json::from_str(full).unwrap();
        assert_eq!(c.paths.len(), 2);
        assert_eq!(c.paths[0].sync, "/a");
        assert_eq!(c.paths[0].cache, "/cache/a");
        assert_eq!(c.paths[0].service.as_deref(), Some("rclone-a.service"));
        assert!(c.paths[1].service.is_none());
        assert_eq!(c.walk.max_depth, Some(5));
        assert_eq!(c.walk.min_file_size_bytes, 0);
        assert_eq!(c.walk.max_file_size_bytes, 5120);
        assert_eq!(c.walk.max_threads, 4);
        assert_eq!(c.walk.width, DISPLAY_WIDTH_DEFAULT);
        assert_eq!(
            c.ignore.names,
            vec![".git".to_string(), "target".to_string()]
        );
        assert_eq!(c.mount_wait.max_wait_secs, 99);
    }

    #[test]
    fn config_load_from_tempfile_and_validation() {
        let td = TempDir::new().expect("tempdir for config test");
        let p = td.path().join("warm-drive-cache.json");

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
            f.write_all(br#"{"paths":[{"sync":"relative/path","cache":"/cache/foo"}]}"#)
                .unwrap();
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
        let p = std::path::Path::new("/this/path/does/not/exist/ever/warm-drive-cache.json");
        let c = load_from_path(p).expect("missing file yields default struct");
        // Our loader returns a struct with empty paths on missing file (caller decides what to do)
        assert!(c.paths.is_empty());
        assert_eq!(c.mount_wait.initial_secs, 3);
        assert!(c.walk.max_depth.is_none());
        assert_eq!(c.walk.min_file_size_bytes, 0);
        assert_eq!(c.walk.max_file_size_bytes, 0);
        assert_eq!(c.walk.max_threads, 8);
        assert_eq!(c.walk.width, DISPLAY_WIDTH_DEFAULT);
    }

    #[test]
    fn display_width_defaults_and_clamps() {
        assert_eq!(clamp_display_width(0), DISPLAY_WIDTH_MIN);
        assert_eq!(clamp_display_width(50), DISPLAY_WIDTH_MIN);
        assert_eq!(clamp_display_width(80), 80);
        assert_eq!(clamp_display_width(120), 120);
        assert_eq!(clamp_display_width(200), DISPLAY_WIDTH_MAX);
        assert_eq!(clamp_display_width(250), DISPLAY_WIDTH_MAX);
        assert_eq!(source_filename_max_chars(50), SOURCE_FILENAME_BASE_CHARS);
        assert_eq!(source_filename_max_chars(80), SOURCE_FILENAME_BASE_CHARS);
        assert_eq!(source_filename_max_chars(120), 80);
        assert_eq!(source_filename_max_chars(200), 160);
        assert_eq!(source_filename_max_chars(999), 160);

        let td = TempDir::new().expect("tempdir");
        let p = td.path().join("warm-drive-cache.json");
        {
            let mut f = File::create(&p).unwrap();
            f.write_all(
                br#"{"paths":[{"sync":"/abs/one","cache":"/cache/abs"}],"walk":{"width":50}}"#,
            )
            .unwrap();
        }
        let c = load_from_path(&p).expect("width below 80 clamps");
        assert_eq!(c.walk.width, DISPLAY_WIDTH_MIN);
        assert_eq!(
            source_filename_max_chars(c.walk.width),
            SOURCE_FILENAME_BASE_CHARS
        );

        {
            let mut f = File::create(&p).unwrap();
            f.write_all(
                br#"{"paths":[{"sync":"/abs/one","cache":"/cache/abs"}],"walk":{"width":250}}"#,
            )
            .unwrap();
        }
        let c = load_from_path(&p).expect("width above 200 clamps");
        assert_eq!(c.walk.width, DISPLAY_WIDTH_MAX);
        assert_eq!(source_filename_max_chars(c.walk.width), 160);

        {
            let mut f = File::create(&p).unwrap();
            f.write_all(
                br#"{"paths":[{"sync":"/abs/one","cache":"/cache/abs"}],"walk":{"width":120}}"#,
            )
            .unwrap();
        }
        let c = load_from_path(&p).expect("in-range width kept");
        assert_eq!(c.walk.width, 120);
        assert_eq!(source_filename_max_chars(c.walk.width), 80);
    }

    #[test]
    fn config_rejects_min_greater_than_max_file_size() {
        let td = TempDir::new().expect("tempdir");
        let p = td.path().join("warm-drive-cache.json");
        {
            let mut f = File::create(&p).unwrap();
            f.write_all(
                br#"{"paths":[{"sync":"/abs/one","cache":"/cache/abs"}],
                 "walk":{"min_file_size_bytes":100,"max_file_size_bytes":50}}"#,
            )
            .unwrap();
        }
        let err = load_from_path(&p).unwrap_err();
        assert!(
            err.contains("min_file_size_bytes") && err.contains("max_file_size_bytes"),
            "expected size range error: {}",
            err
        );
    }

    #[test]
    fn config_accepts_max_file_size_specials() {
        let td = TempDir::new().expect("tempdir");
        let p = td.path().join("warm-drive-cache.json");
        for max in [-1i64, 0, 65536] {
            let body = format!(
                r#"{{"paths":[{{"sync":"/abs/one","cache":"/cache/abs"}}],
                 "walk":{{"max_file_size_bytes":{max}}}}}"#
            );
            let mut f = File::create(&p).unwrap();
            f.write_all(body.as_bytes()).unwrap();
            let c = load_from_path(&p).expect("load");
            assert_eq!(c.walk.max_file_size_bytes, max);
        }
    }

    #[test]
    fn config_rejects_other_negative_max_file_size() {
        let td = TempDir::new().expect("tempdir");
        let p = td.path().join("warm-drive-cache.json");
        {
            let mut f = File::create(&p).unwrap();
            f.write_all(
                br#"{"paths":[{"sync":"/abs/one","cache":"/cache/abs"}],
                 "walk":{"max_file_size_bytes":-2}}"#,
            )
            .unwrap();
        }
        let err = load_from_path(&p).unwrap_err();
        assert!(
            err.contains("configuration error") && err.contains("max_file_size_bytes"),
            "expected invalid max error: {}",
            err
        );
    }

    #[test]
    fn config_rejects_bare_fractional_max_file_size() {
        let td = TempDir::new().expect("tempdir");
        let p = td.path().join("warm-drive-cache.json");
        {
            let mut f = File::create(&p).unwrap();
            f.write_all(
                br#"{"paths":[{"sync":"/abs/one","cache":"/cache/abs"}],
                 "walk":{"max_file_size_bytes":12.5}}"#,
            )
            .unwrap();
        }
        let err = load_from_path(&p).unwrap_err();
        assert!(
            err.contains("configuration error") || err.contains("invalid JSON"),
            "expected bare fractional rejection: {}",
            err
        );
    }

    #[test]
    fn parse_size_expr_units_and_shorthand() {
        assert_eq!(parse_size_expr("65536").unwrap(), 65536);
        assert_eq!(parse_size_expr("64KiB").unwrap(), 65536);
        assert_eq!(parse_size_expr("64kib").unwrap(), 65536);
        assert_eq!(parse_size_expr("64K").unwrap(), 65536);
        assert_eq!(parse_size_expr("64k").unwrap(), 65536);
        assert_eq!(parse_size_expr("64 KB").unwrap(), 65536);
        assert_eq!(parse_size_expr("1MiB").unwrap(), 1024 * 1024);
        assert_eq!(parse_size_expr("1mb").unwrap(), 1024 * 1024);
        assert_eq!(parse_size_expr("1M").unwrap(), 1024 * 1024);
        assert_eq!(parse_size_expr("512B").unwrap(), 512);
        assert_eq!(parse_size_expr("512b").unwrap(), 512);
        assert_eq!(parse_size_expr("-1").unwrap(), -1);
        assert!(parse_size_expr("-2").is_err());
        assert!(parse_size_expr("10XB").is_err());
        for bad in ["1.5KiB", "12.5", "1.0MiB", ".5K"] {
            let err = parse_size_expr(bad).unwrap_err();
            assert!(
                err.contains("whole number") || err.contains("fraction"),
                "expected fraction rejection for {bad}: {err}"
            );
        }
    }

    #[test]
    fn config_rejects_empty_retry_delays() {
        let td = TempDir::new().expect("tempdir");
        let p = td.path().join("warm-drive-cache.json");
        {
            let mut f = File::create(&p).unwrap();
            f.write_all(
                br#"{"paths":[{"sync":"/abs/one","cache":"/cache/abs"}],
                 "mount_wait":{"retry_delays_secs":[]}}"#,
            )
            .unwrap();
        }
        let err = load_from_path(&p).unwrap_err();
        assert!(
            err.contains("retry_delays_secs"),
            "expected empty retry list rejection: {err}"
        );
    }

    #[test]
    fn config_accepts_string_size_units() {
        let td = TempDir::new().expect("tempdir");
        let p = td.path().join("warm-drive-cache.json");
        {
            let mut f = File::create(&p).unwrap();
            f.write_all(
                br#"{"paths":[{"sync":"/abs/one","cache":"/cache/abs"}],
                 "walk":{"min_file_size_bytes":"1K","max_file_size_bytes":"64KiB"}}"#,
            )
            .unwrap();
        }
        let c = load_from_path(&p).expect("load unit strings");
        assert_eq!(c.walk.min_file_size_bytes, 1024);
        assert_eq!(c.walk.max_file_size_bytes, 65536);
    }

    #[test]
    fn config_accepts_string_minus_one() {
        let td = TempDir::new().expect("tempdir");
        let p = td.path().join("warm-drive-cache.json");
        {
            let mut f = File::create(&p).unwrap();
            f.write_all(
                br#"{"paths":[{"sync":"/abs/one","cache":"/cache/abs"}],
                 "walk":{"max_file_size_bytes":"-1"}}"#,
            )
            .unwrap();
        }
        let c = load_from_path(&p).expect("load -1 string");
        assert_eq!(c.walk.max_file_size_bytes, -1);
    }

    #[test]
    fn config_rejects_invalid_max_threads() {
        let td = TempDir::new().expect("tempdir");
        let p = td.path().join("warm-drive-cache.json");
        {
            let mut f = File::create(&p).unwrap();
            f.write_all(
                br#"{"paths":[{"sync":"/abs/one","cache":"/cache/abs"}],
                 "walk":{"max_threads":0}}"#,
            )
            .unwrap();
        }
        let err = load_from_path(&p).unwrap_err();
        assert!(
            err.contains("max_threads"),
            "expected max_threads error: {}",
            err
        );

        {
            let mut f = File::create(&p).unwrap();
            f.write_all(
                br#"{"paths":[{"sync":"/abs/one","cache":"/cache/abs"}],
                 "walk":{"max_threads":100}}"#,
            )
            .unwrap();
        }
        let err = load_from_path(&p).unwrap_err();
        assert!(
            err.contains("max_threads"),
            "expected max_threads error: {}",
            err
        );
    }
}
