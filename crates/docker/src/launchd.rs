//! Socket activation: a per-user launchd job owns the machine's Docker
//! socket and runs the VMM when a client connects.
//!
//! While the machine is stopped nothing of vat runs, yet
//! `~/.vat/run/docker.sock` accepts connections: launchd holds the listener
//! and launches the job (the VMM itself) on the first one. The VMM takes the
//! listener with `launch_activate_socket`, holds clients until dockerd is
//! ready, and leaves the socket in place when it exits (after an idle stop or
//! `vat machine stop`), so the next client boots the machine again.
//!
//! The default home (`~/.vat`) installs the job in `~/Library/LaunchAgents`
//! so it survives logout. Other homes (`$VAT_MACHINE_HOME`, used by tests)
//! load it for the current login session only and never touch `~/Library`.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};

use crate::assets::BootAssets;
use crate::{write_atomic, MachinePaths};

/// Socket name in the job's `Sockets` dictionary.
pub const DOCKER_SOCKET: &str = "docker";

fn default_home() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".vat"))
}

fn is_default_home() -> bool {
    crate::home().ok() == default_home()
}

/// The job label: `dev.vat.machine.<name>`, plus a hash of the machine home
/// when it is not the default one, so test homes never collide with it.
pub fn label(paths: &MachinePaths) -> String {
    let base = format!("dev.vat.machine.{}", paths.name);
    if is_default_home() {
        return base;
    }
    let home = crate::home().unwrap_or_default();
    let digest = Sha256::digest(home.as_os_str().as_encoded_bytes());
    let hex: String = digest[..4].iter().map(|b| format!("{b:02x}")).collect();
    format!("{base}.{hex}")
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn string(s: impl AsRef<str>) -> String {
    format!("<string>{}</string>", xml_escape(s.as_ref()))
}

/// The job: run the VMM for this machine when the Docker socket is used.
pub fn plist(paths: &MachinePaths, boot: &BootAssets, vmm: &Path) -> String {
    let args = [
        vmm.display().to_string(),
        "machine".into(),
        "__vmm".into(),
        "--name".into(),
        paths.name.clone(),
        "--kernel".into(),
        boot.kernel.display().to_string(),
        "--initramfs".into(),
        boot.initramfs.display().to_string(),
    ];
    let args: String = args
        .iter()
        .map(|a| format!("\n    {}", string(a)))
        .collect();
    let env = match std::env::var("VAT_MACHINE_HOME") {
        Ok(home) => format!(
            "\n  <key>EnvironmentVariables</key>\n  <dict><key>VAT_MACHINE_HOME</key>{}</dict>",
            string(home)
        ),
        Err(_) => String::new(),
    };
    let log = paths.vmm_log.display().to_string();
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  {label}
  <key>ProgramArguments</key>
  <array>{args}
  </array>
  <key>Sockets</key>
  <dict>
    <key>{DOCKER_SOCKET}</key>
    <dict>
      <key>SockPathName</key>
      {sock}
      <key>SockPathMode</key>
      <integer>384</integer>
    </dict>
  </dict>{env}
  <key>StandardOutPath</key>
  {out}
  <key>StandardErrorPath</key>
  {err}
  <key>ThrottleInterval</key>
  <integer>1</integer>
  <key>ProcessType</key>
  <string>Interactive</string>
</dict>
</plist>
"#,
        label = string(label(paths)),
        sock = string(paths.docker_sock.display().to_string()),
        out = string(&log),
        err = string(&log),
    )
}

/// Where launchd loads the job from: `~/Library/LaunchAgents` for the
/// default home, else the machine directory.
fn plist_path(paths: &MachinePaths) -> Option<PathBuf> {
    if is_default_home() {
        let agents = dirs::home_dir()?.join("Library/LaunchAgents");
        Some(agents.join(format!("{}.plist", label(paths))))
    } else {
        Some(paths.dir.join("launchd.plist"))
    }
}

fn domain() -> String {
    format!("gui/{}", unsafe { libc::getuid() })
}

fn launchctl(args: &[&str]) -> Result<std::process::Output> {
    Command::new("launchctl")
        .args(args)
        .output()
        .context("run launchctl")
}

/// Whether the job is loaded, i.e. launchd holds the Docker socket.
pub fn loaded(paths: &MachinePaths) -> bool {
    let target = format!("{}/{}", domain(), label(paths));
    launchctl(&["print", &target]).is_ok_and(|o| o.status.success())
}

