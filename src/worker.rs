//! Sync-tree warm worker: WalkDir, parallel READ/ATTR, live multi-line status.
//!
//! Directory listings stay on the walker thread; file open/read runs on a bounded pool.

use crate::config::{self, Config};
use crate::resolver;
use crate::verifier::{self, VerifyOutcome, WaitOptions};
use crate::warm_log::WarmLog;
use std::cell::Cell;
use std::collections::HashSet;
use std::env;
use std::ffi::OsStr;
use std::fmt::Write as _;
use std::fs;
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};
use walkdir::WalkDir;

const STATUS_REFRESH: Duration = Duration::from_millis(80);
/// Dense 8-dot Braille spinner (common CLI “finer” progress glyphs).
const SPINNER_FRAMES: &[char] = &['⣾', '⣽', '⣻', '⢿', '⡿', '⣟', '⣯', '⣷'];

/// Maximum characters for Source filename at the default threads-display width (80).
/// Extra columns from `walk.width` / `--width` above 80 lengthen only this field.
pub const DISPLAY_PATH_MAX_CHARS: usize = config::SOURCE_FILENAME_BASE_CHARS;

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
    /// File size is inside the configured window — File contents read (complete streaming read).
    ByteRead,
    /// File size is outside the window — attributes/metadata only (no File contents read).
    AttrOnly,
}

/// Per-worker slot for the multi-line spinner list.
#[derive(Debug, Clone)]
struct ThreadSlotView {
    mode: ThreadWorkMode,
    size: u64,
    /// Bytes streamed so far on a READ (0 for idle / ATTRIB).
    bytes_done: u64,
    /// Full path string; truncated at render time.
    path: String,
}

impl ThreadSlotView {
    fn idle() -> Self {
        Self {
            mode: ThreadWorkMode::Idle,
            size: 0,
            bytes_done: 0,
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
/// `max_chars`. Prefer the longest matching strip root.
pub fn shorten_path_for_display(path: &Path, strip_roots: &[&Path], max_chars: usize) -> String {
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

    truncate_display(&s, max_chars)
}

/// Cells in the per-row READ progress bar (each cell is 10% of the file).
const PROGRESS_CELLS: usize = 10;
const PROGRESS_EMPTY: char = '□';
const PROGRESS_FULL: char = '■';

/// Ten-cell bar: empty for idle/ATTRIB; floor(done/total×10) filled squares on READ.
/// A zero-length READ is shown as complete (nothing to stream).
fn format_progress_bar(done: u64, total: u64, reading: bool) -> String {
    if !reading {
        return PROGRESS_EMPTY.to_string().repeat(PROGRESS_CELLS);
    }
    if total == 0 {
        return PROGRESS_FULL.to_string().repeat(PROGRESS_CELLS);
    }
    let filled = done
        .saturating_mul(PROGRESS_CELLS as u64)
        .checked_div(total)
        .unwrap_or(0)
        .min(PROGRESS_CELLS as u64) as usize;
    format!(
        "{}{}",
        PROGRESS_FULL.to_string().repeat(filled),
        PROGRESS_EMPTY.to_string().repeat(PROGRESS_CELLS - filled)
    )
}

fn thread_table_header() -> String {
    format!(
        "{:<5}  {:>8}  {:<6}  {:<10}  Source filename",
        "Count", "Size", "Action", "Progress"
    )
}

/// Wrap header + worker rows in a box of exactly `width` cells so the right
/// border is not clipped (compose_status_redraw already limits to width-1).
fn box_table_lines(inner_lines: &[String], width: usize) -> Vec<String> {
    if width < 2 {
        return inner_lines.to_vec();
    }
    let inner = width - 2;
    let mut out = Vec::with_capacity(inner_lines.len() + 2);
    out.push(format!("┌{}┐", "─".repeat(inner)));
    for line in inner_lines {
        let text_w = inner.saturating_sub(1);
        let clipped = clip_to_columns(line, text_w);
        let pad = text_w.saturating_sub(clipped.chars().count());
        out.push(format!("│ {clipped}{}│", " ".repeat(pad)));
    }
    out.push(format!("└{}┘", "─".repeat(inner)));
    out
}

/// Erase the current physical row (`EL 2`) and the viewport from the cursor down (`ED 0`).
const CSI_ERASE_LINE: &str = "\x1b[2K";
const CSI_ERASE_DOWN: &str = "\x1b[J";

/// Columns we may write without triggering an automatic wrap.
///
/// A glyph placed in the last column makes many terminals advance to the next
/// row (the “eat-newline” / last-column wrap). CSI CUU/CPL then under-counts
/// physical rows and the next in-place redraw smears the table.
fn printable_columns(term_cols: usize) -> usize {
    term_cols.saturating_sub(1)
}

/// Truncate `s` to at most `max_cols` Unicode scalars (ASCII-width status text).
fn clip_to_columns(s: &str, max_cols: usize) -> String {
    let n = s.chars().count();
    if n <= max_cols {
        s.to_string()
    } else {
        s.chars().take(max_cols).collect()
    }
}

/// Visible `(columns, rows)` of stdout, or `COLUMNS`/`LINES`, or 80×24.
fn stdout_size() -> (usize, usize) {
    let fd = io::stdout().as_raw_fd();
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    let ok = unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws) } == 0;
    let ioctl_cols = if ok { usize::from(ws.ws_col) } else { 0 };
    let ioctl_rows = if ok { usize::from(ws.ws_row) } else { 0 };
    let cols = if ioctl_cols > 0 {
        ioctl_cols
    } else {
        env::var("COLUMNS")
            .ok()
            .and_then(|v| v.parse().ok())
            .filter(|v: &usize| *v > 0)
            .unwrap_or(80)
    };
    let rows = if ioctl_rows > 0 {
        ioctl_rows
    } else {
        env::var("LINES")
            .ok()
            .and_then(|v| v.parse().ok())
            .filter(|v: &usize| *v > 0)
            .unwrap_or(24)
    };
    (cols, rows)
}

/// In-place redraw of `lines`. `prev_rows` is how many physical rows the last
/// frame occupied (cursor is on the line immediately below that block).
///
/// Each emitted row is clipped to `term_cols - 1` so it cannot wrap, and the
/// block is clipped to `term_rows - 1` so the trailing newline cannot scroll
/// the first row off-screen. Either wrap or scroll would desynchronize
/// `\x1b[{n}F` from the real cursor location.
fn compose_status_redraw(
    prev_rows: usize,
    lines: &[String],
    term_cols: usize,
    term_rows: usize,
) -> (String, usize) {
    let max_cols = printable_columns(term_cols);
    let max_rows = term_rows.saturating_sub(1).max(1);
    let rows: Vec<String> = lines
        .iter()
        .take(max_rows)
        .map(|line| clip_to_columns(line, max_cols))
        .collect();
    let new_rows = rows.len();

    let mut out = String::new();
    if prev_rows > 0 {
        // CPL: column 0 of the first row of the previous block (not CUU, which
        // keeps the current column).
        let _ = write!(out, "\x1b[{prev_rows}F");
    }
    for line in &rows {
        let _ = writeln!(out, "{CSI_ERASE_LINE}{line}");
    }
    // Discard wrap remnants or leftover rows from a taller previous frame.
    out.push_str(CSI_ERASE_DOWN);
    (out, new_rows)
}

fn compose_status_clear(prev_rows: usize) -> String {
    if prev_rows == 0 {
        String::new()
    } else {
        format!("\x1b[{prev_rows}F{CSI_ERASE_DOWN}")
    }
}

fn write_status(buf: &str) {
    if buf.is_empty() {
        return;
    }
    let mut stdout = io::stdout().lock();
    let _ = stdout.write_all(buf.as_bytes());
    let _ = stdout.flush();
}

/// Format one numbered worker line for the live status table.
/// Example: `1        8.1KiB  READ    ■■□□□□□□□□  subdir/file.txt`
fn format_thread_slot_line(index_1based: usize, slot: &ThreadSlotView, path_max: usize) -> String {
    let bar = format_progress_bar(
        slot.bytes_done,
        slot.size,
        slot.mode == ThreadWorkMode::ByteRead,
    );
    match slot.mode {
        ThreadWorkMode::Idle => {
            format!("{index_1based:<5}  {:>8}  {:<6}  {bar}  —", "—", "idle")
        }
        ThreadWorkMode::ByteRead => {
            let name = truncate_display(&slot.path, path_max);
            let sz = format_bytes_compact(slot.size);
            format!("{index_1based:<5}  {sz:>8}  {:<6}  {bar}  {name}", "READ")
        }
        ThreadWorkMode::AttrOnly => {
            let name = truncate_display(&slot.path, path_max);
            let sz = format_bytes_compact(slot.size);
            // Outside size window: attributes-only (no File contents read).
            format!("{index_1based:<5}  {sz:>8}  {:<6}  {bar}  {name}", "ATTRIB")
        }
    }
}

/// Multi-line live progress: summary header + per-worker table.
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
    /// Sync mount root shown as “Local target” (home-shortened when possible).
    local_target: String,
    /// Shared with workers: current file / mode per thread index.
    slots: Arc<Mutex<Vec<ThreadSlotView>>>,
    /// Physical rows the last render occupied (CSI CPL count for the next frame).
    rendered_lines: usize,
    /// Warnings deferred until after [`WalkStatus::finish_line`] so they cannot
    /// desynchronize the live table cursor (and leave the box / summary behind).
    pub warnings: Vec<String>,
    /// Source filename max characters: 40 at width 80, plus (`width` − 80).
    path_max: usize,
}

