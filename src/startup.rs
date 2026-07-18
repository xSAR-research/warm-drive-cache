//! Startup banner, CLI help/information, and compile-time product / licence identity.
//!
//! Version strings are baked from Cargo.toml via `env!("CARGO_PKG_*")`.

use crate::cache_ops;
use crate::config::Config;

/// Package name from Cargo.toml.
pub const PKG_NAME: &str = env!("CARGO_PKG_NAME");

/// Program version — coded into the binary from Cargo.toml `version`.
pub const PKG_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Human-facing codebase version string (used by `-i` / `--information`).
/// Kept aligned with [`PKG_VERSION`] / Cargo.toml unless a marketing label is preferred.
pub const CODEBASE_VERSION: &str = PKG_VERSION;

/// Codebase release date baked into the binary (universal short English form).
/// Update this constant when cutting a release; not derived from the host clock.
pub const CODEBASE_RELEASE: &str = "18th July, 2026";

/// SPDX licence id from Cargo.toml (`license = "AGPL-3.0-only"`).
pub const PKG_LICENSE: &str = env!("CARGO_PKG_LICENSE");

/// Project homepage from Cargo.toml.
pub const PKG_HOMEPAGE: &str = env!("CARGO_PKG_HOMEPAGE");

/// Source repository from Cargo.toml.
pub const PKG_REPOSITORY: &str = env!("CARGO_PKG_REPOSITORY");

/// Human-readable product owner.
pub const PRODUCT_OF: &str = "xSAR";

/// Public website for the product line.
pub const XSAR_WEBSITE: &str = "https://xSAR.com.au";

/// Embedded copy of `config.example.json` (no secrets) for `--help`.
const CONFIG_EXAMPLE_JSON: &str = include_str!("../config.example.json");

/// Result of early CLI flag handling (`-h` / `-i` / `-c` / `-v` / `-l` / normal run).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CliAction {
    /// Continue with normal maintenance run.
    Run {
        dry_run: bool,
        verbose: bool,
        /// Write a time-stamped CSV warm log under `/tmp/`.
        log: bool,
    },
    /// Printed help and should exit 0.
    Help,
    /// Printed product information (version, licence, links) and should exit 0.
    Information,
    /// Validate config.json layout and report service / paths / cache sizes.
    Check,
}

/// Parse process args for help, information, check, verbose, log, and dry-run.
///
/// Unknown flags other than the above are ignored so existing invocations keep working.
pub fn parse_cli_args<I, S>(args: I) -> CliAction
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut dry_run = false;
    let mut help = false;
    let mut information = false;
    let mut check = false;
    let mut verbose = false;
    let mut log = false;

    for arg in args {
        match arg.as_ref() {
            "-h" | "--help" => help = true,
            "-i" | "--information" => information = true,
            "-c" | "--check" => check = true,
            "-v" | "--verbose" => verbose = true,
            "-l" | "--log" => log = true,
            "--dry-run" => dry_run = true,
            _ => {}
        }
    }

    // Precedence: help → information → check → run
    if help {
        CliAction::Help
    } else if information {
        CliAction::Information
    } else if check {
        CliAction::Check
    } else {
        CliAction::Run {
            dry_run,
            verbose,
            log,
        }
    }
}

/// `-i` / `--information`: codebase version + release date, licence, repo, and website.
pub fn print_information() {
    println!("{PKG_NAME}");
    println!("Codebase Version: {CODEBASE_VERSION}");
    println!("Codebase release: {CODEBASE_RELEASE}");
    println!("A product of {PRODUCT_OF}");
    println!("Licence: {PKG_LICENSE} by {PRODUCT_OF}");
    println!("Website: {XSAR_WEBSITE}");
    if !PKG_REPOSITORY.is_empty() {
        println!("Repository: {PKG_REPOSITORY}");
    }
    if !PKG_HOMEPAGE.is_empty() && PKG_HOMEPAGE != XSAR_WEBSITE {
        println!("Homepage: {PKG_HOMEPAGE}");
    }
}

