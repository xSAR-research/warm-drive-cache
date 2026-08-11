//! Per-cache concurrency warning lock.
//!
//! The empty `warm-drive-cache.lock` file is created atomically in each configured cache root.
//! Its presence warns about another live instance or a stale lock left by an unclean exit.
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
};
pub const LOCK_NAME: &str = "warm-drive-cache.lock";
#[derive(Debug)]
pub struct CacheLock {
    path: PathBuf,
    _file: File,
}
impl CacheLock {
    pub fn acquire(cache: &Path) -> Result<Self, String> {
        let path = cache.join(LOCK_NAME);
        match create(&path) {
            Ok(file) => Ok(Self { path, _file: file }),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                print!(
                    "Another instance of the application has been detected, do you wish to continue [y/N]? "
                );
                io::stdout()
                    .flush()
                    .map_err(|e| format!("cannot display lock prompt: {e}"))?;
                let mut answer = String::new();
                io::stdin()
                    .read_line(&mut answer)
                    .map_err(|e| format!("cannot read lock prompt response: {e}"))?;
                if !matches!(answer.trim(), "y" | "Y") {
                    return Err(
                        "continuation declined because warm-drive-cache.lock already exists".into(),
                    );
                }
                fs::remove_file(&path)
                    .map_err(|e| format!("cannot replace stale lock {}: {e}", path.display()))?;
                let file = create(&path)
                    .map_err(|e| format!("cannot create lock {}: {e}", path.display()))?;
                Ok(Self { path, _file: file })
            }
            Err(e) => Err(format!("cannot create lock {}: {e}", path.display())),
        }
    }
    pub fn path(&self) -> &Path {
        &self.path
    }
}
fn create(path: &Path) -> io::Result<File> {
    OpenOptions::new().write(true).create_new(true).open(path)
}
impl Drop for CacheLock {
    fn drop(&mut self) {
        if let Err(e) = fs::remove_file(&self.path)
            && e.kind() != io::ErrorKind::NotFound
        {
            eprintln!(
                "⚠️  Failed to remove concurrency lock {}: {e}",
                self.path.display()
            )
        }
    }
}
