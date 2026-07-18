//! End-of-run summary: completion or graceful stop, thanks, and issue reporting.
//!
//! Always call from `main` before process exit (success, cancel, or after path loop).

use crate::startup::{PKG_NAME, PKG_REPOSITORY, PRODUCT_OF, XSAR_WEBSITE};

/// Build a GitHub issues URL from the repository homepage when possible.
fn issues_url() -> String {
    let repo = PKG_REPOSITORY.trim_end_matches('/');
    if repo.is_empty() {
        return XSAR_WEBSITE.to_string();
    }
    if repo.contains("github.com") {
        format!("{repo}/issues")
    } else {
        repo.to_string()
    }
}

/// Print the final summary after maintenance finishes or is stopped early.
pub fn print_exit_summary(stopped_early: bool) {
    println!();
    if stopped_early {
        println!("⏹  Cache maintenance stopped by user (graceful).");
        println!("   In-flight workers were allowed to finish; no new files were started.");
    } else {
        println!("✅ Cache maintenance complete!");
    }
    println!(
        "   (Because the storage is write-through, no separate cache-clear step is required or executed.)"
    );
    println!();
    println!("Thanks for using {PKG_NAME} — a product of {PRODUCT_OF}.");
    println!("Website: {XSAR_WEBSITE}");
    println!("Raise issues or feature requests: {}", issues_url());
    if !PKG_REPOSITORY.is_empty() {
        println!("Source: {PKG_REPOSITORY}");
    }
}
