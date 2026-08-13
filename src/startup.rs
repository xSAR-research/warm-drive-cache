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
pub const CODEBASE_RELEASE: &str = "13th August, 2026";

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

/// Safety warning shared by live runs, help, and JSON validation output.
pub const MOUNT_MODIFICATION_WARNING: &str = "⚠️  WARNING: Do not modify, add, or delete files in an rclone-mounted directory while warm-drive-cache is running; doing so may cause unexpected behaviour.";

/// Embedded copy of `warm-drive-cache-example.json` (no secrets) for `--help`.
const CONFIG_EXAMPLE_JSON: &str = include_str!("../warm-drive-cache-example.json");

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CliOverrides {
    pub threads: Option<usize>,
    pub size: Option<i64>,
    pub checksum: Option<bool>,
}

/// Result of early CLI flag handling (`-h` / `-i` / `-j` / overrides / normal run).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CliAction {
    /// Continue with normal maintenance run.
    Run {
        dry_run: bool,
        verbose: bool,
        /// Write a time-stamped CSV warm log under `/tmp/`.
        log: bool,
        overrides: CliOverrides,
    },
    /// Printed help and should exit 0.
    Help,
    /// Printed product information (version, licence, links) and should exit 0.
    Information,
    /// Validate warm-drive-cache.json layout and report service / paths / cache sizes.
    JsonValidation { overrides: CliOverrides },
}

/// Parse process arguments into a validated action and typed overrides.
///
/// Help takes first priority. JSON validation is second and still accepts the
/// same `-t`/`-s`/`-c` overrides as a normal run. Information is third and
/// ignores every other argument. Otherwise, unknown, missing, malformed, and
/// conflicting options are rejected.
pub fn parse_cli_args<I, S>(args: I) -> Result<CliAction, String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let args: Vec<String> = args.into_iter().map(|s| s.as_ref().to_owned()).collect();
    if args
        .iter()
        .any(|a| matches!(a.as_str(), "-?" | "-h" | "--help"))
    {
        return Ok(CliAction::Help);
    }
    let json = args.iter().any(|a| matches!(a.as_str(), "-j" | "--json"));
    let information = args
        .iter()
        .any(|a| matches!(a.as_str(), "-i" | "--information"));
    if information && !json {
        return Ok(CliAction::Information);
    }

    let mut dry_run = false;
    let mut verbose = false;
    let mut log = false;
    let mut overrides = CliOverrides::default();
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        match arg.as_str() {
            "-?" | "-h" | "--help" => unreachable!(),
            "-i" | "--information" => {
                // JSON validation outranks information; the flag is otherwise ignored.
            }
            "-j" | "--json" => {}
            "-v" | "--verbose" => {
                if json {
                    return Err(format!("option {arg} is not used with --json"));
                }
                if verbose {
                    return Err("conflicting duplicate --verbose option".into());
                }
                verbose = true;
            }
            "-l" | "--log" => {
                if json {
                    return Err(format!("option {arg} is not used with --json"));
                }
                if log {
                    return Err("conflicting duplicate --log option".into());
                }
                log = true;
            }
            "--dry-run" => {
                if json {
                    return Err(format!("option {arg} is not used with --json"));
                }
                if dry_run {
                    return Err("conflicting duplicate --dry-run option".into());
                }
                dry_run = true;
            }
            "-t" | "--threads" | "-s" | "--size" | "-c" | "--checksum" => {
                let value = args
                    .get(i + 1)
                    .ok_or_else(|| format!("missing value for {arg}"))?;
                if value.is_empty() {
                    return Err(format!("empty value for {arg}"));
                }
                if value.starts_with('-')
                    && !(matches!(arg.as_str(), "-s" | "--size") && value == "-1")
                {
                    return Err(format!(
                        "value for {arg} was interpreted as option: {value}"
                    ));
                }
                match arg.as_str() {
                    "-t" | "--threads" => {
                        if overrides.threads.is_some() {
                            return Err("conflicting duplicate --threads option".into());
                        }
                        let n: usize = value
                            .parse()
                            .map_err(|_| format!("malformed integer for {arg}: {value:?}"))?;
                        if !(1..=64).contains(&n) {
                            return Err(format!("threads must be between 1 and 64 (got {n})"));
                        }
                        overrides.threads = Some(n);
                    }
                    "-s" | "--size" => {
                        if overrides.size.is_some() {
                            return Err("conflicting duplicate --size option".into());
                        }
                        overrides.size = Some(crate::config::parse_size_expr(value)?);
                    }
                    _ => {
                        if overrides.checksum.is_some() {
                            return Err("conflicting duplicate --checksum option".into());
                        }
                        overrides.checksum = Some(crate::config::parse_bool(value)?);
                    }
                }
                i += 1;
            }
            _ => return Err(format!("unknown option: {arg}")),
        }
        i += 1;
    }

    if json {
        return Ok(CliAction::JsonValidation { overrides });
    }

    Ok(CliAction::Run {
        dry_run,
        verbose,
        log,
        overrides,
    })
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

