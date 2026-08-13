use std::{
    fs,
    sync::{Arc, atomic::AtomicBool},
    thread,
    time::Duration,
};
use tempfile::tempdir;
use warm_drive_cache::{
    cache_lock,
    config::{self, Config},
    dirty_check, resolver,
    startup::{CliAction, parse_cli_args},
    verifier::{self, VerifyOutcome, WaitOptions},
    worker::should_read_file_contents,
};
#[test]
fn sample_schema_and_checksum_default() {
    let c: Config = serde_json::from_str(include_str!("../warm-drive-cache-example.json")).unwrap();
    assert!(c.walk.checksum);
    assert_eq!(c.walk.width, 80);
    let c: Config = serde_json::from_str(r#"{"paths":[{"sync":"/a","cache":"/b"}]}"#).unwrap();
    assert!(c.walk.checksum);
    assert_eq!(c.walk.width, 80);
}
#[test]
fn booleans_all_cases() {
    for s in ["TRUE", "True", "tRuE", "YES", "y", "Y"] {
        assert_eq!(config::parse_bool(s), Ok(true))
    }
    for s in ["FALSE", "False", "fAlSe", "NO", "n", "N"] {
        assert_eq!(config::parse_bool(s), Ok(false))
    }
}
#[test]
fn cli_validation_and_precedence() {
    assert!(matches!(
        parse_cli_args(["--json"]).unwrap(),
        CliAction::JsonValidation { .. }
    ));
    assert!(parse_cli_args(["--wat"]).is_err());
    assert!(parse_cli_args(["-t"]).is_err());
    assert!(parse_cli_args(["-t", "0"]).is_err());
    assert!(parse_cli_args(["-c", "maybe"]).is_err());
    assert_eq!(
        parse_cli_args(["--wat", "--help"]).unwrap(),
        CliAction::Help
    );
    assert_eq!(parse_cli_args(["--wat", "-?"]).unwrap(), CliAction::Help);
    assert!(parse_cli_args(["--json", "--wat"]).is_err());
    assert!(parse_cli_args(["--json", "-v"]).is_err());
    assert_eq!(
        parse_cli_args(["--json", "-t", "4", "-c", "NO"]).unwrap(),
        CliAction::JsonValidation {
            overrides: warm_drive_cache::startup::CliOverrides {
                threads: Some(4),
                size: None,
                checksum: Some(false),
                width: None,
            }
        }
    );
    assert!(parse_cli_args(["-v", "-v"]).is_err());
    assert!(parse_cli_args(["--dry-run", "--dry-run"]).is_err());
    assert_eq!(
        parse_cli_args(["--information", "-j"]).unwrap(),
        CliAction::JsonValidation {
            overrides: Default::default()
        }
    );
    assert_eq!(
        parse_cli_args(["--json", "--help"]).unwrap(),
        CliAction::Help
    );
}

#[test]
fn dirty_metadata_is_case_insensitive_and_scans_all_services() {
    let t = tempdir().unwrap();
    let cache = t.path();
    let first = cache.join("vfsMeta/service-a/one.json");
    let second = cache.join("vfsMeta/service-b/two.json");
    fs::create_dir_all(first.parent().unwrap()).unwrap();
    fs::create_dir_all(second.parent().unwrap()).unwrap();
    fs::write(&first, r#"{"Dirty":"tRuE","Size":8192}"#).unwrap();
    fs::write(&second, r#"{"dirty":false,"size":4096}"#).unwrap();
    let entries = dirty_check::scan(cache).unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].metadata_path, first);
    assert_eq!(
        dirty_check::scan_scoped(cache, Some("service-a"))
            .unwrap()
            .len(),
        1
    );
    assert!(
        dirty_check::scan_scoped(cache, Some("service-b"))
            .unwrap()
            .is_empty()
    );
    assert_eq!(entries[0].content_size, 8192);
    assert_eq!(dirty_check::calculated_wait_secs(8192, 30), 2);
    assert_eq!(dirty_check::calculated_wait_secs(8192, 1), 1);
}

#[test]
fn dirty_wait_reopens_and_observes_atomic_rclone_update() {
    let t = tempdir().unwrap();
    let cache = t.path().to_path_buf();
    let metadata = cache.join("vfsMeta/service-a/file.json");
    fs::create_dir_all(metadata.parent().unwrap()).unwrap();
    fs::write(&metadata, r#"{"Dirty":true,"Size":409600}"#).unwrap();

    let replacement_path = metadata.clone();
    let updater = thread::spawn(move || {
        thread::sleep(Duration::from_millis(30));
        let replacement = replacement_path.with_extension("replacement");
        fs::write(&replacement, r#"{"Dirty":false,"Size":409600}"#).unwrap();
        fs::rename(replacement, replacement_path).unwrap();
    });

    let cancelled = Arc::new(AtomicBool::new(false));
    dirty_check::wait_until_clean_with_interval(
        &cache,
        30,
        &cancelled,
        Duration::from_millis(5),
        None,
    )
    .expect("a newly opened read-only handle must observe rclone's replacement file");
    updater.join().unwrap();
}

#[test]
fn cache_lock_is_empty_and_removed_on_drop() {
    let t = tempdir().unwrap();
    let lock = cache_lock::CacheLock::acquire(t.path()).unwrap();
    let path = lock.path().to_path_buf();
    assert_eq!(path.file_name().unwrap(), cache_lock::LOCK_NAME);
    assert_eq!(fs::metadata(&path).unwrap().len(), 0);
    drop(lock);
    assert!(!path.exists());
}

#[test]
fn cache_cleanup_preserves_the_active_lock() {
    let t = tempdir().unwrap();
    let lock = cache_lock::CacheLock::acquire(t.path()).unwrap();
    fs::write(t.path().join("cached-content"), b"delete me").unwrap();
    warm_drive_cache::cache_ops::delete_dir_contents(t.path(), false);
    assert!(lock.path().exists());
    assert!(!t.path().join("cached-content").exists());
}

#[test]
fn every_runtime_option_accepts_short_and_long_forms() {
    for args in [
        vec!["-j"],
        vec!["--json"],
        vec!["-t", "4"],
        vec!["--threads", "4"],
        vec!["-s", "1MiB"],
        vec!["--size", "1MiB"],
        vec!["-c", "NO"],
        vec!["--checksum", "NO"],
        vec!["--json", "-t", "4"],
        vec!["--json", "--size", "1MiB"],
        vec!["-w", "100"],
        vec!["--width", "100"],
        vec!["--json", "-w", "90"],
    ] {
        parse_cli_args(args).expect("documented short and long option must parse");
    }
    assert!(parse_cli_args(["-s", "1.5KiB"]).is_err());
    assert!(parse_cli_args(["-w"]).is_err());
    assert!(parse_cli_args(["-w", "abc"]).is_err());
    assert!(parse_cli_args(["-w", "80", "--width", "100"]).is_err());

    let CliAction::Run { overrides, .. } =
        parse_cli_args(["-t", "4", "-s", "1MiB", "-c", "NO"]).unwrap()
    else {
        panic!("runtime overrides should produce a run action");
    };
    assert_eq!(overrides.threads, Some(4));
    assert_eq!(overrides.size, Some(1024 * 1024));
    assert_eq!(overrides.checksum, Some(false));
    assert_eq!(overrides.width, None);

    let CliAction::Run { overrides, .. } = parse_cli_args(["-w", "50"]).unwrap() else {
        panic!("width override should produce a run action");
    };
    assert_eq!(overrides.width, Some(80));

    let CliAction::Run { overrides, .. } = parse_cli_args(["--width", "250"]).unwrap() else {
        panic!("width override should produce a run action");
    };
    assert_eq!(overrides.width, Some(200));

    let CliAction::Run { overrides, .. } = parse_cli_args(["--width", "120"]).unwrap() else {
        panic!("width override should produce a run action");
    };
    assert_eq!(overrides.width, Some(120));
}
#[test]
fn sizes() {
    for (s, n) in [("-1", -1), ("0", 0), ("2mb", 2 * 1024 * 1024)] {
        assert_eq!(config::parse_size_expr(s).unwrap(), n)
    }
    assert!(config::parse_size_expr("1.5KiB").is_err());
    assert!(config::parse_size_expr("12.5").is_err());
    assert!(config::parse_size_expr("-2").is_err());
    assert!(config::parse_size_expr("999999999999999999999999P").is_err());
    assert!(should_read_file_contents(5, 2, 8));
}
#[test]
fn resolver_uses_vfs_remote_layout() {
    let t = tempdir().unwrap();
    let sync = t.path().join("mount");
    let cache = t.path().join("cache");
    fs::create_dir_all(sync.join("a")).unwrap();
    fs::create_dir_all(cache.join("vfs/remote")).unwrap();
    let got = resolver::resolve(&sync, &cache, &sync.join("a/file")).unwrap();
    assert_eq!(got, cache.join("vfs/remote/a/file"));

    fs::create_dir_all(cache.join("vfs/other")).unwrap();
    assert!(resolver::resolve(&sync, &cache, &sync.join("a/file")).is_err());
    let hinted = resolver::resolve_for_remote(
        &sync,
        &cache,
        &sync.join("a/file"),
        Some(std::ffi::OsStr::new("remote")),
    )
    .unwrap();
    assert_eq!(hinted, cache.join("vfs/remote/a/file"));
}

#[test]
fn parse_rclone_remote_name_from_unit_text() {
    assert_eq!(
        resolver::parse_rclone_remote_name(
            "ExecStart=/usr/bin/rclone mount --cache-dir /var/cache/r rem: /mnt"
        )
        .as_deref(),
        Some("rem")
    );
    assert_eq!(
        resolver::parse_rclone_remote_name("rclone mount gdrive:Documents /mnt").as_deref(),
        Some("gdrive")
    );
    let folded = "\
ExecStart=/usr/bin/rclone mount accessit: /home/user/mounts/project \\
 --vfs-cache-mode full \\
 --cache-dir /home/user/.cache/rclone \\";
    assert_eq!(
        resolver::parse_rclone_remote_name(folded).as_deref(),
        Some("accessit")
    );
}

#[test]
fn delete_remote_trees_leaves_other_remotes() {
    let t = tempdir().unwrap();
    let cache = t.path();
    let keep = cache.join("vfs/other/file");
    let drop_c = cache.join("vfs/accessit/file");
    let drop_m = cache.join("vfsMeta/accessit/meta.json");
    fs::create_dir_all(keep.parent().unwrap()).unwrap();
    fs::create_dir_all(drop_c.parent().unwrap()).unwrap();
    fs::create_dir_all(drop_m.parent().unwrap()).unwrap();
    fs::write(&keep, b"keep").unwrap();
    fs::write(&drop_c, b"drop").unwrap();
    fs::write(&drop_m, b"{}").unwrap();
    warm_drive_cache::cache_ops::delete_remote_trees(cache, "accessit", false);
    assert!(keep.exists());
    assert!(!drop_c.exists());
    assert!(!drop_m.exists());
}
#[test]
fn verify_match_mismatch_disabled_and_attributes() {
    let t = tempdir().unwrap();
    let s = t.path().join("s");
    let d = t.path().join("d");
    fs::write(&s, b"abc").unwrap();
    fs::write(&d, b"abc").unwrap();
    let o = WaitOptions {
        interval: Duration::from_millis(1),
        timeout: Duration::from_millis(20),
        stable_observations: 1,
    };
    let c = AtomicBool::new(false);
    assert_eq!(
        verifier::verify(&s, &d, true, true, o, &c, None),
        VerifyOutcome::Verified
    );
    fs::write(&d, b"abd").unwrap();
    assert_eq!(
        verifier::verify(&s, &d, true, true, o, &c, None),
        VerifyOutcome::ChecksumMismatch
    );
    assert_eq!(
        verifier::verify(&s, &d, false, true, o, &c, None),
        VerifyOutcome::ChecksumDisabled
    );
    assert_eq!(
        verifier::verify(&s, &d, true, false, o, &c, None),
        VerifyOutcome::AttributesOnly
    )
}

#[test]
fn verify_empty_stream_does_not_wait_for_missing_cache_file() {
    let t = tempdir().unwrap();
    let s = t.path().join("empty");
    let d = t.path().join("missing-cache");
    fs::write(&s, b"").unwrap();
    let o = WaitOptions {
        interval: Duration::from_millis(50),
        timeout: Duration::from_millis(400),
        stable_observations: 1,
    };
    let c = AtomicBool::new(false);
    let started = std::time::Instant::now();
    assert_eq!(
        verifier::verify(&s, &d, true, true, o, &c, None),
        VerifyOutcome::ChecksumDisabled
    );
    assert!(
        started.elapsed() < Duration::from_millis(200),
        "zero-byte objects must not wait for a VFS cache file rclone never writes"
    );
}
#[test]
fn projection_boundary() {
    use warm_drive_cache::capacity::Projection;
    assert!(
        !Projection {
            total: 1000,
            used: 800,
            available: 200,
            eligible: 100
        }
        .warns()
    );
    assert!(
        Projection {
            total: 1000,
            used: 800,
            available: 200,
            eligible: 101
        }
        .warns()
    );
}
#[test]
fn rate_limit_is_narrow() {
    use std::io;
    use warm_drive_cache::access::{AccessOperation, classify};
    for op in [
        AccessOperation::DirectoryTraversal,
        AccessOperation::MountMetadata,
        AccessOperation::MountOpen,
        AccessOperation::MountRead,
        AccessOperation::CacheMetadataPoll,
        AccessOperation::CacheOpen,
        AccessOperation::CacheRead,
    ] {
        assert!(
            classify(
                op,
                std::path::Path::new("/synthetic"),
                io::Error::other("HTTP 429 user-rate-limit exceeded")
            )
            .rate_limited
        );
        assert!(
            !classify(
                op,
                std::path::Path::new("/synthetic"),
                io::Error::other("file size limit reached")
            )
            .rate_limited
        )
    }
}
