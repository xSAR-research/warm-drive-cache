use std::env;
use std::fs;
use std::path::Path;

fn main() {
    // Rerun build if sample changes
    println!("cargo:rerun-if-changed=config.example.json");

    // Determine target directory for the current profile (debug or release)
    let profile = env::var("PROFILE").unwrap_or_else(|_| "debug".into());
    let target_dir = env::var("CARGO_TARGET_DIR").unwrap_or_else(|_| "target".into());
    let dest_dir = Path::new(&target_dir).join(&profile);

    // Ensure the directory exists
    if let Err(e) = fs::create_dir_all(&dest_dir) {
        println!(
            "cargo:warning=Could not create {}: {}",
            dest_dir.display(),
            e
        );
        return;
    }

    let src = Path::new("config.example.json");
    let dest = dest_dir.join("config.example.json");

    match fs::copy(src, &dest) {
        Ok(_) => {
            println!("cargo:warning=Sample config pushed to {}", dest.display());
        }
        Err(e) => {
            println!(
                "cargo:warning=Failed to copy sample config to {}: {}",
                dest.display(),
                e
            );
        }
    }
}
