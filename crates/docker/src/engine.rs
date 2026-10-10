//! vat's Docker Engine: the `vat machine` socket is the default Docker
//! endpoint for everything vat runs (`vat run`, `vat build`, compose runners,
//! capability probes), so a stock `docker` CLI and vat agree on one daemon.
//!
//! An explicit `DOCKER_HOST` or `DOCKER_CONTEXT` always wins, and
//! `VAT_ENGINE=external` opts out entirely.

use std::path::PathBuf;
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};

use super::{client, MachinePaths, DEFAULT_MACHINE};

/// Opt-out switch: `VAT_ENGINE=external` leaves Docker endpoint selection to
/// the docker CLI (contexts, Docker Desktop, colima, ...).
pub const ENGINE_ENV: &str = "VAT_ENGINE";

/// Host path of the default machine's Docker Engine socket.
pub fn socket_path() -> Result<PathBuf> {
    Ok(MachinePaths::new(DEFAULT_MACHINE)?.docker_sock)
}

/// `DOCKER_HOST` value for the default machine.
pub fn docker_host() -> Result<String> {
    Ok(format!("unix://{}", socket_path()?.display()))
}

fn opted_out() -> bool {
    std::env::var(ENGINE_ENV).is_ok_and(|v| v.eq_ignore_ascii_case("external"))
}

fn user_selected_endpoint() -> bool {
    ["DOCKER_HOST", "DOCKER_CONTEXT"]
        .iter()
        .any(|k| std::env::var_os(k).is_some_and(|v| !v.is_empty()))
}

/// True when docker calls made by this process target vat's engine.
pub fn targets_vat() -> bool {
    let Ok(ours) = docker_host() else {
        return false;
    };
    std::env::var("DOCKER_HOST").is_ok_and(|v| v == ours)
}

/// Point every docker child process at vat's engine unless the user chose an
/// endpoint. Call once at CLI start, before any threads spawn.
pub fn adopt_env() {
    if opted_out() || user_selected_endpoint() || !cfg!(target_os = "macos") {
        return;
    }
    if let Ok(host) = docker_host() {
        // Called from the single-threaded CLI entry point.
        std::env::set_var("DOCKER_HOST", host);
    }
}

/// Whether the default machine exists (has been started at least once), so
/// vat may boot it on demand.
pub fn created() -> bool {
    MachinePaths::new(DEFAULT_MACHINE).is_ok_and(|p| p.config.is_file())
}

/// Whether docker calls can be served: the engine answers, or it is vat's
/// engine and [`ensure_running`] can boot it. Cheap; never boots anything.
pub fn reachable_or_bootable() -> bool {
    if !targets_vat() {
        return false;
    }
    socket_path().is_ok_and(|s| client::docker_ping(&s)) || created()
}

/// Make sure vat's engine answers, booting the default machine if it exists
/// but is stopped. A no-op when docker targets some other endpoint.
pub fn ensure_running() -> Result<()> {
    if !targets_vat() {
        return Ok(());
    }
    let sock = socket_path()?;
    if client::docker_ping(&sock) {
        return Ok(());
    }
    if !created() {
        bail!("vat's Docker engine is not set up yet; run `vat machine start` once (or set VAT_ENGINE=external)");
    }
    let exe = std::env::current_exe().context("resolve the vat binary")?;
    let out = Command::new(exe)
        .args(["machine", "start", "--json"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .context("start the vat machine")?;
    if !out.status.success() {
        bail!(
            "vat's Docker engine is stopped and `vat machine start` failed:\n{}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    if !client::docker_ping(&sock) {
        bail!("vat machine started but {} does not answer", sock.display());
    }
    Ok(())
}
