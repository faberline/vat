//! `vat k8s` — the persistent K3s cluster inside the shared machine.
//!
//! `up` enables K3s on the default machine (live when it is already running),
//! waits for the API server, writes the host kubeconfig, and installs the
//! pinned `kubectl`. Cluster state, PVCs, and the kubeconfig survive machine
//! restarts. Every verb has a `--json` form for agents.

use std::process::{Command, ExitCode};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde_json::json;

use crate::commands::machine::{self, StartArgs};
use crate::vm::{assets, client, k8s, MachineConfig, MachinePaths, DEFAULT_MACHINE};

/// Options for `vat k8s up`.
#[derive(Debug, Clone)]
pub struct UpArgs {
    pub timeout_s: u64,
    /// Host port for the API server; kept in the machine config.
    pub api_port: Option<u16>,
    pub json: bool,
}

/// Whether the cluster's node reports `Ready=True`.
fn node_ready(kubectl: &std::path::Path, kubeconfig: &std::path::Path) -> bool {
    Command::new(kubectl)
        .args([
            "get",
            "nodes",
            "--request-timeout=3s",
            "-o",
            r#"jsonpath={.items[*].status.conditions[?(@.type=="Ready")].status}"#,
        ])
        .env("KUBECONFIG", kubeconfig)
        .output()
        .is_ok_and(|o| o.status.success() && String::from_utf8_lossy(&o.stdout).trim() == "True")
}

