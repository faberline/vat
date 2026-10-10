// CODEGEN-BEGIN
//! vat-docker — layer 2: the shared Linux machine behind `vat machine` and
//! its Docker Engine socket.
//!
//! One lightweight Linux VM per host (named `default`) runs on Apple's
//! Virtualization.framework. The guest is Alpine on a persistent ext4 data
//! disk with a real `dockerd` (and optionally K3s using the Docker runtime),
//! so Docker compatibility comes from Docker itself rather than a re-
//! implementation. The host reaches guest sockets over virtio-vsock:
//!
//! ```text
//! ~/.vat/
//!   run/docker.sock          host Docker Engine socket  -> guest /var/run/docker.sock
//!   run/vat.sock             control socket: "<dial header>\n" then raw bytes
//!   machine/assets/          kernel, Alpine minirootfs, generated initramfs
//!   machine/bin/vat-vmm      signed copy of vat holding the virtualization entitlement
//!   machine/default/
//!     config.json            cpus / memory / disk / MAC / options
//!     data.img               sparse persistent root disk (ext4 inside)
//!     share/                 virtiofs tag "vat": guest scripts + status written by the guest
//!     console.log, vmm.log, vmm.json
//! ```
//!
//! The guest side lives in `guest/` and is copied into the state share on
//! every start, so guest behavior changes without rebuilding images.
//!
//! Layers built on the machine (K3s and local GCP in `vat-k8s`) plug in
//! through [`addon`] rather than this crate depending on them.

pub mod addon;
pub mod assets;
#[cfg(feature = "machine")]
pub mod bridge;
pub mod client;
pub mod elastic;
pub mod engine;
pub mod launchd;
#[cfg(all(target_os = "macos", feature = "machine"))]
pub mod vmm;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// The single machine name vat manages today.
pub const DEFAULT_MACHINE: &str = "default";

/// vsock port of the guest dialer (host -> guest).
pub const GUEST_DIAL_PORT: u32 = 1024;
/// vsock port of the host uplink listener (guest -> host).
pub const HOST_UPLINK_PORT: u32 = 1025;

/// Root for host-global machine state. Unlike per-repo vat state this is
/// per-user: `$VAT_MACHINE_HOME`, else `~/.vat`.
pub fn home() -> Result<PathBuf> {
    if let Some(custom) = std::env::var_os("VAT_MACHINE_HOME") {
        return Ok(PathBuf::from(custom));
    }
    let home = dirs::home_dir().context("cannot resolve the home directory")?;
    Ok(home.join(".vat"))
}

/// Every path a machine uses, resolved once.
#[derive(Debug, Clone)]
pub struct MachinePaths {
    pub name: String,
    pub dir: PathBuf,
    pub assets: PathBuf,
    pub bin_dir: PathBuf,
    pub share: PathBuf,
    pub guest: PathBuf,
    pub config: PathBuf,
    pub data_img: PathBuf,
    pub console_log: PathBuf,
    pub vmm_log: PathBuf,
    pub vmm_state: PathBuf,
    pub run_dir: PathBuf,
    pub docker_sock: PathBuf,
    pub control_sock: PathBuf,
}

impl MachinePaths {
    pub fn new(name: &str) -> Result<Self> {
        let home = home()?;
        let machine_root = home.join("machine");
        let dir = machine_root.join(name);
        let share = dir.join("share");
        // Unix socket paths are limited to ~104 bytes, so sockets live in a
        // short, flat directory rather than under the machine dir.
        let run_dir = if name == DEFAULT_MACHINE {
            home.join("run")
        } else {
            home.join("run").join(name)
        };
        Ok(Self {
            name: name.to_string(),
            assets: machine_root.join("assets"),
            bin_dir: machine_root.join("bin"),
            guest: share.join("guest"),
            config: dir.join("config.json"),
            data_img: dir.join("data.img"),
            console_log: dir.join("console.log"),
            vmm_log: dir.join("vmm.log"),
            vmm_state: dir.join("vmm.json"),
            docker_sock: run_dir.join("docker.sock"),
            control_sock: run_dir.join("vat.sock"),
            share,
            dir,
            run_dir,
        })
    }

    /// Guest-written heartbeat (`vat-guest agent`).
    pub fn guest_status(&self) -> PathBuf {
        self.share.join("status.json")
    }

    /// The VMM's view of host resources ([`elastic::ElasticState`]).
    pub fn elastic_state(&self) -> PathBuf {
        self.dir.join("elastic.json")
    }

    /// Guest-written boot phase (`bootstrap.sh`).
    pub fn guest_boot(&self) -> PathBuf {
        self.share.join("boot.json")
    }
}

/// A host directory exposed to the guest at the same absolute path.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HostMount {
    /// virtiofs directory name inside the `vat-host` share.
    pub name: String,
    /// Host directory.
    pub source: PathBuf,
    /// Guest mount points (bind mounts of the same share directory).
    pub targets: Vec<String>,
}

