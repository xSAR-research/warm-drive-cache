//! Pre-flight checks before warming: systemd unit state and filesystem permissions.
//!
//! For each path pair this module reports:
//! - whether the optional user systemd unit is active (and may offer to start it)
//! - whether the sync (source) tree is readable
//! - whether the rclone cache dir allows a write/read/delete probe
//! - whether the unit file / unit directory is readable when a service is configured
//!
//! When a required service is not running and the user declines to start it, the pair
//! is skipped — cache warming would not help an unmounted tree.

use crate::cache_ops;
use crate::config::{Config, PathPair};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

/// Run service status, optional start prompt, and permission probes for one pair.
///
/// Returns `true` when main may delete/warm this pair.
/// Call **before** installing raw-mode quit handlers so Y/n prompts use normal stdin.
pub fn check_path_pair(pair: &PathPair, dry_run: bool) -> bool {
    println!("\n🔎 Pre-flight checks");
    println!("   sync:  {}", pair.sync);
    println!("   cache: {}", pair.cache);
    if let Some(svc) = pair
        .service
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        println!("   unit:  {svc} (systemd --user)");
        match ensure_service_ready(svc, dry_run) {
            Ok(true) => {}
            Ok(false) => {
                println!();
                println!(
                    "   ⛔ Service not operational — cache warmer will not run for this pair."
                );
                println!(
                    "   Reason: unit is inactive and was not started; warming an unmounted \
                     tree has no benefit."
                );
                return false;
            }
            Err(e) => {
                println!("   ⚠️  systemd check failed: {e}");
                // Still attempt FS checks; user may have a manual mount.
            }
        }
    } else {
        println!("   unit:  (none configured — skipping systemd checks)");
    }

    let mut ok = true;

    match check_sync_readable(Path::new(&pair.sync)) {
        Ok(msg) => println!("   ✓ sync access: {msg}"),
        Err(e) => {
            println!("   ✗ sync access: {e}");
            ok = false;
        }
    }

    match check_cache_permissions(Path::new(&pair.cache)) {
        Ok(msg) => println!("   ✓ cache access: {msg}"),
        Err(e) => {
            println!("   ✗ cache access: {e}");
            ok = false;
        }
    }

    if let Some(svc) = pair
        .service
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        match check_unit_file_permissions(svc) {
            Ok(msg) => println!("   ✓ unit file: {msg}"),
            Err(e) => {
                println!("   ⚠️  unit file: {e}");
            }
        }
    }

    if !ok {
        println!();
        println!(
            "   ⛔ Permission or path checks failed — cache warmer will not run for this pair."
        );
        println!("   Fix ownership/mode on the sync mount and/or rclone --cache-dir, then retry.");
        return false;
    }

    println!("   ✓ Pre-flight OK — proceeding with cache maintenance for this pair.");
    true
}

/// Ensure the user unit is active; if not, ask (or simulate in dry-run) to start it.
/// Returns Ok(true) if active (or started), Ok(false) if inactive and user declined.
fn ensure_service_ready(unit: &str, dry_run: bool) -> Result<bool, String> {
    let active = is_user_unit_active(unit)?;
    if active {
        println!("   ✓ systemd: {unit} is active");
        return Ok(true);
    }

    println!("   ⚠️  systemd: {unit} is not active");

    if dry_run {
        println!("   --dry-run: would ask to start {unit}; treating as skip for live warm path");
        return Ok(false);
    }

    if !prompt_yes_no(&format!("   Start user unit {unit} now? [Y/n] ")) {
        return Ok(false);
    }

    println!("   … starting {unit} via systemctl --user start …");
    start_user_unit(unit)?;
    std::thread::sleep(std::time::Duration::from_secs(2));
    if is_user_unit_active(unit)? {
        println!("   ✓ systemd: {unit} is now active");
        Ok(true)
    } else {
        Err(format!(
            "started {unit} but is-active still reports inactive (check journalctl --user -u {unit})"
        ))
    }
}

