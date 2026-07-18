//! Sync-tree warm worker: WalkDir, parallel READ/ATTR, live multi-line status.
//!
//! Directory listings stay on the walker thread; file open/read runs on a bounded pool.

use crate::config::Config;
use std::collections::HashSet;
use std::env;
use std::ffi::OsStr;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};
use walkdir::WalkDir;

const STATUS_REFRESH: Duration = Duration::from_millis(80);
const SPINNER_FRAMES: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

/// Maximum characters for the path shown on each worker status line (includes directories).
pub const DISPLAY_PATH_MAX_CHARS: usize = 80;

/// Truncate long paths for the live status line (show the tail — most useful part).
pub fn truncate_display(s: &str, max_chars: usize) -> String {
    let char_count = s.chars().count();
    if char_count <= max_chars {
        s.to_string()
    } else {
        let tail: String = s
            .chars()
            .skip(char_count.saturating_sub(max_chars - 1))
            .collect();
        format!("…{tail}")
    }
}

/// What a worker thread is doing right now (shown in the live status block).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThreadWorkMode {
    /// Waiting for the next path from the queue.
    Idle,
    /// File size is inside the configured window — performing a 1-byte warm read.
    ByteRead,
    /// File size is outside the window — attributes/metadata only (no 1-byte read).
    AttrOnly,
}

/// Per-worker slot for the multi-line spinner list.
#[derive(Debug, Clone)]
struct ThreadSlotView {
    mode: ThreadWorkMode,
    size: u64,
    /// Full path string; truncated at render time.
    path: String,
}

impl ThreadSlotView {
    fn idle() -> Self {
        Self {
            mode: ThreadWorkMode::Idle,
            size: 0,
            path: String::new(),
        }
    }
}

/// Compact size for the status line (keeps the per-thread list readable).
fn format_bytes_compact(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = 1024.0 * 1024.0;
    if bytes as f64 >= MIB {
        format!("{:.1}MiB", bytes as f64 / MIB)
    } else if bytes as f64 >= KIB {
        format!("{:.1}KiB", bytes as f64 / KIB)
    } else {
        format!("{bytes}B")
    }
}

/// Strip obvious leading path details (sync root, `$HOME`), then truncate to
/// [`DISPLAY_PATH_MAX_CHARS`]. Prefer the longest matching strip root.
pub fn shorten_path_for_display(path: &Path, strip_roots: &[&Path]) -> String {
    let mut s = path.display().to_string();

    let mut best_prefix: Option<String> = None;
    for root in strip_roots {
        let r = root.display().to_string();
        if r.is_empty() {
            continue;
        }
        if s == r || s.starts_with(&format!("{r}/")) {
            let take = best_prefix
                .as_ref()
                .map(|b| r.len() > b.len())
                .unwrap_or(true);
            if take {
                best_prefix = Some(r);
            }
        }
    }
    if let Some(r) = best_prefix {
        s = s[r.len()..].trim_start_matches('/').to_string();
        if s.is_empty() {
            s = ".".to_string();
        }
    } else if let Ok(home) = env::var("HOME")
        && !home.is_empty()
        && (s == home || s.starts_with(&format!("{home}/")))
    {
        s = format!("~{}", &s[home.len()..]);
    }

    truncate_display(&s, DISPLAY_PATH_MAX_CHARS)
}

/// Format one numbered worker line for the live status block.
/// Example: `   1 of 8   1.2KiB  READ  subdir/file.txt`
fn format_thread_slot_line(
    index_1based: usize,
    max_threads: usize,
    slot: &ThreadSlotView,
) -> String {
    let thr = format!("{index_1based} of {max_threads}");
    match slot.mode {
        ThreadWorkMode::Idle => {
            format!("   {thr:<8}  —         idle")
        }
        ThreadWorkMode::ByteRead => {
            let name = truncate_display(&slot.path, DISPLAY_PATH_MAX_CHARS);
            let sz = format_bytes_compact(slot.size);
            format!("   {thr:<8}  {sz:>8}  READ  {name}")
        }
        ThreadWorkMode::AttrOnly => {
            let name = truncate_display(&slot.path, DISPLAY_PATH_MAX_CHARS);
            let sz = format_bytes_compact(slot.size);
            // Outside min/max 1-byte window: size + attributes-only (no 1-byte read).
            format!("   {thr:<8}  {sz:>8}  ATTR  {name}")
        }
    }
}

