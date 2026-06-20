use std::fs;
use std::io::{self, Write};
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};
use walkdir::WalkDir;

const STATUS_REFRESH: Duration = Duration::from_millis(80);
const SPINNER_FRAMES: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

/// Pause after the path exists, before we check for real mount content.
const INITIAL_WAIT_SECS: u64 = 3;

/// Retry delays when the directory still looks empty (mount not populated yet).
const RETRY_DELAYS_SECS: &[u64] = &[3, 5, 8];

/// Hard ceiling on total wait time per path — never block longer than this.
const MAX_WAIT_SECS: u64 = 30;

/// Returns true if the directory has at least one entry (not just an empty mount point).
fn directory_has_content(path: &Path) -> bool {
    match fs::read_dir(path) {
        Ok(mut entries) => entries.next().is_some(),
        Err(_) => false,
    }
}

/// Sleep up to `requested` seconds, but never beyond `MAX_WAIT_SECS` total elapsed.
/// Returns false when the budget is already exhausted.
fn sleep_capped(start: &Instant, requested: u64) -> bool {
    let elapsed = start.elapsed().as_secs();
    if elapsed >= MAX_WAIT_SECS {
        return false;
    }

    let remaining = MAX_WAIT_SECS - elapsed;
    let actual = requested.min(remaining);
    thread::sleep(Duration::from_secs(actual));
    true
}

/// Wait for an rclone FUSE mount to populate before walking.
/// FUSE paths often exist immediately while listings are still empty.
fn wait_for_mount_content(path: &Path) -> bool {
    let start = Instant::now();

    println!(
        "   ⏳ Path exists — waiting {}s for mount to settle (max {}s total)...",
        INITIAL_WAIT_SECS, MAX_WAIT_SECS
    );
    if !sleep_capped(&start, INITIAL_WAIT_SECS) {
        eprintln!(
            "   ⚠️  Max wait timeout ({}s) reached — proceeding anyway.",
            MAX_WAIT_SECS
        );
        return directory_has_content(path);
    }

    if directory_has_content(path) {
        println!("   ✓ Directory has content, starting walk.");
        return true;
    }

    println!("   ⏳ Directory looks empty (mount may still be populating)...");

    for (attempt, &delay) in RETRY_DELAYS_SECS.iter().enumerate() {
        if start.elapsed().as_secs() >= MAX_WAIT_SECS {
            eprintln!(
                "   ⚠️  Max wait timeout ({}s) reached — proceeding anyway.",
                MAX_WAIT_SECS
            );
            return directory_has_content(path);
        }

        println!(
            "   ⏳ Retry {}/{}: waiting {}s...",
            attempt + 1,
            RETRY_DELAYS_SECS.len(),
            delay
        );
        if !sleep_capped(&start, delay) {
            eprintln!(
                "   ⚠️  Max wait timeout ({}s) reached — proceeding anyway.",
                MAX_WAIT_SECS
            );
            return directory_has_content(path);
        }

        if directory_has_content(path) {
            println!("   ✓ Directory now has content, starting walk.");
            return true;
        }
    }

    eprintln!(
        "   ⚠️  Still empty after retries — proceeding anyway (may be legitimately empty)."
    );
    false
}

/// Truncate long paths for the live status line (show the tail — most useful part).
fn truncate_display(s: &str, max_chars: usize) -> String {
    let char_count = s.chars().count();
    if char_count <= max_chars {
        s.to_string()
    } else {
        let tail: String = s.chars().skip(char_count.saturating_sub(max_chars - 1)).collect();
        format!("…{tail}")
    }
}

/// Single-line live progress — redraws in place so the walk feels active.
struct WalkStatus {
    started: Instant,
    last_render: Instant,
    frame: usize,
    dirs: usize,
    files: usize,
    errors: usize,
}

impl WalkStatus {
    fn new(dirs: usize, files: usize, errors: usize) -> Self {
        Self {
            started: Instant::now(),
            last_render: Instant::now() - STATUS_REFRESH,
            frame: SPINNER_FRAMES.len() - 1,
            dirs,
            files,
            errors,
        }
    }

    fn record_dir(&mut self) {
        self.dirs += 1;
    }

    fn record_file(&mut self) {
        self.files += 1;
    }

