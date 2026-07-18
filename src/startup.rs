//! Startup banner, CLI help/version, and compile-time product / licence identity.
//!
//! Version strings are baked from Cargo.toml via `env!("CARGO_PKG_*")`.

use crate::config::Config;

/// Package name from Cargo.toml.
pub const PKG_NAME: &str = env!("CARGO_PKG_NAME");

/// Program version — coded into the binary from Cargo.toml `version`.
pub const PKG_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Human-facing codebase version string (used by `-v` / `--version`).
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

/// Result of early CLI flag handling (`-h` / `-v` / `-c` / normal run).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CliAction {
    /// Continue with normal maintenance run.
    Run { dry_run: bool },
    /// Printed help and should exit 0.
    Help,
    /// Printed version and should exit 0.
    Version,
    /// Validate config.json layout and report service / paths / cache sizes.
    Check,
}

/// Parse process args for `-h`/`--help`, `-v`/`--version`, `-c`/`--check`, and `--dry-run`.
///
/// Unknown flags other than the above are ignored so existing invocations keep working.
pub fn parse_cli_args<I, S>(args: I) -> CliAction
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut dry_run = false;
    let mut help = false;
    let mut version = false;
    let mut check = false;

    for arg in args {
        match arg.as_ref() {
            "-h" | "--help" => help = true,
            "-v" | "--version" => version = true,
            "-c" | "--check" => check = true,
            "--dry-run" => dry_run = true,
            _ => {}
        }
    }

    // Precedence: help → version → check → run
    if help {
        CliAction::Help
    } else if version {
        CliAction::Version
    } else if check {
        CliAction::Check
    } else {
        CliAction::Run { dry_run }
    }
}

/// `-v` / `--version`: codebase version + release date, licence, repo, and website.
pub fn print_version() {
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
    println!("  -h, --help       Show this help and example configuration");
    println!("  -v, --version    Show version, release date, licence, and project links");
    println!("  -c, --check      Validate config.json and list services / paths / cache sizes");
    println!("      --dry-run    Simulate cache deletion only (no writes / no warm)");
    println!();
    println!("Configuration:");
    println!("  Place a file named config.json next to the executable (same directory).");
    println!("  Copy from config.example.json (shipped with the binary/source) and set your");
    println!("  absolute sync/cache paths and optional systemd user unit names.");
    println!("  Alternatives: WARM_DRIVE_CACHE_CONFIG=/path/to.json or XDG config dir.");
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

/// Startup identity block: product, website, and compile-time licence metadata.
pub fn print_startup_banner() {
    println!("┌────────────────────────────────────────────────────────────┐");
    println!("│  {PKG_NAME}");
    println!("│  Codebase Version: {CODEBASE_VERSION}");
    println!("│  Codebase release: {CODEBASE_RELEASE}");
    println!("│  A product of {PRODUCT_OF}");
    println!("│  Website: {XSAR_WEBSITE}");
    println!("│  Licence: {PKG_LICENSE}  (full text: LICENSE in the source tree)");
    println!("│  Homepage: {PKG_HOMEPAGE}");
    if !PKG_REPOSITORY.is_empty() {
        println!("│  Source:  {PKG_REPOSITORY}");
    }
    println!("└────────────────────────────────────────────────────────────┘");
    println!("   Rust utility for removing rclone cache staleness and warming mounts.");
    println!(
        "   Quit gracefully: Ctrl+C (SIGINT) or press q (TTY) — finishes in-flight workers, starts no new work."
    );
    println!();
}

/// Print resolved config once at startup (application-wide settings from JSON).
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
            cfg.walk.min_file_size_bytes
        );
    }
    if cfg.walk.max_file_size_bytes == 0 {
        println!("   walk.max_file_size_bytes: 0 (no maximum)");
    } else {
        println!(
            "   walk.max_file_size_bytes: {}",
            cfg.walk.max_file_size_bytes
        );
    }
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
    fn parse_cli_help_and_version() {
        assert_eq!(parse_cli_args(["--help"]), CliAction::Help);
        assert_eq!(parse_cli_args(["-h"]), CliAction::Help);
        assert_eq!(parse_cli_args(["-v"]), CliAction::Version);
        assert_eq!(parse_cli_args(["--version"]), CliAction::Version);
        assert_eq!(parse_cli_args(["-c"]), CliAction::Check);
        assert_eq!(parse_cli_args(["--check"]), CliAction::Check);
        assert_eq!(
            parse_cli_args(["--dry-run"]),
            CliAction::Run { dry_run: true }
        );
        assert_eq!(
            parse_cli_args(std::iter::empty::<&str>()),
            CliAction::Run { dry_run: false }
        );
        // help wins if both present
        assert_eq!(parse_cli_args(["-v", "--help"]), CliAction::Help);
        assert_eq!(parse_cli_args(["-c", "-v"]), CliAction::Version);
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
