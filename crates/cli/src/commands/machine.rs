// CODEGEN-BEGIN
//! `vat machine` — the shared Linux VM behind the Docker Engine socket and
//! the local K3s cluster.
//!
//! `start` prepares assets, keeps a signed VMM copy of this binary, hands it
//! to launchd as a socket-activated job (or spawns it detached when launchd
//! cannot take it), and waits until `dockerd` answers on the host socket.
//! Every verb has a `--json` form for agents.
//!
//! After a vat upgrade, launchd would keep waking the VMM and guest files the
//! previous vat staged, so machine verbs [`refresh`] them first: a stopped
//! machine is re-staged silently, an idle one restarted onto the new build,
//! and a busy one keeps running until its next start (see [`vm::stage`]).

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde::Serialize;
use serde_json::{json, Value};

use crate::k3s::K8sConfig;
use crate::vm::stage::{self, Job, Observed, Repair};
use crate::vm::{self, assets, client, launchd, MachineConfig, MachinePaths, VmmState};

/// Options for `vat machine start`.
#[derive(Debug, Clone, Default)]
pub struct StartArgs {
    pub name: String,
    pub cpus: Option<u32>,
    pub memory_mib: Option<u64>,
    pub disk_gib: Option<u64>,
    pub k8s: Option<bool>,
    /// Stop after this many idle seconds (0: never); applies live.
    pub idle_stop_secs: Option<u64>,
    pub no_wait: bool,
    pub timeout_s: u64,
    pub json: bool,
}

pub(crate) fn read_json(path: &Path) -> Option<Value> {
    std::fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
}

fn vmm_state(paths: &MachinePaths) -> Option<VmmState> {
    std::fs::read(&paths.vmm_state)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
}

/// PID of the live VMM, if any.
/// Exclusive lock held for the whole of [`boot`]; released when dropped.
fn lock_boot(paths: &MachinePaths) -> Result<std::fs::File> {
    use std::os::fd::AsRawFd;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(paths.dir.join("boot.lock"))
        .context("open the machine boot lock")?;
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
        bail!(
            "lock the machine for boot: {}",
            std::io::Error::last_os_error()
        );
    }
    Ok(file)
}

pub(crate) fn running_pid(paths: &MachinePaths) -> Option<u32> {
    let st = vmm_state(paths)?;
    (st.state != "stopped" && vm::pid_alive(st.pid)).then_some(st.pid)
}

/// SIGTERM the VMM and wait for it to exit, killing it after 40s. Returns
/// whether it had to be killed.
fn terminate(pid: u32) -> bool {
    unsafe { libc::kill(pid as i32, libc::SIGTERM) };
    let deadline = Instant::now() + Duration::from_secs(40);
    while vm::pid_alive(pid) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
    let forced = vm::pid_alive(pid);
    if forced {
        unsafe { libc::kill(pid as i32, libc::SIGKILL) };
    }
    forced
}

/// Bring what launchd runs for machine `name` up to this vat build. A
/// stopped machine is re-staged; an idle VMM on an older build is restarted
/// when `restart_idle` (for commands about to use the machine); a busy one
/// keeps running and is re-staged for its next start. Returns whether a VMM
/// still runs an older build.
pub(crate) fn refresh(name: &str, restart_idle: bool) -> Result<bool> {
    if !cfg!(target_os = "macos") {
        return Ok(false);
    }
    let paths = MachinePaths::new(name)?;
    let Some(cfg) = MachineConfig::load(&paths.config)? else {
        return Ok(false);
    };
    match Observed::read(&paths, Some(&cfg))?.plan(restart_idle) {
        Repair::Nothing => return Ok(false),
        Repair::Restart => {
            // boot() decides again under its lock.
            let b = boot(&StartArgs {
                name: name.to_string(),
                timeout_s: 900,
                json: true,
                ..Default::default()
            })?;
            return Ok(b.vmm_outdated);
        }
        Repair::Restage | Repair::Defer => {}
    }
    let _boot_lock = lock_boot(&paths)?;
    let obs = Observed::read(&paths, Some(&cfg))?;
    if !obs.staged.matches(&obs.sha256) {
        let live = obs.running.is_some();
        stage::restage(&paths, &cfg, &obs.sha256, live, Job::Refresh)?;
    }
    Ok(obs.outdated())
}

