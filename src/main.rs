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
//! - [`config`] — JSON load / validation (`config.json`)
//! - [`cache_check`] — systemd unit status, start prompt, FS permission probes
//! - [`shutdown`] — SIGINT / TTY `q` graceful stop
//! - [`cache_ops`] — cache size + non-interactive delete
//! - [`worker`] — parallel sync-tree warm (READ / ATTR)
//! - [`warm_log`] — optional `/tmp` CSV log (`-l` / `--log`)
//! - [`cleanup`] — end-of-run summary, thanks, GitHub issues link
//! - [`mount_wait`] — optional FUSE settle helpers

mod cache_check;
mod cache_ops;
mod cleanup;
mod config;
mod mount_wait;
mod shutdown;
mod startup;
mod warm_log;
mod worker;

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

fn main() {
    // Early CLI: -h/--help, -i/--information, -c/--check exit before (or without) maintenance.
    match startup::parse_cli_args(std::env::args().skip(1)) {
        startup::CliAction::Help => startup::print_help(),
        startup::CliAction::Information => startup::print_information(),
        startup::CliAction::Check => run_check(),
        startup::CliAction::Run {
            dry_run,
            verbose,
            log,
        } => run(dry_run, verbose, log),
    }
}

/// Load and validate config.json, then print a grouped service / path / cache report.
fn run_check() {
    startup::print_startup_banner();
    let cfg = match config::load() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("❌ warm-drive-cache configuration error: {}", e);
            eprintln!(
                "   Place config.json next to the executable, or set WARM_DRIVE_CACHE_CONFIG."
            );
            cleanup::print_exit_summary(false);
            std::process::exit(1);
        }
    };
    if cfg.paths.is_empty() {
        eprintln!("❌ No paths configured in config.json.");
        cleanup::print_exit_summary(false);
        std::process::exit(1);
    }
    let ok = cache_check::run_config_check(&cfg);
    cleanup::print_exit_summary(false);
    if !ok {
        std::process::exit(1);
    }
}

fn run(dry_run: bool, verbose: bool, log: bool) {
    // Banner: product of xSAR, website, AGPL-3.0-only (baked from Cargo.toml).
    startup::print_startup_banner();

    // JSON config (run-dir / env / XDG). Paths are secrets — never hardcoded.
    let cfg = match config::load() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("❌ warm-drive-cache configuration error: {}", e);
            eprintln!("   Typical location: config.json next to the binary (run directory)");
            eprintln!(
                "   Copy from config.example.json and set your local paths (config.json is gitignored)."
            );
            eprintln!("   Override with: WARM_DRIVE_CACHE_CONFIG=/path/to/config.json");
            eprintln!("   See the configuration section in the README.");
            cleanup::print_exit_summary(false);
            std::process::exit(1);
        }
    };

    if cfg.paths.is_empty() {
        eprintln!("❌ No paths configured.");
        eprintln!(
            "   Create config.json (gitignored) from config.example.json with at least one path pair \
             (sync, cache, optional service)."
        );
        eprintln!("   Example is documented in the README.");
        cleanup::print_exit_summary(false);
        std::process::exit(1);
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
                std::process::exit(1);
            }
        }
    } else {
        None
    };

    let shutdown = Arc::new(AtomicBool::new(false));
    // Installed after interactive pre-flight prompts so Y/n uses cooked stdin.
    let mut stdin_guard = None;

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
            stdin_guard = Some(shutdown::install_graceful_shutdown(Arc::clone(&shutdown)));
        }

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
        println!();
        let service_name = pair
            .service
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("");
        let mut status = worker::warm_tree(
            sync_path,
            &cfg,
            &shutdown,
            service_name,
            warm_log.as_ref(),
        );
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
}