impl WalkStatus {
    pub fn new(
        dirs: usize,
        files: usize,
        errors: usize,
        max_threads: usize,
        local_target: impl Into<String>,
        width: usize,
    ) -> Self {
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
            local_target: local_target.into(),
            slots,
            rendered_lines: 0,
            warnings: Vec::new(),
            path_max: config::source_filename_max_chars(width),
        }
    }

    pub fn record_dir(&mut self) {
        self.dirs += 1;
    }

    #[allow(dead_code)] // Public library API; warm_tree uses its shared atomic counter internally.
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

    pub fn render(&mut self, _current: &Path, force: bool) {
        if !force && self.last_render.elapsed() < STATUS_REFRESH {
            return;
        }

        self.frame = (self.frame + 1) % SPINNER_FRAMES.len();
        self.last_render = Instant::now();

        let spinner = SPINNER_FRAMES[self.frame];
        let elapsed = self.started.elapsed().as_secs();
        let target = truncate_display(&self.local_target, 48);

        // Fixed multi-line layout (each row must stay ≤ one physical terminal
        // row after compose_status_redraw clips to width-1):
        // 1 Running:
        // 2 spinner + counters + elapsed
        // 3 Local target
        // 4 STOPPING (only when cancelled)
        // 5 blank
        // 6 Running threads:
        // 7 blank
        // 8.. boxed table (top, header, worker rows, bottom)
        let summary = format!(
            "{spinner}  Directories: {dirs:>6}  Files: {files:>6}  Threads: {active}/{max_thr}  Errors: {errs}  Elapsed: {elapsed}s",
            dirs = self.dirs,
            files = self.files,
            active = self.active_threads,
            max_thr = self.max_threads,
            errs = self.errors,
        );
        let target_line = format!("    Local target: {target}");

        let max_thr = self.max_threads;
        let path_max = self.path_max;
        let slot_lines: Vec<String> = match self.slots.lock() {
            Ok(slots) => slots
                .iter()
                .enumerate()
                .map(|(i, s)| format_thread_slot_line(i + 1, s, path_max))
                .collect(),
            Err(_) => (0..max_thr)
                .map(|i| format_thread_slot_line(i + 1, &ThreadSlotView::idle(), path_max))
                .collect(),
        };

        let (cols, rows) = stdout_size();
        let table_width = printable_columns(cols).max(2);
        let mut table = Vec::with_capacity(1 + slot_lines.len());
        table.push(thread_table_header());
        table.extend(slot_lines);
        let boxed = box_table_lines(&table, table_width);

        let mut lines: Vec<String> = Vec::with_capacity(7 + boxed.len());
        lines.push("Running:".into());
        lines.push(summary);
        lines.push(target_line);
        if self.cancelled {
            lines.push("STOPPING (finish in-flight, no new work)".into());
        }
        lines.push(String::new());
        lines.push("Running threads:".into());
        lines.push(String::new());
        lines.extend(boxed);

        let (frame, n) = compose_status_redraw(self.rendered_lines, &lines, cols, rows);
        write_status(&frame);
        self.rendered_lines = n;
    }

    /// Erase the live multi-line status block (header + per-worker lines) so
    /// completed services do not leave idle-thread clutter on the terminal.
    /// Subsequent summary lines print in its place.
    pub fn finish_line(&mut self) {
        write_status(&compose_status_clear(self.rendered_lines));
        self.rendered_lines = 0;
    }
}

