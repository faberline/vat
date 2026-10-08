//! The machine addon for this layer: stages K3s and the local GCP guest
//! files, forwards the K3s API to the host, and serves the GCP builtins from
//! the VMM process.

use anyhow::Result;
use vat_docker::addon::{HostForward, MachineAddon};
use vat_docker::{MachineConfig, MachinePaths, Uplink};

use crate::k3s::{self, K8sConfig};

/// The addons this crate contributes, for `vat_docker::addon::install`.
pub fn addons() -> Vec<Box<dyn MachineAddon>> {
    vec![Box::new(K8s)]
}

/// K3s plus local GCP on the shared machine.
pub struct K8s;

/// GCP settings when the `gcp` feature is built and the machine enables it.
#[cfg(feature = "gcp")]
fn gcp_enabled(cfg: &MachineConfig) -> Option<crate::gcp::GcpConfig> {
    crate::gcp::GcpConfig::of(cfg)
        .ok()
        .filter(|gcp| gcp.enabled)
}

impl MachineAddon for K8s {
    fn uplinks(&self, cfg: &MachineConfig) -> Vec<Uplink> {
        #[cfg(feature = "gcp")]
        if let Some(gcp) = gcp_enabled(cfg) {
            return gcp.uplinks();
        }
        let _ = cfg;
        Vec::new()
    }

    fn hosts(&self, cfg: &MachineConfig) -> Vec<String> {
        #[cfg(feature = "gcp")]
        if let Some(gcp) = gcp_enabled(cfg) {
            return gcp.hosts();
        }
        let _ = cfg;
        Vec::new()
    }

    fn stage(&self, paths: &MachinePaths, cfg: &MachineConfig) -> Result<()> {
        stage_gcp(paths, cfg)?;
        let k3s_flag = paths.guest.join("k3s.enabled");
        if K8sConfig::of(cfg)?.enabled {
            k3s::stage_k3s(paths)?;
            vat_docker::write_atomic(&k3s_flag, b"1")?;
        } else if k3s_flag.exists() {
            std::fs::remove_file(&k3s_flag)?;
        }
        Ok(())
    }

    fn forwards(&self, cfg: &MachineConfig) -> Vec<HostForward> {
        let k8s = K8sConfig::of(cfg).unwrap_or_default();
        vec![HostForward {
            name: "the K3s API",
            record: "k8s-api.json",
            ports: k8s.enabled.then_some((k8s.api_port, k3s::GUEST_API_PORT)),
        }]
    }

    #[cfg(feature = "gcp")]
    fn builtins<'a>(
        &'a self,
        paths: &'a MachinePaths,
        cfg: &'a MachineConfig,
        exec: vat_docker::addon::GuestExec,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = vat_docker::addon::Builtins> + Send + 'a>>
    {
        Box::pin(start_gcp(paths, cfg, exec))
    }
}

/// Guest side of the local GCP services: the CA (trusted by dockerd for the
/// Artifact Registry host) and the K3s manifest for CoreDNS and the webhook.
fn stage_gcp(paths: &MachinePaths, cfg: &MachineConfig) -> Result<()> {
    let ca = paths.guest.join("ca.pem");
    let registries = paths.guest.join("registry.hosts");
    let manifest = paths.guest.join("k3s-vat-gcp.yaml");
    #[cfg(feature = "gcp")]
    if let Some(gcp) = gcp_enabled(cfg) {
        use crate::gcp::{ca::Ca, services};
        use vat_docker::write_atomic;
        let authority = Ca::ensure(&services::ca_dir(&paths.dir))?;
        write_atomic(&ca, authority.pem().as_bytes())?;
        write_atomic(&registries, format!("{}\n", gcp.registry_host()).as_bytes())?;
        write_atomic(&manifest, gcp.k3s_manifest(authority.pem()).as_bytes())?;
        return Ok(());
    }
    let _ = cfg;
    for stale in [ca, registries, manifest] {
        let _ = std::fs::remove_file(stale);
    }
    Ok(())
}

/// Start the local GCP services in the VMM and record their endpoints in
/// `gcp.json`.
#[cfg(feature = "gcp")]
async fn start_gcp(
    paths: &MachinePaths,
    cfg: &MachineConfig,
    exec: vat_docker::addon::GuestExec,
) -> vat_docker::addon::Builtins {
    use std::sync::Arc;

    use crate::gcp::{conn, services};
    use vat_docker::{bridge, write_atomic};

    let state = paths.dir.join("gcp.json");
    let mut builtins = vat_docker::addon::Builtins::new();
    let Some(gcp) = gcp_enabled(cfg) else {
        let _ = std::fs::remove_file(&state);
        return builtins;
    };
    match services::start(&paths.dir, &gcp, exec).await {
        Ok(running) => {
            for (name, inbox) in running.inboxes {
                let serve: bridge::Builtin = Arc::new(move |stream, peer: String| {
                    let inbox = inbox.clone();
                    tokio::spawn(async move {
                        let _ = inbox
                            .send(conn::Conn::new(stream, conn::parse_peer(&peer)))
                            .await;
                    });
                });
                builtins.insert(name, serve);
            }
            let mut report = running.report;
            report["running"] = serde_json::json!(true);
            let _ = write_atomic(&state, report.to_string().as_bytes());
        }
        Err(err) => {
            eprintln!("vmm: local GCP services: {err:#}");
            let report = serde_json::json!({ "running": false, "error": format!("{err:#}") });
            let _ = write_atomic(&state, report.to_string().as_bytes());
        }
    }
    builtins
}