/// `-c` / `--check`: validate loaded config layout and print a clear report per service.
///
/// For each path pair, groups:
/// - service name (systemd user unit)
/// - file directory (`sync` mount path from config)
/// - cache path from the unit's `--cache-dir` (falls back to config `cache` with a warning)
/// - current on-disk size of that cache directory
///
/// Returns `true` if layout is OK (even if some units are missing `--cache-dir`).
pub fn run_config_check(cfg: &Config) -> bool {
    println!();
    println!("════════════════════════════════════════════════════════════");
    println!("  Config check  (−c / --check)");
    println!("════════════════════════════════════════════════════════════");
    println!();
    println!("Schema version:  {}", cfg.version);
    println!("Path pairs:      {}", cfg.paths.len());
    println!(
        "Walk:            max_depth={:?}  min_size={}  max_size={}  max_threads={}",
        cfg.walk.max_depth,
        cfg.walk.min_file_size_bytes,
        cfg.walk.max_file_size_bytes,
        cfg.walk.max_threads
    );
    println!("Ignore names:    {:?}", cfg.ignore.names);
    println!(
        "Mount wait:      initial={}s  retries={:?}  max={}s",
        cfg.mount_wait.initial_secs, cfg.mount_wait.retry_delays_secs, cfg.mount_wait.max_wait_secs
    );
    println!();

    if cfg.paths.is_empty() {
        eprintln!("✗ Config layout invalid: \"paths\" is empty.");
        return false;
    }

    let mut all_ok = true;
    for (i, pair) in cfg.paths.iter().enumerate() {
        let service = pair
            .service
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("(none — no paths[].service in config)");

        println!("────────────────────────────────────────────────────────────");
        println!("  Entry {i}");
        println!("────────────────────────────────────────────────────────────");
        println!();
        println!("  Service name");
        println!("    {service}");
        println!();
        println!("  File directory  (config paths[].sync — mount / warm target)");
        println!("    {}", pair.sync);
        if Path::new(&pair.sync).is_dir() {
            println!("    status: present (directory)");
        } else {
            println!("    status: missing or not a directory");
            all_ok = false;
        }
        println!();
        println!("  Cache directory");
        println!("    config paths[].cache:  {}", pair.cache);

        let unit_cache = if service.starts_with('(') {
            None
        } else {
            match extract_cache_dir_from_unit(service) {
                Ok(p) => Some(p),
                Err(e) => {
                    println!("    from unit --cache-dir:  (not found)  [{e}]");
                    None
                }
            }
        };

        let effective_cache = if let Some(ref uc) = unit_cache {
            println!("    from unit --cache-dir:  {uc}");
            if uc != &pair.cache {
                println!(
                    "    note: config cache differs from unit --cache-dir \
                     (using unit path for size report)"
                );
            }
            uc.as_str()
        } else {
            println!("    effective for size:     {}  (config value)", pair.cache);
            pair.cache.as_str()
        };

        let cache_path = Path::new(effective_cache);
        if cache_path.is_dir() {
            let bytes = cache_ops::dir_size(cache_path);
            println!(
                "    current size:           {}",
                cache_ops::format_bytes(bytes)
            );
        } else if cache_path.exists() {
            println!("    current size:           (path exists but is not a directory)");
            all_ok = false;
        } else {
            println!("    current size:           (directory does not exist yet)");
        }

        if !service.starts_with('(') {
            match is_user_unit_active(service) {
                Ok(true) => println!("  systemd:  active"),
                Ok(false) => println!("  systemd:  inactive"),
                Err(e) => println!("  systemd:  (check failed: {e})"),
            }
        }
        println!();
    }

    println!("════════════════════════════════════════════════════════════");
    if all_ok {
        println!("  Layout check finished — no blocking path issues detected.");
    } else {
        println!("  Layout check finished — review entries marked missing / not a directory.");
    }
    println!("════════════════════════════════════════════════════════════");
    println!();
    all_ok
}

