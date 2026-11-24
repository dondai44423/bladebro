//! Build script: embed the git commit SHA + dirty flag into
//! the binary so `bladebro -v` can identify exactly what code
//! it's running, and on Windows MSVC attach a PE VERSIONINFO
//! resource and `asInvoker` application manifest.
//!
//! If git is unavailable (release tarball), the SHA is
//! "unknown" and the version string stands alone.

use std::path::{Path, PathBuf};
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
    println!("cargo:rerun-if-env-changed=CARGO_PKG_VERSION");

    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let target_env = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    if target_os == "windows" && target_env == "msvc" {
        embed_windows_resources();
    }
}

/// Compile and link a standard Win32 `VS_VERSION_INFO` + `asInvoker` manifest
/// into `bladebro.exe` on Windows MSVC builds. Shipping proper PE version
/// metadata and a standard execution-level manifest avoids unsigned-stripped-
/// binary heuristics in antivirus scanners.
fn embed_windows_resources() {
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR set by cargo"));
    let version = std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.0.0".into());
    let mut parts = version.split('.').map(|p| p.parse::<u16>().unwrap_or(0));
    let major = parts.next().unwrap_or(0);
    let minor = parts.next().unwrap_or(0);
    let patch = parts.next().unwrap_or(0);
    let ver_comma = format!("{major},{minor},{patch},0");
    let ver_quad = format!("{major}.{minor}.{patch}.0");

    let manifest = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <assemblyIdentity version="{ver_quad}" processorArchitecture="amd64" name="Bladebro.CLI" type="win32"/>
  <description>Bladebro - Agentic Browser Driver</description>
  <trustInfo xmlns="urn:schemas-microsoft-com:asm.v3">
    <security>
      <requestedPrivileges>
        <requestedExecutionLevel level="asInvoker" uiAccess="false"/>
      </requestedPrivileges>
    </security>
  </trustInfo>
  <compatibility xmlns="urn:schemas-microsoft-com:compatibility.v1">
    <application>
      <supportedOS Id="{{8e0f7a12-bfb3-4fe8-b9a5-48fd50a15a9a}}"/>
    </application>
  </compatibility>
</assembly>
"#
    );

    let rc = format!(
        r#"1 VERSIONINFO
FILEVERSION {ver_comma}
PRODUCTVERSION {ver_comma}
FILEFLAGSMASK 0x3fL
FILEFLAGS 0x0L
FILEOS 0x40004L
FILETYPE 0x1L
FILESUBTYPE 0x0L
BEGIN
    BLOCK "StringFileInfo"
    BEGIN
        BLOCK "040904b0"
        BEGIN
            VALUE "CompanyName", "Bladebro"
            VALUE "FileDescription", "Bladebro - Agentic Browser Driver"
            VALUE "FileVersion", "{ver_quad}"
            VALUE "InternalName", "bladebro"
            VALUE "LegalCopyright", "Copyright (c) Bladebro Contributors (Apache-2.0)"
            VALUE "OriginalFilename", "bladebro.exe"
            VALUE "ProductName", "Bladebro"
            VALUE "ProductVersion", "{version}"
        END
    END
    BLOCK "VarFileInfo"
    BEGIN
        VALUE "Translation", 0x0409, 1200
    END
END

1 24 "bladebro.manifest"
"#
    );

    let manifest_path = out_dir.join("bladebro.manifest");
    let rc_path = out_dir.join("bladebro.rc");
    let res_path = out_dir.join("bladebro.res");
    std::fs::write(&manifest_path, manifest).expect("write bladebro.manifest");
    std::fs::write(&rc_path, rc).expect("write bladebro.rc");

    let Some(rc_exe) = find_windows_rc() else {
        if std::env::var_os("CI").is_some() {
            panic!("rc.exe not found on Windows MSVC CI runner — cannot embed PE version metadata");
        }
        println!("cargo:warning=rc.exe not found; skipping Windows PE VERSIONINFO resource");
        return;
    };

    let status = Command::new(&rc_exe)
        .current_dir(&out_dir)
        .args(["/nologo", "/fo", "bladebro.res", "bladebro.rc"])
        .status()
        .expect("invoke rc.exe");
    assert!(
        status.success() && res_path.is_file(),
        "rc.exe ({}) failed with {status}",
        rc_exe.display()
    );
    println!("cargo:rustc-link-arg-bin=bladebro={}", res_path.display());
}

fn find_windows_rc() -> Option<PathBuf> {
    for candidate in ["rc.exe", "llvm-rc.exe"] {
        if Command::new(candidate)
            .arg("/?")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok()
        {
            return Some(PathBuf::from(candidate));
        }
    }
    let arch = match std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() {
        Ok("aarch64") => "arm64",
        Ok("x86") => "x86",
        _ => "x64",
    };
    let mut kits = Vec::new();
    if let Ok(sdk) = std::env::var("WindowsSdkDir") {
        kits.push(PathBuf::from(sdk).join("bin"));
    }
    if let Ok(pf86) = std::env::var("ProgramFiles(x86)") {
        kits.push(PathBuf::from(pf86).join(r"Windows Kits\10\bin"));
    }
    kits.push(PathBuf::from(r"C:\Program Files (x86)\Windows Kits\10\bin"));

    for bin_root in kits {
        if let Some(found) = newest_sdk_rc(&bin_root, arch) {
            return Some(found);
        }
    }
    None
}

fn newest_sdk_rc(bin_root: &Path, arch: &str) -> Option<PathBuf> {
    let mut versions: Vec<PathBuf> = std::fs::read_dir(bin_root)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    versions.sort();
    for dir in versions.into_iter().rev() {
        let candidate = dir.join(arch).join("rc.exe");
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    let direct = bin_root.join(arch).join("rc.exe");
    direct.is_file().then_some(direct)
}