/// `-?` / `-h` / `--help`: usage, configuration location, and the embedded example schema.
pub fn print_help() {
    println!("{PKG_NAME} — Codebase Version: {CODEBASE_VERSION} ({CODEBASE_RELEASE})");
    println!("rclone FUSE cache maintenance (product of {PRODUCT_OF})");
    println!();
    println!("Usage:");
    println!("  {PKG_NAME} [OPTIONS]");
    println!();
    println!("Options:");
    println!("  -?, -h, --help      Show this help and example configuration");
    println!(
        "  -j, --json          Validate JSON, all configured paths, and service discoverability"
    );
    println!("  -i, --information   Show version, release date, licence, and project links");
    println!("  -t, --threads VALUE Override worker count (1..=64); also applies with --json");
    println!("  -s, --size VALUE    Override maximum: -1, 0, or a positive whole size/unit");
    println!(
        "  -c, --checksum VALUE Override checksum (TRUE/YES/Y or FALSE/NO/N); also applies with --json"
    );
    println!("  -v, --verbose       Show Configuration and Pre-flight checks detail");
    println!("  -l, --log           Write time-stamped CSV log under /tmp (READ/ATTRIB per file)");
    println!("      --dry-run       Simulate deletion/no warm; concurrency lock is still created");
    println!();
    println!("Configuration:");
    println!("  Place a file named warm-drive-cache.json next to the executable (same directory).");
    println!(
        "  Copy from warm-drive-cache-example.json (shipped with the binary/source) and set your"
    );
    println!("  absolute sync/cache paths and optional systemd unit names (system or --user).");
    println!(
        "  CLI -t/-s/-c override the matching walk fields after JSON is loaded (including --json)."
    );
    println!("  Checksum verification is ENABLED BY DEFAULT.");
    println!("  JSON checksum values: true/false or quoted TRUE/YES/Y/FALSE/NO/N.");
    println!(
        "  Full reads populate the cache; workers then poll the local VFS cache with a finite timeout."
    );
    println!("  Before deletion, vfsMeta/<remote> is checked every second for Dirty=true entries.");
    println!(
        "  Normal runs create warm-drive-cache.lock in each cache; an existing lock defaults to No."
    );
    println!();
    println!("walk.max_file_size_bytes special values:");
    println!("  -1  metadata only (no File contents read for any file)");
    println!("   0  File contents read for every file (any size)");
    println!("  N>0 File contents read when file size is within min..N window");
    println!();
    println!("Sizes may be JSON numbers (whole bytes) or strings with units (case-insensitive):");
    println!("  65536 | \"64KiB\" | \"64K\" | \"64KB\" | \"1MiB\" | \"1M\" | \"512B\"");
    println!("  Units: B, K/KB/KiB, M/MB/MiB, G/GB/GiB, T/TB/TiB, P/PB/PiB (binary 1024).");
    println!("  Fractional values are not allowed (not 12.5, not \"1.5KiB\").");
    println!();
    println!("Embedded warm-drive-cache-example.json template (save as warm-drive-cache.json):");
    println!("{CONFIG_EXAMPLE_JSON}");
    println!("For full documentation see README.md in the repository:");
    if !PKG_REPOSITORY.is_empty() {
        println!("  {PKG_REPOSITORY}");
        println!("  {PKG_REPOSITORY}/blob/main/README.md");
    } else {
        println!("  {XSAR_WEBSITE}");
    }
    println!("Website: {XSAR_WEBSITE}");
    println!();
    print_mount_modification_warning();
}

/// Print the prominent mounted-file safety warning as a standalone line.
pub fn print_mount_modification_warning() {
    println!("{MOUNT_MODIFICATION_WARNING}");
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
    println!(
        "   walk.checksum: {} (enabled by default)",
        cfg.walk.checksum
    );
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
