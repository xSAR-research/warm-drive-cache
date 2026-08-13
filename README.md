# warm-drive-cache

> External configuration via `warm-drive-cache.json` (paths, walk depth/size gates/threads, ignore names).
> See the "Configuration via `warm-drive-cache.json`" section below.


> **Checksum verification is enabled by default.** Every selected content file is streamed completely, then its local rclone VFS cache file is awaited and BLAKE3-verified. Set `walk.checksum` or `--checksum FALSE` to skip digest comparison; full reads still occur.

Rust utility for maintenance of rclone FUSE mount cache directories. Part of the [xSAR](https://xSAR.com.au) toolkit.

**Critical distinction (safety):**
- "sync" (exposed) directories: the paths rclone mounts (e.g. /home/user/mounts/myproject). The tool ONLY traverses these and warms the VFS: **File contents read** when file size is inside the configured window, otherwise **attributes/metadata only**. **NEVER delete from sync directories** — they contain your live data.
- "cache" directory: the separate directory given to rclone via `--cache-dir` (e.g. ~/.rclone_cache). The tool calculates on-disk size (before/after) and performs complete deletion of contents here only, to clear stale cached data.

The cache dir is often shared across multiple sync dirs. See your rclone systemd units for the exact `--cache-dir` value.

## Configuration
The tool loads a `warm-drive-cache.json` file containing an array of path pairs. Each pair has:
- `sync`: the rclone-exposed directory to traverse and warm (size-gated **File contents read** or metadata only; concurrent workers).
- `cache`: the rclone cache directory for size checks and deletion.
- `service` (optional): systemd unit that mounts `sync` (system or user; name must match the real unit).

With **`-v` / `--verbose`**, the full **Configuration** block and **Pre-flight checks** detail are printed. Without verbose, those sections are suppressed (failures, start prompts, and the warm path summary still appear).

### Size display format

All human-readable sizes use one shared formatter (IEC binary units):

| Range | Example |
|-------|---------|
| `< 1024` | `482 Bytes` |
| `< 1 MiB` | `64KiB (65536 Bytes)` |
| `< 1 GiB` | `2.60MiB (2724922 Bytes)` |
| larger | `GiB` / `TiB` / `PiB` the same way |

Whole multiples omit decimals (`1MiB (1048576 Bytes)`); other values use two decimal places. Config fields such as `walk.max_file_size_bytes` use the same formatter when shown on screen (special values `-1` / `0` are described in words — see `walk` table below).

### Size *input* in `warm-drive-cache.json`

`min_file_size_bytes` and `max_file_size_bytes` accept either:

| Form | Examples | Meaning |
|------|----------|---------|
| JSON number (whole bytes) | `65536`, `0`, `-1` | Exact byte count (`-1` only on max) |
| JSON string, no unit | `"65536"`, `"-1"` | Same as number |
| JSON string with unit | `"64KiB"`, `"64K"`, `"64kb"`, `"1MiB"`, `"1M"`, `"512B"` | Coefficient × unit |

**Units (case-insensitive, binary powers of 1024):**

| Suffix | Multiplier |
|--------|------------|
| *(none)* / `B` / `b` | 1 (bytes; `B` and `b` are treated the same) |
| `K` / `KB` / `KiB` | 1024 (`K` alone is allowed — omit the `B`) |
| `M` / `MB` / `MiB` | 1024² |
| `G` / `GB` / `GiB` | 1024³ |
| `T` / `TB` / `TiB` | 1024⁴ |
| `P` / `PB` / `PiB` | 1024⁵ |

Fractional values are **not allowed** — not a bare float (`12.5`) and not a unit string (`"1.5KiB"`). Use a whole coefficient (`1536`, `"2KiB"`). Optional spaces: `"64 KiB"`.

## Reporting
The application reports the on-disk size of the **cache** directory before and after refresh using the formatter above. End-of-pair counters include **File contents read** and **Metadata-only** (not “1-byte reads”). The live status block shows a summary (directories / files / threads / errors / elapsed / local target) plus a boxed per-worker table (`Count`, compact size, `READ`/`ATTRIB`/`idle`, a ten-cell read progress bar, path shortened by stripping the sync root / `$HOME` then truncated to 40 characters at the default width of 80). Extra columns from `walk.width` / `-w` / `--width` (hard-capped to 80–200; values below 80 become 80) lengthen only the Source filename field. Each progress cell is 10% of the mount-file stream (empty `□`, filled `■`); idle and metadata-only rows stay empty. The whole live block, including the box, is erased when the pair finishes. Graceful stop: **Ctrl+C** or **q** (TTY) finishes in-flight workers and does not start new files.

## Cache Maintenance
Before removing cache content, the tool scans JSON entries below `<cache-dir>/vfsMeta/<remote>/`, where `<remote>` is the rclone identifier from the unit `ExecStart` (`accessit:` in `rclone mount accessit: /mount`). Combined with `--cache-dir`, content lives in `<cache-dir>/vfs/<remote>/` and metadata in `<cache-dir>/vfsMeta/<remote>/`. A native or string `Dirty` value of `true` (case-insensitive) means rclone has not finished saving the modified content to its source. The tool does not purge those trees while any such entry remains. It checks the local metadata again every 1,000 ms and prints the metadata filename plus an elapsed-seconds counter on every check. Other remotes that share the same `--cache-dir` are left untouched.

Each observation opens the metadata file afresh with read-only access, reads one snapshot, and closes the handle before sleeping. The application does not apply an advisory or exclusive file lock and does not retain an open descriptor between checks, so it cannot block rclone from truncating, rewriting, or atomically replacing the file. Normal local-filesystem cache coherence makes rclone's completed write visible to the next open and directory scan. The wait is also bounded by the deadline below, so an entry cannot produce a perpetual loop.

Each dirty entry may wait for one second per 4 KiB of its recorded `Size`, with a minimum of one second. `mount_wait.max_wait_secs` caps that calculated period when it is non-zero. If the entry remains dirty at the deadline, the run exits with an explanation and leaves the cache intact. Every service directory and every metadata entry is checked before deletion begins.

Only after the dirty check succeeds does the tool delete files and subdirectories **in the cache directory only** (never the sync/exposed directories) to prevent stale data accumulation. The active `warm-drive-cache.lock` is always excluded. Deletion is **non-interactive** (no confirmation prompt). Use `--dry-run` to preview what would be deleted.

## Concurrency Protection

For each distinct cache directory, the application atomically creates an empty `warm-drive-cache.lock` file before normal processing. If the file already exists, another instance may be running or an earlier run may have ended prematurely. The prompt `Another instance of the application has been detected, do you wish to continue [y/N]?` defaults to No; only `y` or `Y` continues. Locks are removed as the final filesystem operation during normal completion, validation failure after acquisition, cancellation, and handled Ctrl+C/SIGINT shutdown. No application can remove a lock after an uncatchable SIGKILL or sudden power loss; the next run therefore treats that file as potentially stale and presents the same safe-default prompt.

Do not modify, add, move, rename, or delete files in any configured rclone-mounted directory until warm-drive-cache has completed. The discovery totals, source metadata, streamed content, cache destination, and checksum comparisons are snapshots taken at different stages of the run. Concurrent user or application changes can invalidate those relationships and produce source-changed, size-mismatch, checksum-mismatch, or other unexpected outcomes. A prominent warning is printed immediately above the live thread display and as the final standalone line of help and JSON-validation output.

## Documentation & Secrets Policy
An example warm-drive-cache.json is provided. The README.md, all source comments, and the example file contain no concrete local paths, usernames, or sensitive values. All references use generic placeholders (e.g. rclone://example-remote/example/path or /path/to/cache). Paths are classified as secrets.

## CLI options

| Option | Description |
|--------|-------------|
| `-?`, `-h`, `--help` | Brief usage, where `warm-drive-cache.json` must live, embedded `warm-drive-cache-example.json`, `max_file_size_bytes` specials, link to README. Help takes precedence over every other option and performs no file checks or modifications. |
| `-j`, `--json` | Validate config layout; for each entry print **service name**, **sync directory**, **`--cache-dir` from the unit** (preferred), **current cache size**, and **systemd active/inactive (system or user)**. Second-highest priority. Accepts the same `-t` / `-s` / `-c` / `-w` overrides as a normal run; `-v` / `-l` / `--dry-run` are rejected. |
| `-i`, `--information` | Product information only: `Codebase Version`, `Codebase release` date, AGPL-3.0-only, repo + https://xSAR.com.au (exits) |
| `-t VALUE`, `--threads VALUE` | Override JSON worker count; validated in `1..=64`. Applies to a normal run and to `--json`. |
| `-s VALUE`, `--size VALUE` | Override JSON maximum using the shared size parser (`-1`, `0`, or a positive whole size/unit). Applies to a normal run and to `--json`. |
| `-c VALUE`, `--checksum VALUE` | Override checksum in either direction: `TRUE`/`YES`/`Y` or `FALSE`/`NO`/`N` (case-insensitive). Applies to a normal run and to `--json`. |
| `-w VALUE`, `--width VALUE` | Override the threads-display width. Defaults to **80** characters if omitted. Values below 80 become 80; values above 200 become 200. Extra columns above 80 lengthen only the Source filename field. Applies to a normal run and to `--json`. |
| `-v`, `--verbose` | On a normal run: print **Configuration** and full **Pre-flight checks** detail. Quiet is the default. |
| `-l`, `--log` | Write a time-stamped CSV under `/tmp/warm-drive-cache-YYYYMMDD-HHMMSS.csv` (with a process-specific suffix if that name already exists) with columns **Service name**, **path**, **filename**, **size (bytes)**, **status** (`READ` or `ATTRIB`). The path is printed again after a blank line at program end. |
| `--dry-run` | Simulate cache deletion only (no warm). Concurrency locks are still created and removed; cache content is unchanged. May be combined with `-v` / `-l`. |

**Precedence when several flags are present:** `-?` / `-h` / `--help` → `-j` / `--json` → `-i` / `--information` → normal run. Help ignores every other argument and exits before configuration loading, path checks, lock creation, or cache modification. JSON validation loads and checks the configured JSON and paths, applies `-t` / `-s` / `-c` / `-w` if present, and performs no maintenance. Duplicate flags are rejected.

Normal runs always print the startup identity banner (product of xSAR, licence, website, source). Use `-i` for the short product-information dump without loading config.

### Typical invocations

```bash
# Product information (no config load)
warm-drive-cache -i

# Validate layout + service/cache report
warm-drive-cache -j

# Quiet maintenance run (default)
warm-drive-cache

# Verbose: Configuration + Pre-flight detail
warm-drive-cache -v

# Preview deletions only
warm-drive-cache --dry-run
warm-drive-cache -v --dry-run
```

## Program Flow

```mermaid
flowchart TD
    Start[Start] --> Cli{"CLI flags?"}

    Cli -->|-? / -h / --help| Help["Print help only; no config,\npath, lock, or cache checks"]
    Cli -->|-j / --json| CheckBanner["Startup banner"]
    Cli -->|-i / --information| Info["Codebase Version +\nCodebase release +\nAGPL + repo + website"]
    CheckBanner --> CheckLoad["Load + validate warm-drive-cache.json"]
    CheckLoad --> CheckReport["Per entry:\nservice name\nsync directory\n--cache-dir from unit\ncache size IEC format\nsystemd active/inactive"]
    CheckReport --> CleanupCheck["cleanup summary"]

    Cli -->|run / -v / --dry-run| Banner["Startup banner\nCodebase Version/release\nAGPL + xSAR + website"]
    Banner --> LoadConfig["Load config\nrun-dir → env → XDG"]
    LoadConfig --> Validate{"Valid config?\npaths non-empty?\nmax_file_size OK?"}
    Validate -->|No| Error["configuration error\nexit 1"]
    Validate -->|Yes| Locks["Install SIGINT handler; atomically create\nwarm-drive-cache.lock in each cache"]
    Locks --> Existing{"Lock already exists?"}
    Existing -->|Yes| LockPrompt["Prompt to continue [y/N]"]
    LockPrompt -->|not y/Y| LockExit["Remove acquired locks; exit 1"]
    LockPrompt -->|y/Y| Verbose
    Existing -->|No| Verbose{"-v / --verbose?"}
    Verbose -->|Yes| PrintCfg["Print Configuration\npaths + walk sizes +\nservices + mount_wait"]
    Verbose -->|No| ForEach["For each path pair"]
    PrintCfg --> ForEach

    ForEach --> StopCheck{"Shutdown\nrequested?"}
    StopCheck -->|Yes| EndStop["Skip remaining pairs"]
    StopCheck -->|No| HasSvc{"paths[].service\nset?"}

    HasSvc -->|No| Settle["mount_wait settle\non sync path"]
    HasSvc -->|Yes| Scope["Detect systemd scope\nsystem vs --user"]
    Scope --> Active{"Unit\nactive?"}
    Active -->|Yes| Settle
    Active -->|No| DrySvc{"--dry-run?"}
    DrySvc -->|Yes| SkipSvc["Skip pair\nwould have prompted start"]
    DrySvc -->|No| Prompt["Prompt: start unit?\nY/n default yes"]
    Prompt -->|No| SkipSvc
    Prompt -->|Yes| Reload["daemon-reload"]
    Reload --> Enable["enable unit"]
    Enable --> StartU["start unit\nsudo retry if system\npermission denied"]
    StartU --> Verify{"enabled AND\nactive?"}
    Verify -->|No| FailSvc["Error / skip pair"]
    Verify -->|Yes| Settle

    Settle --> Probes["FS probes:\nsync readable\ncache write probe\nunit file readable"]
    Probes -->|fail| Notice["Notice: path not usable\nwarmer skipped"]
    Probes -->|ok| SizeBefore["Report cache size before\nIEC format_bytes"]

    SkipSvc --> Notice
    FailSvc --> Notice
    Notice --> ForEach

    SizeBefore --> Dry{"--dry-run?"}
    Dry -->|Yes| SimDelete["Simulate delete only"]
    SimDelete --> ForEach
    Dry -->|No| QuitHandlers["Install SIGINT / q\nraw TTY quit"]
    QuitHandlers --> DirtyScan["Scan every JSON file below\nvfsMeta/service directories"]
    DirtyScan --> HasDirty{"Any Dirty=true?"}
    HasDirty -->|Yes| DirtyWait["Print file + elapsed seconds;\nsleep 1000 ms; scan again"]
    DirtyWait --> DirtyLimit{"1 second per 4 KiB deadline\n(configured maximum cap) reached?"}
    DirtyLimit -->|No| DirtyScan
    DirtyLimit -->|Yes| DirtyFail["Leave cache intact; exit 1"]
    HasDirty -->|No| DeleteCache["Delete cache contents except active lock\nnon-interactive"]
    DeleteCache --> WarmTree["warm_tree: WalkDir sync\nmax_depth + ignore.names"]
    WarmTree --> Workers["Worker pool max_threads\nmax=-1 metadata only\nmax=0 all File contents read\nmax=N size window"]
    Workers --> Status["Live status block\nN of M · size · READ/ATTR · path"]
    Status --> Drain["Drain in-flight on cancel"]
    Drain --> Summary["Per-path summary\nFile contents read\nMetadata-only"]
    Summary --> ForEach

    ForEach -->|done| Cleanup["cleanup: thanks +\nGitHub issues link"]
    EndStop --> Cleanup
    DirtyFail --> Cleanup
    LockExit --> End
    Cleanup --> RemoveLocks["Remove concurrency locks as final\nfilesystem operation"]
    RemoveLocks --> End[End]
    Help --> End
    Info --> End
    CleanupCheck --> End

    classDef error fill:#f99,stroke:#333
    class Error,Notice,SkipSvc,FailSvc error
```

**Flow (narrative):** optional CLI exit (help first, JSON validation second, information third) → otherwise banner → load and validate config (including `max_file_size_bytes` specials) → acquire one concurrency lock per cache → optional **verbose Configuration** dump → for each path pair: **systemd** (detect scope; if inactive and user agrees: `daemon-reload` → `enable` → `start`, with **sudo** retry for system units; require **enabled + active**) **before** **mount settle**, then permission probes → report cache size → scan every service directory below **`vfsMeta`** and wait for all `Dirty` entries to clear → wipe **cache** while preserving the active lock (unless `--dry-run`) → print the mounted-file modification warning immediately above the live thread display → parallel warm of **sync** (**File contents read** vs metadata per size policy) → summary → remove concurrency locks as the final filesystem operation. Ctrl+C / `q` finishes in-flight work only and removes locks during graceful shutdown.

## Configuration via `warm-drive-cache.json`

Paths, ignore names, mount-wait timings, and walk policy come from a JSON file; there are no hardcoded paths in the source. After the file is loaded, `-t` / `--threads`, `-s` / `--size`, `-c` / `--checksum`, and `-w` / `--width` override `walk.max_threads`, `walk.max_file_size_bytes`, `walk.checksum`, and `walk.width`.

### Location (in priority order)

1. **`warm-drive-cache.json` next to the executable** (same directory as the binary). Preferred for desktop/systemd wrappers. **This file is gitignored** when it holds real paths.
2. `WARM_DRIVE_CACHE_CONFIG` environment variable → full path to a `.json` file (CI / alternate profiles).
3. XDG: `$XDG_CONFIG_HOME/warm-drive-cache/warm-drive-cache.json` or `~/.config/warm-drive-cache/warm-drive-cache.json`.

`WARM_DRIVE_CACHE_CONFIG` is an explicit arbitrary-path override in the discovery order above; it is not a command-line option.

**Tracked vs local**

| File | Git | Purpose |
|------|-----|---------|
| `warm-drive-cache-example.json` | tracked | Public template with **placeholders only** (no real users/paths) |
| `warm-drive-cache.json` | **untracked** | Your live machine paths + real systemd unit names |
| `live.json` | **untracked** | Optional local alternate profile |

Copy: `cp warm-drive-cache-example.json warm-drive-cache.json` (or next to `target/release/` after build) and edit.

### Missing config behaviour

If no file is found, the loader returns empty `paths` and the program exits with instructions to create the file. The tool is useless without at least one path pair.

### Layout overview

```text
{
  "version": 1,                 // optional schema version
  "paths": [ ... ],             // required: one object per mount/cache pair
  "walk": { ... },              // optional walk / warm controls
  "ignore": { "names": [...] }, // optional basename skip list
  "mount_wait": { ... }         // optional FUSE settle timings
}
```

### `paths[]` entries (required array)

Each element describes **one** mount to warm and the cache directory that may be cleared.

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `sync` | string | **yes** | Absolute path to the **rclone mount** (live data). Traversed and warmed only — **never deleted**. |
| `cache` | string | **yes** | Absolute path to the rclone **`--cache-dir`** used by that mount (or a shared cache). Used for size reports and non-interactive content deletion. |
| `service` | string | no | systemd unit name (system or user), e.g. `gdrive-project-a.service`. Use the **real** unit name (do not invent an `rclone-` prefix). Pre-flight auto-detects scope. |

**Safety rules encoded in the loader**

- `sync` and `cache` must be **absolute** (start with `/`).
- `sync` and `cache` must **not** nest or equal each other (prevents deleting live data).
- `service`, if present, must not contain `/` or control characters.
- At least one path pair is required.
- `walk.max_file_size_bytes` must resolve to **`-1`**, **`0`**, or a **positive** byte count (see special values and size input above).
- Size fields may be whole numbers or whole unit strings; unknown units and any fractional value (`12.5`, `"1.5KiB"`) are configuration errors.

### `walk` (optional)

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `checksum` | boolean or supported string | `true` | **Enabled by default.** BLAKE3-verify the completely streamed mount file against stable local VFS cache content. |
| `max_depth` | integer or `null` | `null` | `WalkDir` max depth; `null` = unlimited. `0` is rejected. |
| `min_file_size_bytes` | number or size string | `0` | Min size for File contents read when `max_file_size_bytes > 0`; `0` = no lower bound. Accepts bytes or unit strings (see **Size input**). Displayed with the shared IEC formatter. |
| `max_file_size_bytes` | number or size string | `0` | **File contents read** policy (see special values). Accepts bytes or unit strings. |
| `max_threads` | integer | `8` | Concurrent warm workers (`1`–`64`). |
| `width` | integer | `80` | Nominal full width of the threads display block, in characters. Defaults to **80** if omitted. Values below 80 become 80; values above 200 become 200. Extra columns above 80 lengthen only the Source filename field. |

`walk.checksum` accepts native JSON `true` and `false`, or a quoted, case-insensitive string: `"TRUE"`, `"YES"`, `"Y"`, `"FALSE"`, `"NO"`, or `"N"`. The command-line `-c` / `--checksum` option requires one of those text values so either direction can be selected explicitly.

#### `max_file_size_bytes` special values

| Value | Meaning |
|-------|---------|
| **`-1`** | **No** File contents read — metadata/attributes only for every file. |
| **`0`** | File contents read for **all** files, any size (ignores `min_file_size_bytes`). |
| **`N > 0`** | File contents read when file size is in the window `[min_file_size_bytes, N]` (`min` of `0` = no lower bound). Outside window → metadata only. |
| Other negatives | **Configuration error** — program prints a warning/explanation and exits. |
| Fractional (e.g. `12.5`, `"1.5KiB"`) | **Configuration error** — whole byte counts only; program exits. |

When the limit is a positive `N`, it is displayed like cache sizes, e.g. `64KiB (65536 Bytes)`.

### `ignore` (optional)

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `names` | string[] | `[]` | Exact basenames to prune (e.g. `.git`, `node_modules`). Matching directories skip their whole subtree. Config roots are never ignored. |

### `mount_wait` (optional)

Runs **after** systemd enable/active verification (when a service is configured) and **before** permission probes and cache wipe. Quiet mode still waits; verbose mode prints settle progress.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `initial_secs` | integer | `3` | Wait after path exists before content probe. |
| `retry_delays_secs` | integer[] | `[3, 5, 8]` | Delays while the mount listing looks empty. Must not be `[]`; omit the field to use the default. |
| `max_wait_secs` | integer | `30` | Hard ceiling per path. |

### Pre-flight and systemd (normal run)

For each path pair, in order:

1. If `service` is set: detect **system** vs **user** unit (`LoadState`).
2. If inactive and not `--dry-run`: prompt to start; on yes → `daemon-reload` → `enable` → `start`.
3. Confirm unit is **enabled** and **active** before continuing (settle wait comes next).
4. **System** units: mutating `systemctl` calls retry with **`sudo`** if the unprivileged call fails.
5. `mount_wait` settle on `sync`.
6. Permission probes: sync readable, cache write/read/delete probe, unit file readable (when configured).

Failures skip that pair; other pairs continue.

### Full example (placeholders only)

```json
{
  "version": 1,
  "paths": [
    {
      "sync": "/home/user/mounts/project-a",
      "cache": "/home/user/.rclone_cache",
      "service": "gdrive-project-a.service"
    },
    {
      "sync": "/home/user/mounts/project-b",
      "cache": "/home/user/.rclone_cache",
      "service": "gdrive-project-b.service"
    }
  ],
  "walk": {
    "checksum": true,
    "max_depth": null,
    "min_file_size_bytes": 0,
    "max_file_size_bytes": "64KiB",
    "max_threads": 8,
    "width": 80
  },
  "ignore": {
    "names": [".git", ".svn", "node_modules", ".cache", "target", "__pycache__"]
  },
  "mount_wait": {
    "initial_secs": 3,
    "retry_delays_secs": [3, 5, 8],
    "max_wait_secs": 30
  }
}
```

In this example `"max_file_size_bytes": "64KiB"` (also valid: `65536`, `"64K"`, `"64KB"`) means File contents read for files up to **64KiB (65536 Bytes)**; larger files get metadata only. Use `0` for all sizes, or `-1` for metadata-only.

### `-j` / `--json` report fields

For each `paths[]` entry the check mode prints a spaced group:

1. **Service name** — `paths[].service` (or note if omitted)
2. **File directory** — `paths[].sync` (mount target) + exists/dir status
3. **Cache path** — prefers **`--cache-dir` parsed from the unit** (`systemctl cat` or `systemctl --user cat`, auto-detected); falls back to `paths[].cache` with a note if the flag is missing or differs
4. **Current cache size** — recursive on-disk size in IEC form (`NKiB (… Bytes)`, etc.)
5. **systemd** — active / inactive when a unit name is set (**system** or **user** scope auto-detected)

### Important rules (summary)

- Prefer absolute paths only; relative paths are rejected.
- Never delete under `sync`; only warm.
- Delete only under `cache` (non-interactive; use `--dry-run` to preview).
- Keep `warm-drive-cache-example.json` free of real usernames and host paths when publishing.
- Omitted sections or fields fall back to the values shown in the table above.
- When the file is completely missing, the program still uses the defaults for `walk`, `ignore`, and `mount_wait`, but `paths` becomes empty and triggers a helpful startup error.

**Safety**: The tool will refuse overlapping sync/cache paths. Always double-check your rclone service `--cache-dir` vs mount points.

See also the ready-to-copy example at `warm-drive-cache-example.json` in the repository root.

### Creating the file
The build process copies `warm-drive-cache-example.json` into the release directory (e.g. `target/release/warm-drive-cache-example.json`).

```bash
# After `cargo build --release`
cp target/release/warm-drive-cache-example.json target/release/warm-drive-cache.json
# edit target/release/warm-drive-cache.json with your {"sync": "...", "cache": "..."} pairs
```

Alternatively for XDG:
```bash
mkdir -p ~/.config/warm-drive-cache
cp warm-drive-cache-example.json ~/.config/warm-drive-cache/warm-drive-cache.json
```

## Requirements

- Rust stable (2024 edition)
- rclone remote(s) configured (sync/cache path pairs provided via warm-drive-cache.json; see your rclone --cache-dir)
- Linux (uses standard `std::fs`; developed on Arch)

## Build & run

```bash
cargo build --release
./target/release/warm-drive-cache -i          # product information
./target/release/warm-drive-cache -j          # config / service check
./target/release/warm-drive-cache             # quiet maintenance run
./target/release/warm-drive-cache -v          # verbose Configuration + Pre-flight
./target/release/warm-drive-cache --dry-run   # simulate cache wipe only
```

Optional install to a directory on your `PATH` (e.g. `~/.local/bin`):

```bash
cp target/release/warm-drive-cache ~/.local/bin/
# place warm-drive-cache.json next to the binary, or use WARM_DRIVE_CACHE_CONFIG / XDG
```

Debug build:

```bash
cargo run -- -c
cargo run -- -v --dry-run
```

## Testing

```bash
cargo test
cargo test -- --quiet
cargo test format_bytes          # size formatter
cargo test should_read_file      # File contents read policy
cargo test parse_cli             # CLI flags
```

Unit tests cover the pure helpers and core logic using synthetic `tempfile` trees only. Production paths are never exercised by tests (paths are loaded from warm-drive-cache.json and treated as secrets).

## Example output

Quiet dry-run sketch (placeholders only):

```
Rust utility for removing rclone cache staleness and warming mounts.
Quit gracefully: Ctrl+C (SIGINT) or press q (TTY) — finishes in-flight workers, starts no new work.

┌─────────────────────────────────────────────────────────────────┐
│  warm-drive-cache                                               │
│  Codebase Version: 0.2.0                                        │
│  Codebase release: 13th August, 2026                            │
│  Website: https://xSAR.com.au                                   │
│  Licence: AGPL-3.0-only (see LICENSE file)                      │
│  Homepage: https://xSAR.com.au                                  │
│  Source:  https://github.com/xSAR-research/warm-drive-cache     │
└─────────────────────────────────────────────────────────────────┘

📂 Sync dir (traverse/warm only): /home/user/mounts/project-a
   Cache dir (size/delete only): /home/user/.rclone_cache
   systemd unit: gdrive-project-a.service
   Before size (cache): 12.34MiB (12939427 Bytes)
   --dry-run enabled: simulating full deletion (no changes made)
   After size (simulated, cache): 0 Bytes

✅ Cache maintenance complete!
```

After a live warm, counters look like:

```
   Size after warming (cache): 2.60MiB (2724922 Bytes)
   Directories processed: …
   Files processed: …
   File contents read: …
   Metadata-only: …
   Errors: 0
```

## Deployment tip

Run periodically via a **systemd timer** after rclone mounts come up at login or boot to keep caches fresh (use --dry-run first).

## Sample systemd Unit (user or system)

This tool is designed to work with rclone VFS mounts managed by systemd.

Set `paths[].service` to the **exact** unit name (for example `gdrive-myproject.service` — do **not** invent an `rclone-` prefix unless that is how the unit is actually installed). At runtime the tool auto-detects:

| Scope | Typical location | Commands used |
|-------|------------------|---------------|
| **system** | `/etc/systemd/system/` | `systemctl …` (retries with `sudo` on permission errors) |
| **user** | `~/.config/systemd/user/` | `systemctl --user …` |

If the user agrees to start a stopped unit, the program runs **`daemon-reload` → `enable` → `start`**, then requires **enabled** and **active** **before** `mount_wait` settle.

The following is a **sample** of a systemd **user unit** (with personal secrets stripped). System units under `/etc/systemd/system/` with `User=` also work — put the real unit name in config.

### Important notes
- The mount point directory **must** be created in advance and must be writable/accessible by your user:
  ```bash
  mkdir -p ~/mounts/myproject
  ```
- Your rclone remote (here called `myremote`) must already be configured.
- Match `paths[].cache` to the unit’s `--cache-dir`.

### Where to install the unit file
User units belong in your user's systemd configuration directory:

```
~/.config/systemd/user/gdrive-myproject.service
```

System units (example path):

```
/etc/systemd/system/gdrive-myproject.service
```

### Commands to enable and start the service

**User unit:**

```bash
# 1. Place the unit file in ~/.config/systemd/user/
# 2. Reload, enable, and start
systemctl --user daemon-reload
systemctl --user enable --now gdrive-myproject.service

systemctl --user status gdrive-myproject.service
journalctl --user -u gdrive-myproject.service -f
```

**System unit** (often needs root / sudo):

```bash
sudo systemctl daemon-reload
sudo systemctl enable --now gdrive-myproject.service
systemctl status gdrive-myproject.service
journalctl -u gdrive-myproject.service -f
```

### Sample unit file (`gdrive-myproject.service`)

```ini
[Unit]
Description=rclone VFS mount for MyProject
After=network-online.target
Wants=network-online.target

[Service]
Type=notify
ExecStartPre=/bin/mkdir -p %h/mounts/myproject
ExecStart=/usr/bin/rclone mount myremote: %h/mounts/myproject \
    --config=%h/.config/rclone/rclone.conf \
    --vfs-cache-mode full \
    --cache-dir=%h/.rclone_cache \
    --dir-cache-time 5m \
    --poll-interval 1m \
    --vfs-read-chunk-size 64M \
    --log-level INFO

ExecStop=/bin/fusermount -u %h/mounts/myproject

Restart=on-failure
RestartSec=10

[Install]
WantedBy=default.target
```

**Notes on the sample:**
- Uses systemd specifiers like `%h` (expands to the user's home directory) so the unit is easy to reuse.
- The `--cache-dir` points to the rclone cache directory used by the mount (see your `warm-drive-cache.json` for the exact value used by `warm-drive-cache`).
- A similar unit can be created for another remote (e.g. `gdrive-archive.service` pointing at the `archive` remote and its mount/cache paths).
- You can create a systemd user timer (or use `warm-drive-cache` directly via a timer) that starts after these mounts are active.

## More from xSAR

For more tools, guides, and projects, visit [xSAR](https://xSAR.com.au).

## Licence

This project is licensed under the [GNU Affero General Public License v3.0 only](LICENSE) (AGPL-3.0-only). See the `LICENSE` file for the full text.
## rclone VFS cache layout and verification assumptions

The resolver expects rclone's content layout `<cache-dir>/vfs/<remote-name>/<remote-path>`, not files directly below `--cache-dir`. On a shared `--cache-dir` the remote name is taken from the pair's systemd unit (`rclone mount remote:…`) or from `/proc/self/mounts` for the sync path. Mount-relative paths are mapped beneath that remote directory; existing parents are canonicalized and traversal outside the configured cache root is rejected. Filenames remain native OS strings (no lossy conversion). Live-table warnings are held until the boxed status is erased so they cannot leave the frame on screen.

Selected content files are streamed fully and incrementally through BLAKE3. The worker retains its slot while polling only the local cache destination (500 ms, finite 30 second timeout, two stable observations), which avoids repeated remote API requests and provides thread-count backpressure. Attribute-only files are never opened and verification is reported as not applicable. Disabling checksums still performs the full mount read and cache stability/size checks.

Before warming, eligible content bytes should be aggregated for cache paths on the same filesystem. Projected use above 90% is a warning, not a refusal; metadata-only files contribute zero bytes. Dry runs cannot claim post-cleanup free space because deletion was simulated. Sparse files, hard links, duplicate mappings, inaccessible/changing metadata, and saturating overflow must be reported conservatively.

`-j` / `--json` validates every pair rather than accepting the first usable pair. It checks absolute/non-overlapping paths, directory existence and read/traverse access, and cache modification access with a uniquely named create/remove probe. Service syntax/discoverability is reported separately without changing systemd state.
