//! Keeping what launchd runs in step with the vat CLI.
//!
//! A Docker client wakes the machine through launchd, which runs the staged
//! `machine/bin/vat-vmm` without vat being involved. After a vat upgrade only
//! the CLI can notice that this copy, the guest files and the job came from
//! an older build, so machine entry points compare them with the running
//! binary, re-stage them ([`restage`]), and [`plan`] whether a VMM still
//! running the old build may be restarted onto the new one.
//!
//! Every staged artifact is stamped with the SHA-256 of the vat binary it
//! came from: `machine/bin/vat-vmm.sha256` for the VMM copy (shared by all
//! machines), and `machine/<name>/staged.sha256` once that machine's guest
//! files and launchd job are staged as well. The guest scripts and the guest
//! agent are embedded in vat, so the binary's hash covers them too. The VMM
//! records both stamps in `vmm.json` when it starts ([`VmmState`]).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use anyhow::{bail, Context, Result};

use crate::assets::{self, BootAssets};
use crate::elastic::ElasticState;
use crate::{launchd, pid_alive, write_atomic, MachineConfig, MachinePaths, VmmState};

const ENTITLEMENTS: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict><key>com.apple.security.virtualization</key><true/></dict></plist>
"#;

/// SHA-256 of the vat binary this process runs.
pub fn self_sha256(paths: &MachinePaths) -> Result<String> {
    static SHA: OnceLock<String> = OnceLock::new();
    if let Some(sha) = SHA.get() {
        return Ok(sha.clone());
    }
    let exe = std::env::current_exe().context("locate the vat binary")?;
    let sha = cached_sha256(&exe, &paths.bin_dir.join("vat.source"))?;
    Ok(SHA.get_or_init(|| sha).clone())
}

/// Hash `exe`, reusing the hash recorded in `cache` while the file keeps its
/// path, size, mtime and inode: machine commands run often, the binary is
/// tens of MB, and it only changes on an upgrade or rebuild.
fn cached_sha256(exe: &Path, cache: &Path) -> Result<String> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::metadata(exe).with_context(|| format!("stat {}", exe.display()))?;
    let id = format!(
        "{}\t{}\t{}.{}\t{}",
        exe.display(),
        meta.len(),
        meta.mtime(),
        meta.mtime_nsec(),
        meta.ino()
    );
    if let Ok(text) = std::fs::read_to_string(cache) {
        if let Some((seen, sha)) = text.trim_end().rsplit_once('\n') {
            if seen == id && sha.len() == 64 {
                return Ok(sha.to_string());
            }
        }
    }
    let sha = assets::sha256_file(exe)?;
    if cache.parent().is_some_and(Path::is_dir) {
        let _ = write_atomic(cache, format!("{id}\n{sha}\n").as_bytes());
    }
    Ok(sha)
}

fn vmm_stamp(paths: &MachinePaths) -> PathBuf {
    paths.bin_dir.join("vat-vmm.sha256")
}

fn machine_stamp(paths: &MachinePaths) -> PathBuf {
    paths.dir.join("staged.sha256")
}

fn read_stamp(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let sha = text.trim();
    (!sha.is_empty()).then(|| sha.to_string())
}

/// The vat builds staged for a machine.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Staged {
    /// The shared, signed `vat-vmm` copy.
    pub vmm: Option<String>,
    /// This machine's guest files and launchd job.
    pub machine: Option<String>,
}

impl Staged {
    pub fn read(paths: &MachinePaths) -> Self {
        let binary = paths.bin_dir.join("vat-vmm").is_file();
        Self {
            vmm: binary.then(|| read_stamp(&vmm_stamp(paths))).flatten(),
            machine: read_stamp(&machine_stamp(paths)),
        }
    }

    /// Whether a boot or socket wake now would run the vat build `sha256`.
    pub fn matches(&self, sha256: &str) -> bool {
        self.vmm.as_deref() == Some(sha256) && self.machine.as_deref() == Some(sha256)
    }
}

/// A live VMM, as the CLI sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Running {
    pub pid: u32,
    /// It runs the CLI's build ([`VmmState::runs`]).
    pub current: bool,
    /// Nothing uses it: no client, no container, no addon (K8s) keeping it
    /// awake. Unknown (an older VMM, still booting) counts as busy.
    pub idle: bool,
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Option<T> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