/// Outcome of warming a single file (File contents read vs attributes-only).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)] // Public library API; the binary uses the richer private worker result.
pub enum WarmOutcome {
    ByteRead,
    MetadataOnly,
    Error,
}

/// Whether this file should get a **File contents read** under the size policy.
///
/// `max_file_size_bytes` semantics:
/// - `-1` — never (metadata only)
/// - `0` — always (all sizes; ignores `min`)
/// - `N > 0` — when `len` is within `[min, N]` (`min == 0` means no lower bound)
pub fn should_read_file_contents(len: u64, min: u64, max: i64) -> bool {
    if max < 0 {
        // -1 (and any other negative after validation) → metadata only
        return false;
    }
    if max == 0 {
        return true;
    }
    let max_u = max as u64;
    if min != 0 && len < min {
        return false;
    }
    if len > max_u {
        return false;
    }
    true
}

fn log_path_fields(path: &Path) -> (String, String) {
    let filename = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let dir = path
        .parent()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    (dir, filename)
}

fn report_log_write_failure(log: &WarmLog) {
    if let Some(error) = log.claim_failure_report() {
        eprintln!("   ⚠️  warm log {error}");
    }
}

/// Write one successful CSV row for a warmed file (best-effort).
fn log_warm_success(log: &WarmLog, service: &str, path: &Path, size: u64, status: &str) {
    let (dir, filename) = log_path_fields(path);
    if log
        .log_file(service, &dir, &filename, size, status)
        .is_err()
    {
        report_log_write_failure(log);
    }
}

/// Write one detailed CSV error row for a file (best-effort).
fn log_warm_error(log: &WarmLog, service: &str, path: &Path, size: Option<u64>, details: &str) {
    let (dir, filename) = log_path_fields(path);
    if log
        .log_error(service, &dir, &filename, size, details)
        .is_err()
    {
        report_log_write_failure(log);
    }
}

/// Write a traversal-level error. Its path names the affected traversal target,
/// so the per-file filename and size fields remain blank.
fn log_traversal_error(log: &WarmLog, service: &str, path: Option<&Path>, details: &str) {
    let dir = path.map(|p| p.display().to_string()).unwrap_or_default();
    if log.log_error(service, &dir, "", None, details).is_err() {
        report_log_write_failure(log);
    }
}

/// Local VFS cache to wait on (and optionally BLAKE3-compare) after a content read.
pub struct WarmCache<'a> {
    pub sync_root: &'a Path,
    pub cache_root: &'a Path,
    pub checksum: bool,
    pub cancel: &'a AtomicBool,
    /// Bytes streamed from the mount file so far (source digest only).
    pub on_progress: Option<&'a dyn Fn(u64)>,
    /// rclone remote name (`gdrive` in `gdrive:folder`) when the cache is shared.
    pub vfs_remote: Option<&'a OsStr>,
    /// Collect messages instead of printing during the live table.
    pub warnings: Option<&'a Mutex<Vec<String>>>,
}

fn note_warning(cache: &WarmCache<'_>, msg: String) {
    if let Some(buf) = cache.warnings
        && let Ok(mut v) = buf.lock()
    {
        v.push(msg);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum WarmSuccess {
    ByteRead(u64),
    MetadataOnly(u64),
}

fn cache_failure(cache: &WarmCache<'_>, path: &Path, details: String) -> String {
    note_warning(cache, format!("verify {}: {details}", path.display()));
    details
}

/// Detailed implementation used by the tree worker. The public wrapper below
/// deliberately preserves the exported `WarmOutcome` API.
fn warm_file_detailed(
    path: &Path,
    min: u64,
    max: i64,
    on_classified: Option<&dyn Fn(u64, ThreadWorkMode)>,
    cache: Option<WarmCache<'_>>,
) -> Result<WarmSuccess, String> {
    let meta = fs::symlink_metadata(path)
        .map_err(|e| format!("source metadata {}: {e}", path.display()))?;
    let len = meta.len();
    // Drive Docs/Sheets and similar objects often stat (and read) as 0 bytes.
    // rclone then never writes a VFS cache file; treat them as metadata-only.
    let do_read = len > 0 && should_read_file_contents(len, min, max);
    let mode = if do_read {
        ThreadWorkMode::ByteRead
    } else {
        ThreadWorkMode::AttrOnly
    };
    if let Some(hook) = on_classified {
        hook(len, mode);
    }

    if !do_read {
        // Attributes already loaded via symlink_metadata — skip open/read for large blobs etc.
        return Ok(WarmSuccess::MetadataOnly(len));
    }

    if let Some(cache) = cache {
        let dest =
            resolver::resolve_for_remote(cache.sync_root, cache.cache_root, path, cache.vfs_remote)
                .map_err(|e| {
                    let details = format!("cache path resolution: {e}");
                    note_warning(&cache, format!("cache path {}: {e}", path.display()));
                    details
                })?;
        let wait = WaitOptions::default();
        let outcome = verifier::verify(
            path,
            &dest,
            cache.checksum,
            true,
            wait,
            cache.cancel,
            cache.on_progress,
        );
        return match outcome {
            VerifyOutcome::Verified | VerifyOutcome::ChecksumDisabled => {
                Ok(WarmSuccess::ByteRead(len))
            }
            VerifyOutcome::AttributesOnly => Ok(WarmSuccess::MetadataOnly(len)),
            VerifyOutcome::Cancelled => Err("cache verification cancelled".into()),
            VerifyOutcome::CacheFileTimeout => Err(cache_failure(
                &cache,
                path,
                format!(
                    "cache verification timeout: {} did not appear at the expected size and remain stable within {}s",
                    dest.display(),
                    wait.timeout.as_secs()
                ),
            )),
            VerifyOutcome::SourceChanged => Err(cache_failure(
                &cache,
                path,
                format!("source changed while being read: {}", path.display()),
            )),
            VerifyOutcome::DestinationChanged => Err(cache_failure(
                &cache,
                path,
                format!(
                    "cache file changed while being verified: {}",
                    dest.display()
                ),
            )),
            VerifyOutcome::SizeMismatch => Err(cache_failure(
                &cache,
                path,
                format!(
                    "verification size mismatch while reading source or cache: source={}, cache destination={}",
                    path.display(),
                    dest.display()
                ),
            )),
            VerifyOutcome::ChecksumMismatch => Err(cache_failure(
                &cache,
                path,
                format!(
                    "source/cache BLAKE3 checksum mismatch: source={}, cache={}",
                    path.display(),
                    dest.display()
                ),
            )),
            VerifyOutcome::RateLimited => Err(cache_failure(
                &cache,
                path,
                format!(
                    "cache verification access was rate limited: {}",
                    dest.display()
                ),
            )),
            VerifyOutcome::IoError(details) => Err(cache_failure(
                &cache,
                path,
                format!("cache verification I/O: {details}"),
            )),
        };
    }

    let mut file =
        fs::File::open(path).map_err(|e| format!("source open {}: {e}", path.display()))?;
    let mut buf = [0u8; 128 * 1024];
    let mut bytes = 0u64;
    loop {
        match file.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                bytes = bytes.checked_add(n as u64).ok_or_else(|| {
                    format!("source read byte count overflow: {}", path.display())
                })?;
            }
            Err(e) => {
                eprintln!("   ⚠️  mount read {}: {e}", path.display());
                return Err(format!("source read {}: {e}", path.display()));
            }
        }
    }

    // A complete stream is required to populate rclone VFS. Detect concurrent changes.
    let after = fs::symlink_metadata(path)
        .map_err(|e| format!("source metadata after read {}: {e}", path.display()))?;
    if bytes != len {
        return Err(format!(
            "source read size mismatch: expected {len} bytes, read {bytes} bytes ({})",
            path.display()
        ));
    }
    if after.len() != len {
        return Err(format!(
            "source changed while being read: size was {len} bytes and is now {} bytes ({})",
            after.len(),
            path.display()
        ));
    }
    Ok(WarmSuccess::ByteRead(len))
}

