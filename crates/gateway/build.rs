//! Build the vendored SpeexDSP echo canceller (../speexdsp) for the host and
//! every cross target. The flags mirror EchoMuse's cgo line verbatim
//! (`-DFLOATING_POINT -DUSE_KISS_FFT -DEXPORT= -O2`); no config.h is used.

use std::process::Command;

/// Build identity of the firmware/gateway binary: explicit `HM_FIRMWARE_BUILD`
/// (CI), else `git rev-parse --short=12 HEAD` plus `-dirty`. The M8a release
/// packer refuses a `-dirty` firmware, exactly like it refuses a dirty core.
fn firmware_build() -> String {
    if let Ok(v) = std::env::var("HM_FIRMWARE_BUILD") {
        let v = v.trim().to_string();
        if !v.is_empty() {
            return v;
        }
    }
    let repo_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("crate dir nested two levels below the workspace root");
    let git = |args: &[&str]| -> Option<String> {
        let out = Command::new("git")
            .arg("-C")
            .arg(repo_root)
            .args(args)
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
    };
    let mut id = git(&["rev-parse", "--short=12", "HEAD"]).unwrap_or_else(|| "unknown".into());
    let dirty = git(&["status", "--porcelain", "--untracked-files=no"])
        .map(|s| !s.is_empty())
        .unwrap_or(false);
    if dirty && id != "unknown" {
        id.push_str("-dirty");
    }
    id
}

fn main() {
    println!("cargo:rerun-if-env-changed=HM_FIRMWARE_BUILD");
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-changed=../../.git/refs/heads");
    // The dirty flag is a property of the SOURCE TREE, so any source change
    // must re-run this script. Without it Cargo keeps a cached clean id from
    // the last build and a dirty binary ships as "clean" (measured
    // 2026-11-14: editing three sources and rebuilding kept `<sha>` until a
    // touch forced the script to run).
    println!("cargo:rerun-if-changed=src");
    // A dependency change (Cargo.toml / Cargo.lock) can rebuild the binary
    // without touching src, which kept a stale clean id (audit 2026-11-14).
    println!("cargo:rerun-if-changed=Cargo.toml");
    println!("cargo:rerun-if-changed=../../Cargo.toml");
    println!("cargo:rerun-if-changed=../../Cargo.lock");
    println!("cargo:rustc-env=HM_FIRMWARE_BUILD={}", firmware_build());
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("crate dir nested two levels below the workspace root")
        .join("speexdsp");
    let mut build = cc::Build::new();
    build
        .include(root.join("include"))
        .include(root.join("src"))
        .define("FLOATING_POINT", None)
        .define("USE_KISS_FFT", None)
        .define("EXPORT", Some(""))
        .opt_level(2)
        .warnings(false)
        .files(
            ["mdf.c", "fftwrap.c", "kiss_fft.c", "kiss_fftr.c"]
                .iter()
                .map(|f| root.join("src").join(f)),
        );
    // mdf.c's `#include "config.h"` sits behind #ifdef HAVE_CONFIG_H, which
    // we never define — the -D flags above are the whole configuration,
    // exactly like EchoMuse's cgo build.
    build.compile("speexdsp_aec");
    println!("cargo:rerun-if-changed={}", root.join("src").display());
    println!("cargo:rerun-if-changed={}", root.join("include").display());
}