/// `-h` / `--help`: brief usage, where config lives, and the example `config.json` schema.
pub fn print_help() {
    println!("{PKG_NAME} — Codebase Version: {CODEBASE_VERSION} ({CODEBASE_RELEASE})");
    println!("rclone FUSE cache maintenance (product of {PRODUCT_OF})");
    println!();
    println!("Usage:");
    println!("  {PKG_NAME} [OPTIONS]");
    println!();
    println!("Options:");
    println!("  -h, --help          Show this help and example configuration");
    println!("  -i, --information   Show version, release date, licence, and project links");
    println!("  -c, --check         Validate config.json and list services / paths / cache sizes");
    println!("  -v, --verbose       Show Configuration and Pre-flight checks detail");
    println!("  -l, --log           Write time-stamped CSV log under /tmp (READ/ATTRIB per file)");
    println!("      --dry-run       Simulate cache deletion only (no writes / no warm)");
    println!();
    println!("Configuration:");
    println!("  Place a file named config.json next to the executable (same directory).");
    println!("  Copy from config.example.json (shipped with the binary/source) and set your");
    println!("  absolute sync/cache paths and optional systemd unit names (system or --user).");
    println!("  Alternatives: WARM_DRIVE_CACHE_CONFIG=/path/to.json or XDG config dir.");
    println!();
    println!("walk.max_file_size_bytes special values:");
    println!("  -1  metadata only (no File contents read for any file)");
    println!("   0  File contents read for every file (any size)");
    println!("  N>0 File contents read when file size is within min..N window");
    println!();
    println!("Sizes may be JSON numbers (bytes) or strings with units (case-insensitive):");
    println!("  65536 | \"64KiB\" | \"64K\" | \"64KB\" | \"1MiB\" | \"1M\" | \"512B\"");
    println!("  Units: B, K/KB/KiB, M/MB/MiB, G/GB/GiB, T/TB/TiB, P/PB/PiB (binary 1024).");
    println!();
    println!("Example config.json (placeholders only — no secrets):");
    println!("{CONFIG_EXAMPLE_JSON}");
    println!("For full documentation see README.md in the repository:");
    if !PKG_REPOSITORY.is_empty() {
        println!("  {PKG_REPOSITORY}");
        println!("  {PKG_REPOSITORY}/blob/main/README.md");
    } else {
        println!("  {XSAR_WEBSITE}");
    }
    println!("Website: {XSAR_WEBSITE}");
}

/// Interior width of the startup identity box (characters between the side borders).
const BANNER_BOX_INNER: usize = 65;

/// One closed box row: `│` + padded content + `│`.
fn print_banner_box_row(content: &str) {
    let mut inner: String = content.chars().take(BANNER_BOX_INNER).collect();
    let pad = BANNER_BOX_INNER.saturating_sub(inner.chars().count());
    if pad > 0 {
        inner.push_str(&" ".repeat(pad));
    }
    println!("│{inner}│");
}

/// Startup identity block: product, website, and compile-time licence metadata.
pub fn print_startup_banner() {
    // Tagline first (no indent), then blank line, then closed identity box.
    println!("Rust utility for removing rclone cache staleness and warming mounts.");
    println!(
        "Quit gracefully: Ctrl+C (SIGINT) or press q (TTY) — finishes in-flight workers, starts no new work."
    );
    println!();

    let rule = "─".repeat(BANNER_BOX_INNER);
    println!("┌{rule}┐");
    print_banner_box_row(&format!("  {PKG_NAME}"));
    print_banner_box_row(&format!("  Codebase Version: {CODEBASE_VERSION}"));
    print_banner_box_row(&format!("  Codebase release: {CODEBASE_RELEASE}"));
    print_banner_box_row(&format!("  Website: {XSAR_WEBSITE}"));
    print_banner_box_row(&format!("  Licence: {PKG_LICENSE} (see LICENSE file)"));
    print_banner_box_row(&format!("  Homepage: {PKG_HOMEPAGE}"));
    if !PKG_REPOSITORY.is_empty() {
        print_banner_box_row(&format!("  Source:  {PKG_REPOSITORY}"));
    }
    println!("└{rule}┘");
    println!();
}

