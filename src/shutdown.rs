//! Graceful early exit: SIGINT (Ctrl+C) and optional TTY single-key `q`.
//!
//! Sets a shared flag so walkers stop enqueueing and workers finish in-flight work only.

use std::io;
use std::os::fd::AsRawFd;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

/// Restores stdin termios on drop (used for single-key `q` quit).
pub struct StdinRawGuard {
    fd: i32,
    original: libc::termios,
}

impl StdinRawGuard {
    /// Put stdin into non-canonical mode so a single `q` is readable without Enter.
    /// Returns None when stdin is not a TTY (e.g. piped / CI).
    pub fn enable() -> Option<Self> {
        let fd = io::stdin().as_raw_fd();
        if unsafe { libc::isatty(fd) } == 0 {
            return None;
        }
        let mut original: libc::termios = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(fd, &mut original) } != 0 {
            return None;
        }
        let mut raw = original;
        // Non-canonical, no echo; VMIN=0 VTIME=1 → read returns after ~100ms with 0/1 bytes.
        raw.c_lflag &= !(libc::ICANON | libc::ECHO);
        raw.c_cc[libc::VMIN] = 0;
        raw.c_cc[libc::VTIME] = 1;
        if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw) } != 0 {
            return None;
        }
        Some(Self { fd, original })
    }
}

impl Drop for StdinRawGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = libc::tcsetattr(self.fd, libc::TCSANOW, &self.original);
        }
    }
}

/// Install the SIGINT/Ctrl+C handler without changing terminal input mode.
pub fn install_sigint_handler(shutdown: Arc<AtomicBool>) {
    let flag = Arc::clone(&shutdown);
    if let Err(e) = ctrlc::set_handler(move || {
        flag.store(true, Ordering::SeqCst);
    }) {
        eprintln!("   ⚠️  Could not install SIGINT handler: {e}");
    }
}

/// Install the optional `q` key listener after all interactive prompts have completed.
/// Returns a termios guard that must be held for the process lifetime of quit listening.
pub fn install_quit_listener(shutdown: Arc<AtomicBool>) -> Option<StdinRawGuard> {
    let term_guard = StdinRawGuard::enable();
    if term_guard.is_some() {
        let flag = Arc::clone(&shutdown);
        thread::spawn(move || {
            let mut buf = [0u8; 1];
            while !flag.load(Ordering::SeqCst) {
                let n = unsafe {
                    libc::read(
                        io::stdin().as_raw_fd(),
                        buf.as_mut_ptr() as *mut libc::c_void,
                        1,
                    )
                };
                if n == 1 && (buf[0] == b'q' || buf[0] == b'Q') {
                    flag.store(true, Ordering::SeqCst);
                    break;
                }
            }
        });
    }
    term_guard
}