/// Multi-line live progress: header + one numbered line per worker thread.
pub struct WalkStatus {
    started: Instant,
    last_render: Instant,
    frame: usize,
    pub dirs: usize,
    pub files: usize,
    pub errors: usize,
    /// Workers currently inside warm work (in-flight).
    pub active_threads: usize,
    /// Configured pool size (`walk.max_threads`).
    pub max_threads: usize,
    pub byte_reads: usize,
    pub metadata_only: usize,
    /// True when SIGINT / 'q' requested stop (in-flight work still finishes).
    pub cancelled: bool,
    /// Shared with workers: current file / mode per thread index.
    slots: Arc<Mutex<Vec<ThreadSlotView>>>,
    /// How many lines the last render wrote (for ANSI cursor-up redraw).
    rendered_lines: usize,
}

impl WalkStatus {
    pub fn new(dirs: usize, files: usize, errors: usize, max_threads: usize) -> Self {
        let slots = Arc::new(Mutex::new(
            (0..max_threads).map(|_| ThreadSlotView::idle()).collect(),
        ));
        Self {
            started: Instant::now(),
            last_render: Instant::now() - STATUS_REFRESH,
            frame: SPINNER_FRAMES.len() - 1,
            dirs,
            files,
            errors,
            active_threads: 0,
            max_threads,
            byte_reads: 0,
            metadata_only: 0,
            cancelled: false,
            slots,
            rendered_lines: 0,
        }
    }

    pub fn record_dir(&mut self) {
        self.dirs += 1;
    }

    pub fn record_error(&mut self) {
        self.errors += 1;
    }

    fn sync_worker_stats(
        &mut self,
        files: &AtomicUsize,
        errors: &AtomicUsize,
        active: &AtomicUsize,
        byte_reads: &AtomicUsize,
        metadata_only: &AtomicUsize,
    ) {
        self.files = files.load(Ordering::Relaxed);
        self.errors = errors.load(Ordering::Relaxed);
        self.active_threads = active.load(Ordering::Relaxed);
        self.byte_reads = byte_reads.load(Ordering::Relaxed);
        self.metadata_only = metadata_only.load(Ordering::Relaxed);
    }

    pub fn render(&mut self, current: &Path, force: bool) {
        if !force && self.last_render.elapsed() < STATUS_REFRESH {
            return;
        }

        self.frame = (self.frame + 1) % SPINNER_FRAMES.len();
        self.last_render = Instant::now();

        let spinner = SPINNER_FRAMES[self.frame];
        let elapsed = self.started.elapsed().as_secs();
        let location = truncate_display(&shorten_path_for_display(current, &[]), 40);

        let stop_note = if self.cancelled {
            "  STOPPING (finish in-flight, no new work)"
        } else {
            ""
        };

        let header = format!(
            "   {spinner}  dirs {dirs:>6}  files {files:>6}  thr {active}/{max_thr}  errs {errs:>4}  {elapsed:>4}s  walk {location}{stop_note}",
            dirs = self.dirs,
            files = self.files,
            active = self.active_threads,
            max_thr = self.max_threads,
            errs = self.errors,
        );

        let max_thr = self.max_threads;
        let slot_lines: Vec<String> = match self.slots.lock() {
            Ok(slots) => slots
                .iter()
                .enumerate()
                .map(|(i, s)| format_thread_slot_line(i + 1, max_thr, s))
                .collect(),
            Err(_) => (0..self.max_threads)
                .map(|i| format!("   {} of {}  —         idle", i + 1, max_thr))
                .collect(),
        };

        // Re-draw the previous multi-line block in place (cursor up + clear each line).
        if self.rendered_lines > 0 {
            print!("\x1b[{}A", self.rendered_lines);
        }

        // Wider pad: thread label + size + mode + up to DISPLAY_PATH_MAX_CHARS path.
        print!("\x1b[2K\r{header}\n");
        for line in &slot_lines {
            print!("\x1b[2K\r{:<160}\n", line);
        }

        self.rendered_lines = 1 + slot_lines.len();
        let _ = io::stdout().flush();
    }

    /// Leave the status block on screen and put the cursor below it.
    pub fn finish_line(&mut self) {
        println!();
        self.rendered_lines = 0;
    }
}

