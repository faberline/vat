// CODEGEN-BEGIN
//! `vat machine` — the shared Linux VM behind the Docker Engine socket and
//! the local K3s cluster.
//!
//! `start` prepares assets, keeps a signed VMM copy of this binary, spawns it
//! detached, and waits until `dockerd` answers on the host socket. Every verb
//! has a `--json` form for agents.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde::Serialize;
use serde_json::{json, Value};

use crate::k3s::K8sConfig;
use crate::vm::{self, assets, client, MachineConfig, MachinePaths, VmmState};

/// Options for `vat machine start`.
#[derive(Debug, Clone, Default)]
pub struct StartArgs {
    pub name: String,
    pub cpus: Option<u32>,
    pub memory_mib: Option<u64>,
    pub disk_gib: Option<u64>,
    pub k8s: Option<bool>,
    pub no_wait: bool,
    pub timeout_s: u64,
    pub json: bool,
}

const ENTITLEMENTS: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict><key>com.apple.security.virtualization</key><true/></dict></plist>
"#;

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

/// Keep `~/.vat/machine/bin/vat-vmm` identical to this binary and signed with
/// the virtualization entitlement (unsigned binaries cannot create VMs).
fn ensure_vmm_binary(paths: &MachinePaths) -> Result<PathBuf> {
    let exe = std::env::current_exe().context("locate the vat binary")?;
    let dest = paths.bin_dir.join("vat-vmm");
    let stamp = paths.bin_dir.join("vat-vmm.sha256");
    let want = assets::sha256_file(&exe)?;
    if dest.is_file() && std::fs::read_to_string(&stamp).ok().as_deref() == Some(want.as_str()) {
        return Ok(dest);
    }
    std::fs::create_dir_all(&paths.bin_dir)?;
    let ent = paths.bin_dir.join("vat-vmm.entitlements");
    std::fs::write(&ent, ENTITLEMENTS)?;
    let tmp = paths.bin_dir.join("vat-vmm.part");
    std::fs::copy(&exe, &tmp).with_context(|| format!("copy {}", exe.display()))?;
    let out = Command::new("codesign")
        .args(["-s", "-", "-f", "--entitlements"])
        .arg(&ent)
        .arg(&tmp)
        .output()
        .context("run codesign")?;
    if !out.status.success() {
        bail!(
            "codesign failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    std::fs::rename(&tmp, &dest)?;
    std::fs::write(&stamp, &want)?;
    Ok(dest)
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

    // Serialize starters: preparing can take minutes (downloads), and a second
    // starter must see the first one's VMM instead of racing it for the disk.
    std::fs::create_dir_all(&paths.dir)?;
    let _boot_lock = lock_boot(&paths)?;
    if let Some(pid) = running_pid(&paths) {
        if changed && !first_create {
            bail!(
                "machine {} is running; stop it before changing its configuration",
                args.name
            );
        }
        let ready = client::docker_ping(&paths.docker_sock);
        return Ok(Booted {
            paths,
            cfg,
            pid,
            timings: json!({ "already_running": true, "docker_ready": ready }),
        });
    }

    cfg.save(&paths.config)?;
    let boot = assets::ensure_boot_assets(&paths)?;
    assets::ensure_data_disk(&paths, &cfg)?;
    assets::write_guest_files(&paths, &cfg)?;
    let vmm = ensure_vmm_binary(&paths)?;
    let prepare_ms = t0.elapsed().as_millis() as u64;

    let _ = std::fs::remove_file(&paths.vmm_state);
    let log = std::fs::File::create(&paths.vmm_log)?;
    let mut cmd = Command::new(&vmm);
    cmd.args(["machine", "__vmm", "--name", &args.name, "--kernel"])
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

    if args.no_wait {
        return Ok(Booted {
            paths,
            cfg,
            pid,
            timings: json!({ "prepare_ms": prepare_ms }),
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
        }),
        paths,
        cfg,
        pid,
    })
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

pub fn stop(name: &str, json_out: bool) -> Result<ExitCode> {
    let paths = MachinePaths::new(name)?;
    let Some(pid) = running_pid(&paths) else {
        return emit(
            json_out,
            json!({ "machine": name, "state": "stopped", "was_running": false }),
            "machine is not running",
        );
    };
    let t0 = Instant::now();
    unsafe { libc::kill(pid as i32, libc::SIGTERM) };
    let deadline = t0 + Duration::from_secs(40);
    while vm::pid_alive(pid) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
    let forced = vm::pid_alive(pid);
    if forced {
        unsafe { libc::kill(pid as i32, libc::SIGKILL) };
    }
    emit(
        json_out,
        json!({
            "machine": name,
            "state": "stopped",
            "was_running": true,
            "forced": forced,
            "stop_ms": t0.elapsed().as_millis() as u64,
        }),
        "machine stopped",
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
    vmm_rss_kib: Option<u64>,
    /// The Virtualization.framework process that holds guest memory: its
    /// physical footprint (what macOS counts) and CPU time.
    vm_footprint_kib: Option<u64>,
    vm_cpu_ms: Option<u64>,
    /// The VM process and guest clock sync, as the VMM last recorded them.
    elastic: Option<Value>,
    guest: Option<Value>,
    ports: Option<Value>,
    data_disk_allocated_kib: Option<u64>,
}

pub fn status(name: &str, json_out: bool) -> Result<ExitCode> {
    let paths = MachinePaths::new(name)?;
    let pid = running_pid(&paths);
    let docker_ready = pid.is_some() && client::docker_ping(&paths.docker_sock);
    let disk_kib = std::fs::metadata(&paths.data_img).ok().map(|m| {
        use std::os::unix::fs::MetadataExt;
        m.blocks() / 2
    });
    let elastic = pid.and_then(|_| read_json(&paths.elastic_state()));
    let vm_usage = elastic
        .as_ref()
        .and_then(|e| e["vm_pid"].as_u64())
        .and_then(|p| crate::vm::elastic::proc_usage(p as u32));
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
        config: MachineConfig::load(&paths.config)?,
        vmm: vmm_state(&paths),
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
        println!("machine  {} ({})", st.machine, st.state);
        if let Some(pid) = st.pid {
            println!("pid      {pid}");
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
    if running_pid(&paths).is_some() {
        stop(name, false)?;
    }
    if paths.dir.exists() {
        std::fs::remove_dir_all(&paths.dir)?;
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
// CODEGEN-END
