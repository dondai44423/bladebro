//! Build script: embed the git commit SHA + dirty flag into
//! the binary so `bladebro -v` can identify exactly what code
//! it's running. When the tree is clean, the SHA matches a
//! commit; when dirty, "-dirty" marks an unreleased build.
//!
//! If git is unavailable (release tarball), the SHA is
//! "unknown" and the version string stands alone.

use std::process::Command;

fn main() {
    let sha = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "unknown".into());

    let dirty = Command::new("git")
        .args(["status", "--porcelain"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| !o.stdout.is_empty())
        .unwrap_or(false);

    let build_id = if sha == "unknown" {
        sha
    } else if dirty {
        format!("{sha}-dirty")
    } else {
        sha
    };

    println!("cargo:rustc-env=BLADE_BUILD_ID={build_id}");
    // Rebuild when the HEAD commit or tree state changes.
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/index");
}
