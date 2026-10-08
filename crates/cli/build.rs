//! Build script: stamp provenance (`VAT_TARGET` / `VAT_GIT_SHA` /
//! `VAT_BUILT_AT`) so `vat upgrade` can pick the matching release asset and
//! `vat issue create` can attach build diagnostics.

use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn main() {
    stamp_provenance();
}

/// Stamp build provenance into the binary. All three are best-effort: outside a
/// git checkout the sha falls back to "unknown"; `TARGET` is always set by cargo
/// for build scripts.
fn stamp_provenance() {
    // Re-run when HEAD moves so the stamped sha stays current. The repository
    // `.git` lives two levels up from this crate; in a linked worktree `.git`
    // is a file rather than a dir, so guard the rerun hint.
    let head = std::path::Path::new("../../.git/HEAD");
    if head.exists() {
        println!("cargo:rerun-if-changed={}", head.display());
    }

    let git_sha = short_sha().unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=VAT_GIT_SHA={git_sha}");

    let built_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs().to_string())
        .unwrap_or_else(|_| "unknown".to_string());
    println!("cargo:rustc-env=VAT_BUILT_AT={built_at}");

    let target = std::env::var("TARGET").unwrap_or_else(|_| "unknown".to_string());
    println!("cargo:rustc-env=VAT_TARGET={target}");
}

/// Best-effort short SHA of HEAD. Returns `None` outside a git workspace.
fn short_sha() -> Option<String> {
    let out = Command::new("git")
        .args(["rev-parse", "--short=8", "HEAD"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let sha = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if sha.is_empty() {
        None
    } else {
        Some(sha)
    }
}