/// Read the user unit definition and extract the rclone `--cache-dir` path.
pub fn extract_cache_dir_from_unit(unit: &str) -> Result<String, String> {
    let out = Command::new("systemctl")
        .args(["--user", "cat", unit])
        .output()
        .map_err(|e| format!("systemctl --user cat {unit}: {e}"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let detail = stderr.trim();
        let detail = if detail.is_empty() {
            "not found or not loaded"
        } else {
            detail
        };
        return Err(format!("cannot read unit {unit}: {detail}"));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    parse_cache_dir_flag(&text).ok_or_else(|| format!("no --cache-dir found in unit {unit}"))
}

/// Parse `--cache-dir PATH` or `--cache-dir=PATH` from unit / ExecStart text.
pub fn parse_cache_dir_flag(text: &str) -> Option<String> {
    const FLAG: &str = "--cache-dir";
    let mut search = text;
    while let Some(idx) = search.find(FLAG) {
        let after = &search[idx + FLAG.len()..];
        let after = after.trim_start();
        let value = if let Some(rest) = after.strip_prefix('=') {
            rest.trim_start()
        } else if after.is_empty() {
            search = &search[idx + FLAG.len()..];
            continue;
        } else {
            after
        };
        // Token until whitespace (unit lines rarely quote paths with spaces)
        let end = value
            .find(|c: char| c.is_whitespace() || c == '"' || c == '\'')
            .unwrap_or(value.len());
        let path = value[..end].trim().trim_matches(|c| c == '"' || c == '\'');
        if !path.is_empty() && path.starts_with('/') {
            return Some(path.to_string());
        }
        search = &search[idx + FLAG.len()..];
    }
    None
}

fn is_user_unit_active(unit: &str) -> Result<bool, String> {
    let out = Command::new("systemctl")
        .args(["--user", "is-active", "--quiet", unit])
        .status()
        .map_err(|e| {
            format!("cannot run systemctl --user (is systemd user session available?): {e}")
        })?;
    Ok(out.success())
}

fn start_user_unit(unit: &str) -> Result<(), String> {
    let out = Command::new("systemctl")
        .args(["--user", "start", unit])
        .output()
        .map_err(|e| format!("systemctl --user start failed to spawn: {e}"))?;
    if out.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    Err(format!(
        "systemctl --user start {unit} failed (exit {:?}): {}",
        out.status.code(),
        stderr.trim()
    ))
}

/// Resolve unit file path and check that the file and its parent directory are readable.
fn check_unit_file_permissions(unit: &str) -> Result<String, String> {
    let out = Command::new("systemctl")
        .args(["--user", "show", "-p", "FragmentPath", "--value", unit])
        .output()
        .map_err(|e| format!("systemctl show FragmentPath: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "could not resolve unit path for {unit} (not installed?)"
        ));
    }
    let path_str = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if path_str.is_empty() {
        return Err(format!("FragmentPath empty for {unit}"));
    }
    let path = PathBuf::from(&path_str);
    if !path.exists() {
        return Err(format!("unit file missing: {path_str}"));
    }
    // Parent directory listing/read metadata
    if let Some(parent) = path.parent() {
        fs::metadata(parent).map_err(|e| {
            permission_message("service unit directory", parent, e, "read metadata / list")
        })?;
        // Attempt readdir on the unit directory (permission probe)
        fs::read_dir(parent)
            .map_err(|e| permission_message("service unit directory", parent, e, "list entries"))?;
    }
    // Unit file must be readable
    let mut f = File::open(&path)
        .map_err(|e| permission_message("service unit file", &path, e, "open for read"))?;
    let mut buf = [0u8; 1];
    let _ = f
        .read(&mut buf)
        .map_err(|e| permission_message("service unit file", &path, e, "read"))?;
    Ok(format!("readable ({path_str})"))
}

/// Sync tree must exist as a directory and allow listing (read access).
fn check_sync_readable(sync: &Path) -> Result<String, String> {
    let meta = fs::metadata(sync)
        .map_err(|e| permission_message("sync (source) directory", sync, e, "stat / access"))?;
    if !meta.is_dir() {
        return Err(format!("sync path is not a directory: {}", sync.display()));
    }
    let mut rd = fs::read_dir(sync)
        .map_err(|e| permission_message("sync (source) directory", sync, e, "list / read"))?;
    // Consume one entry if present to prove readdir works end-to-end
    let _ = rd.next();
    Ok("directory readable (list OK)".into())
}

/// Cache dir: confirm dir, then create a probe file, write, read back, delete.
fn check_cache_permissions(cache: &Path) -> Result<String, String> {
    let meta = fs::metadata(cache)
        .map_err(|e| permission_message("rclone cache directory", cache, e, "stat / access"))?;
    if !meta.is_dir() {
        return Err(format!(
            "cache path is not a directory: {}",
            cache.display()
        ));
    }

    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let probe_name = format!(".warm-drive-cache-probe-{stamp}");
    let probe = cache.join(&probe_name);

    // Create + write
    {
        let mut f = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&probe)
            .map_err(|e| {
                permission_message(
                    "rclone cache directory",
                    cache,
                    e,
                    "create/write probe file",
                )
            })?;
        f.write_all(b"wdc-ok")
            .map_err(|e| permission_message("rclone cache probe file", &probe, e, "write"))?;
        f.sync_all().ok();
    }

    // Read back
    {
        let mut f = File::open(&probe).map_err(|e| {
            permission_message("rclone cache probe file", &probe, e, "open for read")
        })?;
        let mut buf = String::new();
        f.read_to_string(&mut buf)
            .map_err(|e| permission_message("rclone cache probe file", &probe, e, "read"))?;
        if buf != "wdc-ok" {
            let _ = fs::remove_file(&probe);
            return Err(format!(
                "cache probe read-back mismatch at {} (got {buf:?})",
                probe.display()
            ));
        }
    }

    // Delete
    fs::remove_file(&probe)
        .map_err(|e| permission_message("rclone cache probe file", &probe, e, "delete"))?;

    Ok("write/read/delete probe OK".into())
}

