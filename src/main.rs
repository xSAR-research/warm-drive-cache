//! warm-drive-cache
//!
//! Rust utility for maintenance of rclone FUSE mount cache directories.
//!
//! A product of xSAR — https://xSAR.com.au
//! Licensed under the GNU Affero General Public License v3.0 only (AGPL-3.0-only).
//! See the `LICENSE` file in the repository for the full licence text.
//!
//! Module map (see README mermaid “Program Flow”):
//! - [`startup`] — product banner + print resolved config
//! - [`config`] — JSON load / validation (`warm-drive-cache.json`)
//! - [`cache_check`] — systemd unit status, start prompt, FS permission probes
//! - [`shutdown`] — SIGINT / TTY `q` graceful stop
//! - [`cache_ops`] — cache size + non-interactive delete
//! - [`worker`] — parallel sync-tree warm (READ / ATTR)
//! - [`warm_log`] — optional `/tmp` CSV log (`-l` / `--log`)
//! - [`cleanup`] — end-of-run summary, thanks, GitHub issues link
//! - [`mount_wait`] — optional FUSE settle helpers

mod cache_check;
mod cache_lock;
mod cache_ops;
mod cleanup;
mod config;
mod dirty_check;
mod mount_wait;
mod shutdown;
mod startup;
mod warm_log;
mod worker;

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

fn main() {
    // Early CLI: -h/--help, -i/--information, -j/--json exit before (or without) maintenance.
    match startup::parse_cli_args(std::env::args().skip(1)) {
        Err(e) => {
            eprintln!("❌ command-line error: {e}");
            eprintln!("Try --help for usage.");
            std::process::exit(2);
        }
        Ok(startup::CliAction::Help) => startup::print_help(),
        Ok(startup::CliAction::Information) => startup::print_information(),
        Ok(startup::CliAction::JsonValidation { overrides }) => run_check(overrides),
        Ok(startup::CliAction::Run {
            dry_run,
            verbose,
            log,
            overrides,
        }) => {
            if !run(dry_run, verbose, log, overrides) {
                std::process::exit(1);
            }
        }
    }
}

/// Load and validate warm-drive-cache.json, then print a grouped service / path / cache report.
fn apply_overrides(cfg: &mut config::Config, o: startup::CliOverrides) -> Result<(), String> {
    if let Some(v) = o.threads {
        cfg.walk.max_threads = v;
    }
    if let Some(v) = o.size {
        cfg.walk.max_file_size_bytes = v;
    }
    if let Some(v) = o.checksum {
        cfg.walk.checksum = v;
    }
    config::validate_effective(cfg)
}

fn run_check(overrides: startup::CliOverrides) {
    startup::print_startup_banner();
    let mut cfg = match config::load() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("❌ warm-drive-cache configuration error: {}", e);
            eprintln!(
                "   Place warm-drive-cache.json next to the executable, or set WARM_DRIVE_CACHE_CONFIG."
            );
            cleanup::print_exit_summary(false);
            startup::print_mount_modification_warning();
            std::process::exit(1);
        }
    };
    if let Err(e) = apply_overrides(&mut cfg, overrides) {
        eprintln!("❌ effective configuration error: {e}");
        startup::print_mount_modification_warning();
        std::process::exit(1);
    }
    if cfg.paths.is_empty() {
        eprintln!("❌ No paths configured in warm-drive-cache.json.");
        cleanup::print_exit_summary(false);
        startup::print_mount_modification_warning();
        std::process::exit(1);
    }
    let ok = cache_check::run_config_check(&cfg);
    cleanup::print_exit_summary(false);
    startup::print_mount_modification_warning();
    if !ok {
        std::process::exit(1);
    }
}