    fn record_error(&mut self) {
        self.errors += 1;
    }

    fn render(&mut self, current: &Path, force: bool) {
        if !force && self.last_render.elapsed() < STATUS_REFRESH {
            return;
        }

        self.frame = (self.frame + 1) % SPINNER_FRAMES.len();
        self.last_render = Instant::now();

        let spinner = SPINNER_FRAMES[self.frame];
        let elapsed = self.started.elapsed().as_secs();
        let location = truncate_display(&current.display().to_string(), 48);

        let line = format!(
            "   {spinner}  dirs {dirs:>6}  files {files:>6}  errs {errs:>4}  {elapsed:>4}s  {location}",
            dirs = self.dirs,
            files = self.files,
            errs = self.errors,
        );

        // \r overwrites the same line; padding clears leftover characters from longer paths.
        print!("\r{:<120}", line);
        let _ = io::stdout().flush();
    }

    fn finish_line(&self) {
        println!();
    }
}

fn main() {
    println!("🚀 warm-drive-cache starting - VFS cache warmer for rclone mounts");

    // === EDIT THIS ARRAY TO ADD/REMOVE PATHS ===
    let paths = [
        "/home/charlie/Documents/Gdrive/AccessIT",
        "/home/charlie/Documents/Gdrive/xSAR",
    ];
    // ===========================================

    let mut total_dirs: usize = 0;
    let mut total_files: usize = 0;
    let mut errors: usize = 0;

    for root in paths {
        println!("\n📂 Warming path: {}", root);

        let root_path = Path::new(root);
        match root_path.try_exists() {
            Ok(true) => {}
            Ok(false) => {
                eprintln!("   ⚠️  Path does not exist or not mounted: {}", root);
                errors += 1;
                continue;
            }
            Err(e) => {
                eprintln!("   ⚠️  Cannot check path {}: {}", root, e);
                errors += 1;
                continue;
            }
        }

        wait_for_mount_content(root_path);

        let path_start_dirs = total_dirs;
        let path_start_files = total_files;
        let mut status = WalkStatus::new(total_dirs, total_files, errors);
        println!("   Walking…");

        let walker = WalkDir::new(root)
            .follow_links(false) // Don't follow symlinks for safety on mounts
            .into_iter()
            .filter_entry(|_e| {
                // Optional: skip some noisy dirs if needed, e.g. .git but for Gdrive unlikely
                true
            });

        for entry in walker {
            match entry {
                Ok(entry) => {
                    let path = entry.path();

                    // Touch metadata - this forces VFS to populate dir/file cache
                    if fs::symlink_metadata(path).is_err() {
                        // Silently ignore some transient errors (common with cloud mounts).
                        // symlink_metadata() avoids following symlinks while still touching metadata.
                    }

                    if entry.file_type().is_dir() {
                        status.record_dir();
                        // Explicitly read dir to ensure directory listing is cached
                        if let Err(_) = fs::read_dir(path) {
                            status.record_error();
                        }
                    } else {
                        status.record_file();
                    }

                    status.render(path, false);
                }
                Err(e) => {
                    status.record_error();
                    status.render(
                        e.path().unwrap_or_else(|| Path::new("<unknown>")),
                        status.errors % 10 == 0,
                    );

                    // Only log serious ones occasionally (above the live status line)
                    if status.errors % 100 == 0 {
                        status.finish_line();
                        let path_str = e
                            .path()
                            .map_or_else(|| "<unknown>".to_string(), |p| p.display().to_string());
                        eprintln!("   ⚠️  Walk error at {}: {}", path_str, e);
                    }
                }
            }
        }

        total_dirs = status.dirs;
        total_files = status.files;
        errors = status.errors;

        status.render(root_path, true);
        status.finish_line();
        println!(
            "   ✓ Finished {} — {} dirs, {} files",
            root,
            total_dirs - path_start_dirs,
            total_files - path_start_files,
        );
    }

    println!("\n✅ Cache warming complete!");
    println!("   Directories touched: {}", total_dirs);
    println!("   Files touched:       {}", total_files);
    println!("   Errors encountered:  {}", errors);
    println!("   (Most errors are transient on cloud mounts - normal)");
    println!("\n💡 Tip: Run this periodically via systemd timer. Your VFS cache should now be nice and warm.");
}