fn permission_message(what: &str, path: &Path, err: io::Error, op: &str) -> String {
    let kind = err.kind();
    let hint = match kind {
        io::ErrorKind::PermissionDenied => {
            "permission denied — check ownership (chown) and mode (chmod); \
             for FUSE mounts ensure the mount is up and you are the mounting user"
        }
        io::ErrorKind::NotFound => {
            "path not found — create the directory or start the mount/service first"
        }
        io::ErrorKind::NotADirectory => "not a directory",
        _ => "see OS error detail",
    };
    format!("{what} {}: cannot {op}: {err} ({hint})", path.display())
}

/// Pacman-style Y/n (default yes). Empty line or y/Y → true.
fn prompt_yes_no(prompt: &str) -> bool {
    eprint!("{prompt}");
    let _ = io::stderr().flush();
    let mut input = String::new();
    match io::stdin().read_line(&mut input) {
        Ok(_) => {
            let s = input.trim().to_lowercase();
            s.is_empty() || s.starts_with('y')
        }
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn check_sync_readable_on_tempdir() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("f"), b"x").unwrap();
        assert!(check_sync_readable(tmp.path()).is_ok());
    }

    #[test]
    fn check_cache_permissions_probe_cycle() {
        let tmp = TempDir::new().unwrap();
        let msg = check_cache_permissions(tmp.path()).expect("probe");
        assert!(msg.contains("probe OK"));
        // No leftover probe files
        let left: Vec<_> = fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        assert!(left.is_empty());
    }

    #[test]
    fn check_sync_missing_errors() {
        let err = check_sync_readable(Path::new("/no/such/sync/path/wdc-test")).unwrap_err();
        assert!(err.contains("not found") || err.contains("sync"));
    }

    #[test]
    fn parse_cache_dir_flag_variants() {
        assert_eq!(
            parse_cache_dir_flag(
                "ExecStart=/usr/bin/rclone mount --cache-dir /var/cache/r rem: /mnt"
            ),
            Some("/var/cache/r".into())
        );
        assert_eq!(
            parse_cache_dir_flag("foo --cache-dir=/home/user/.rclone_cache bar"),
            Some("/home/user/.rclone_cache".into())
        );
        assert!(parse_cache_dir_flag("no flag here").is_none());
    }
}