fn run(dry_run: bool, verbose: bool, log: bool, overrides: startup::CliOverrides) -> bool {
    // Banner: product of xSAR, website, AGPL-3.0-only (baked from Cargo.toml).
    startup::print_startup_banner();

    // JSON config (run-dir / env / XDG). Paths are secrets — never hardcoded.
    let mut cfg = match config::load() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("❌ warm-drive-cache configuration error: {}", e);
            eprintln!(
                "   Typical location: warm-drive-cache.json next to the binary (run directory)"
            );
            eprintln!(
                "   Copy from warm-drive-cache-example.json and set your local paths (warm-drive-cache.json is gitignored)."
            );
            eprintln!("   Override with: WARM_DRIVE_CACHE_CONFIG=/path/to/warm-drive-cache.json");
            eprintln!("   See the configuration section in the README.");
            cleanup::print_exit_summary(false);
            return false;
        }
    };
    if let Err(e) = apply_overrides(&mut cfg, overrides) {
        eprintln!("❌ effective configuration error: {e}");
        return false;
    }

    if cfg.paths.is_empty() {
        eprintln!("❌ No paths configured.");
        eprintln!(
            "   Create warm-drive-cache.json (gitignored) from warm-drive-cache-example.json with at least one path pair \
             (sync, cache, optional service)."
        );
        eprintln!("   Example is documented in the README.");
        cleanup::print_exit_summary(false);
        return false;
    }

    let shutdown = Arc::new(AtomicBool::new(false));
    shutdown::install_sigint_handler(Arc::clone(&shutdown));

    // Acquire one atomic warning lock per distinct cache before any cache is inspected or changed.
    let mut cache_locks = Vec::new();
    let mut locked_paths = std::collections::HashSet::new();
    for pair in &cfg.paths {
        if locked_paths.insert(pair.cache.clone()) {
            match cache_lock::CacheLock::acquire(Path::new(&pair.cache)) {
                Ok(lock) => {
                    if verbose {
                        println!("   Concurrency lock: {}", lock.path().display());
                    }
                    cache_locks.push(lock);
                }
                Err(e) => {
                    eprintln!("❌ Concurrency protection: {e}");
                    cleanup::print_exit_summary(true);
                    drop(cache_locks);
                    return false;
                }
            }
        }
    }

    if verbose {
        startup::print_loaded_config(&cfg);
    }

    let warm_log = if log {
        match warm_log::WarmLog::create() {
            Ok(l) => {
                println!("   CSV log: {}", l.path().display());
                Some(Arc::new(l))
            }
            Err(e) => {
                eprintln!("❌ warm-drive-cache log error: {e}");
                cleanup::print_exit_summary(false);
                drop(cache_locks);
                return false;
            }
        }
    } else {
        None
    };

    // Installed after interactive pre-flight prompts so Y/n uses cooked stdin.
    let mut stdin_guard = None;
    let mut fatal_error = false;

    for pair in &cfg.paths {
        if shutdown.load(Ordering::SeqCst) {
            println!("\n⏹  Shutdown requested — skipping remaining path pairs.");
            break;
        }

        // Service status, optional start (daemon-reload/enable/active), settle, permission probes.
        if !cache_check::check_path_pair(pair, dry_run, verbose, &cfg.mount_wait) {
            continue;
        }

        let sync_path = Path::new(&pair.sync);
        let cache_path = Path::new(&pair.cache);

        let before = cache_ops::dir_size(cache_path);
        println!("\n📂 Sync dir (traverse/warm only): {}", pair.sync);
        println!("   Cache dir (size/delete only): {}", pair.cache);
        println!(
            "   Checksum verification: {}",
            if cfg.walk.checksum {
                "enabled"
            } else {
                "disabled"
            }
        );
        if let Some(svc) = &pair.service {
            println!("   systemd unit: {svc}");
        }
        println!(
            "   Before size (cache): {}",
            cache_ops::format_bytes(before)
        );

        if dry_run {
            println!("   --dry-run enabled: simulating full deletion (no changes made)");
            let _ = cache_ops::delete_dir_contents(cache_path, true);
            println!(
                "   After size (simulated, cache): {}",
                cache_ops::format_bytes(0)
            );
            continue;
        }

        if stdin_guard.is_none() {
            stdin_guard = shutdown::install_quit_listener(Arc::clone(&shutdown));
        }

        println!("   Checking rclone vfsMeta entries for unsaved files...");
        if let Err(e) =
            dirty_check::wait_until_clean(cache_path, cfg.mount_wait.max_wait_secs, &shutdown)
        {
            eprintln!("❌ Cache purge cancelled: {e}");
            fatal_error = true;
            break;
        }
        println!("   ✓ No Dirty=true rclone metadata entries remain.");

        println!("   Performing complete deletion of all files and subdirectories in cache dir...");
        let deleted = cache_ops::delete_dir_contents(cache_path, false);
        let after_delete = cache_ops::dir_size(cache_path);
        println!(
            "   After deletion size (cache): {} (deleted {})",
            cache_ops::format_bytes(after_delete),
            cache_ops::format_bytes(deleted)
        );

        println!(
            "   Walking sync dir (max_threads={}, min_size={}, max_size={})...",
            cfg.walk.max_threads,
            cache_ops::format_bytes(cfg.walk.min_file_size_bytes),
            cache_ops::format_max_file_size_limit(cfg.walk.max_file_size_bytes)
        );
        startup::print_mount_modification_warning();
        println!();
        let service_name = pair
            .service
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("");
        let mut status =
            worker::warm_tree(sync_path, &cfg, &shutdown, service_name, warm_log.as_ref());
        status.finish_line();

        let after_warm = cache_ops::dir_size(cache_path);
        println!(
            "   Size after warming (cache): {}",
            cache_ops::format_bytes(after_warm)
        );
        println!("   Directories processed: {}", status.dirs);
        println!("   Files processed: {}", status.files);
        println!("   File contents read: {}", status.byte_reads);
        println!("   Metadata-only: {}", status.metadata_only);
        println!("   Errors: {}", status.errors);
        if status.cancelled {
            println!(
                "   Stopped early (SIGINT or q) — in-flight workers finished; no new files started."
            );
        }
    }

    // Keep guard alive until here (Drop restores termios).
    drop(stdin_guard);

    if let Some(ref log) = warm_log {
        let _ = log.flush();
    }

    cleanup::print_exit_summary(shutdown.load(Ordering::SeqCst));

    // After the normal exit summary: blank line then CSV path when logging was enabled.
    if let Some(ref log) = warm_log {
        println!();
        println!("CSV log written to: {}", log.path().display());
    }

    // Lock removal is deliberately the final filesystem operation on a normal/graceful exit.
    drop(warm_log);
    drop(cache_locks);
    !fatal_error
}