/// Print resolved config once at startup (application-wide settings from JSON).
/// Shown only with `-v` / `--verbose`.
pub fn print_loaded_config(cfg: &Config) {
    println!("📋 Configuration");
    println!("   version: {}", cfg.version);
    println!("   path pairs: {}", cfg.paths.len());
    for (i, pair) in cfg.paths.iter().enumerate() {
        let svc = pair
            .service
            .as_deref()
            .filter(|s| !s.is_empty())
            .unwrap_or("(none)");
        println!(
            "   paths[{i}]: service={svc}  sync={}  cache={}",
            pair.sync, pair.cache
        );
    }
    match cfg.walk.max_depth {
        Some(d) => println!("   walk.max_depth: {d}"),
        None => println!("   walk.max_depth: unlimited"),
    }
    if cfg.walk.min_file_size_bytes == 0 {
        println!("   walk.min_file_size_bytes: 0 (no minimum)");
    } else {
        println!(
            "   walk.min_file_size_bytes: {}",
            cache_ops::format_bytes(cfg.walk.min_file_size_bytes)
        );
    }
    println!(
        "   walk.max_file_size_bytes: {}",
        cache_ops::format_max_file_size_limit(cfg.walk.max_file_size_bytes)
    );
    println!("   walk.max_threads: {}", cfg.walk.max_threads);
    if cfg.ignore.names.is_empty() {
        println!("   ignore.names: []");
    } else {
        println!("   ignore.names: {:?}", cfg.ignore.names);
    }
    println!(
        "   mount_wait: initial={}s retries={:?} max={}s",
        cfg.mount_wait.initial_secs, cfg.mount_wait.retry_delays_secs, cfg.mount_wait.max_wait_secs
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_cli_help_information_verbose() {
        assert_eq!(parse_cli_args(["--help"]), CliAction::Help);
        assert_eq!(parse_cli_args(["-h"]), CliAction::Help);
        assert_eq!(parse_cli_args(["-i"]), CliAction::Information);
        assert_eq!(parse_cli_args(["--information"]), CliAction::Information);
        assert_eq!(parse_cli_args(["-c"]), CliAction::Check);
        assert_eq!(parse_cli_args(["--check"]), CliAction::Check);
        assert_eq!(
            parse_cli_args(["--dry-run"]),
            CliAction::Run {
                dry_run: true,
                verbose: false,
                log: false
            }
        );
        assert_eq!(
            parse_cli_args(["-v"]),
            CliAction::Run {
                dry_run: false,
                verbose: true,
                log: false
            }
        );
        assert_eq!(
            parse_cli_args(["-l"]),
            CliAction::Run {
                dry_run: false,
                verbose: false,
                log: true
            }
        );
        assert_eq!(
            parse_cli_args(["--verbose", "--dry-run", "--log"]),
            CliAction::Run {
                dry_run: true,
                verbose: true,
                log: true
            }
        );
        assert_eq!(
            parse_cli_args(std::iter::empty::<&str>()),
            CliAction::Run {
                dry_run: false,
                verbose: false,
                log: false
            }
        );
        // help wins if both present
        assert_eq!(parse_cli_args(["-i", "--help"]), CliAction::Help);
        // information wins over check
        assert_eq!(parse_cli_args(["-c", "-i"]), CliAction::Information);
        // verbose with check is still Check (verbose only applies to Run)
        assert_eq!(parse_cli_args(["-c", "-v"]), CliAction::Check);
    }

    #[test]
    fn version_and_release_constants() {
        assert!(!PKG_VERSION.is_empty());
        assert_eq!(CODEBASE_VERSION, PKG_VERSION);
        assert!(!CODEBASE_RELEASE.is_empty());
        assert!(
            CODEBASE_RELEASE.contains("2026"),
            "release date should include year: {CODEBASE_RELEASE}"
        );
        assert_eq!(PKG_LICENSE, "AGPL-3.0-only");
        assert!(XSAR_WEBSITE.contains("xSAR.com.au") || XSAR_WEBSITE.contains("xsar.com.au"));
    }
}