/// Warm one file: File contents read when size policy allows, otherwise attributes only.
///
/// When `on_classified` is provided, it is called after size is known and before
/// open/read so the live spinner can show path, size, and READ vs ATTR mode.
///
/// When `cache` is set, a content read streams the mount file, waits for the
/// matching rclone VFS cache object, and BLAKE3-compares the two if
/// `cache.checksum` is true. When checksum is false the full read and cache
/// stability/size checks still run. `cache == None` streams the mount file only
/// (size-policy unit tests).
#[allow(dead_code)] // Public library API and unit-test seam; warm_tree calls warm_file_detailed.
pub fn warm_file_with_hook(
    path: &Path,
    min: u64,
    max: i64,
    on_classified: Option<&dyn Fn(u64, ThreadWorkMode)>,
    cache: Option<WarmCache<'_>>,
) -> WarmOutcome {
    match warm_file_detailed(path, min, max, on_classified, cache) {
        Ok(WarmSuccess::ByteRead(_)) => WarmOutcome::ByteRead,
        Ok(WarmSuccess::MetadataOnly(_)) => WarmOutcome::MetadataOnly,
        Err(_) => WarmOutcome::Error,
    }
}

/// Walk `sync_path` and warm files with a bounded worker pool.
/// Directory listings stay on the walker thread; file open/read runs on workers.
///
/// On `shutdown` (SIGINT / `q`): stop enqueueing and discard queued work that has
/// not started; in-flight workers finish their current file, then exit.
///
/// When `log` is set, warm and traversal outcomes write CSV rows with detailed errors.
pub fn warm_tree(
    sync_path: &Path,
    cache_path: &Path,
    cfg: &Config,
    shutdown: &Arc<AtomicBool>,
    service_name: &str,
    log: Option<&Arc<WarmLog>>,
    vfs_remote: Option<&OsStr>,
) -> WalkStatus {
    let max_threads = cfg.walk.max_threads.max(1);
    let min_size = cfg.walk.min_file_size_bytes;
    let max_size = cfg.walk.max_file_size_bytes;
    let checksum = cfg.walk.checksum;
    let channel_cap = max_threads.saturating_mul(4).max(4);

    let (tx, rx) = mpsc::sync_channel::<PathBuf>(channel_cap);
    let rx = Arc::new(Mutex::new(rx));

    let files = Arc::new(AtomicUsize::new(0));
    let errors = Arc::new(AtomicUsize::new(0));
    let active = Arc::new(AtomicUsize::new(0));
    let byte_reads = Arc::new(AtomicUsize::new(0));
    let metadata_only = Arc::new(AtomicUsize::new(0));
    let pending = Arc::new(AtomicUsize::new(0));

    let path_max = config::source_filename_max_chars(cfg.walk.width);
    // Local target is not the Source filename field; keep the 40-character base.
    let local_target = shorten_path_for_display(sync_path, &[], DISPLAY_PATH_MAX_CHARS);
    let mut status = WalkStatus::new(0, 0, 0, max_threads, local_target, cfg.walk.width);
    let slots = Arc::clone(&status.slots);
    let ignore_names: HashSet<&OsStr> = cfg.ignore.names.iter().map(OsStr::new).collect();
    let sync_root = sync_path.to_path_buf();
    let cache_root = cache_path.to_path_buf();
    let service_name = service_name.to_string();
    let log = log.cloned();
    let vfs_remote = vfs_remote.map(OsStr::to_os_string);
    let warnings = Arc::new(Mutex::new(Vec::new()));

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
            let cache_root = cache_root.clone();
            let service_name = service_name.clone();
            let log = log.clone();
            let vfs_remote = vfs_remote.clone();
            let warnings = Arc::clone(&warnings);

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
                                shorten_path_for_display(&path, &[sync_root.as_path()], path_max);
                            let slots_for_hook = Arc::clone(&slots);
                            let slots_for_progress = Arc::clone(&slots);
                            let seen_size = Cell::new(None::<u64>);
                            let outcome = warm_file_detailed(
                                &path,
                                min_size,
                                max_size,
                                Some(&|size, mode| {
                                    seen_size.set(Some(size));
                                    if let Ok(mut slots) = slots_for_hook.lock()
                                        && let Some(slot) = slots.get_mut(worker_id)
                                    {
                                        *slot = ThreadSlotView {
                                            mode,
                                            size,
                                            bytes_done: 0,
                                            path: display_path.clone(),
                                        };
                                    }
                                }),
                                Some(WarmCache {
                                    sync_root: sync_root.as_path(),
                                    cache_root: cache_root.as_path(),
                                    checksum,
                                    cancel: &shutdown,
                                    on_progress: Some(&|n| {
                                        if let Ok(mut slots) = slots_for_progress.lock()
                                            && let Some(slot) = slots.get_mut(worker_id)
                                        {
                                            slot.bytes_done = n;
                                        }
                                    }),
                                    vfs_remote: vfs_remote.as_deref(),
                                    warnings: Some(warnings.as_ref()),
                                }),
                            );
                            match outcome {
                                Ok(WarmSuccess::ByteRead(size)) => {
                                    byte_reads.fetch_add(1, Ordering::Relaxed);
                                    if let Some(ref log) = log {
                                        log_warm_success(log, &service_name, &path, size, "READ");
                                    }
                                }
                                Ok(WarmSuccess::MetadataOnly(size)) => {
                                    metadata_only.fetch_add(1, Ordering::Relaxed);
                                    if let Some(ref log) = log {
                                        log_warm_success(log, &service_name, &path, size, "ATTRIB");
                                    }
                                }
                                Err(details) => {
                                    errors.fetch_add(1, Ordering::Relaxed);
                                    if let Some(ref log) = log {
                                        log_warm_error(
                                            log,
                                            &service_name,
                                            &path,
                                            seen_size.get(),
                                            &details,
                                        );
                                    }
                                }
                            }
                        }
                        Err(_) => break, // sender dropped — no more work
                    }
                }
            });
        }
        // Only workers should own receivers. If they all exit unexpectedly,
        // try_send must report Disconnected instead of spinning forever on Full.
        drop(rx);

        let mut walker = WalkDir::new(sync_path).follow_links(false);
        if let Some(depth) = cfg.walk.max_depth {
            walker = walker.max_depth(depth);
        }
        let walker = walker
            .into_iter()
            .filter_entry(|entry| !should_skip_entry(entry, &ignore_names));
        let mut traversal_errors = 0usize;
        let mut failed_directory_listings = HashSet::<PathBuf>::new();

        'walk: for entry in walker {
            if shutdown.load(Ordering::SeqCst) {
                status.cancelled = true;
                break;
            }
            match entry {
                Ok(entry) => {
                    let path = entry.path();
                    if entry.file_type().is_dir() {
                        status.record_dir();
                        if let Err(e) = fs::read_dir(path) {
                            failed_directory_listings.insert(path.to_path_buf());
                            errors.fetch_add(1, Ordering::Relaxed);
                            traversal_errors += 1;
                            let details = format!("directory listing {}: {e}", path.display());
                            if let Some(ref log) = log {
                                log_traversal_error(log, &service_name, Some(path), &details);
                            }
                            if traversal_errors.is_multiple_of(100) {
                                status.finish_line();
                                eprintln!(
                                    "   ⚠️  Directory listing error at {}: {e}",
                                    path.display()
                                );
                            }
                        }
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
                                    // No receiver remains, so queued work cannot drain.
                                    pending.store(0, Ordering::Relaxed);
                                    errors.fetch_add(1, Ordering::Relaxed);
                                    if let Some(ref log) = log {
                                        log_warm_error(
                                            log,
                                            &service_name,
                                            path,
                                            None,
                                            "worker queue disconnected before file dispatch",
                                        );
                                    }
                                    break 'walk;
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
                    // An explicit directory warm can fail before WalkDir reports
                    // the same path; retain one error row/count for that failure.
                    if e.path()
                        .is_some_and(|path| failed_directory_listings.remove(path))
                    {
                        continue;
                    }
                    errors.fetch_add(1, Ordering::Relaxed);
                    traversal_errors += 1;
                    let details = format!("directory traversal: {e}");
                    if let Some(ref log) = log {
                        log_traversal_error(log, &service_name, e.path(), &details);
                    }
                    status.sync_worker_stats(&files, &errors, &active, &byte_reads, &metadata_only);
                    status.render(
                        e.path().unwrap_or_else(|| Path::new("<unknown>")),
                        traversal_errors.is_multiple_of(10),
                    );
                    if traversal_errors.is_multiple_of(100) {
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
    if let Ok(mut w) = warnings.lock() {
        status.warnings = std::mem::take(&mut *w);
    }
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
    fn clip_to_columns_ascii_and_unicode() {
        assert_eq!(clip_to_columns("abcd", 10), "abcd");
        assert_eq!(clip_to_columns("abcd", 4), "abcd");
        assert_eq!(clip_to_columns("abcd", 3), "abc");
        assert_eq!(clip_to_columns("", 5), "");
        assert_eq!(clip_to_columns("⣾  Directories", 1), "⣾");
        assert_eq!(
            clip_to_columns("x".repeat(200).as_str(), 79)
                .chars()
                .count(),
            79
        );
    }

    #[test]
    fn printable_columns_leaves_last_cell_empty() {
        assert_eq!(printable_columns(80), 79);
        assert_eq!(printable_columns(1), 0);
        assert_eq!(printable_columns(0), 0);
    }

    #[test]
    fn compose_status_redraw_first_frame_does_not_move_cursor() {
        let lines = vec!["Running:".into(), "summary".into(), "row".into()];
        let (out, n) = compose_status_redraw(0, &lines, 80, 24);
        assert_eq!(n, 3);
        assert!(
            !out.contains("F") && !out.contains("A"),
            "first frame must not emit CUU/CPL: {out:?}"
        );
        assert!(out.contains(CSI_ERASE_LINE));
        assert!(out.ends_with(CSI_ERASE_DOWN));
    }

    #[test]
    fn compose_status_redraw_repositions_by_previous_physical_rows() {
        let lines = vec!["Running:".into(), "summary".into(), "row".into()];
        let (_, n1) = compose_status_redraw(0, &lines, 80, 24);
        let (out, n2) = compose_status_redraw(n1, &lines, 80, 24);
        assert_eq!(n1, 3);
        assert_eq!(n2, 3);
        assert!(
            out.starts_with("\x1b[3F"),
            "must CPL by the previous physical row count, got {out:?}"
        );
    }

    #[test]
    fn compose_status_redraw_clips_so_no_row_can_wrap() {
        let long = "x".repeat(200);
        let thread = format_thread_slot_line(
            1,
            &ThreadSlotView {
                mode: ThreadWorkMode::ByteRead,
                size: 8300,
                bytes_done: 8300,
                path: "a".repeat(DISPLAY_PATH_MAX_CHARS),
            },
            DISPLAY_PATH_MAX_CHARS,
        );
        let lines = vec![long, thread];
        let (out, n) = compose_status_redraw(0, &lines, 80, 24);
        assert_eq!(n, 2);
        let max_cols = printable_columns(80);
        let visible_rows = visible_rows_from_frame(&out);
        assert_eq!(visible_rows.len(), 2);
        for row in &visible_rows {
            assert!(
                row.chars().count() <= max_cols,
                "row would wrap on 80-col terminal ({} cells): {row:?}",
                row.chars().count()
            );
            assert!(
                !row.contains(&" ".repeat(20)),
                "must not space-pad rows (that was wrapping at 200 columns): {row:?}"
            );
        }
        assert!(!out.contains(&"x".repeat(80)));
        assert!(visible_rows[0].chars().all(|c| c == 'x'));
        assert_eq!(visible_rows[0].chars().count(), max_cols);
    }

    #[test]
    fn compose_status_redraw_caps_block_to_terminal_height() {
        let lines: Vec<String> = (0..30).map(|i| format!("line-{i}")).collect();
        let (out, n) = compose_status_redraw(0, &lines, 80, 10);
        assert_eq!(
            n, 9,
            "must leave one row free so the last newline does not scroll"
        );
        let visible = visible_rows_from_frame(&out);
        assert_eq!(visible.len(), 9);
        assert_eq!(visible[0], "line-0");
        assert_eq!(visible[8], "line-8");
    }

    #[test]
    fn compose_status_redraw_realistic_table_stays_one_row_per_line() {
        let summary = format!(
            "{}  Directories: {:>6}  Files: {:>6}  Threads: 8/8  Errors: 0  Elapsed: 12s",
            SPINNER_FRAMES[0], 1234, 5678
        );
        let slot = format_thread_slot_line(
            1,
            &ThreadSlotView {
                mode: ThreadWorkMode::ByteRead,
                size: 8300,
                bytes_done: 4150,
                path: "subdir/file.txt".into(),
            },
            DISPLAY_PATH_MAX_CHARS,
        );
        let mut table = vec![thread_table_header()];
        table.extend((0..8).map(|i| {
            if i == 0 {
                slot.clone()
            } else {
                format_thread_slot_line(i + 1, &ThreadSlotView::idle(), DISPLAY_PATH_MAX_CHARS)
            }
        }));
        let boxed = box_table_lines(&table, printable_columns(80));
        let mut lines = vec![
            "Running:".into(),
            summary,
            "    Local target: ~/mounts/project".into(),
            String::new(),
            "Running threads:".into(),
            String::new(),
        ];
        lines.extend(boxed);
        assert!(
            lines[1].chars().count() <= 79,
            "counter line must fit an 80-col terminal: {} cells",
            lines[1].chars().count()
        );
        assert!(
            lines[2].chars().count() <= 79,
            "target line must fit an 80-col terminal: {} cells",
            lines[2].chars().count()
        );
        assert_eq!(lines.len(), 17);
        let (first, n1) = compose_status_redraw(0, &lines, 80, 24);
        let (second, n2) = compose_status_redraw(n1, &lines, 80, 24);
        assert_eq!(n1, 17);
        assert_eq!(n2, 17);
        assert!(second.starts_with("\x1b[17F"));
        let framed = visible_rows_from_frame(&first);
        assert!(framed[6].starts_with('┌') && framed[6].ends_with('┐'));
        assert!(framed[16].starts_with('└') && framed[16].ends_with('┘'));
        assert_eq!(framed[6].chars().count(), printable_columns(80));
        let max_cols = printable_columns(80);
        for row in visible_rows_from_frame(&first) {
            assert!(row.chars().count() <= max_cols);
        }
    }

    #[test]
    fn compose_status_clear_returns_to_block_origin() {
        assert_eq!(compose_status_clear(0), "");
        assert_eq!(
            compose_status_clear(15),
            format!("\x1b[15F{CSI_ERASE_DOWN}")
        );
    }

    fn visible_rows_from_frame(frame: &str) -> Vec<String> {
        let mut rows = Vec::new();
        let mut current = String::new();
        let mut chars = frame.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\u{1b}' {
                if chars.peek() == Some(&'[') {
                    chars.next();
                    for next in chars.by_ref() {
                        if next.is_ascii_alphabetic() {
                            break;
                        }
                    }
                }
                continue;
            }
            if c == '\n' {
                rows.push(std::mem::take(&mut current));
                continue;
            }
            current.push(c);
        }
        // Trailing ED 0 leaves no extra row.
        rows
    }

    #[test]
    fn walkstatus_new_initializes_counters() {
        let mut s = WalkStatus::new(3, 7, 1, 8, "~/Documents/Gdrive/AccessIT", 80);
        let p = Path::new(".");
        s.render(p, true);
        assert_eq!(s.max_threads, 8);
        assert_eq!(s.active_threads, 0);
        assert!(s.local_target.contains("AccessIT") || s.local_target.contains("Gdrive"));
    }

    #[test]
    fn walkstatus_record_methods_increment_correctly() {
        let mut s = WalkStatus::new(10, 20, 2, 4, "/tmp/mount", 80);
        s.record_dir();
        s.record_error();
        s.render(Path::new("/tmp"), true);
    }

    #[test]
    fn walkstatus_render_rate_and_frame_advances() {
        let mut s = WalkStatus::new(0, 0, 0, 8, "~/mounts/project", 80);
        let p = Path::new("some/long/path/for/display/truncation/test");
        s.render(p, true);
        s.render(p, false);
    }

    #[test]
    fn format_progress_bar_tenths() {
        assert_eq!(format_progress_bar(0, 100, false), "□□□□□□□□□□");
        assert_eq!(format_progress_bar(0, 100, true), "□□□□□□□□□□");
        assert_eq!(format_progress_bar(9, 100, true), "□□□□□□□□□□");
        assert_eq!(format_progress_bar(10, 100, true), "■□□□□□□□□□");
        assert_eq!(format_progress_bar(50, 100, true), "■■■■■□□□□□");
        assert_eq!(format_progress_bar(100, 100, true), "■■■■■■■■■■");
        assert_eq!(format_progress_bar(0, 0, true), "■■■■■■■■■■");
        assert_eq!(format_progress_bar(0, 1, false), "□□□□□□□□□□");
    }

    #[test]
    fn box_table_lines_is_continuous_and_fixed_width() {
        let inner = vec!["Count  Size".into(), "1      8B".into()];
        let boxed = box_table_lines(&inner, 40);
        assert_eq!(boxed.len(), 4);
        assert!(boxed[0].starts_with('┌') && boxed[0].ends_with('┐'));
        assert!(boxed[1].starts_with('│') && boxed[1].ends_with('│'));
        assert!(boxed[3].starts_with('└') && boxed[3].ends_with('┘'));
        for row in &boxed {
            assert_eq!(row.chars().count(), 40);
        }
    }

    #[test]
    fn format_thread_slot_line_modes() {
        let idle = ThreadSlotView::idle();
        let idle_line = format_thread_slot_line(1, &idle, DISPLAY_PATH_MAX_CHARS);
        assert!(idle_line.contains("idle"));
        assert!(idle_line.starts_with('1'));

        let read = ThreadSlotView {
            mode: ThreadWorkMode::ByteRead,
            size: 100,
            bytes_done: 50,
            path: "subdir/small.txt".into(),
        };
        let read_line = format_thread_slot_line(2, &read, DISPLAY_PATH_MAX_CHARS);
        assert!(read_line.starts_with('2'));
        assert!(read_line.contains("READ"));
        assert!(read_line.contains("subdir/small.txt"));
        assert!(read_line.contains(&format_progress_bar(50, 100, true)));

        let attr = ThreadSlotView {
            mode: ThreadWorkMode::AttrOnly,
            size: 5 * 1024 * 1024,
            bytes_done: 0,
            path: "big.bin".into(),
        };
        let attr_line = format_thread_slot_line(3, &attr, DISPLAY_PATH_MAX_CHARS);
        assert!(attr_line.starts_with('3'));
        assert!(attr_line.contains("ATTRIB"));
        assert!(attr_line.contains(&format_progress_bar(0, 0, false)));
    }

    #[test]
    fn shorten_path_strips_root_and_respects_max_chars() {
        let root = Path::new("/mnt/sync");
        let full = Path::new("/mnt/sync/dir/file.txt");
        assert_eq!(
            shorten_path_for_display(full, &[root], DISPLAY_PATH_MAX_CHARS),
            "dir/file.txt"
        );

        let long = "a".repeat(DISPLAY_PATH_MAX_CHARS + 20);
        let long_path = root.join(&long);
        let truncated = shorten_path_for_display(&long_path, &[root], DISPLAY_PATH_MAX_CHARS);
        assert!(truncated.chars().count() <= DISPLAY_PATH_MAX_CHARS);

        let wide = config::source_filename_max_chars(120);
        let wide_trunc = shorten_path_for_display(&long_path, &[root], wide);
        assert_eq!(wide, 80);
        assert!(wide_trunc.chars().count() <= wide);
        assert!(wide_trunc.chars().count() > DISPLAY_PATH_MAX_CHARS);
    }

    #[test]
    fn source_filename_grows_only_with_extra_width() {
        let path = "x".repeat(200);
        let slot = ThreadSlotView {
            mode: ThreadWorkMode::ByteRead,
            size: 1,
            bytes_done: 1,
            path: path.clone(),
        };
        let at_80 = format_thread_slot_line(1, &slot, config::source_filename_max_chars(80));
        let at_120 = format_thread_slot_line(1, &slot, config::source_filename_max_chars(120));
        let name_80 = at_80.rsplit("  ").next().unwrap();
        let name_120 = at_120.rsplit("  ").next().unwrap();
        assert_eq!(name_80.chars().count(), DISPLAY_PATH_MAX_CHARS);
        assert_eq!(name_120.chars().count(), 80);
        assert_eq!(
            at_120.chars().count() - at_80.chars().count(),
            40,
            "only Source filename should grow with extra width"
        );
    }

    #[test]
    fn should_read_file_contents_zero_means_all() {
        assert!(should_read_file_contents(0, 0, 0));
        assert!(should_read_file_contents(u64::MAX, 0, 0));
        // max 0 ignores min
        assert!(should_read_file_contents(1, 100, 0));
    }

    #[test]
    fn should_read_file_contents_minus_one_is_metadata_only() {
        assert!(!should_read_file_contents(0, 0, -1));
        assert!(!should_read_file_contents(u64::MAX, 0, -1));
    }

    #[test]
    fn should_read_file_contents_max_only() {
        assert!(should_read_file_contents(5120, 0, 5120));
        assert!(!should_read_file_contents(5121, 0, 5120));
    }

    #[test]
    fn should_read_file_contents_min_and_max() {
        assert!(!should_read_file_contents(4, 5, 100));
        assert!(should_read_file_contents(50, 5, 100));
        assert!(!should_read_file_contents(101, 5, 100));
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
            warm_file_with_hook(&small, 0, 100, None, None),
            WarmOutcome::ByteRead
        );
        assert_eq!(
            warm_file_with_hook(&large, 0, 100, None, None),
            WarmOutcome::MetadataOnly
        );
        assert_eq!(
            warm_file_with_hook(&large, 0, 0, None, None),
            WarmOutcome::ByteRead
        );
    }

    #[test]
    fn warm_file_error_retains_source_metadata_detail() {
        let tmp = TempDir::new().expect("tempdir");
        let missing = tmp.path().join("missing.txt");
        let result = warm_file_detailed(&missing, 0, 0, None, None);
        let details = result.expect_err("missing source must fail");
        assert!(details.starts_with("source metadata "));
        assert!(details.contains("missing.txt"));
        assert!(!details.ends_with(':'));
        assert_eq!(
            warm_file_with_hook(&missing, 0, 0, None, None),
            WarmOutcome::Error
        );
    }

    #[test]
    fn warm_file_honours_checksum_flag() {
        let tmp = TempDir::new().expect("tempdir");
        let sync = tmp.path().join("sync");
        let cache = tmp.path().join("cache");
        fs::create_dir_all(&sync).unwrap();
        let src = sync.join("a.txt");
        File::create(&src).unwrap().write_all(b"abc").unwrap();
        let dest_dir = cache.join("vfs").join("remote");
        fs::create_dir_all(&dest_dir).unwrap();
        File::create(dest_dir.join("a.txt"))
            .unwrap()
            .write_all(b"abd")
            .unwrap();
        let cancel = AtomicBool::new(false);
        let mismatch = warm_file_detailed(
            &src,
            0,
            0,
            None,
            Some(WarmCache {
                sync_root: &sync,
                cache_root: &cache,
                checksum: true,
                cancel: &cancel,
                on_progress: None,
                vfs_remote: None,
                warnings: None,
            }),
        )
        .expect_err("different contents must fail checksum verification");
        assert!(mismatch.contains("source/cache BLAKE3 checksum mismatch"));
        assert!(mismatch.contains("a.txt"));
        assert_eq!(
            warm_file_with_hook(
                &src,
                0,
                0,
                None,
                Some(WarmCache {
                    sync_root: &sync,
                    cache_root: &cache,
                    checksum: false,
                    cancel: &cancel,
                    on_progress: None,
                    vfs_remote: None,
                    warnings: None,
                }),
            ),
            WarmOutcome::ByteRead
        );
        File::create(dest_dir.join("a.txt"))
            .unwrap()
            .write_all(b"abc")
            .unwrap();
        assert_eq!(
            warm_file_with_hook(
                &src,
                0,
                0,
                None,
                Some(WarmCache {
                    sync_root: &sync,
                    cache_root: &cache,
                    checksum: true,
                    cancel: &cancel,
                    on_progress: None,
                    vfs_remote: None,
                    warnings: None,
                }),
            ),
            WarmOutcome::ByteRead
        );
    }

    #[test]
    fn warm_tree_processes_mixed_files() {
        let tmp = TempDir::new().expect("tempdir");
        let root = tmp.path().join("sync");
        let cache = tmp.path().join("cache");
        fs::create_dir_all(root.join("sub")).unwrap();
        File::create(root.join("a.txt"))
            .unwrap()
            .write_all(b"hi")
            .unwrap();
        File::create(root.join("sub").join("b.txt"))
            .unwrap()
            .write_all(&[1u8; 200])
            .unwrap();
        // rclone VFS layout for the content-read file (a.txt is 2 bytes, within max=50).
        let vfs = cache.join("vfs").join("remote");
        fs::create_dir_all(&vfs).unwrap();
        File::create(vfs.join("a.txt"))
            .unwrap()
            .write_all(b"hi")
            .unwrap();

        let cfg = Config {
            version: 1,
            paths: vec![],
            walk: crate::config::WalkOptions {
                checksum: true,
                max_depth: None,
                min_file_size_bytes: 0,
                max_file_size_bytes: 50,
                max_threads: 2,
                width: 80,
            },
            ignore: crate::config::IgnoreOptions::default(),
            mount_wait: crate::config::MountWait::default(),
        };

        let shutdown = Arc::new(AtomicBool::new(false));
        let log = Arc::new(WarmLog::create().expect("create log"));
        let log_path = log.path().to_path_buf();
        let mut status = warm_tree(
            &root,
            &cache,
            &cfg,
            &shutdown,
            "demo.service",
            Some(&log),
            None,
        );
        status.finish_line();
        log.flush().unwrap();
        assert_eq!(status.files, 2);
        assert_eq!(status.byte_reads, 1);
        assert_eq!(status.metadata_only, 1);
        assert_eq!(status.errors, 0);
        assert!(!status.cancelled);
        assert!(status.dirs >= 1);
        let text = fs::read_to_string(&log_path).unwrap();
        let rows: Vec<&str> = text.lines().skip(1).collect();
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().any(|row| row.starts_with("\"demo.service\",")
            && row.ends_with(",\"a.txt\",2,\"READ\",\"\"")));
        assert!(rows.iter().any(|row| {
            row.starts_with("\"demo.service\",") && row.ends_with(",\"b.txt\",200,\"ATTRIB\",\"\"")
        }));
        let _ = fs::remove_file(log_path);
    }

    #[test]
    fn warm_tree_logs_resolver_error_with_known_size() {
        let tmp = TempDir::new().expect("tempdir");
        let root = tmp.path().join("sync");
        let cache = tmp.path().join("cache");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&cache).unwrap();
        File::create(root.join("a.txt"))
            .unwrap()
            .write_all(b"abc")
            .unwrap();

        let cfg = Config {
            version: 1,
            paths: vec![],
            walk: crate::config::WalkOptions {
                checksum: true,
                max_depth: None,
                min_file_size_bytes: 0,
                max_file_size_bytes: 0,
                max_threads: 1,
                width: 80,
            },
            ignore: crate::config::IgnoreOptions::default(),
            mount_wait: crate::config::MountWait::default(),
        };

        let shutdown = Arc::new(AtomicBool::new(false));
        let log = Arc::new(WarmLog::create().expect("create log"));
        let log_path = log.path().to_path_buf();
        let mut status = warm_tree(
            &root,
            &cache,
            &cfg,
            &shutdown,
            "demo.service",
            Some(&log),
            None,
        );
        status.finish_line();
        log.flush().unwrap();

        assert_eq!(status.files, 1);
        assert_eq!(status.errors, 1);
        assert_eq!(status.byte_reads, 0);
        assert_eq!(status.metadata_only, 0);
        let text = fs::read_to_string(&log_path).unwrap();
        assert!(text.contains("\"demo.service\","));
        assert!(text.contains(",\"a.txt\",3,\"ERROR\","));
        assert!(text.contains("cache path resolution:"));
        assert_eq!(text.matches(",\"ERROR\",").count(), status.errors);
        let _ = fs::remove_file(log_path);
    }

    #[test]
    fn warm_tree_retains_and_logs_traversal_error_count() {
        let tmp = TempDir::new().expect("tempdir");
        let missing_root = tmp.path().join("missing-sync-root");
        let cache = tmp.path().join("cache");
        fs::create_dir_all(&cache).unwrap();
        let cfg = Config {
            version: 1,
            paths: vec![],
            walk: crate::config::WalkOptions {
                checksum: true,
                max_depth: None,
                min_file_size_bytes: 0,
                max_file_size_bytes: 0,
                max_threads: 1,
                width: 80,
            },
            ignore: crate::config::IgnoreOptions::default(),
            mount_wait: crate::config::MountWait::default(),
        };

        let shutdown = Arc::new(AtomicBool::new(false));
        let log = Arc::new(WarmLog::create().expect("create log"));
        let log_path = log.path().to_path_buf();
        let mut status = warm_tree(
            &missing_root,
            &cache,
            &cfg,
            &shutdown,
            "demo.service",
            Some(&log),
            None,
        );
        status.finish_line();
        log.flush().unwrap();

        assert_eq!(status.files, 0);
        assert_eq!(status.errors, 1);
        let text = fs::read_to_string(&log_path).unwrap();
        assert!(text.contains("\"demo.service\","));
        assert!(text.contains(",\"\",,\"ERROR\",\"directory traversal:"));
        assert_eq!(text.matches(",\"ERROR\",").count(), status.errors);
        let _ = fs::remove_file(log_path);
    }
}