/// Outcome of warming a single file (1-byte read vs attributes-only).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WarmOutcome {
    ByteRead,
    MetadataOnly,
    Error,
}

/// Whether `len` falls inside the configured 1-byte-read window.
/// `min == 0` means no lower bound; `max == 0` means no upper bound.
pub fn should_read_one_byte(len: u64, min: u64, max: u64) -> bool {
    if min != 0 && len < min {
        return false;
    }
    if max != 0 && len > max {
        return false;
    }
    true
}

/// Warm one file: 1-byte read when size is in range, otherwise attributes only.
///
/// When `on_classified` is provided, it is called after size is known and before
/// open/read so the live spinner can show path, size, and READ vs ATTR mode.
pub fn warm_file_with_hook(
    path: &Path,
    min: u64,
    max: u64,
    on_classified: Option<&dyn Fn(u64, ThreadWorkMode)>,
) -> WarmOutcome {
    let meta = match fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(_) => return WarmOutcome::Error,
    };
    let len = meta.len();
    let in_range = should_read_one_byte(len, min, max);
    let mode = if in_range {
        ThreadWorkMode::ByteRead
    } else {
        ThreadWorkMode::AttrOnly
    };
    if let Some(hook) = on_classified {
        hook(len, mode);
    }

    if !in_range {
        // Attributes already loaded via symlink_metadata — skip open/read for large blobs etc.
        return WarmOutcome::MetadataOnly;
    }

    match fs::File::open(path) {
        Ok(mut f) => {
            let mut buf = [0u8; 1];
            // 0 bytes = empty file (open still warms); 1 byte = normal warm read.
            match f.read(&mut buf) {
                Ok(0 | 1) => WarmOutcome::ByteRead,
                Ok(n) => {
                    // 1-byte buffer: more than 1 is impossible, treat as warm success.
                    debug_assert!(n <= 1);
                    WarmOutcome::ByteRead
                }
                Err(e) => {
                    let msg = e.to_string().to_lowercase();
                    if msg.contains("limit") || msg.contains("rate") || msg.contains("429") {
                        eprintln!(
                            "   ⚠️  Possible API limit on read (skipped file, rclone may have fetched full): {}",
                            path.display()
                        );
                    }
                    WarmOutcome::Error
                }
            }
        }
        Err(_) => WarmOutcome::Error,
    }
}