/// Load (or reload, when it changed) the job. Returns `false` when launchd
/// cannot take it, e.g. over ssh without a GUI session; the caller then runs
/// the VMM directly.
pub fn install(paths: &MachinePaths, plist: &str) -> Result<bool> {
    if !cfg!(target_os = "macos") {
        return Ok(false);
    }
    let path = plist_path(paths).context("resolve the LaunchAgents directory")?;
    let unchanged = std::fs::read_to_string(&path).is_ok_and(|old| old == plist);
    if unchanged && loaded(paths) {
        return Ok(true);
    }
    write_atomic(&path, plist.as_bytes())?;
    // launchd creates a missing socket directory as root, and the VMM could
    // then not put its control socket next to docker.sock.
    if let Some(dir) = paths.docker_sock.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let target = format!("{}/{}", domain(), label(paths));
    let _ = launchctl(&["bootout", &target]);
    let out = launchctl(&["bootstrap", &domain(), &path.display().to_string()])?;
    if !out.status.success() {
        eprintln!(
            "vat machine: launchd did not take the socket ({}); docker.sock will not wake the machine",
            String::from_utf8_lossy(&out.stderr).trim()
        );
        return Ok(false);
    }
    Ok(true)
}

/// Run the job now, without waiting for a client.
pub fn kickstart(paths: &MachinePaths) -> Result<()> {
    let target = format!("{}/{}", domain(), label(paths));
    let out = launchctl(&["kickstart", &target])?;
    anyhow::ensure!(
        out.status.success(),
        "launchctl kickstart {target}: {}",
        String::from_utf8_lossy(&out.stderr).trim()
    );
    Ok(())
}

/// Unload the job and delete its plist; the Docker socket goes with it.
pub fn uninstall(paths: &MachinePaths) -> Result<()> {
    if !cfg!(target_os = "macos") {
        return Ok(());
    }
    let target = format!("{}/{}", domain(), label(paths));
    let _ = launchctl(&["bootout", &target]);
    if let Some(path) = plist_path(paths) {
        let _ = std::fs::remove_file(path);
    }
    let _ = std::fs::remove_file(&paths.docker_sock);
    Ok(())
}

/// The listener launchd passes to the job, or `None` when this process was
/// not launched by launchd for it.
#[cfg(target_os = "macos")]
pub fn activated_listener(name: &str) -> Option<std::os::unix::net::UnixListener> {
    use std::os::fd::FromRawFd;
    extern "C" {
        fn launch_activate_socket(
            name: *const libc::c_char,
            fds: *mut *mut libc::c_int,
            cnt: *mut libc::size_t,
        ) -> libc::c_int;
    }
    let name = std::ffi::CString::new(name).ok()?;
    let mut fds: *mut libc::c_int = std::ptr::null_mut();
    let mut cnt: libc::size_t = 0;
    let rc = unsafe { launch_activate_socket(name.as_ptr(), &mut fds, &mut cnt) };
    if rc != 0 || fds.is_null() {
        return None;
    }
    let all = unsafe { std::slice::from_raw_parts(fds, cnt) }.to_vec();
    unsafe { libc::free(fds.cast()) };
    let mut all = all.into_iter();
    let first = all.next()?;
    for extra in all {
        unsafe { libc::close(extra) };
    }
    let listener = unsafe { std::os::unix::net::UnixListener::from_raw_fd(first) };
    listener.set_nonblocking(true).ok()?;
    Some(listener)
}

#[cfg(not(target_os = "macos"))]
pub fn activated_listener(_name: &str) -> Option<std::os::unix::net::UnixListener> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plist_owns_the_docker_socket_and_runs_the_vmm() {
        let paths = MachinePaths::new("default").unwrap();
        let boot = BootAssets {
            kernel: "/k/vmlinuz".into(),
            initramfs: "/k/initramfs & co".into(),
        };
        let p = plist(&paths, &boot, Path::new("/bin/vat-vmm"));
        assert!(p.contains(&format!("<string>{}</string>", label(&paths))));
        assert!(p.contains("<key>Sockets</key>"));
        assert!(p.contains(&format!(
            "<key>SockPathName</key>\n      <string>{}</string>",
            paths.docker_sock.display()
        )));
        assert!(p.contains("<string>__vmm</string>"));
        assert!(p.contains("<string>/k/initramfs &amp; co</string>"));
        assert!(!p.contains("KeepAlive"));
    }

    #[test]
    fn label_is_plain_only_for_the_default_home() {
        let paths = MachinePaths::new("default").unwrap();
        let l = label(&paths);
        if is_default_home() {
            assert_eq!(l, "dev.vat.machine.default");
        } else {
            let hash = l.strip_prefix("dev.vat.machine.default.").unwrap();
            assert_eq!(hash.len(), 8);
        }
    }
}
