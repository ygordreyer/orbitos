use std::{env, path::PathBuf, process::Command};

fn git(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn main() {
    println!("cargo:rerun-if-env-changed=ORBITOS_BUILD_ID");
    for name in ["HEAD", "packed-refs"] {
        if let Some(path) = git(&["rev-parse", "--git-path", name]) {
            println!("cargo:rerun-if-changed={path}");
        }
    }
    if let Some(reference) = git(&["symbolic-ref", "-q", "HEAD"]) {
        if let Some(path) = git(&["rev-parse", "--git-path", &reference]) {
            println!("cargo:rerun-if-changed={path}");
        }
    }
    println!(
        "cargo:rerun-if-changed={}",
        PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap())
            .join("Cargo.toml")
            .display()
    );
    let id = env::var("ORBITOS_BUILD_ID")
        .ok()
        .or_else(|| git(&["rev-parse", "--short=8", "HEAD"]))
        .filter(|id| !id.is_empty() && id.chars().all(|c| c.is_ascii_hexdigit()))
        .unwrap_or_else(|| "unknown".into());
    println!("cargo:rustc-env=ORBITOS_BUILD_ID={id}");
}
