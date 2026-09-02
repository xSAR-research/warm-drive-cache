//! Optional CSV warm log (`-l` / `--log`): warm and traversal outcomes under `/tmp/`.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::{
    Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::{SystemTime, UNIX_EPOCH};

/// Thread-safe CSV logger for warm and traversal outcomes.
pub struct WarmLog {
    inner: Mutex<WarmLogInner>,
    path: PathBuf,
    failure_reported: AtomicBool,
}

struct WarmLogInner {
    file: File,
    first_error: Option<String>,
}

impl WarmLog {
    /// Create `/tmp/warm-drive-cache-YYYYMMDD-HHMMSS.csv` with a header row.
    /// A process-specific suffix is added if another logger used the same second.
    pub fn create() -> Result<Self, String> {
        let stamp = local_timestamp_slug();
        let mut selected = None;
        for attempt in 0..100u8 {
            let suffix = if attempt == 0 {
                String::new()
            } else {
                format!("-{}-{attempt}", std::process::id())
            };
            let path = PathBuf::from(format!("/tmp/warm-drive-cache-{stamp}{suffix}.csv"));
            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&path)
            {
                Ok(file) => {
                    selected = Some((path, file));
                    break;
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => {
                    return Err(format!("cannot create log file {}: {e}", path.display()));
                }
            }
        }
        let (path, mut file) = selected.ok_or_else(|| {
            format!("cannot create a unique warm log for timestamp {stamp} after 100 attempts")
        })?;
        writeln!(
            file,
            "Service name,path,filename,size (bytes),status,error details"
        )
        .map_err(|e| format!("cannot write log header {}: {e}", path.display()))?;
        file.flush()
            .map_err(|e| format!("cannot flush log header {}: {e}", path.display()))?;
        Ok(Self {
            inner: Mutex::new(WarmLogInner {
                file,
                first_error: None,
            }),
            path,
            failure_reported: AtomicBool::new(false),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Append one successful CSV row. `status` is `READ` or `ATTRIB`.
    pub fn log_file(
        &self,
        service: &str,
        dir_path: &str,
        filename: &str,
        size_bytes: u64,
        status: &str,
    ) -> io::Result<()> {
        self.write_row(service, dir_path, filename, Some(size_bytes), status, "")
    }

    /// Append an `ERROR` row. Size is blank when metadata was unavailable.
    pub fn log_error(
        &self,
        service: &str,
        dir_path: &str,
        filename: &str,
        size_bytes: Option<u64>,
        error_details: &str,
    ) -> io::Result<()> {
        self.write_row(
            service,
            dir_path,
            filename,
            size_bytes,
            "ERROR",
            error_details,
        )
    }

    fn write_row(
        &self,
        service: &str,
        dir_path: &str,
        filename: &str,
        size_bytes: Option<u64>,
        status: &str,
        error_details: &str,
    ) -> io::Result<()> {
        let mut guard = self
            .inner
            .lock()
            .map_err(|_| io::Error::other("warm log mutex poisoned"))?;
        if let Some(error) = &guard.first_error {
            return Err(io::Error::other(format!(
                "warm log disabled after earlier failure: {error}"
            )));
        }
        let size_bytes = size_bytes.map(|size| size.to_string()).unwrap_or_default();
        if let Err(error) = writeln!(
            guard.file,
            "{},{},{},{},{},{}",
            csv_escape(service),
            csv_escape(dir_path),
            csv_escape(filename),
            size_bytes,
            csv_escape(status),
            csv_escape(error_details)
        ) {
            let row_path = if filename.is_empty() {
                PathBuf::from(dir_path)
            } else {
                Path::new(dir_path).join(filename)
            };
            guard.first_error = Some(format!("row write for {row_path:?}: {error}"));
            return Err(error);
        }
        Ok(())
    }

    /// Return the original failure context to only one reporting caller.
    pub(crate) fn claim_failure_report(&self) -> Option<String> {
        if self.failure_reported.swap(true, Ordering::AcqRel) {
            return None;
        }
        Some(match self.inner.lock() {
            Ok(guard) => guard
                .first_error
                .clone()
                .unwrap_or_else(|| "warm log write failed".into()),
            Err(_) => "warm log mutex poisoned".into(),
        })
    }

    pub fn flush(&self) -> io::Result<()> {
        let mut guard = self
            .inner
            .lock()
            .map_err(|_| io::Error::other("warm log mutex poisoned"))?;
        let earlier_error = guard.first_error.clone();
        if let Err(error) = guard.file.flush() {
            let flush_error = format!("final flush failed: {error}");
            if guard.first_error.is_none() {
                guard.first_error = Some(flush_error.clone());
            }
            return Err(io::Error::other(match earlier_error {
                Some(earlier) => format!("earlier logging failure: {earlier}; {flush_error}"),
                None => flush_error,
            }));
        }
        earlier_error.map_or(Ok(()), |error| {
            Err(io::Error::other(format!(
                "earlier logging failure: {error}"
            )))
        })
    }
}

/// Neutralise spreadsheet formula prefixes, then quote the complete text field.
fn csv_escape(field: &str) -> String {
    let normalised = field.replace('\0', "�");
    let trimmed = normalised.trim_start_matches([' ', '\t', '\r', '\n']);
    let formula_like = normalised.starts_with(['\t', '\r'])
        || trimmed.starts_with(['=', '+', '-', '@', '＝', '＋', '－', '＠']);
    let safe = if formula_like {
        format!("'{normalised}")
    } else {
        normalised
    };
    let escaped = safe.replace('"', "\"\"");
    format!("\"{escaped}\"")
}

/// Local wall-clock stamp `YYYYMMDD-HHMMSS` for the log file name.
fn local_timestamp_slug() -> String {
    // Prefer libc localtime (already a project dependency) for a stable local stamp.
    // SAFETY: time(2)/localtime(3) are used only to format a display string; no concurrent
    // libc localtime calls elsewhere in this process during logging setup.
    unsafe {
        let t = libc::time(std::ptr::null_mut());
        if t != -1 {
            let tm = libc::localtime(&t);
            if !tm.is_null() {
                let tm = *tm;
                return format!(
                    "{:04}{:02}{:02}-{:02}{:02}{:02}",
                    tm.tm_year + 1900,
                    tm.tm_mon + 1,
                    tm.tm_mday,
                    tm.tm_hour,
                    tm.tm_min,
                    tm.tm_sec
                );
            }
        }
    }
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| format!("epoch-{}", d.as_secs()))
        .unwrap_or_else(|_| "unknown".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn csv_escape_quotes_specials() {
        assert_eq!(csv_escape("plain"), "\"plain\"");
        assert_eq!(csv_escape("a,b"), "\"a,b\"");
        assert_eq!(csv_escape("say \"hi\""), "\"say \"\"hi\"\"\"");
    }

    #[test]
    fn csv_escape_neutralises_spreadsheet_formulas() {
        assert_eq!(csv_escape("=1+1"), "\"'=1+1\"");
        assert_eq!(csv_escape("  @SUM(A1:A2)"), "\"'  @SUM(A1:A2)\"");
        assert_eq!(csv_escape("\t+cmd"), "\"'\t+cmd\"");
        assert_eq!(csv_escape("－42"), "\"'－42\"");
        assert_eq!(csv_escape("safe;=cmd"), "\"safe;=cmd\"");
        assert_eq!(csv_escape("nul\0byte"), "\"nul�byte\"");
        assert_eq!(csv_escape("ordinary + value"), "\"ordinary + value\"");
    }

    #[test]
    fn create_log_writes_private_file_with_expanded_header() {
        let log = WarmLog::create().expect("create log");
        let path = log.path().to_path_buf();
        assert!(path.starts_with("/tmp/"));
        assert!(
            path.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("warm-drive-cache-") && n.ends_with(".csv"))
        );
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o077, 0, "group/other must have no log access");
        log.log_file("svc.service", "/tmp/dir", "file.txt", 42, "READ")
            .unwrap();
        log.flush().unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            text,
            concat!(
                "Service name,path,filename,size (bytes),status,error details\n",
                "\"svc.service\",\"/tmp/dir\",\"file.txt\",42,\"READ\",\"\"\n"
            )
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn error_rows_support_known_and_unknown_sizes() {
        let log = WarmLog::create().expect("create log");
        let path = log.path().to_path_buf();
        log.log_error(
            "svc.service",
            "/tmp/dir",
            "known.txt",
            Some(42),
            "cache checksum mismatch",
        )
        .unwrap();
        log.log_error(
            "svc.service",
            "/tmp/dir",
            "unknown.txt",
            None,
            "source metadata: permission denied",
        )
        .unwrap();
        log.flush().unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains(
            "\"svc.service\",\"/tmp/dir\",\"known.txt\",42,\"ERROR\",\"cache checksum mismatch\"\n"
        ));
        assert!(text.contains(
            "\"svc.service\",\"/tmp/dir\",\"unknown.txt\",,\"ERROR\",\"source metadata: permission denied\"\n"
        ));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn error_details_are_rfc4180_escaped() {
        let log = WarmLog::create().expect("create log");
        let path = log.path().to_path_buf();
        log.log_error(
            "svc,\"quoted\"",
            "/tmp/dir",
            "file.txt",
            None,
            "open failed, \"temporarily\"\nretry\rlater",
        )
        .unwrap();
        log.flush().unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            text,
            concat!(
                "Service name,path,filename,size (bytes),status,error details\n",
                "\"svc,\"\"quoted\"\"\",\"/tmp/dir\",\"file.txt\",,\"ERROR\",",
                "\"open failed, \"\"temporarily\"\"\nretry\rlater\"\n"
            )
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn row_write_failure_is_retained_until_final_flush() {
        let created = WarmLog::create().expect("create log");
        let path = created.path().to_path_buf();
        drop(created);

        let read_only = File::open(&path).expect("reopen log read-only");
        let log = WarmLog {
            inner: Mutex::new(WarmLogInner {
                file: read_only,
                first_error: None,
            }),
            path: path.clone(),
            failure_reported: AtomicBool::new(false),
        };
        assert!(
            log.log_file("svc.service", "/tmp", "file.txt", 1, "READ")
                .is_err()
        );
        let final_error = log.flush().expect_err("row failure must remain sticky");
        assert!(final_error.to_string().contains("earlier logging failure"));
        let first_report = log
            .claim_failure_report()
            .expect("first reporter receives the original failure");
        assert!(first_report.contains("file.txt"));
        assert!(log.claim_failure_report().is_none());
        let _ = std::fs::remove_file(path);
    }
}
