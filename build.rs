//! Records the git commit the kernel is built from, for the `version` command.

use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8(output.stdout).ok()?.trim().to_string())
}

fn main() {
    let commit = git(&["rev-parse", "--short", "HEAD"]).unwrap_or_else(|| "unknown".into());
    let dirty = git(&["status", "--porcelain"]).is_some_and(|status| !status.is_empty());
    println!("cargo:rustc-env=GIT_VERSION={}{}", commit, if dirty { "-dirty" } else { "" });

    // Rebuild when the commit or the working tree changes.
    for path in [".git/HEAD", ".git/index", ".git/refs", "src", "build.rs", "linker.ld", "Cargo.toml"] {
        println!("cargo:rerun-if-changed={}", path);
    }
}
