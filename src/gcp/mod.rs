//! Local GCP for the shared machine: the GKE-style metadata server with
//! Workload Identity, a local Artifact Registry, and persistent Pub/Sub and
//! Cloud Storage emulators wired into every pod.
//!
//! Everything runs inside the VMM process on the host. The guest agent binds
//! link-local addresses on its loopback and relays each connection over vsock
//! (an uplink with a `builtin:<service>` target), so pods and containers reach
//! the services at the addresses real GKE code expects:
//!
//! ```text
//! 169.254.169.254:80    metadata.google.internal (metadata server, WI)
//! 169.254.169.251:443   <region>-docker.pkg.dev  (Artifact Registry, TLS)
//! 169.254.169.252:8085  Pub/Sub emulator         (PUBSUB_EMULATOR_HOST)
//! 169.254.169.252:9023  Cloud Storage emulator   (STORAGE_EMULATOR_HOST)
//! 169.254.169.253:443   admission webhook that injects the two env vars
//! ```
//!
//! The same Pub/Sub and Storage state is also served on host loopback ports,
//! so host processes and pods share topics and buckets.

#[cfg(feature = "gcp")]
pub mod ca;
#[cfg(feature = "gcp")]
pub mod conn;
#[cfg(feature = "gcp")]
pub mod metadata;
#[cfg(feature = "gcp")]
pub mod services;
#[cfg(feature = "gcp")]
pub mod webhook;

use serde::{Deserialize, Serialize};

use crate::vm::Uplink;

pub const METADATA_IP: &str = "169.254.169.254";
pub const REGISTRY_IP: &str = "169.254.169.251";
pub const EMULATOR_IP: &str = "169.254.169.252";
pub const WEBHOOK_IP: &str = "169.254.169.253";
pub const PUBSUB_PORT: u16 = 8085;
pub const STORAGE_PORT: u16 = 9023;

/// GKE annotation naming the Google service account a KSA impersonates.
pub const WI_ANNOTATION: &str = "iam.gke.io/gcp-service-account";
/// Namespace label (`disabled`) or pod annotation (`"false"`) that opts out of
/// emulator env injection.
pub const OPT_OUT: &str = "vat.dev/gcp-emulators";

/// Built-in uplink service names.
pub const SVC_METADATA: &str = "gcp-metadata";
pub const SVC_REGISTRY: &str = "gcp-registry";
pub const SVC_PUBSUB: &str = "gcp-pubsub";
pub const SVC_STORAGE: &str = "gcp-storage";
pub const SVC_WEBHOOK: &str = "gcp-webhook";

/// The machine's local GCP identity and host ports.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GcpConfig {
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default = "default_project")]
    pub project: String,
    #[serde(default = "default_zone")]
    pub zone: String,
    /// Host loopback port for the shared Pub/Sub emulator (0 = guest only).
    #[serde(default = "default_host_pubsub")]
    pub host_pubsub_port: u16,
    /// Host loopback port for the shared Storage emulator (0 = guest only).
    #[serde(default = "default_host_storage")]
    pub host_storage_port: u16,
}

fn yes() -> bool {
    true
}
fn default_project() -> String {
    "vat-local".into()
}
fn default_zone() -> String {
    "us-central1-a".into()
}
fn default_host_pubsub() -> u16 {
    18085
}
fn default_host_storage() -> u16 {
    19023
}

impl Default for GcpConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            project: default_project(),
            zone: default_zone(),
            host_pubsub_port: default_host_pubsub(),
            host_storage_port: default_host_storage(),
        }
    }
}

impl GcpConfig {
    /// `us-central1-a` -> `us-central1`.
    pub fn region(&self) -> &str {
        match self.zone.rsplit_once('-') {
            Some((region, suffix)) if suffix.len() == 1 => region,
            _ => &self.zone,
        }
    }

    /// The Artifact Registry Docker host served locally.
    pub fn registry_host(&self) -> String {
        format!("{}-docker.pkg.dev", self.region())
    }

    /// A stable 12-digit project number derived from the project id.
    pub fn project_number(&self) -> u64 {
        let digest = blake3::hash(self.project.as_bytes());
        let n = u64::from_le_bytes(digest.as_bytes()[..8].try_into().unwrap());
        100_000_000_000 + n % 900_000_000_000
    }

    /// The node (Compute Engine default) service account, used for callers
    /// that are not pods.
    pub fn node_service_account(&self) -> String {
        format!(
            "{}-compute@developer.gserviceaccount.com",
            self.project_number()
        )
    }

    /// The Workload Identity pool every KSA belongs to.
    pub fn workload_pool(&self) -> String {
        format!("{}.svc.id.goog", self.project)
    }

    /// Guest listeners relayed to the VMM's built-in services.
    pub fn uplinks(&self) -> Vec<Uplink> {
        if !self.enabled {
            return Vec::new();
        }
        let up = |service: &str, bind: &str, port: u16| Uplink {
            service: service.into(),
            bind: bind.into(),
            port,
            target: format!("builtin:{service}"),
            proxy_protocol: false,
        };
        vec![
            up(SVC_METADATA, METADATA_IP, 80),
            up(SVC_REGISTRY, REGISTRY_IP, 443),
            up(SVC_PUBSUB, EMULATOR_IP, PUBSUB_PORT),
            up(SVC_STORAGE, EMULATOR_IP, STORAGE_PORT),
            up(SVC_WEBHOOK, WEBHOOK_IP, 443),
        ]
    }