/// The VMM serving `paths`, if one is up, compared with the vat build
/// `sha256`.
pub fn running(paths: &MachinePaths, sha256: &str, cfg: Option<&MachineConfig>) -> Option<Running> {
    let st: VmmState = read_json(&paths.vmm_state)?;
    if st.state == "stopped" || !pid_alive(st.pid) {
        return None;
    }
    let es: Option<ElasticState> = read_json(&paths.elastic_state());
    let idle = st.state == "running"
        && es.is_some_and(|e| e.vmm_pid == Some(st.pid) && e.idle)
        && cfg.and_then(MachineConfig::keeps_awake).is_none();
    Some(Running {
        pid: st.pid,
        current: st.runs(sha256),
        idle,
    })
}

/// What to tell a user whose machine kept running an older build.
pub const OUTDATED_BUSY: &str = "the machine runs a VMM from an older vat build and is busy, \
so it was left running; it updates on its next start (`vat machine stop`, or when it stops idle)";

/// What a machine command does about a VMM, guest files or job staged from
/// another vat build.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Repair {
    /// Everything matches this binary.
    Nothing,
    /// Re-stage; no VMM runs an old build.
    Restage,
    /// Re-stage and restart the idle VMM onto the new build.
    Restart,
    /// Re-stage (if needed) for the next boot or wake, and leave the old VMM
    /// running: it is busy, or the command must not restart it.
    Defer,
}

/// Decide what to do. `restart_idle` is for commands about to use the
/// machine (start, exec, `k8s up`, a Docker call); a status check or stop
/// never restarts it. A busy VMM is never restarted: it would kill
/// containers or cut off clients.
pub fn plan(staged_current: bool, running: Option<&Running>, restart_idle: bool) -> Repair {
    match running {
        Some(r) if !r.current && r.idle && restart_idle => Repair::Restart,
        Some(r) if !r.current => Repair::Defer,
        _ if staged_current => Repair::Nothing,
        _ => Repair::Restage,
    }
}

/// Everything [`plan`] needs, read from disk.
#[derive(Debug, Clone)]
pub struct Observed {
    pub sha256: String,
    pub staged: Staged,
    pub running: Option<Running>,
}

impl Observed {
    pub fn read(paths: &MachinePaths, cfg: Option<&MachineConfig>) -> Result<Self> {
        let sha256 = self_sha256(paths)?;
        Ok(Self {
            staged: Staged::read(paths),
            running: running(paths, &sha256, cfg),
            sha256,
        })
    }

    pub fn plan(&self, restart_idle: bool) -> Repair {
        plan(
            self.staged.matches(&self.sha256),
            self.running.as_ref(),
            restart_idle,
        )
    }

    /// A VMM runs an older (or unknown) vat build.
    pub fn outdated(&self) -> bool {
        self.running.as_ref().is_some_and(|r| !r.current)
    }
}

/// How [`restage`] treats the launchd job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Job {
    /// Load it (`vat machine start`).
    Install,
    /// Reload a job that is loaded but stale; never load one that `vat
    /// machine stop --no-wake` unloaded.
    Refresh,
}

/// What [`restage`] staged.
#[derive(Debug, Clone)]
pub struct Restaged {
    pub boot: BootAssets,
    pub vmm: PathBuf,
    /// launchd holds the socket and runs `vmm` for it.
    pub launchd: bool,
    /// The job is current too; `false` when reloading it had to wait for a
    /// running VMM to stop.
    pub complete: bool,
}

/// Stage everything a boot or socket wake runs, from this binary (whose
/// hash is `sha256`): boot assets, guest files, the signed VMM copy, and
/// the launchd job. With `live` a VMM is running: its guest keeps its
/// reports, and a job whose definition changed is left alone, because
/// reloading it would stop the VMM.
pub fn restage(
    paths: &MachinePaths,
    cfg: &MachineConfig,
    sha256: &str,
    live: bool,
    job: Job,
) -> Result<Restaged> {
    let boot = assets::ensure_boot_assets(paths)?;
    assets::stage_guest_files(paths, cfg)?;
    if !live {
        assets::clear_guest_reports(paths);
    }
    let vmm = ensure_vmm_binary(paths, sha256)?;
    let plist = launchd::plist(paths, &boot, &vmm);
    let (launchd, complete) = match job {
        Job::Install => (launchd::install(paths, &plist)?, true),
        Job::Refresh if !launchd::loaded(paths) => (false, true),
        Job::Refresh if live && !launchd::is_current(paths, &plist) => (true, false),
        Job::Refresh => (launchd::install(paths, &plist)?, true),
    };
    if complete {
        write_atomic(&machine_stamp(paths), sha256.as_bytes())?;
    }
    Ok(Restaged {
        boot,
        vmm,
        launchd,
        complete,
    })
}