/// Wait until the VMM reports the API forwarder on the configured port, and
/// fail fast when the port is taken (otherwise kubectl could reach a different
/// cluster on that port).
fn wait_forwarder(paths: &MachinePaths, cfg: &MachineConfig, deadline: Instant) -> Result<()> {
    let want = format!("127.0.0.1:{}", cfg.k8s_api_port);
    loop {
        if let Some(api) = machine::read_json(&paths.dir.join("k8s-api.json")) {
            if api["addr"] == want.as_str() {
                if api["listening"] == true {
                    return Ok(());
                }
                bail!(
                    "cannot forward the K3s API on {want}: {}; pick another port with `vat k8s up --api-port <port>`",
                    api["error"].as_str().unwrap_or("bind failed")
                );
            }
        }
        if Instant::now() > deadline {
            bail!("the machine never started the K3s API forwarder on {want}");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn server(cfg: &MachineConfig) -> String {
    format!("https://127.0.0.1:{}", cfg.k8s_api_port)
}

fn guest_k8s_ready(paths: &MachinePaths) -> bool {
    machine::read_json(&paths.guest_status()).is_some_and(|g| g["k8s_ready"] == true)
}

/// `kubectl get --raw /readyz` through the forwarded port and host kubeconfig.
fn api_ready(kubectl: &std::path::Path, kubeconfig: &std::path::Path) -> bool {
    Command::new(kubectl)
        .args(["get", "--raw", "/readyz", "--request-timeout=3s"])
        .env("KUBECONFIG", kubeconfig)
        .output()
        .is_ok_and(|o| o.status.success())
}

pub fn up(args: UpArgs) -> Result<ExitCode> {
    let t0 = Instant::now();
    let deadline = t0 + Duration::from_secs(args.timeout_s);
    let paths = MachinePaths::new(DEFAULT_MACHINE)?;
    // Fetch kubectl while the machine boots; both can take a while cold.
    let kubectl_fetch = std::thread::spawn(k8s::ensure_kubectl);
    let mut enabled_live = false;
    let cfg = match machine::running_pid(&paths) {
        Some(pid) => {
            let mut cfg =
                MachineConfig::load(&paths.config)?.context("the machine has no config")?;
            let reload = !cfg.k8s || args.api_port.is_some_and(|p| p != cfg.k8s_api_port);
            if let Some(port) = args.api_port {
                cfg.k8s_api_port = port;
            }
            if !cfg.k8s {
                cfg.k8s = true;
                cfg.save(&paths.config)?;
                assets::write_guest_files(&paths, &cfg)?;
                let out = client::exec(
                    &paths,
                    "/mnt/vat/guest/configure.sh / >/dev/null && rc-service k3s start",
                )?;
                if out.exit_code != 0 {
                    bail!("enable K3s in the running machine: {}", out.output.trim());
                }
                enabled_live = true;
            }
            if reload {
                cfg.save(&paths.config)?;
                // The VMM re-reads its config and (re)binds the API forwarder.
                unsafe { libc::kill(pid as i32, libc::SIGHUP) };
            }
            cfg
        }
        None => {
            if let Some(port) = args.api_port {
                let mut cfg = MachineConfig::load(&paths.config)?.unwrap_or_default();
                cfg.k8s_api_port = port;
                std::fs::create_dir_all(&paths.dir)?;
                cfg.save(&paths.config)?;
            }
            machine::boot(&StartArgs {
                name: DEFAULT_MACHINE.to_string(),
                k8s: Some(true),
                timeout_s: args.timeout_s,
                json: args.json,
                ..Default::default()
            })?
            .cfg
        }
    };
    let kubectl = kubectl_fetch
        .join()
        .map_err(|_| anyhow::anyhow!("the kubectl download panicked"))??;
    while !guest_k8s_ready(&paths) {
        if Instant::now() > deadline {
            bail!(
                "timed out after {}s waiting for K3s\n--- k3s.log ---\n{}",
                args.timeout_s,
                client::exec(&paths, "tail -n 30 /var/log/k3s.log")
                    .map(|o| o.output)
                    .unwrap_or_default()
            );
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    wait_forwarder(&paths, &cfg, deadline)?;
    let kubeconfig = k8s::sync_kubeconfig(&paths, &cfg)?;
    while !api_ready(&kubectl, &kubeconfig) {
        if Instant::now() > deadline {
            bail!(
                "K3s is ready in the machine but {} does not answer",
                server(&cfg)
            );
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    // /readyz answers before the kubelet re-reports after a restart; pods
    // only schedule once the node is Ready.
    // On a fresh cluster the node registers a moment after /readyz.
    while !node_ready(&kubectl, &kubeconfig) {
        if Instant::now() > deadline {
            bail!("the K3s node did not become Ready");
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    let report = json!({
        "k8s": "ready",
        "context": k8s::CONTEXT,
        "server": server(&cfg),
        "kubeconfig": kubeconfig,
        "kubectl": kubectl,
        "k3s_version": k8s::k3s_version(),
        "kubernetes_version": k8s::kubernetes_version(),
        "enabled_live": enabled_live,
        "ready_ms": t0.elapsed().as_millis() as u64,
    });
    if args.json {
        crate::commands::print_json(&report, false)?;
    } else {
        println!(
            "k8s ready at {} ({} ms)",
            server(&cfg),
            t0.elapsed().as_millis()
        );
        println!("export KUBECONFIG={}", kubeconfig.display());
        println!("kubectl: {}", kubectl.display());
    }
    Ok(ExitCode::SUCCESS)
}

pub fn status(json_out: bool) -> Result<ExitCode> {
    let paths = MachinePaths::new(DEFAULT_MACHINE)?;
    let cfg = MachineConfig::load(&paths.config)?;
    let running = machine::running_pid(&paths).is_some();
    let enabled = cfg.as_ref().is_some_and(|c| c.k8s);
    let ready = running && enabled && guest_k8s_ready(&paths);
    let kubeconfig = k8s::kubeconfig_path()?;
    let kubectl = k8s::kubectl_path()?;
    let report = json!({
        "enabled": enabled,
        "machine": if running { "running" } else if cfg.is_some() { "stopped" } else { "absent" },
        "ready": ready,
        "context": k8s::CONTEXT,
        "server": cfg.as_ref().map(server),
        "api_forward": running.then(|| machine::read_json(&paths.dir.join("k8s-api.json"))).flatten(),
        "kubeconfig": { "path": kubeconfig, "exists": kubeconfig.is_file() },
        "kubectl": { "path": kubectl, "installed": k8s::kubectl_installed(&kubectl) },
        "k3s_version": k8s::k3s_version(),
        "kubernetes_version": k8s::kubernetes_version(),
    });
    if json_out {
        crate::commands::print_json(&report, false)?;
    } else {
        let state = match (enabled, running, ready) {
            (false, _, _) => "disabled",
            (true, false, _) => "stopped",
            (true, true, false) => "starting",
            (true, true, true) => "ready",
        };
        println!("k8s      {state} (K3s {})", k8s::k3s_version());
        if let Some(server) = report["server"].as_str() {
            println!("server   {server}");
        }
        println!("config   {}", kubeconfig.display());
    }
    Ok(ExitCode::SUCCESS)
}

/// Refresh the host kubeconfig from the running cluster and report it.
pub fn kubeconfig(json_out: bool) -> Result<ExitCode> {
    let paths = MachinePaths::new(DEFAULT_MACHINE)?;
    let cfg = MachineConfig::load(&paths.config)?.unwrap_or_default();
    let path = if machine::running_pid(&paths).is_some() && guest_k8s_ready(&paths) {
        k8s::sync_kubeconfig(&paths, &cfg)?
    } else {
        let path = k8s::kubeconfig_path()?;
        if !path.is_file() {
            bail!("no kubeconfig yet; run `vat k8s up`");
        }
        path
    };
    if json_out {
        crate::commands::print_json(
            &json!({ "path": path, "context": k8s::CONTEXT, "server": server(&cfg) }),
            false,
        )?;
    } else {
        println!("{}", path.display());
    }
    Ok(ExitCode::SUCCESS)
}

/// Run the pinned `kubectl` against the machine cluster.
pub fn kubectl(args: Vec<String>) -> Result<ExitCode> {
    let kubeconfig = k8s::kubeconfig_path()?;
    if !kubeconfig.is_file() {
        bail!("no kubeconfig yet; run `vat k8s up`");
    }
    let kubectl = k8s::ensure_kubectl()?;
    let status = Command::new(kubectl)
        .args(&args)
        .env("KUBECONFIG", &kubeconfig)
        .status()
        .context("run kubectl")?;
    Ok(ExitCode::from(
        status.code().unwrap_or(1).clamp(0, 255) as u8
    ))
}

/// Disable K3s. Cluster state stays on the data disk for the next `up`.
pub fn down(json_out: bool) -> Result<ExitCode> {
    let paths = MachinePaths::new(DEFAULT_MACHINE)?;
    let Some(mut cfg) = MachineConfig::load(&paths.config)? else {
        bail!("no machine; nothing to disable");
    };
    let was_enabled = cfg.k8s;
    cfg.k8s = false;
    cfg.save(&paths.config)?;
    let mut stopped_live = false;
    if let Some(pid) = machine::running_pid(&paths) {
        if was_enabled {
            // Pods run as Docker containers under cri-dockerd; stop them with K3s.
            let out = client::exec(
                &paths,
                "rc-service k3s stop >/dev/null 2>&1; \
                 rm -f /etc/runlevels/default/k3s /mnt/vat/guest/k3s.enabled; \
                 ids=$(docker ps -aq --filter label=io.kubernetes.docker.type); \
                 [ -z \"$ids\" ] || docker rm -f $ids >/dev/null",
            )?;
            if out.exit_code != 0 {
                bail!("stop K3s: {}", out.output.trim());
            }
            unsafe { libc::kill(pid as i32, libc::SIGHUP) };
            stopped_live = true;
        }
    }
    let report =
        json!({ "enabled": false, "was_enabled": was_enabled, "stopped_live": stopped_live });
    if json_out {
        crate::commands::print_json(&report, false)?;
    } else {
        println!("k8s disabled (cluster state kept)");
    }
    Ok(ExitCode::SUCCESS)
}