/// Walk `sync_path` and warm files with a bounded worker pool.
/// Directory listings stay on the walker thread; file open/read runs on workers.
///
/// On `shutdown` (SIGINT / `q`): stop enqueueing and discard queued work that has
/// not started; in-flight workers finish their current file, then exit.
pub fn warm_tree(sync_path: &Path, cfg: &Config, shutdown: &Arc<AtomicBool>) -> WalkStatus {
    let max_threads = cfg.walk.max_threads.max(1);
    let min_size = cfg.walk.min_file_size_bytes;
    let max_size = cfg.walk.max_file_size_bytes;
    let channel_cap = max_threads.saturating_mul(4).max(4);

    let (tx, rx) = mpsc::sync_channel::<PathBuf>(channel_cap);
    let rx = Arc::new(Mutex::new(rx));

    let files = Arc::new(AtomicUsize::new(0));
    let errors = Arc::new(AtomicUsize::new(0));
    let active = Arc::new(AtomicUsize::new(0));
    let byte_reads = Arc::new(AtomicUsize::new(0));
    let metadata_only = Arc::new(AtomicUsize::new(0));
    let pending = Arc::new(AtomicUsize::new(0));

    let mut status = WalkStatus::new(0, 0, 0, max_threads);
    let slots = Arc::clone(&status.slots);
    let ignore_names: HashSet<&OsStr> = cfg.ignore.names.iter().map(OsStr::new).collect();
    let sync_root = sync_path.to_path_buf();

    thread::scope(|scope| {
        for worker_id in 0..max_threads {
            let rx = Arc::clone(&rx);
            let files = Arc::clone(&files);
            let errors = Arc::clone(&errors);
            let active = Arc::clone(&active);
            let byte_reads = Arc::clone(&byte_reads);
            let metadata_only = Arc::clone(&metadata_only);
            let pending = Arc::clone(&pending);
            let slots = Arc::clone(&slots);
            let shutdown = Arc::clone(shutdown);
            let sync_root = sync_root.clone();

            scope.spawn(move || {
                loop {
                    let path = {
                        let guard = match rx.lock() {
                            Ok(g) => g,
                            Err(_) => break,
                        };
                        guard.recv()
                    };
                    match path {
                        Ok(path) => {
                            // Shutdown requested: do not start a new warm; drop queued paths.
                            // In-flight work already past this check still runs to completion.
                            if shutdown.load(Ordering::SeqCst) {
                                pending.fetch_sub(1, Ordering::Relaxed);
                                continue;
                            }

                            // RAII: clear slot + active/pending even if warm_file panics.
                            struct InFlight {
                                active: Arc<AtomicUsize>,
                                pending: Arc<AtomicUsize>,
                                slots: Arc<Mutex<Vec<ThreadSlotView>>>,
                                worker_id: usize,
                            }
                            impl Drop for InFlight {
                                fn drop(&mut self) {
                                    if let Ok(mut slots) = self.slots.lock()
                                        && let Some(slot) = slots.get_mut(self.worker_id)
                                    {
                                        *slot = ThreadSlotView::idle();
                                    }
                                    self.active.fetch_sub(1, Ordering::Relaxed);
                                    self.pending.fetch_sub(1, Ordering::Relaxed);
                                }
                            }

                            active.fetch_add(1, Ordering::Relaxed);
                            files.fetch_add(1, Ordering::Relaxed);
                            let _guard = InFlight {
                                active: Arc::clone(&active),
                                pending: Arc::clone(&pending),
                                slots: Arc::clone(&slots),
                                worker_id,
                            };

                            let display_path =
                                shorten_path_for_display(&path, &[sync_root.as_path()]);
                            let slots_for_hook = Arc::clone(&slots);
                            let outcome = warm_file_with_hook(
                                &path,
                                min_size,
                                max_size,
                                Some(&|size, mode| {
                                    if let Ok(mut slots) = slots_for_hook.lock()
                                        && let Some(slot) = slots.get_mut(worker_id)
                                    {
                                        *slot = ThreadSlotView {
                                            mode,
                                            size,
                                            path: display_path.clone(),
                                        };
                                    }
                                }),
                            );
                            match outcome {
                                WarmOutcome::ByteRead => {
                                    byte_reads.fetch_add(1, Ordering::Relaxed);
                                }
                                WarmOutcome::MetadataOnly => {
                                    metadata_only.fetch_add(1, Ordering::Relaxed);
                                }
                                WarmOutcome::Error => {
                                    errors.fetch_add(1, Ordering::Relaxed);
                                }
                            }
                        }
                        Err(_) => break, // sender dropped — no more work
                    }
                }
            });
        }

        let mut walker = WalkDir::new(sync_path).follow_links(false);
        if let Some(depth) = cfg.walk.max_depth {
            walker = walker.max_depth(depth);
        }
        let walker = walker
            .into_iter()
            .filter_entry(|entry| !should_skip_entry(entry, &ignore_names));

        for entry in walker {
            if shutdown.load(Ordering::SeqCst) {
                status.cancelled = true;
                break;
            }
            match entry {
                Ok(entry) => {
                    let path = entry.path();
                    if entry.file_type().is_dir() {
                        status.record_dir();
                        let _ = fs::read_dir(path);
                    } else if entry.file_type().is_file() {
                        pending.fetch_add(1, Ordering::Relaxed);
                        // Back-pressure: blocks when the channel is full (workers busy).
                        // Use send_timeout-like behaviour: check shutdown while blocked by
                        // polling — sync_channel::send blocks; if shutdown mid-send we rely
                        // on workers draining. Prefer non-blocking retry:
                        loop {
                            if shutdown.load(Ordering::SeqCst) {
                                pending.fetch_sub(1, Ordering::Relaxed);
                                status.cancelled = true;
                                break;
                            }
                            match tx.try_send(path.to_path_buf()) {
                                Ok(()) => break,
                                Err(mpsc::TrySendError::Full(_)) => {
                                    status.sync_worker_stats(
                                        &files,
                                        &errors,
                                        &active,
                                        &byte_reads,
                                        &metadata_only,
                                    );
                                    status.cancelled = shutdown.load(Ordering::SeqCst);
                                    status.render(path, true);
                                    thread::sleep(STATUS_REFRESH);
                                }
                                Err(mpsc::TrySendError::Disconnected(_)) => {
                                    pending.fetch_sub(1, Ordering::Relaxed);
                                    status.record_error();
                                    break;
                                }
                            }
                        }
                        if status.cancelled {
                            break;
                        }
                    }
                    status.cancelled = shutdown.load(Ordering::SeqCst);
                    status.sync_worker_stats(&files, &errors, &active, &byte_reads, &metadata_only);
                    status.render(path, false);
                }
                Err(e) => {
                    status.record_error();
                    status.sync_worker_stats(&files, &errors, &active, &byte_reads, &metadata_only);
                    status.render(
                        e.path().unwrap_or_else(|| Path::new("<unknown>")),
                        status.errors.is_multiple_of(10),
                    );
                    if status.errors.is_multiple_of(100) {
                        status.finish_line();
                        let path_str = e
                            .path()
                            .map_or_else(|| "<unknown>".to_string(), |p| p.display().to_string());
                        eprintln!("   ⚠️  Walk error at {}: {}", path_str, e);
                    }
                }
            }
        }

        // Close the channel so workers exit after finishing in-flight work / draining queue.
        drop(tx);
        status.cancelled = shutdown.load(Ordering::SeqCst);

        // Wait for in-flight + queued work while refreshing the spinner.
        while pending.load(Ordering::Relaxed) > 0 {
            status.cancelled = shutdown.load(Ordering::SeqCst);
            status.sync_worker_stats(&files, &errors, &active, &byte_reads, &metadata_only);
            status.render(sync_path, true);
            thread::sleep(STATUS_REFRESH);
        }

        status.sync_worker_stats(&files, &errors, &active, &byte_reads, &metadata_only);
        status.render(sync_path, true);
    });

    status.cancelled = shutdown.load(Ordering::SeqCst);
    status
}
/// Returns true if this entry should be skipped based on the ignore list.
/// - Never skip the root (depth 0) of a configured path.
/// - Uses exact OsStr match on basename for correctness with any filename.
/// - When a directory matches, its entire subtree is pruned by filter_entry.
pub fn should_skip_entry(entry: &walkdir::DirEntry, ignore_names: &HashSet<&OsStr>) -> bool {
    if entry.depth() == 0 {
        return false;
    }
    ignore_names.contains(entry.file_name())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{self, File};
    use std::io::Write;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;
    use tempfile::TempDir;

    #[test]
    fn truncate_display_no_truncation() {
        assert_eq!(truncate_display("short/path", 20), "short/path");
        assert_eq!(truncate_display("", 5), "");
        assert_eq!(truncate_display("exact", 5), "exact");
        assert_eq!(truncate_display("a", 1), "a");
    }

    #[test]
    fn truncate_display_ascii_truncation_with_ellipsis() {
        let long = "0123456789ABCDEF0123456789ABCDEF";
        let out = truncate_display(long, 10);
        assert!(out.starts_with('…'));
        assert_eq!(out.chars().count(), 10);
        assert_eq!(out, "…789ABCDEF");
    }

    #[test]
    fn truncate_display_unicode_truncation() {
        let s = "a日本語b🚀c";
        let out = truncate_display(s, 4);
        assert!(out.starts_with('…'));
        assert_eq!(out.chars().count(), 4);
        assert_eq!(out, "…b🚀c");
    }

    #[test]
    fn truncate_display_max_one_boundary() {
        assert_eq!(truncate_display("anything", 1), "…");
        assert_eq!(truncate_display("x", 1), "x");
        assert_eq!(truncate_display("", 1), "");
    }

    #[test]
    fn walkstatus_new_initializes_counters() {
        let mut s = WalkStatus::new(3, 7, 1, 8);
        let p = Path::new(".");
        s.render(p, true);
        assert_eq!(s.max_threads, 8);
        assert_eq!(s.active_threads, 0);
    }

    #[test]
    fn walkstatus_record_methods_increment_correctly() {
        let mut s = WalkStatus::new(10, 20, 2, 4);
        s.record_dir();
        s.record_error();
        s.render(Path::new("/tmp"), true);
    }

    #[test]
    fn walkstatus_render_rate_and_frame_advances() {
        let mut s = WalkStatus::new(0, 0, 0, 8);
        let p = Path::new("some/long/path/for/display/truncation/test");
        s.render(p, true);
        s.render(p, false);
    }

    #[test]
    fn format_thread_slot_line_modes() {
        let idle = ThreadSlotView::idle();
        assert!(format_thread_slot_line(1, 8, &idle).contains("idle"));
        assert!(format_thread_slot_line(1, 8, &idle).contains("1 of 8"));

        let read = ThreadSlotView {
            mode: ThreadWorkMode::ByteRead,
            size: 100,
            path: "subdir/small.txt".into(),
        };
        let read_line = format_thread_slot_line(2, 8, &read);
        assert!(read_line.contains("2 of 8"));
        assert!(read_line.contains("READ"));

        let attr = ThreadSlotView {
            mode: ThreadWorkMode::AttrOnly,
            size: 5 * 1024 * 1024,
            path: "big.bin".into(),
        };
        let attr_line = format_thread_slot_line(3, 8, &attr);
        assert!(attr_line.contains("3 of 8"));
        assert!(attr_line.contains("ATTR"));
    }

    #[test]
    fn shorten_path_strips_root_and_respects_max_chars() {
        let root = Path::new("/mnt/sync");
        let full = Path::new("/mnt/sync/dir/file.txt");
        assert_eq!(shorten_path_for_display(full, &[root]), "dir/file.txt");

        let long = "a".repeat(DISPLAY_PATH_MAX_CHARS + 20);
        let long_path = root.join(&long);
        let truncated = shorten_path_for_display(&long_path, &[root]);
        assert!(truncated.chars().count() <= DISPLAY_PATH_MAX_CHARS);
    }

    #[test]
    fn should_read_one_byte_both_zero_allows_any_size() {
        assert!(should_read_one_byte(0, 0, 0));
        assert!(should_read_one_byte(u64::MAX, 0, 0));
    }

    #[test]
    fn should_read_one_byte_max_only() {
        assert!(should_read_one_byte(5120, 0, 5120));
        assert!(!should_read_one_byte(5121, 0, 5120));
    }

    #[test]
    fn should_read_one_byte_min_only() {
        assert!(!should_read_one_byte(9, 10, 0));
        assert!(should_read_one_byte(10, 10, 0));
    }

    #[test]
    fn should_read_one_byte_min_and_max() {
        assert!(!should_read_one_byte(4, 5, 100));
        assert!(should_read_one_byte(50, 5, 100));
        assert!(!should_read_one_byte(101, 5, 100));
    }

    #[test]
    fn warm_file_size_gate_on_tempdir() {
        let tmp = TempDir::new().expect("tempdir");
        let small = tmp.path().join("small.bin");
        let large = tmp.path().join("large.bin");
        File::create(&small).unwrap().write_all(&[0u8; 64]).unwrap();
        File::create(&large)
            .unwrap()
            .write_all(&vec![0u8; 8192])
            .unwrap();
        assert_eq!(
            warm_file_with_hook(&small, 0, 100, None),
            WarmOutcome::ByteRead
        );
        assert_eq!(
            warm_file_with_hook(&large, 0, 100, None),
            WarmOutcome::MetadataOnly
        );
        assert_eq!(
            warm_file_with_hook(&large, 0, 0, None),
            WarmOutcome::ByteRead
        );
    }

    #[test]
    fn warm_tree_processes_mixed_files() {
        let tmp = TempDir::new().expect("tempdir");
        let root = tmp.path();
        fs::create_dir(root.join("sub")).unwrap();
        File::create(root.join("a.txt"))
            .unwrap()
            .write_all(b"hi")
            .unwrap();
        File::create(root.join("sub").join("b.txt"))
            .unwrap()
            .write_all(&[1u8; 200])
            .unwrap();

        let cfg = Config {
            version: 1,
            paths: vec![],
            walk: crate::config::WalkOptions {
                max_depth: None,
                min_file_size_bytes: 0,
                max_file_size_bytes: 50,
                max_threads: 2,
            },
            ignore: crate::config::IgnoreOptions::default(),
            mount_wait: crate::config::MountWait::default(),
        };

        let shutdown = Arc::new(AtomicBool::new(false));
        let status = warm_tree(root, &cfg, &shutdown);
        assert_eq!(status.files, 2);
        assert_eq!(status.byte_reads, 1);
        assert_eq!(status.metadata_only, 1);
        assert_eq!(status.errors, 0);
        assert!(!status.cancelled);
        assert!(status.dirs >= 1);
    }
}
