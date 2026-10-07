//! Starts the local GCP services inside the VMM and hands back one inbox per
//! built-in uplink service.

use std::collections::HashMap;
use std::future::Future;
use std::net::IpAddr;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;

use anyhow::{Context, Result};
use serde_json::{json, Value};

use super::ca::Ca;
use super::conn::{self, Inbox, Peer};
use super::metadata::{self, Metadata, PodIdentity, Resolver};
use super::{webhook, GcpConfig};
use super::{
    REGISTRY_IP, SVC_METADATA, SVC_PUBSUB, SVC_REGISTRY, SVC_STORAGE, SVC_WEBHOOK, WEBHOOK_IP,
};

/// Runs `sh -c <script>` in the guest; returns (exit code, combined output).
pub type GuestExec = Arc<
    dyn Fn(String) -> Pin<Box<dyn Future<Output = Result<(i32, String)>> + Send>> + Send + Sync,
>;

pub struct Running {
    /// Built-in uplink service name -> where its connections go.
    pub inboxes: HashMap<String, Inbox>,
    /// What `vat gcp status` reports.
    pub report: Value,
}

/// The CA directory for a machine.
pub fn ca_dir(machine_dir: &Path) -> std::path::PathBuf {
    machine_dir.join("gcp")
}

/// Ask the guest's K3s which pod owns `ip`, and its KSA's GSA annotation.
fn resolver(exec: GuestExec) -> Resolver {
    Arc::new(move |ip: IpAddr| {
        let exec = exec.clone();
        Box::pin(async move {
            let script = format!(
                r#"k=/usr/local/bin/k3s
[ -x $k ] || exit 3
line=$($k kubectl get pods -A --field-selector=status.podIP={ip} -o jsonpath='{{range .items[*]}}{{.metadata.namespace}}|{{.metadata.name}}|{{.spec.serviceAccountName}}|{{.spec.hostNetwork}}{{"\n"}}{{end}}' 2>/dev/null | grep -v '|true$' | head -n1)
[ -n "$line" ] || exit 4
ns=${{line%%|*}}; rest=${{line#*|}}; sa=$(echo "$rest" | cut -d'|' -f2)
gsa=$($k kubectl get sa -n "$ns" "${{sa:-default}}" -o jsonpath='{{.metadata.annotations.iam\.gke\.io/gcp-service-account}}' 2>/dev/null)
echo "$line|$gsa""#
            );
            match exec(script).await {
                Ok((0, out)) => parse_pod(&out),
                Ok(_) => None,
                Err(err) => {
                    eprintln!("gcp: resolve {ip}: {err:#}");
                    None
                }
            }
        }) as metadata::ResolveFuture
    })
}

/// `ns|pod|ksa|hostNetwork|gsa`
fn parse_pod(out: &str) -> Option<PodIdentity> {
    let line = out.lines().rfind(|l| l.contains('|'))?;
    let f: Vec<&str> = line.split('|').collect();
    if f.len() < 5 || f[0].is_empty() {
        return None;
    }
    Some(PodIdentity {
        namespace: f[0].to_string(),
        pod: f[1].to_string(),
        ksa: if f[2].is_empty() { "default" } else { f[2] }.to_string(),
        gsa: Some(f[4].trim().to_string()).filter(|g| !g.is_empty()),
    })
}

async fn host_listener(port: u16, inbox: Inbox) -> Value {
    if port == 0 {
        return Value::Null;
    }
    let addr = format!("127.0.0.1:{port}");
    match tokio::net::TcpListener::bind(&addr).await {
        Ok(listener) => {
            tokio::spawn(conn::accept_tcp(listener, inbox));
            json!({ "addr": addr, "listening": true })
        }
        Err(err) => json!({ "addr": addr, "listening": false, "error": err.to_string() }),
    }
}

pub async fn start(machine_dir: &Path, cfg: &GcpConfig, exec: GuestExec) -> Result<Running> {
    let ca = Ca::ensure(&ca_dir(machine_dir))?;
    let mut inboxes = HashMap::new();

    let md = Metadata::new(cfg.clone(), resolver(exec));
    let (tx, listener) = conn::plain(256);
    tokio::spawn(async move {
        let app = metadata::router(md).into_make_service_with_connect_info::<Peer>();
        let _ = axum::serve(listener, app).await;
    });
    inboxes.insert(SVC_METADATA.to_string(), tx);

    let registry_root = machine_dir.join("registry");
    let app = crate::registry::router(crate::registry::Config {
        root: registry_root.clone(),
        auth: crate::registry::Auth::None,
    })
    .context("open the local Artifact Registry")?;
    let tls = ca.server_config(&[cfg.registry_host()], &[REGISTRY_IP])?;
    let (tx, listener) = conn::tls(64, tls);
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    inboxes.insert(SVC_REGISTRY.to_string(), tx);

    let (tx, listener) = conn::plain(256);
    tokio::spawn(async move {
        let _ = axum::serve(listener, crate::emulator::storage::router()).await;
    });
    let host_storage = host_listener(cfg.host_storage_port, tx.clone()).await;
    inboxes.insert(SVC_STORAGE.to_string(), tx);

    let (tx, incoming) = conn::incoming(256);
    tokio::spawn(async move {
        if let Err(err) = crate::emulator::pubsub::serve_incoming(incoming).await {
            eprintln!("gcp: pubsub: {err:#}");
        }
    });
    let host_pubsub = host_listener(cfg.host_pubsub_port, tx.clone()).await;
    inboxes.insert(SVC_PUBSUB.to_string(), tx);

    let tls = ca.server_config(&[], &[WEBHOOK_IP])?;
    let (tx, listener) = conn::tls(64, tls);
    let app = webhook::router(cfg.clone());
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    inboxes.insert(SVC_WEBHOOK.to_string(), tx);

    let report = json!({
        "project": cfg.project,
        "project_number": cfg.project_number(),
        "zone": cfg.zone,
        "registry": { "host": cfg.registry_host(), "root": registry_root },
        "ca": ca_dir(machine_dir).join("ca.pem"),
        "host": { "pubsub": host_pubsub, "storage": host_storage },
    });
    Ok(Running { inboxes, report })
}

#[cfg(test)]
mod tests {
    use super::parse_pod;

    #[test]
    fn parses_resolver_output() {
        let pod = parse_pod("app|web-1|web||web@p.iam.gserviceaccount.com\n").unwrap();
        assert_eq!(pod.namespace, "app");
        assert_eq!(pod.pod, "web-1");
        assert_eq!(pod.ksa, "web");
        assert_eq!(pod.gsa.as_deref(), Some("web@p.iam.gserviceaccount.com"));
        let plain = parse_pod("app|p||false|\n").unwrap();
        assert_eq!(plain.ksa, "default");
        assert_eq!(plain.gsa, None);
        assert!(parse_pod("").is_none());
    }
}
