# warm-drive-cache

> External configuration via `config.json` (supports paths, `max_depth`, and ignore names such as `.git`).
> See the "Configuration via `config.json`" section below.

Rust utility that pre-warms the VFS cache on [rclone](https://rclone.org/) FUSE mounts. Cloud-backed directories often appear instantly while listings stay empty for several seconds. This tool waits for mounts to settle, then walks configured paths — touching metadata and directory listings so subsequent access is faster.

## What it does

1. **Checks each configured root path** — skips paths that do not exist or cannot be read.
2. **Waits for mount content** — FUSE mount points can exist before Google Drive (or other remote) content is visible:
   - 3 s initial settle time
   - Retries at 3 s, 5 s, and 8 s if the directory still looks empty
   - 30 s hard cap per path; proceeds anyway if the budget is exhausted
3. **Walks the tree** — uses `walkdir` with `follow_links(false)` for safety on cloud mounts.
4. **Touches cache entries** — for each entry:
   - `symlink_metadata()` to pull file/dir metadata into the VFS cache (does not follow symlinks)
   - `read_dir()` on directories to cache listing data
5. **Reports live progress** — single-line spinner with dir/file/error counts and current path.
6. **Summarises results** — per-path and grand totals; occasional error logging (every 100 walk errors) to avoid noise from transient cloud-mount failures.

## Program Flow

```mermaid
flowchart TD
    Start[Start] --> Banner[Print startup banner]
    Banner --> LoadConfig[Load config<br/>WARM_DRIVE_CACHE_CONFIG env<br/>or XDG ~/.config/.../config.json]
    LoadConfig --> Validate{Valid config?<br/>paths non-empty?}
    Validate -->|No| Error[Print clear error<br/>exit 1]
    Validate -->|Yes| ForEach[For each root path in config.paths]
    ForEach --> Exists{try_exists?}
    Exists -->|No| Skip[errors++<br/>continue]
    Exists -->|Yes| Wait[wait_for_mount_content<br/>using config.mount_wait<br/>sleep_capped + retries]
    Wait --> Status[Init WalkStatus counters]
    Status --> Walker[Build WalkDir<br/>follow_links=false<br/>max_depth if set<br/>filter_entry for ignore.names]
    Walker --> EntryLoop[For each entry]
    EntryLoop --> Touch[symlink_metadata<br/>touch to warm VFS]
    Touch --> IsDir{entry.is_dir?}
    IsDir -->|Yes| Dir[record_dir<br/>read_dir to cache listing]
    IsDir -->|No| File[record_file]
    Dir & File --> Render[render live progress<br/>spinner + truncate + counts]
    Render --> EntryLoop
    EntryLoop -->|walk complete| PerPath[Sync totals<br/>final render<br/>print per-path summary]
    PerPath --> ForEach
    ForEach -->|all paths done| Grand[Print grand totals<br/>dirs/files/errors]
    Grand --> End[End]
    
    classDef error fill:#f99,stroke:#333
    class Error,Skip error
```

The diagram shows the high-level flow from startup through config-driven path processing, mount settling, controlled walking (with depth and ignores), metadata touching for VFS warming, and live + final reporting.

## Configuration via `config.json`

The tool reads a JSON config file instead of a hardcoded array.

**Location (in priority order):**
- `WARM_DRIVE_CACHE_CONFIG=/absolute/path/to/config.json` (env override — great for testing / CI / multiple setups)
- `$XDG_CONFIG_HOME/warm-drive-cache/config.json` (or `~/.config/warm-drive-cache/config.json` on Arch)

**Minimal working example**
```json
{
  "paths": [
    "/home/charlie/Documents/Gdrive/AccessIT",
    "/home/charlie/Documents/Gdrive/xSAR"
  ]
}
```

**Full example with new options**
```json
{
  "version": 1,
  "paths": [
    "/home/charlie/Documents/Gdrive/AccessIT",
    "/home/charlie/Documents/Gdrive/xSAR"
  ],
  "walk": {
    "max_depth": 5
  },
  "ignore": {
    "names": [".git", ".svn", "node_modules", "target", "__pycache__"]
  },
  "mount_wait": {
    "initial_secs": 3,
    "retry_delays_secs": [3, 5, 8],
    "max_wait_secs": 30
  }
}
```

- `walk.max_depth` — optional, `null`/omitted = unlimited (old default behaviour).
- `ignore.names` — basenames (dirs or files) pruned early. The classic `.git` case is now first-class.
- `mount_wait` values are fully tunable; omitting any field uses the classic defaults.

**Absolute paths required** (the loader rejects relatives for safety and predictability).

Create the directory and file once:
```bash
mkdir -p ~/.config/warm-drive-cache
# then drop your config.json there
```

See also the example shipped as `config.example.json` (if present in the repo).

## Requirements

- Rust stable (2024 edition)
- rclone remote(s) already mounted via FUSE (e.g. under `~/Documents/Gdrive/…`)
- Linux (uses standard `std::fs` + directory walk; developed on Arch)

## Build & run

```bash
cargo build --release
./target/release/warm-drive-cache
```

Debug build:

```bash
cargo run
```

## Testing

```bash
cargo test
cargo test -- --quiet
cargo test truncate_display   # narrow filter
```

Unit tests cover the pure helpers and core logic using synthetic `tempfile` trees only. Production paths are never exercised by tests (hardcoded Gdrive locations live only in `main`).

## Example output

```
🚀 warm-drive-cache starting - VFS cache warmer for rclone mounts

📂 Warming path: /home/charlie/Documents/Gdrive/AccessIT
   ⏳ Path exists — waiting 3s for mount to settle (max 30s total)...
   ✓ Directory has content, starting walk.
   Walking…
   ⠹  dirs    142  files   1083  errs    2    12s  …/AccessIT/projects/foo
   ✓ Finished /home/charlie/Documents/Gdrive/AccessIT — 142 dirs, 1083 files

✅ Cache warming complete!
   Directories touched: 142
   Files touched:       1083
   Errors encountered:  2
   (Most errors are transient on cloud mounts - normal)
```

## Timing constants (source)

| Constant | Value | Purpose |
|----------|-------|---------|
| `INITIAL_WAIT_SECS` | 3 | Pause after path exists, before content check |
| `RETRY_DELAYS_SECS` | 3, 5, 8 | Back-off when directory listing is still empty |
| `MAX_WAIT_SECS` | 30 | Maximum wait per path |
| `STATUS_REFRESH` | 80 ms | Live status line refresh interval |

## Deployment tip

Run periodically via a **systemd timer** after rclone mounts come up at login or boot — keeps the VFS cache warm without manual runs.

## Licence

TBD.