/// [`refresh`] for commands that work either way: a failure is a warning.
pub(crate) fn refresh_or_warn(name: &str, restart_idle: bool) -> bool {
    refresh(name, restart_idle).unwrap_or_else(|err| {
        eprintln!("vat machine: could not update the machine to this vat build: {err:#}");
        false
    })
}

fn tail(path: &Path, lines: usize) -> String {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let all: Vec<&str> = text.lines().collect();
    all[all.len().saturating_sub(lines)..].join("\n")
}

pub fn start(args: StartArgs) -> Result<ExitCode> {
    let b = boot(&args)?;
    report_start(&args, &b.paths, &b.cfg, b.pid, b.timings)
}

/// A machine whose VMM is up (and, unless `no_wait`, whose dockerd answers).
pub(crate) struct Booted {
    pub paths: MachinePaths,
    pub cfg: MachineConfig,
    pub pid: u32,
    pub timings: Value,
    /// The VMM runs an older vat build: it was busy, so it was left running.
    pub vmm_outdated: bool,
}

/// Create or boot the machine and wait for dockerd, without printing.
pub(crate) fn boot(args: &StartArgs) -> Result<Booted> {
    if !cfg!(target_os = "macos") {
        bail!("vat machine requires macOS on Apple Silicon (Virtualization.framework)");
    }
    let t0 = Instant::now();
    let paths = MachinePaths::new(&args.name)?;
    let existing = MachineConfig::load(&paths.config)?;
    let first_create = existing.is_none();
    let mut cfg = existing.unwrap_or_default();
    let mut changed = first_create;
    if let Some(v) = args.cpus {
        changed |= cfg.cpus != v;
        cfg.cpus = v;
    }
    if let Some(v) = args.memory_mib {
        changed |= cfg.memory_mib != v;
        cfg.memory_mib = v;
    }
    if let Some(v) = args.disk_gib {
        if v < cfg.disk_gib {
            bail!(
                "the data disk cannot shrink ({} GiB -> {v} GiB)",
                cfg.disk_gib
            );
        }
        changed |= cfg.disk_gib != v;
        cfg.disk_gib = v;
    }
    if let Some(v) = args.k8s {
        let mut k8s = K8sConfig::of(&cfg)?;
        changed |= k8s.enabled != v;
        k8s.enabled = v;
        k8s.store(&mut cfg);
    }
    // Not a reason to restart: a running VMM re-reads it on SIGHUP.
    let idle_changed = args
        .idle_stop_secs
        .is_some_and(|v| !first_create && v != cfg.idle_stop_secs);
    if let Some(v) = args.idle_stop_secs {
        cfg.idle_stop_secs = v;
    }

    // Serialize starters: preparing can take minutes (downloads), and a second
    // starter must see the first one's VMM instead of racing it for the disk.
    std::fs::create_dir_all(&paths.dir)?;
    let _boot_lock = lock_boot(&paths)?;
    let mut upgraded_from = None;
    if let Some(pid) = running_pid(&paths) {
        if changed && !first_create {
            bail!(
                "machine {} is running; stop it before changing its configuration",
                args.name
            );
        }
        if idle_changed {
            cfg.save(&paths.config)?;
            unsafe { libc::kill(pid as i32, libc::SIGHUP) };
        }
        let obs = Observed::read(&paths, Some(&cfg))?;
        if obs.plan(true) == Repair::Restart {
            if !args.json {
                eprintln!("vat machine: restarting the idle machine onto this vat build");
            }
            terminate(pid);
            upgraded_from = Some(pid);
        } else {
            if !obs.staged.matches(&obs.sha256) {
                stage::restage(&paths, &cfg, &obs.sha256, true, Job::Refresh)?;
            }
            let outdated = obs.outdated();
            if outdated && !args.json {
                eprintln!("vat machine: {}", stage::OUTDATED_BUSY);
            }
            let ready = client::docker_ping(&paths.docker_sock);
            return Ok(Booted {
                paths,
                cfg,
                pid,
                timings: json!({
                    "already_running": true,
                    "docker_ready": ready,
                    "vmm_outdated": outdated,
                }),
                vmm_outdated: outdated,
            });
        }
    }

    cfg.save(&paths.config)?;
    assets::ensure_data_disk(&paths, &cfg)?;
    let sha256 = stage::self_sha256(&paths)?;
    let staged = stage::restage(&paths, &cfg, &sha256, false, Job::Install)?;
    let prepare_ms = t0.elapsed().as_millis() as u64;

    let _ = std::fs::remove_file(&paths.vmm_state);
    std::fs::File::create(&paths.vmm_log)?;
    let pid = if staged.launchd {
        launchd::kickstart(&paths)?;
        wait_vmm_pid(&paths, Duration::from_secs(10))?
    } else {
        spawn_vmm(&paths, &staged.vmm, &staged.boot)?
    };

    if args.no_wait {
        return Ok(Booted {
            paths,
            cfg,
            pid,
            timings: json!({ "prepare_ms": prepare_ms, "upgraded_from_pid": upgraded_from }),
            vmm_outdated: false,
        });
    }
    let boot_t0 = Instant::now();
    let deadline = boot_t0 + Duration::from_secs(args.timeout_s);
    let mut last_phase = String::new();
    let mut provisioned = false;
    loop {
        if !vm::pid_alive(pid) {
            bail!(
                "the VMM exited during boot\n--- vmm.log ---\n{}\n--- console.log ---\n{}",
                tail(&paths.vmm_log, 20),
                tail(&paths.console_log, 40)
            );
        }
        if let Some(boot) = read_json(&paths.guest_boot()) {
            let phase = boot["phase"].as_str().unwrap_or_default().to_string();
            if phase == "failed" {
                bail!(
                    "guest boot failed: {}\n--- console.log ---\n{}",
                    boot["detail"].as_str().unwrap_or_default(),
                    tail(&paths.console_log, 40)
                );
            }
            if phase != last_phase {
                provisioned |= phase == "provisioning";
                if !args.json {
                    eprintln!("vat machine: {phase}");
                }
                last_phase = phase;
            }
        }
        if client::docker_ping(&paths.docker_sock) {
            break;
        }
        if Instant::now() > deadline {
            bail!(
                "timed out after {}s waiting for dockerd\n--- console.log ---\n{}",
                args.timeout_s,
                tail(&paths.console_log, 40)
            );
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let docker_ready_ms = boot_t0.elapsed().as_millis() as u64;
    let vm_start_ms = vmm_state(&paths).map(|s| s.vm_start_ms);
    Ok(Booted {
        timings: json!({
            "prepare_ms": prepare_ms,
            "vm_start_ms": vm_start_ms,
            "docker_ready_ms": docker_ready_ms,
            "first_boot_provisioned": provisioned,
            "upgraded_from_pid": upgraded_from,
        }),
        paths,
        cfg,
        pid,
        vmm_outdated: false,
    })
}

/// Run the VMM detached, outside launchd: the machine works, but its socket
/// goes away when it stops.
fn spawn_vmm(paths: &MachinePaths, vmm: &Path, boot: &assets::BootAssets) -> Result<u32> {
    let log = std::fs::OpenOptions::new()
        .append(true)
        .open(&paths.vmm_log)?;
    let mut cmd = Command::new(vmm);
    cmd.args(["machine", "__vmm", "--name", &paths.name, "--kernel"])
        .arg(&boot.kernel)
        .arg("--initramfs")
        .arg(&boot.initramfs)
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let child = cmd.spawn().context("spawn the VMM")?;
    let pid = child.id();
    // The VMM is a daemon now; it is reaped by launchd once we exit.
    std::mem::forget(child);
    Ok(pid)
}

/// The pid launchd's VMM records once it runs.
fn wait_vmm_pid(paths: &MachinePaths, within: Duration) -> Result<u32> {
    let deadline = Instant::now() + within;
    loop {
        if let Some(pid) = running_pid(paths) {
            return Ok(pid);
        }
        if Instant::now() > deadline {
            bail!(
                "launchd did not start the VMM within {}s\n--- vmm.log ---\n{}",
                within.as_secs(),
                tail(&paths.vmm_log, 20)
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Parse `--idle-stop`: `30s`, `5m`, `1h`, plain seconds, or `0`/`never`.
pub fn parse_idle_stop(s: &str) -> std::result::Result<u64, String> {
    let s = s.trim();
    if s == "never" {
        return Ok(0);
    }
    let (num, unit) = s.split_at(s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len()));
    let n: u64 = num
        .parse()
        .map_err(|_| format!("expected a duration like 30s, 5m, 1h, or 0; got {s:?}"))?;
    match unit {
        "" | "s" => Ok(n),
        "m" => Ok(n * 60),
        "h" => Ok(n * 3600),
        _ => Err(format!("unknown unit {unit:?} in {s:?}; use s, m, or h")),
    }
}

fn human_secs(secs: u64) -> String {
    match secs {
        s if s % 3600 == 0 => format!("{}h", s / 3600),
        s if s % 60 == 0 => format!("{}m", s / 60),
        s => format!("{s}s"),
    }
}

fn report_start(
    args: &StartArgs,
    paths: &MachinePaths,
    cfg: &MachineConfig,
    pid: u32,
    timings: Value,
) -> Result<ExitCode> {
    let docker_host = format!("unix://{}", paths.docker_sock.display());
    if args.json {
        crate::commands::print_json(
            &json!({
                "machine": args.name,
                "state": "running",
                "pid": pid,
                "docker_host": docker_host,
                "cpus": cfg.cpus,
                "memory_mib": cfg.memory_mib,
                "disk_gib": cfg.disk_gib,
                "k8s": K8sConfig::of(cfg)?.enabled,
                "timings": timings,
            }),
            false,
        )?;
    } else {
        println!("machine {} running (pid {pid})", args.name);
        if let Some(ms) = timings["docker_ready_ms"].as_u64() {
            println!("dockerd ready in {ms} ms");
        }
        println!("export DOCKER_HOST={docker_host}");
    }
    Ok(ExitCode::SUCCESS)
}

/// Stop the machine. Its socket stays with launchd, so the next Docker
/// client boots it again, unless `no_wake` unloads the job too.
pub fn stop(name: &str, no_wake: bool, json_out: bool) -> Result<ExitCode> {
    let paths = MachinePaths::new(name)?;
    if no_wake {
        launchd::uninstall(&paths)?;
    }
    let wakes = !no_wake && launchd::loaded(&paths);
    let Some(pid) = running_pid(&paths) else {
        refresh_or_warn(name, false);
        return emit(
            json_out,
            json!({ "machine": name, "state": "stopped", "was_running": false, "wake_on_socket": wakes }),
            "machine is not running",
        );
    };
    let t0 = Instant::now();
    let forced = terminate(pid);
    let stop_ms = t0.elapsed().as_millis() as u64;
    // Stopped now: the next start or wake runs this vat build.
    refresh_or_warn(name, false);
    emit(
        json_out,
        json!({
            "machine": name,
            "state": "stopped",
            "was_running": true,
            "forced": forced,
            "stop_ms": stop_ms,
            "wake_on_socket": wakes,
        }),
        if wakes {
            "machine stopped; the next Docker client starts it again"
        } else {
            "machine stopped"
        },
    )
}

fn emit(json_out: bool, value: Value, human: &str) -> Result<ExitCode> {
    if json_out {
        crate::commands::print_json(&value, false)?;
    } else {
        println!("{human}");
    }
    Ok(ExitCode::SUCCESS)
}

fn process_rss_kib(pid: u32) -> Option<u64> {
    let out = Command::new("ps")
        .args(["-o", "rss=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

#[derive(Serialize)]
struct Status {
    machine: String,
    state: &'static str,
    pid: Option<u32>,
    docker_host: String,
    docker_ready: bool,
    config: Option<MachineConfig>,
    vmm: Option<VmmState>,
    /// The running VMM is from another vat build than this CLI (it was busy
    /// when the CLI changed); restarting the machine updates it.
    vmm_outdated: bool,
    /// The vat build of this CLI, of the VMM copy a boot or wake runs, and
    /// of the running VMM.
    vat_sha256: Option<String>,
    vmm_staged_sha256: Option<String>,
    vmm_running_sha256: Option<String>,
    vmm_rss_kib: Option<u64>,
    /// The Virtualization.framework process that holds guest memory: its
    /// physical footprint (what macOS counts) and CPU time.
    vm_footprint_kib: Option<u64>,
    vm_cpu_ms: Option<u64>,
    /// The VM process, guest clock sync, and last stop, as the VMM last
    /// recorded them.
    elastic: Option<Value>,
    /// launchd holds the Docker socket: a client boots a stopped machine.
    wake_on_socket: bool,
    /// Seconds idle before the VMM stops (0: never), and what keeps it up
    /// regardless.
    idle_stop_secs: Option<u64>,
    keeps_awake: Option<&'static str>,
    guest: Option<Value>,
    ports: Option<Value>,
    data_disk_allocated_kib: Option<u64>,
}

pub fn status(name: &str, json_out: bool) -> Result<ExitCode> {
    let paths = MachinePaths::new(name)?;
    refresh_or_warn(name, false);
    let pid = running_pid(&paths);
    // Only a VMM that is up and serving: launchd queues a connection to a
    // stopping VMM's socket and starts the machine again for it, and a
    // status check should not wake the machine.
    let serving = pid.is_some() && vmm_state(&paths).is_some_and(|s| s.state == "running");
    let docker_ready = serving && client::docker_ping(&paths.docker_sock);
    let disk_kib = std::fs::metadata(&paths.data_img).ok().map(|m| {
        use std::os::unix::fs::MetadataExt;
        m.blocks() / 2
    });
    let elastic = read_json(&paths.elastic_state());
    let vm_usage = elastic
        .as_ref()
        .filter(|_| pid.is_some())
        .and_then(|e| e["vm_pid"].as_u64())
        .and_then(|p| crate::vm::elastic::proc_usage(p as u32));
    let config = MachineConfig::load(&paths.config)?;
    let wake_on_socket = config.is_some() && launchd::loaded(&paths);
    let build = config
        .as_ref()
        .and_then(|c| Observed::read(&paths, Some(c)).ok());
    let vmm = vmm_state(&paths);
    let st = Status {
        machine: name.to_string(),
        state: if pid.is_some() {
            "running"
        } else if paths.config.exists() {
            "stopped"
        } else {
            "absent"
        },
        pid,
        docker_host: format!("unix://{}", paths.docker_sock.display()),
        docker_ready,
        idle_stop_secs: config.as_ref().map(|c| c.idle_stop_secs),
        keeps_awake: config.as_ref().and_then(MachineConfig::keeps_awake),
        config,
        wake_on_socket,
        vmm_outdated: build.as_ref().is_some_and(Observed::outdated),
        vat_sha256: build.as_ref().map(|b| b.sha256.clone()),
        vmm_staged_sha256: build.as_ref().and_then(|b| b.staged.vmm.clone()),
        vmm_running_sha256: pid.and(vmm.as_ref()).and_then(|v| v.vat_sha256.clone()),
        vmm,
        vmm_rss_kib: pid.and_then(process_rss_kib),
        vm_footprint_kib: vm_usage.map(|u| u.footprint_kib),
        vm_cpu_ms: vm_usage.map(|u| u.cpu_ms),
        elastic,
        guest: pid.and_then(|_| read_json(&paths.guest_status())),
        ports: pid.and_then(|_| read_json(&paths.dir.join("ports.json"))),
        data_disk_allocated_kib: disk_kib,
    };
    if json_out {
        crate::commands::print_json(&st, false)?;
    } else {
        let wake = if st.state == "stopped" && wake_on_socket {
            ", wakes on docker.sock"
        } else {
            ""
        };
        println!("machine  {} ({}{wake})", st.machine, st.state);
        if let Some(pid) = st.pid {
            println!("pid      {pid}");
        }
        if st.vmm_outdated {
            println!("vmm      outdated (restart to update)");
        }
        println!(
            "docker   {} ({})",
            st.docker_host,
            if docker_ready { "ready" } else { "not ready" }
        );
        if let Some(g) = &st.guest {
            let total = g["mem_total_kib"].as_u64().unwrap_or(0);
            let avail = g["mem_available_kib"].as_u64().unwrap_or(0);
            println!(
                "guest    ip {}  mem used {} MiB / {} MiB",
                g["ip"].as_str().unwrap_or("?"),
                (total - avail.min(total)) / 1024,
                total / 1024
            );
        }
        if let Some(fp) = st.vm_footprint_kib {
            println!("host     {} MiB memory (VM process footprint)", fp / 1024);
        }
        match (st.keeps_awake, st.idle_stop_secs) {
            (Some(why), _) => println!("idle     stays up ({why})"),
            (None, Some(0)) => println!("idle     never stops"),
            (None, Some(s)) => println!("idle     stops after {} idle", human_secs(s)),
            (None, None) => {}
        }
        if st.pid.is_none() {
            if let Some(e) = &st.elastic {
                if let (Some(by), Some(at)) = (e["stopped_by"].as_str(), e["stopped_at"].as_i64()) {
                    let at = chrono::DateTime::from_timestamp(at, 0)
                        .map(|t| t.with_timezone(&chrono::Local).format("%F %T").to_string())
                        .unwrap_or_default();
                    println!("last     stopped ({by}) at {at}");
                }
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn shell_quote(arg: &str) -> String {
    if !arg.is_empty()
        && arg
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./=:,@%+".contains(c))
    {
        arg.to_string()
    } else {
        format!("'{}'", arg.replace('\'', r"'\''"))
    }
}

pub fn exec(name: &str, command: Vec<String>, json_out: bool) -> Result<ExitCode> {
    if command.is_empty() {
        bail!("usage: vat machine exec -- <command> [args...]");
    }
    let paths = MachinePaths::new(name)?;
    refresh_or_warn(name, true);
    let script = command
        .iter()
        .map(|a| shell_quote(a))
        .collect::<Vec<_>>()
        .join(" ");
    if json_out {
        let out = client::exec(&paths, &script)?;
        crate::commands::print_json(&out, false)?;
        return Ok(ExitCode::from(out.exit_code.clamp(0, 255) as u8));
    }
    let mut stdout = std::io::stdout();
    let code = client::exec_streaming(&paths, &script, &mut stdout)?;
    stdout.flush()?;
    Ok(ExitCode::from(code.clamp(0, 255) as u8))
}

pub fn env(name: &str, json_out: bool) -> Result<ExitCode> {
    let paths = MachinePaths::new(name)?;
    let docker_host = format!("unix://{}", paths.docker_sock.display());
    if json_out {
        crate::commands::print_json(&json!({ "DOCKER_HOST": docker_host }), false)?;
    } else {
        println!("export DOCKER_HOST={docker_host}");
    }
    Ok(ExitCode::SUCCESS)
}

pub fn logs(name: &str, vmm_log: bool, lines: usize) -> Result<ExitCode> {
    let paths = MachinePaths::new(name)?;
    let path = if vmm_log {
        &paths.vmm_log
    } else {
        &paths.console_log
    };
    println!("{}", tail(path, lines));
    Ok(ExitCode::SUCCESS)
}

pub fn rm(name: &str, yes: bool, json_out: bool) -> Result<ExitCode> {
    let paths = MachinePaths::new(name)?;
    if !yes {
        bail!(
            "`vat machine rm` deletes the data disk (images, volumes, cluster state); pass --yes"
        );
    }
    launchd::uninstall(&paths)?;
    if let Some(pid) = running_pid(&paths) {
        terminate(pid);
    }
    if paths.dir.exists() {
        std::fs::remove_dir_all(&paths.dir)?;
    }
    // Then what only machines use, so that removing the last one leaves
    // nothing for launchd or a Docker client to run. Downloaded assets stay
    // cached.
    let _ = std::fs::remove_file(&paths.docker_sock);
    let _ = std::fs::remove_file(&paths.control_sock);
    let _ = std::fs::remove_dir(&paths.run_dir);
    if let Some(run) = paths
        .run_dir
        .parent()
        .filter(|_| paths.name != vm::DEFAULT_MACHINE)
    {
        let _ = std::fs::remove_dir(run);
    }
    if !stage::other_machines(&paths) && paths.bin_dir.exists() {
        std::fs::remove_dir_all(&paths.bin_dir)?;
    }
    emit(
        json_out,
        json!({ "machine": name, "state": "absent" }),
        "machine removed",
    )
}

/// Hidden `vat machine __vmm`: become the VMM for `name`.
#[cfg(all(target_os = "macos", feature = "machine"))]
pub fn vmm(name: &str, kernel: PathBuf, initramfs: PathBuf) -> Result<ExitCode> {
    vm::vmm::run(name, assets::BootAssets { kernel, initramfs })?;
    Ok(ExitCode::SUCCESS)
}

#[cfg(not(all(target_os = "macos", feature = "machine")))]
pub fn vmm(_name: &str, _kernel: PathBuf, _initramfs: PathBuf) -> Result<ExitCode> {
    bail!("this vat build has no VMM (needs macOS and the `machine` feature)")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_stop_takes_seconds_minutes_hours_or_never() {
        assert_eq!(parse_idle_stop("30s"), Ok(30));
        assert_eq!(parse_idle_stop("5m"), Ok(300));
        assert_eq!(parse_idle_stop("1h"), Ok(3600));
        assert_eq!(parse_idle_stop("90"), Ok(90));
        assert_eq!(parse_idle_stop("0"), Ok(0));
        assert_eq!(parse_idle_stop("never"), Ok(0));
        assert!(parse_idle_stop("5d").is_err());
        assert!(parse_idle_stop("m").is_err());
        assert_eq!(human_secs(300), "5m");
        assert_eq!(human_secs(45), "45s");
    }
}
// CODEGEN-END