/// A guest TCP listener whose connections are relayed to a host target.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Uplink {
    /// Service name carried in the uplink header.
    pub service: String,
    /// Guest bind address (127.0.0.1 or a link-local address added to lo).
    pub bind: String,
    pub port: u16,
    /// Host target: `tcp:<host>:<port>` or `unix:<path>`.
    pub target: String,
    /// Prefix the host stream with a PROXY v1 header carrying the guest peer
    /// address (used by servers that need the caller's pod IP).
    #[serde(default)]
    pub proxy_protocol: bool,
}

/// Persisted machine configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MachineConfig {
    pub cpus: u32,
    pub memory_mib: u64,
    pub disk_gib: u64,
    /// Stable MAC so the guest keeps its DHCP lease across restarts.
    #[serde(default)]
    pub mac: Option<String>,
    #[serde(default)]
    pub host_mounts: Vec<HostMount>,
    #[serde(default)]
    pub uplinks: Vec<Uplink>,
    /// Extra `/etc/hosts` lines for the guest.
    #[serde(default)]
    pub extra_hosts: Vec<String>,
    /// Bind address for published container ports on the host.
    #[serde(default = "default_publish_addr")]
    pub publish_addr: String,
    /// Stop the VM after this long with no containers running and no
    /// client connected, giving its memory back to macOS; 0 never stops.
    /// With socket activation the next Docker client boots it again.
    #[serde(default = "default_idle_stop")]
    pub idle_stop_secs: u64,
    /// Settings owned by the layers built on the machine (`k8s`,
    /// `k8s_api_port`, `gcp`), kept as raw JSON so this crate doesn't depend
    /// on them; see [`addon`].
    #[serde(flatten)]
    pub layers: serde_json::Map<String, serde_json::Value>,
}

fn default_publish_addr() -> String {
    "127.0.0.1".to_string()
}

fn default_idle_stop() -> u64 {
    300
}

impl Default for MachineConfig {
    fn default() -> Self {
        let host_cpus = std::thread::available_parallelism()
            .map(|n| n.get() as u32)
            .unwrap_or(4);
        Self {
            cpus: host_cpus.clamp(2, 8),
            memory_mib: 4096,
            disk_gib: 64,
            mac: None,
            host_mounts: default_host_mounts(),
            uplinks: Vec::new(),
            extra_hosts: Vec::new(),
            publish_addr: default_publish_addr(),
            idle_stop_secs: default_idle_stop(),
            layers: Default::default(),
        }
    }
}

/// Host paths shared by default, mirroring where macOS tools put files that
/// end up in `docker run -v` and Testcontainers mounts.
pub fn default_host_mounts() -> Vec<HostMount> {
    vec![
        HostMount {
            name: "Users".into(),
            source: "/Users".into(),
            targets: vec!["/Users".into()],
        },
        HostMount {
            name: "tmp".into(),
            source: "/private/tmp".into(),
            targets: vec!["/private/tmp".into()],
        },
        HostMount {
            name: "folders".into(),
            source: "/private/var/folders".into(),
            targets: vec!["/private/var/folders".into(), "/var/folders".into()],
        },
    ]
}

impl MachineConfig {
    pub fn load(path: &Path) -> Result<Option<Self>> {
        match std::fs::read(path) {
            Ok(bytes) => Ok(Some(
                serde_json::from_slice(&bytes)
                    .with_context(|| format!("parse {}", path.display()))?,
            )),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err).with_context(|| format!("read {}", path.display())),
        }
    }

    /// Configured uplinks plus the ones the installed addons add.
    pub fn effective_uplinks(&self) -> Vec<Uplink> {
        let mut all = self.uplinks.clone();
        for addon in addon::installed() {
            all.extend(addon.uplinks(self));
        }
        all
    }

    /// Extra guest `/etc/hosts` lines, including the addons' names.
    pub fn effective_hosts(&self) -> Vec<String> {
        let mut all = self.extra_hosts.clone();
        for addon in addon::installed() {
            all.extend(addon.hosts(self));
        }
        all
    }

    /// Why an installed addon keeps the machine from stopping when idle.
    pub fn keeps_awake(&self) -> Option<&'static str> {
        addon::installed().iter().find_map(|a| a.keeps_awake(self))
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        write_atomic(path, serde_json::to_vec_pretty(self)?.as_slice())
    }
}

/// What the VMM process records about itself.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VmmState {
    pub pid: u32,
    pub started_at: i64,
    /// Time from process start to `VZVirtualMachine.start` completing.
    pub vm_start_ms: u64,
    pub rosetta: bool,
    pub state: String,
}

pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("rename to {}", path.display()))?;
    Ok(())
}

/// Whether `pid` names a live process.
pub fn pid_alive(pid: u32) -> bool {
    pid > 0 && unsafe { libc::kill(pid as i32, 0) } == 0
}
// CODEGEN-END
