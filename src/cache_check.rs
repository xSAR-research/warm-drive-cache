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
use crate::config::{Config, MountWait, PathPair};
use crate::mount_wait;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

/// Whether a unit is managed by system systemd or the per-user instance.
///
/// rclone mounts may be either:
/// - **system** units under `/etc/systemd/system/` (common when installed with
///   `User=` in the unit and enabled for multi-user.target)
/// - **user** units under `~/.config/systemd/user/` (`systemctl --user`)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SystemdScope {
    System,
    User,
}

impl SystemdScope {
    fn label(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::User => "user",
        }
    }

    /// Extra argv before the systemctl verb (`--user` or nothing).
    fn flag(self) -> &'static [&'static str] {
        match self {
            Self::System => &[],
            Self::User => &["--user"],
        }
    }
}

/// Run service status, optional start prompt, settle wait, and permission probes for one pair.
///
/// Returns `true` when main may delete/warm this pair.
/// Call **before** installing raw-mode quit handlers so Y/n prompts use normal stdin.
///
/// When `verbose` is false, successful pre-flight detail is suppressed; failures and
/// interactive start prompts are always shown.
pub fn check_path_pair(
    pair: &PathPair,
    dry_run: bool,
    verbose: bool,
    mount_wait_cfg: &MountWait,
) -> bool {
    if verbose {
        println!("\n🔎 Pre-flight checks");
        println!("   sync:  {}", pair.sync);
        println!("   cache: {}", pair.cache);
    }

    if let Some(svc) = pair
        .service
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        match resolve_unit_scope(svc) {
            Ok(scope) => {
                if verbose {
                    println!("   unit:  {svc} (systemd {})", scope.label());
                }
                match ensure_service_ready(svc, scope, dry_run, verbose) {
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
            }
            Err(e) => {
                if verbose {
                    println!("   unit:  {svc}");
                }
                println!("   ⚠️  systemd: {e}");
                // Still attempt FS checks; user may have a manual mount.
            }
        }
    } else if verbose {
        println!("   unit:  (none configured — skipping systemd checks)");
    }

    // Service enable/active must complete before mount settle.
    let _ = mount_wait::wait_for_mount_content(Path::new(&pair.sync), mount_wait_cfg, verbose);

    let mut ok = true;

    match check_sync_readable(Path::new(&pair.sync)) {
        Ok(msg) => {
            if verbose {
                println!("   ✓ sync access: {msg}");
            }
        }
        Err(e) => {
            println!("   ✗ sync access: {e}");
            ok = false;
        }
    }

    match check_cache_permissions(Path::new(&pair.cache)) {
        Ok(msg) => {
            if verbose {
                println!("   ✓ cache access: {msg}");
            }
        }
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
            Ok(msg) => {
                if verbose {
                    println!("   ✓ unit file: {msg}");
                }
            }
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

    if verbose {
        println!("   ✓ Pre-flight OK — proceeding with cache maintenance for this pair.");
    }
    true
}

/// Ensure the unit is active; if not, ask (or simulate in dry-run) to start it.
///
/// On user-confirmed start: `daemon-reload` → `enable` → `start`, then verify
/// **enabled** and **active** (before any mount settle wait). System units retry
/// with `sudo` when permission is denied.
///
/// Returns Ok(true) if active (or started), Ok(false) if inactive and user declined.
fn ensure_service_ready(
    unit: &str,
    scope: SystemdScope,
    dry_run: bool,
    verbose: bool,
) -> Result<bool, String> {
    let active = is_unit_active(unit, scope)?;
    if active {
        if verbose {
            let en = is_unit_enabled(unit, scope).unwrap_or(false);
            println!(
                "   ✓ systemd ({}): {unit} is active{}",
                scope.label(),
                if en { " and enabled" } else { "" }
            );
        }
        return Ok(true);
    }

    // Always surface inactive state (needs a decision).
    println!(
        "   ⚠️  systemd ({}): {unit} is not active",
        scope.label()
    );

    if dry_run {
        println!("   --dry-run: would ask to start {unit}; treating as skip for live warm path");
        return Ok(false);
    }

    let start_hint = match scope {
        SystemdScope::User => format!("Start user unit {unit} now? [Y/n] "),
        SystemdScope::System => {
            format!("Start system unit {unit} now? (may require sudo) [Y/n] ")
        }
    };
    if !prompt_yes_no(&format!("   {start_hint}")) {
        return Ok(false);
    }

    println!(
        "   … systemd {}: daemon-reload, enable, start for {unit} …",
        scope.label()
    );
    daemon_reload(scope)?;
    enable_unit(unit, scope)?;
    start_unit(unit, scope)?;

    // Verify enabled + active before any settle/wait section.
    let enabled = is_unit_enabled(unit, scope)?;
    let active_now = is_unit_active(unit, scope)?;
    if enabled && active_now {
        println!(
            "   ✓ systemd ({}): {unit} is enabled and active",
            scope.label()
        );
        return Ok(true);
    }

    let journal = match scope {
        SystemdScope::User => format!("journalctl --user -u {unit}"),
        SystemdScope::System => format!("journalctl -u {unit}"),
    };
    Err(format!(
        "after enable/start, {unit} is enabled={enabled} active={active_now} \
         (check {journal}; system units may need sudo / polkit)"
    ))
}

/// `-c` / `--check`: validate loaded config layout and print a clear report per service.
///
/// For each path pair, groups:
/// - service name (systemd system or user unit)
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
        cache_ops::format_bytes(cfg.walk.min_file_size_bytes),
        cache_ops::format_max_file_size_limit(cfg.walk.max_file_size_bytes),
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
            match resolve_unit_scope(service) {
                Ok(scope) => match is_unit_active(service, scope) {
                    Ok(true) => println!("  systemd:  active ({})", scope.label()),
                    Ok(false) => println!("  systemd:  inactive ({})", scope.label()),
                    Err(e) => println!("  systemd:  (check failed: {e})"),
                },
                Err(e) => println!("  systemd:  (not found: {e})"),
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

/// Read the unit definition (system or user) and extract the rclone `--cache-dir` path.
pub fn extract_cache_dir_from_unit(unit: &str) -> Result<String, String> {
    let scope = resolve_unit_scope(unit)?;
    let out = systemctl_output(scope, &["cat", unit])
        .map_err(|e| format!("systemctl {} cat {unit}: {e}", scope.label()))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let detail = stderr.trim();
        let detail = if detail.is_empty() {
            "not found or not loaded"
        } else {
            detail
        };
        return Err(format!(
            "cannot read {} unit {unit}: {detail}",
            scope.label()
        ));
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

/// Resolve whether `unit` is a system or user unit via `LoadState`.
///
/// Prefers the scope where `LoadState=loaded`. If both are loaded (rare),
/// prefers the scope that is currently active, then system.
fn resolve_unit_scope(unit: &str) -> Result<SystemdScope, String> {
    let system_loaded = unit_load_state(unit, SystemdScope::System)
        .map(|s| s == "loaded")
        .unwrap_or(false);
    let user_loaded = unit_load_state(unit, SystemdScope::User)
        .map(|s| s == "loaded")
        .unwrap_or(false);

    match (system_loaded, user_loaded) {
        (true, false) => Ok(SystemdScope::System),
        (false, true) => Ok(SystemdScope::User),
        (true, true) => {
            // Prefer whichever is currently active; default to system.
            if is_unit_active(unit, SystemdScope::System).unwrap_or(false) {
                Ok(SystemdScope::System)
            } else if is_unit_active(unit, SystemdScope::User).unwrap_or(false) {
                Ok(SystemdScope::User)
            } else {
                Ok(SystemdScope::System)
            }
        }
        (false, false) => Err(format!(
            "unit {unit} not found as a system or user unit \
             (check the name; drop a mistaken rclone- prefix if the unit is gdrive-*.service)"
        )),
    }
}

fn unit_load_state(unit: &str, scope: SystemdScope) -> Result<String, String> {
    let out = systemctl_output(scope, &["show", "-p", "LoadState", "--value", unit])
        .map_err(|e| format!("systemctl {} show LoadState: {e}", scope.label()))?;
    if !out.status.success() {
        return Err(format!(
            "systemctl {} show LoadState failed for {unit}",
            scope.label()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn is_unit_active(unit: &str, scope: SystemdScope) -> Result<bool, String> {
    let status = systemctl_status(scope, &["is-active", "--quiet", unit]).map_err(|e| {
        format!(
            "cannot run systemctl {} is-active (is systemd available?): {e}",
            scope.label()
        )
    })?;
    Ok(status.success())
}

fn is_unit_enabled(unit: &str, scope: SystemdScope) -> Result<bool, String> {
    // is-enabled: 0 = enabled (or enabled-runtime / static treated as success for --quiet varies).
    // Use non-quiet and accept "enabled" / "enabled-runtime" / "static" / "alias" as OK.
    let out = systemctl_output(scope, &["is-enabled", unit]).map_err(|e| {
        format!(
            "cannot run systemctl {} is-enabled: {e}",
            scope.label()
        )
    })?;
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let ok = matches!(
        text.as_str(),
        "enabled" | "enabled-runtime" | "static" | "indirect" | "generated" | "alias"
    ) || out.status.success();
    Ok(ok)
}

fn daemon_reload(scope: SystemdScope) -> Result<(), String> {
    systemctl_mut(scope, &["daemon-reload"]).map(|_| ())
}

fn enable_unit(unit: &str, scope: SystemdScope) -> Result<(), String> {
    systemctl_mut(scope, &["enable", unit]).map(|_| ())
}

fn start_unit(unit: &str, scope: SystemdScope) -> Result<(), String> {
    systemctl_mut(scope, &["start", unit]).map(|_| ())
}

/// Read-only / status systemctl (no sudo).
fn systemctl_output(scope: SystemdScope, args: &[&str]) -> io::Result<std::process::Output> {
    let mut cmd = Command::new("systemctl");
    cmd.args(scope.flag());
    cmd.args(args);
    cmd.output()
}

fn systemctl_status(scope: SystemdScope, args: &[&str]) -> io::Result<std::process::ExitStatus> {
    let mut cmd = Command::new("systemctl");
    cmd.args(scope.flag());
    cmd.args(args);
    cmd.status()
}

/// Mutating systemctl: try as current user; for **system** units retry with `sudo` on failure.
fn systemctl_mut(scope: SystemdScope, args: &[&str]) -> Result<std::process::Output, String> {
    let out = systemctl_output(scope, args).map_err(|e| {
        format!(
            "systemctl {} {} failed to spawn: {e}",
            scope.label(),
            args.join(" ")
        )
    })?;
    if out.status.success() {
        return Ok(out);
    }

    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
    if matches!(scope, SystemdScope::System) {
        println!(
            "   … elevating with sudo for: systemctl {} …",
            args.join(" ")
        );
        let mut cmd = Command::new("sudo");
        cmd.arg("systemctl");
        cmd.args(args);
        let out2 = cmd.output().map_err(|e| {
            format!(
                "sudo systemctl {} failed to spawn (is sudo available?): {e}",
                args.join(" ")
            )
        })?;
        if out2.status.success() {
            return Ok(out2);
        }
        let stderr2 = String::from_utf8_lossy(&out2.stderr).trim().to_string();
        return Err(format!(
            "systemctl {} failed (exit {:?}): {stderr}; sudo retry (exit {:?}): {stderr2}",
            args.join(" "),
            out.status.code(),
            out2.status.code()
        ));
    }

    Err(format!(
        "systemctl --user {} failed (exit {:?}): {stderr}",
        args.join(" "),
        out.status.code()
    ))
}

/// Resolve unit file path and check that the file and its parent directory are readable.
fn check_unit_file_permissions(unit: &str) -> Result<String, String> {
    let scope = resolve_unit_scope(unit)?;
    let out = systemctl_output(
        scope,
        &["show", "-p", "FragmentPath", "--value", unit],
    )
    .map_err(|e| format!("systemctl {} show FragmentPath: {e}", scope.label()))?;
    if !out.status.success() {
        return Err(format!(
            "could not resolve unit path for {unit} (not installed as {} unit?)",
            scope.label()
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
    Ok(format!("readable ({path_str}, {} scope)", scope.label()))
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
