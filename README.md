# warm-drive-cache

> **First draft** — this is an early proof-of-concept. Paths are hardcoded in `src/main.rs` today.
> **Planned next:** external configuration via `settings.json` for Google Drive / rclone mount paths (no more editing source to add paths).

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

## Current configuration (hardcoded)

Paths are defined in `src/main.rs`:

```rust
let paths = [
    "/home/charlie/Documents/Gdrive/AccessIT",
    "/home/charlie/Documents/Gdrive/xSAR",
];
```

Edit this array and rebuild until `settings.json` support lands.

### Planned `settings.json` (not implemented yet)

```json
{
  "paths": [
    "/home/charlie/Documents/Gdrive/AccessIT",
    "/home/charlie/Documents/Gdrive/xSAR"
  ],
  "mount_wait": {
    "initial_secs": 3,
    "retry_delays_secs": [3, 5, 8],
    "max_wait_secs": 30
  }
}
```

Exact schema and load path (e.g. `~/.config/warm-drive-cache/settings.json` vs project-local) are TBD.

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