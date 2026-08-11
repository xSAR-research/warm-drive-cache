//! Optional CSV warm log (`-l` / `--log`): one row per processed file under `/tmp/`.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

/// Thread-safe CSV logger for File contents read / attributes-only outcomes.
pub struct WarmLog {
    inner: Mutex<WarmLogInner>,
    path: PathBuf,
}

struct WarmLogInner {
    file: File,
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
            match OpenOptions::new().write(true).create_new(true).open(&path) {
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
        writeln!(file, "Service name,path,filename,size (bytes),status")
            .map_err(|e| format!("cannot write log header {}: {e}", path.display()))?;
        file.flush()
            .map_err(|e| format!("cannot flush log header {}: {e}", path.display()))?;
        Ok(Self {
            inner: Mutex::new(WarmLogInner { file }),
            path,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Append one CSV data row. `status` is `READ` or `ATTRIB`.
    pub fn log_file(
        &self,
        service: &str,
        dir_path: &str,
        filename: &str,
        size_bytes: u64,
        status: &str,
    ) -> io::Result<()> {
        let mut guard = self
            .inner
            .lock()
            .map_err(|_| io::Error::other("warm log mutex poisoned"))?;
        writeln!(
            guard.file,
            "{},{},{},{},{}",
            csv_escape(service),
            csv_escape(dir_path),
            csv_escape(filename),
            size_bytes,
            csv_escape(status)
        )?;
        Ok(())
    }

    pub fn flush(&self) -> io::Result<()> {
        let mut guard = self
            .inner
            .lock()
            .map_err(|_| io::Error::other("warm log mutex poisoned"))?;
        guard.file.flush()
    }
}

/// RFC4180-style field escape (quote when needed).
fn csv_escape(field: &str) -> String {
    if field.contains(',') || field.contains('"') || field.contains('\n') || field.contains('\r') {
        let escaped = field.replace('"', "\"\"");
        format!("\"{escaped}\"")
    } else {
        field.to_string()
    }
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

    #[test]
    fn csv_escape_quotes_specials() {
        assert_eq!(csv_escape("plain"), "plain");
        assert_eq!(csv_escape("a,b"), "\"a,b\"");
        assert_eq!(csv_escape("say \"hi\""), "\"say \"\"hi\"\"\"");
    }

    #[test]
    fn create_log_writes_header() {
        let log = WarmLog::create().expect("create log");
        let path = log.path().to_path_buf();
        assert!(path.starts_with("/tmp/"));
        assert!(
            path.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("warm-drive-cache-") && n.ends_with(".csv"))
        );
        log.log_file("svc.service", "/tmp/dir", "file.txt", 42, "READ")
            .unwrap();
        log.flush().unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("Service name,path,filename,size (bytes),status\n"));
        assert!(text.contains("svc.service,/tmp/dir,file.txt,42,READ"));
        let _ = std::fs::remove_file(path);
    }
}
