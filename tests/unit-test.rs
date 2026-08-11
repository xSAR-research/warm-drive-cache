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
    let c: Config = serde_json::from_str(r#"{"paths":[{"sync":"/a","cache":"/b"}]}"#).unwrap();
    assert!(c.walk.checksum)
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
    assert_eq!(
        parse_cli_args(["--json", "--wat", "-t"]).unwrap(),
        CliAction::JsonValidation {
            overrides: Default::default()
        }
    );
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
    dirty_check::wait_until_clean_with_interval(&cache, 30, &cancelled, Duration::from_millis(5))
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
    ] {
        parse_cli_args(args).expect("documented short and long option must parse");
    }

    let CliAction::Run { overrides, .. } =
        parse_cli_args(["-t", "4", "-s", "1MiB", "-c", "NO"]).unwrap()
    else {
        panic!("runtime overrides should produce a run action");
    };
    assert_eq!(overrides.threads, Some(4));
    assert_eq!(overrides.size, Some(1024 * 1024));
    assert_eq!(overrides.checksum, Some(false));
}
#[test]
fn sizes() {
    for (s, n) in [
        ("-1", -1),
        ("0", 0),
        ("1.5KiB", 1536),
        ("2mb", 2 * 1024 * 1024),
    ] {
        assert_eq!(config::parse_size_expr(s).unwrap(), n)
    }
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
        verifier::verify(&s, &d, true, true, o, &c),
        VerifyOutcome::Verified
    );
    fs::write(&d, b"abd").unwrap();
    assert_eq!(
        verifier::verify(&s, &d, true, true, o, &c),
        VerifyOutcome::ChecksumMismatch
    );
    assert_eq!(
        verifier::verify(&s, &d, false, true, o, &c),
        VerifyOutcome::ChecksumDisabled
    );
    assert_eq!(
        verifier::verify(&s, &d, true, false, o, &c),
        VerifyOutcome::AttributesOnly
    )
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