    /// Guest `/etc/hosts` lines (dockerd resolves the registry through them).
    pub fn hosts(&self) -> Vec<String> {
        if !self.enabled {
            return Vec::new();
        }
        vec![
            format!("{METADATA_IP} metadata.google.internal metadata"),
            format!("{REGISTRY_IP} {}", self.registry_host()),
        ]
    }

    /// Env vars injected into pods (and printed for guest containers).
    pub fn pod_env(&self) -> Vec<(&'static str, String)> {
        vec![
            (
                "PUBSUB_EMULATOR_HOST",
                format!("{EMULATOR_IP}:{PUBSUB_PORT}"),
            ),
            (
                "STORAGE_EMULATOR_HOST",
                format!("http://{EMULATOR_IP}:{STORAGE_PORT}"),
            ),
        ]
    }

    /// Env vars for host processes sharing the same emulator state.
    pub fn host_env(&self) -> Vec<(&'static str, String)> {
        let mut env = vec![
            ("GOOGLE_CLOUD_PROJECT", self.project.clone()),
            ("CLOUDSDK_CORE_PROJECT", self.project.clone()),
        ];
        if self.host_pubsub_port != 0 {
            env.push((
                "PUBSUB_EMULATOR_HOST",
                format!("127.0.0.1:{}", self.host_pubsub_port),
            ));
        }
        if self.host_storage_port != 0 {
            env.push((
                "STORAGE_EMULATOR_HOST",
                format!("http://127.0.0.1:{}", self.host_storage_port),
            ));
        }
        env
    }

    #[cfg(feature = "gcp")]
    /// K3s auto-deploy manifest: CoreDNS resolves `metadata.google.internal`
    /// for pods, and the webhook injects the emulator env vars.
    pub fn k3s_manifest(&self, ca_pem: &str) -> String {
        use base64::Engine as _;
        let ca = base64::engine::general_purpose::STANDARD.encode(ca_pem);
        format!(
            r#"# Written by vat on every boot; edits are overwritten.
apiVersion: v1
kind: ConfigMap
metadata:
  name: coredns-custom
  namespace: kube-system
data:
  vat-gcp.server: |
    metadata.google.internal:53 {{
        hosts {{
            {METADATA_IP} metadata.google.internal
        }}
    }}
---
apiVersion: admissionregistration.k8s.io/v1
kind: MutatingWebhookConfiguration
metadata:
  name: vat-gcp-emulators
webhooks:
- name: gcp-emulators.vat.dev
  admissionReviewVersions: [v1]
  sideEffects: None
  failurePolicy: Ignore
  timeoutSeconds: 5
  clientConfig:
    url: https://{WEBHOOK_IP}/mutate
    caBundle: {ca}
  rules:
  - operations: [CREATE]
    apiGroups: [""]
    apiVersions: [v1]
    resources: [pods]
  namespaceSelector:
    matchExpressions:
    - {{key: kubernetes.io/metadata.name, operator: NotIn, values: [kube-system, kube-public, kube-node-lease]}}
    - {{key: {OPT_OUT}, operator: NotIn, values: [disabled]}}
"#
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn region_and_registry_host_follow_the_zone() {
        let mut cfg = GcpConfig::default();
        assert_eq!(cfg.region(), "us-central1");
        assert_eq!(cfg.registry_host(), "us-central1-docker.pkg.dev");
        cfg.zone = "europe-west4-b".into();
        assert_eq!(cfg.registry_host(), "europe-west4-docker.pkg.dev");
        cfg.zone = "us-east1".into();
        assert_eq!(cfg.region(), "us-east1");
    }

    #[test]
    fn project_number_is_stable_and_twelve_digits() {
        let cfg = GcpConfig::default();
        let n = cfg.project_number();
        assert_eq!(n, cfg.project_number());
        assert_eq!(n.to_string().len(), 12);
        assert!(cfg.node_service_account().starts_with(&n.to_string()));
    }

    #[test]
    fn disabled_config_adds_no_uplinks_or_hosts() {
        let cfg = GcpConfig {
            enabled: false,
            ..Default::default()
        };
        assert!(cfg.uplinks().is_empty());
        assert!(cfg.hosts().is_empty());
        let on = GcpConfig::default();
        assert_eq!(on.uplinks().len(), 5);
        assert!(on
            .uplinks()
            .iter()
            .all(|u| u.target == format!("builtin:{}", u.service)));
    }

    #[cfg(feature = "gcp")]
    #[test]
    fn manifest_embeds_the_ca_and_webhook_url() {
        let m = GcpConfig::default().k3s_manifest("PEM");
        assert!(m.contains("caBundle: UEVN"));
        assert!(m.contains("url: https://169.254.169.253/mutate"));
        assert!(m.contains("169.254.169.254 metadata.google.internal"));
        // Every document parses as YAML.
        for doc in m.split("\n---\n") {
            serde_yaml::from_str::<serde_yaml::Value>(doc).expect("yaml");
        }
    }
}