/// Keep `machine/bin/vat-vmm` identical to this binary and signed with the
/// virtualization entitlement (unsigned binaries cannot create VMs). The
/// copy is renamed into place, so a VMM running the previous one is
/// unaffected.
fn ensure_vmm_binary(paths: &MachinePaths, sha256: &str) -> Result<PathBuf> {
    let dest = paths.bin_dir.join("vat-vmm");
    if dest.is_file() && read_stamp(&vmm_stamp(paths)).as_deref() == Some(sha256) {
        return Ok(dest);
    }
    let exe = std::env::current_exe().context("locate the vat binary")?;
    std::fs::create_dir_all(&paths.bin_dir)?;
    let ent = paths.bin_dir.join("vat-vmm.entitlements");
    std::fs::write(&ent, ENTITLEMENTS)?;
    // Per process: machines share the directory, and their starts don't
    // share a lock.
    let tmp = paths
        .bin_dir
        .join(format!("vat-vmm.{}.part", std::process::id()));
    std::fs::copy(&exe, &tmp).with_context(|| format!("copy {}", exe.display()))?;
    let out = Command::new("codesign")
        .args(["-s", "-", "-f", "--entitlements"])
        .arg(&ent)
        .arg(&tmp)
        .output()
        .context("run codesign")?;
    if !out.status.success() {
        let _ = std::fs::remove_file(&tmp);
        bail!(
            "codesign failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    std::fs::rename(&tmp, &dest)?;
    // After the rename: a VMM starting in between records the old stamp
    // and reads as outdated, never the reverse.
    write_atomic(&vmm_stamp(paths), sha256.as_bytes())?;
    Ok(dest)
}

/// Whether machines other than `paths.name` exist under the same home.
pub fn other_machines(paths: &MachinePaths) -> bool {
    let Some(root) = paths.dir.parent() else {
        return false;
    };
    std::fs::read_dir(root).is_ok_and(|entries| {
        entries
            .flatten()
            .any(|e| e.file_name() != paths.name.as_str() && e.path().join("config.json").is_file())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const OLD: &str = "1111111111111111111111111111111111111111111111111111111111111111";
    const NEW: &str = "2222222222222222222222222222222222222222222222222222222222222222";

    fn vmm(current: bool, idle: bool) -> Running {
        Running {
            pid: 1,
            current,
            idle,
        }
    }

    #[test]
    fn a_stopped_machine_is_restaged_silently() {
        assert_eq!(plan(true, None, true), Repair::Nothing);
        assert_eq!(plan(false, None, true), Repair::Restage);
        assert_eq!(plan(false, None, false), Repair::Restage);
    }

    #[test]
    fn an_idle_outdated_vmm_restarts_only_for_commands_that_use_it() {
        assert_eq!(plan(false, Some(&vmm(false, true)), true), Repair::Restart);
        assert_eq!(plan(true, Some(&vmm(false, true)), true), Repair::Restart);
        assert_eq!(plan(false, Some(&vmm(false, true)), false), Repair::Defer);
    }

    #[test]
    fn a_busy_outdated_vmm_is_never_restarted() {
        assert_eq!(plan(false, Some(&vmm(false, false)), true), Repair::Defer);
        assert_eq!(plan(true, Some(&vmm(false, false)), true), Repair::Defer);
    }

    #[test]
    fn a_current_vmm_only_needs_missing_stamps_restaged() {
        assert_eq!(plan(true, Some(&vmm(true, true)), true), Repair::Nothing);
        assert_eq!(plan(true, Some(&vmm(true, false)), true), Repair::Nothing);
        assert_eq!(plan(false, Some(&vmm(true, false)), true), Repair::Restage);
    }

    #[test]
    fn staged_builds_need_both_stamps_and_the_binary() {
        let home = tempfile::tempdir().unwrap();
        let paths = MachinePaths::at(home.path(), "default");
        assert!(!Staged::read(&paths).matches(NEW));
        std::fs::create_dir_all(&paths.bin_dir).unwrap();
        std::fs::create_dir_all(&paths.dir).unwrap();
        // The pre-stamp format: no trailing newline.
        std::fs::write(vmm_stamp(&paths), NEW).unwrap();
        std::fs::write(machine_stamp(&paths), format!("{NEW}\n")).unwrap();
        assert!(
            !Staged::read(&paths).matches(NEW),
            "a stamp without the binary"
        );
        std::fs::write(paths.bin_dir.join("vat-vmm"), b"").unwrap();
        assert!(Staged::read(&paths).matches(NEW));
        assert!(!Staged::read(&paths).matches(OLD));
        // Another machine re-staged the shared binary only.
        std::fs::write(machine_stamp(&paths), OLD).unwrap();
        assert!(!Staged::read(&paths).matches(NEW));
    }

    fn write_vmm(paths: &MachinePaths, st: serde_json::Value) {
        std::fs::create_dir_all(&paths.dir).unwrap();
        std::fs::write(&paths.vmm_state, st.to_string()).unwrap();
    }

    #[test]
    fn a_vmm_without_recorded_builds_is_outdated_and_busy() {
        let home = tempfile::tempdir().unwrap();
        let paths = MachinePaths::at(home.path(), "default");
        let pid = std::process::id();
        // vmm.json and elastic.json as a VMM from before the stamps wrote them.
        write_vmm(
            &paths,
            serde_json::json!({"pid": pid, "started_at": 0, "vm_start_ms": 1, "rosetta": false, "state": "running"}),
        );
        std::fs::write(
            paths.elastic_state(),
            r#"{"vm_pid": null, "clock_synced_at": null, "clock_skew_ms": null, "host_sleeps": 0, "stopped_by": null, "stopped_at": null}"#,
        )
        .unwrap();
        let r = running(&paths, NEW, None).unwrap();
        assert!(!r.current && !r.idle);
        assert_eq!(plan(true, Some(&r), true), Repair::Defer);
    }

    #[test]
    fn idleness_comes_from_this_vmm_only() {
        let home = tempfile::tempdir().unwrap();
        let paths = MachinePaths::at(home.path(), "default");
        let pid = std::process::id();
        let state = |state: &str, sha: &str| {
            serde_json::json!({
                "pid": pid, "started_at": 0, "vm_start_ms": 1, "rosetta": false,
                "state": state, "vat_version": "0.5.0", "vat_sha256": sha, "staged_sha256": sha,
            })
        };
        let elastic = |vmm_pid: u32, idle: bool| {
            let es = ElasticState {
                vmm_pid: Some(vmm_pid),
                idle,
                ..Default::default()
            };
            std::fs::write(paths.elastic_state(), serde_json::to_vec(&es).unwrap()).unwrap();
        };
        write_vmm(&paths, state("running", OLD));
        elastic(pid, true);
        let r = running(&paths, NEW, None).unwrap();
        assert!(!r.current && r.idle);
        assert_eq!(plan(true, Some(&r), true), Repair::Restart);
        assert!(running(&paths, OLD, None).unwrap().current);

        // The previous VMM's last word, before this one wrote its own.
        elastic(pid + 1, true);
        assert!(!running(&paths, NEW, None).unwrap().idle);
        elastic(pid, false);
        assert!(!running(&paths, NEW, None).unwrap().idle);
        // Booting counts as busy whatever elastic.json says.
        write_vmm(&paths, state("booting", OLD));
        elastic(pid, true);
        assert!(!running(&paths, NEW, None).unwrap().idle);
        // Stopped VMMs are not running at all.
        write_vmm(&paths, state("stopped", OLD));
        assert!(running(&paths, NEW, None).is_none());
    }

    #[test]
    fn the_binary_hash_is_reused_while_the_file_is_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("vat");
        let cache = dir.path().join("vat.source");
        std::fs::write(&exe, b"one").unwrap();
        let first = cached_sha256(&exe, &cache).unwrap();
        assert_eq!(first, assets::sha256_file(&exe).unwrap());
        // A cache hit returns the recorded hash without reading the file.
        let text = std::fs::read_to_string(&cache).unwrap();
        std::fs::write(&cache, text.replace(&first, NEW)).unwrap();
        assert_eq!(cached_sha256(&exe, &cache).unwrap(), NEW);
        // A new binary at the same path is hashed again.
        std::fs::remove_file(&exe).unwrap();
        std::fs::write(&exe, b"two, longer").unwrap();
        assert_eq!(
            cached_sha256(&exe, &cache).unwrap(),
            assets::sha256_file(&exe).unwrap()
        );
    }

    #[test]
    fn other_machines_are_ones_with_a_config() {
        let home = tempfile::tempdir().unwrap();
        let paths = MachinePaths::at(home.path(), "default");
        std::fs::create_dir_all(&paths.dir).unwrap();
        std::fs::create_dir_all(&paths.bin_dir).unwrap();
        std::fs::create_dir_all(&paths.assets).unwrap();
        std::fs::write(&paths.config, "{}").unwrap();
        assert!(!other_machines(&paths));
        let other = MachinePaths::at(home.path(), "other");
        std::fs::create_dir_all(&other.dir).unwrap();
        assert!(!other_machines(&paths), "a directory without a config");
        std::fs::write(&other.config, "{}").unwrap();
        assert!(other_machines(&paths));
        assert!(other_machines(&other));
    }
}
