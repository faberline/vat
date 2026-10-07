//! Persistent K3s inside the shared machine.
//!
//! K3s runs as an OpenRC service in the guest with `--docker`, so images built
//! through the Docker Engine socket are what the kubelet sees (no load step),
//! and its state lives on the persistent root disk. The host side here stages
//! the pinned K3s binary, renders the server flags, writes a host kubeconfig
//! pointing at the forwarded API port, and vends a matching `kubectl`.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use super::{assets, client, home, write_atomic, MachineConfig, MachinePaths};

/// Pinned guest versions, shared with the guest scripts.
pub const VERSIONS_ENV: &str = include_str!("guest/versions.env");

/// `kubectl` matching the K3s minor version, for darwin/arm64.
const KUBECTL_SHA256: &str = "c5850a4a6b9469b26cce47c1ab456285f58e308283dcdf72b4ddda3db49b0ec0";

/// Guest path of the API server port (fixed; the host side is configurable).
pub const GUEST_API_PORT: u16 = 6443;

/// Kubeconfig context, cluster, and user name vat writes.
pub const CONTEXT: &str = "vat";

fn version_value(key: &str) -> &'static str {
    VERSIONS_ENV
        .lines()
        .find_map(|l| l.strip_prefix(key)?.strip_prefix('='))
        .unwrap_or_default()
}

/// The pinned K3s release, e.g. `v1.36.5+k3s1`.
pub fn k3s_version() -> &'static str {
    version_value("K3S_VERSION")
}

/// The Kubernetes version K3s embeds, e.g. `v1.36.5`.
pub fn kubernetes_version() -> &'static str {
    k3s_version().split('+').next().unwrap_or_default()
}

/// Host kubeconfig for the machine cluster.
pub fn kubeconfig_path() -> Result<PathBuf> {
    Ok(home()?.join("kube").join("config"))
}

/// The `kubectl` vat vends.
pub fn kubectl_path() -> Result<PathBuf> {
    Ok(home()?.join("bin").join("kubectl"))
}

/// Server flags. Traefik is off (GKE has no default ingress controller);
/// ServiceLB stays so `type: LoadBalancer` gets an address.
pub fn server_args() -> String {
    [
        "--docker",
        "--disable=traefik",
        "--node-name=vat",
        "--tls-san=127.0.0.1",
        "--tls-san=localhost",
        "--write-kubeconfig-mode=0644",
    ]
    .join(" ")
}

/// Download (once, pinned by SHA-256) the K3s binary on the host and place it
/// in the state share with its server flags, where `configure.sh` installs it.
pub fn stage_k3s(paths: &MachinePaths) -> Result<()> {
    let version = k3s_version();
    let cached = paths.assets.join(format!("k3s-{version}"));
    let sha = version_value("K3S_SHA256");
    if !(cached.is_file() && assets::sha256_file(&cached)? == sha) {
        let url = format!(
            "https://github.com/k3s-io/k3s/releases/download/{}/k3s-arm64",
            version.replace('+', "%2B")
        );
        assets::download(&url, &cached, sha).context("download K3s")?;
    }
    let staged = paths.guest.join("k3s");
    let same = std::fs::metadata(&staged)
        .ok()
        .zip(std::fs::metadata(&cached).ok())
        .is_some_and(|(a, b)| a.len() == b.len());
    if !same {
        let _ = std::fs::remove_file(&staged);
        if std::fs::hard_link(&cached, &staged).is_err() {
            std::fs::copy(&cached, &staged)?;
        }
    }
    write_atomic(&paths.guest.join("k3s.args"), server_args().as_bytes())?;
    Ok(())
}

/// Rewrite the guest's `k3s.yaml` for the host: the forwarded API port and
/// `vat` as the cluster, user, and context name.
pub fn host_kubeconfig(guest_yaml: &str, api_port: u16) -> Result<String> {
    if !guest_yaml.contains("certificate-authority-data") {
        bail!("the guest kubeconfig is incomplete");
    }
    let server = format!("https://127.0.0.1:{api_port}");
    let out: Vec<String> = guest_yaml
        .lines()
        .map(|line| {
            let trimmed = line.trim_start();
            let indent = &line[..line.len() - trimmed.len()];
            if trimmed.starts_with("server: ") {
                format!("{indent}server: {server}")
            } else if let Some(key) = [
                "- name: ",
                "name: ",
                "cluster: ",
                "user: ",
                "current-context: ",
            ]
            .iter()
            .find(|k| trimmed == format!("{k}default"))
            {
                format!("{indent}{key}{CONTEXT}")
            } else {
                line.to_string()
            }
        })
        .collect();
    Ok(out.join("\n") + "\n")
}

/// Namespace a `cluster = "machine"` service gets for one run: the run id
/// (`vat-<stamp>`) plus the service id, as a DNS-1123 label.
pub fn run_namespace(run_id: &str, service_id: &str) -> String {
    let raw: String = format!("{run_id}-{service_id}")
        .chars()
        .map(|c| {
            let c = c.to_ascii_lowercase();
            if c.is_ascii_alphanumeric() {
                c
            } else {
                '-'
            }
        })
        .collect();
    let mut name = raw.trim_matches('-').to_string();
    name.truncate(63);
    let name = name.trim_end_matches('-').to_string();
    if name.is_empty() {
        "vat-run".to_string()
    } else {
        name
    }
}

/// Manifest for a run namespace, labelled so stray ones can be found with
/// `kubectl get ns -l app.kubernetes.io/managed-by=vat`.
pub fn namespace_manifest(namespace: &str, run_id: &str) -> String {
    format!(
        "apiVersion: v1\nkind: Namespace\nmetadata:\n  name: {namespace}\n  labels:\n    \
         app.kubernetes.io/managed-by: vat\n    vat.dev/run: {run_id}\n"
    )
}

/// Copy of the host kubeconfig whose `vat` context selects `namespace`.
pub fn namespaced_kubeconfig(host_yaml: &str, namespace: &str) -> Result<String> {
    let mut doc: serde_yaml::Value =
        serde_yaml::from_str(host_yaml).context("parse the host kubeconfig")?;
    let context = doc
        .get_mut("contexts")
        .and_then(|c| c.as_sequence_mut())
        .and_then(|contexts| {
            contexts
                .iter_mut()
                .find(|c| c.get("name").and_then(|n| n.as_str()) == Some(CONTEXT))
        })
        .and_then(|c| c.get_mut("context"))
        .and_then(|c| c.as_mapping_mut())
        .with_context(|| format!("the host kubeconfig has no `{CONTEXT}` context"))?;
    context.insert("namespace".into(), namespace.into());
    if let Some(map) = doc.as_mapping_mut() {
        map.insert("current-context".into(), CONTEXT.into());
    }
    serde_yaml::to_string(&doc).context("render the run kubeconfig")
}

/// Fetch the kubeconfig from the running guest and write the host copy.
pub fn sync_kubeconfig(paths: &MachinePaths, cfg: &MachineConfig) -> Result<PathBuf> {
    let out = client::exec(paths, "cat /etc/rancher/k3s/k3s.yaml")?;
    if out.exit_code != 0 {
        bail!("read the guest kubeconfig: {}", out.output.trim());
    }
    let yaml = host_kubeconfig(&out.output, cfg.k8s_api_port)?;
    let path = kubeconfig_path()?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    write_atomic(&path, yaml.as_bytes())?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    Ok(path)
}

/// Install the pinned `kubectl` (darwin/arm64) if it is missing or stale.
pub fn ensure_kubectl() -> Result<PathBuf> {
    let path = kubectl_path()?;
    if path.is_file() && assets::sha256_file(&path)? == KUBECTL_SHA256 {
        return Ok(path);
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let url = format!(
        "https://dl.k8s.io/release/{}/bin/darwin/arm64/kubectl",
        kubernetes_version()
    );
    assets::download(&url, &path, KUBECTL_SHA256).context("download kubectl")?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;
    Ok(path)
}

/// Whether `path` holds the pinned kubectl.
pub fn kubectl_installed(path: &Path) -> bool {
    path.is_file() && assets::sha256_file(path).is_ok_and(|h| h == KUBECTL_SHA256)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_are_pinned() {
        assert!(k3s_version().starts_with('v'));
        assert!(k3s_version().contains("+k3s"));
        assert_eq!(
            kubernetes_version(),
            k3s_version().split('+').next().unwrap()
        );
    }

    #[test]
    fn host_kubeconfig_points_at_forwarded_port_and_renames_default() {
        let guest = "apiVersion: v1\nclusters:\n- cluster:\n    certificate-authority-data: AAA\n    server: https://127.0.0.1:6443\n  name: default\ncontexts:\n- context:\n    cluster: default\n    user: default\n  name: default\ncurrent-context: default\nkind: Config\nusers:\n- name: default\n  user:\n    client-certificate-data: BBB\n";
        let host = host_kubeconfig(guest, 16443).unwrap();
        assert!(host.contains("    server: https://127.0.0.1:16443\n"));
        assert!(host.contains("  name: vat\n"));
        assert!(host.contains("    cluster: vat\n"));
        assert!(host.contains("    user: vat\n"));
        assert!(host.contains("current-context: vat\n"));
        assert!(host.contains("- name: vat\n"));
        assert!(!host.contains("default"));
        assert!(host.contains("client-certificate-data: BBB"));
    }

    const GUEST: &str = "apiVersion: v1\nclusters:\n- cluster:\n    certificate-authority-data: AAA\n    server: https://127.0.0.1:6443\n  name: default\ncontexts:\n- context:\n    cluster: default\n    user: default\n  name: default\ncurrent-context: default\nkind: Config\nusers:\n- name: default\n  user:\n    client-certificate-data: BBB\n";

    #[test]
    fn namespaced_kubeconfig_sets_the_vat_context_namespace() {
        let host = host_kubeconfig(GUEST, 16443).unwrap();
        let run = namespaced_kubeconfig(&host, "vat-7f3k1q9-k8s").unwrap();
        let doc: serde_yaml::Value = serde_yaml::from_str(&run).unwrap();
        let ctx = &doc["contexts"][0];
        assert_eq!(ctx["name"].as_str(), Some("vat"));
        assert_eq!(
            ctx["context"]["namespace"].as_str(),
            Some("vat-7f3k1q9-k8s")
        );
        assert_eq!(ctx["context"]["cluster"].as_str(), Some("vat"));
        assert_eq!(ctx["context"]["user"].as_str(), Some("vat"));
        assert_eq!(doc["current-context"].as_str(), Some("vat"));
        assert_eq!(
            doc["clusters"][0]["cluster"]["server"].as_str(),
            Some("https://127.0.0.1:16443")
        );
        assert_eq!(
            doc["users"][0]["user"]["client-certificate-data"].as_str(),
            Some("BBB")
        );
        // Re-namespacing replaces rather than duplicates the key.
        let again = namespaced_kubeconfig(&run, "other").unwrap();
        assert_eq!(again.matches("namespace:").count(), 1);
        assert!(again.contains("namespace: other"));
    }

    #[test]
    fn namespaced_kubeconfig_requires_the_vat_context() {
        assert!(namespaced_kubeconfig(GUEST, "ns").is_err());
        assert!(namespaced_kubeconfig("apiVersion: v1\n", "ns").is_err());
    }

    #[test]
    fn run_namespace_is_a_dns_label() {
        assert_eq!(run_namespace("vat-7f3k1q9", "k8s"), "vat-7f3k1q9-k8s");
        assert_eq!(run_namespace("vat-7F3", "my.E2E_svc"), "vat-7f3-my-e2e-svc");
        let long = run_namespace("vat-7f3k1q9", &"x".repeat(80));
        assert!(long.len() <= 63);
        for name in [long, run_namespace("vat-a", "b--"), run_namespace("", "")] {
            assert!(!name.is_empty());
            assert!(!name.starts_with('-') && !name.ends_with('-'), "{name}");
            assert!(name
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'));
        }
    }

    #[test]
    fn namespace_manifest_labels_the_run() {
        let m = namespace_manifest("vat-1-k8s", "vat-1");
        let doc: serde_yaml::Value = serde_yaml::from_str(&m).unwrap();
        assert_eq!(doc["kind"].as_str(), Some("Namespace"));
        assert_eq!(doc["metadata"]["name"].as_str(), Some("vat-1-k8s"));
        assert_eq!(
            doc["metadata"]["labels"]["app.kubernetes.io/managed-by"].as_str(),
            Some("vat")
        );
        assert_eq!(
            doc["metadata"]["labels"]["vat.dev/run"].as_str(),
            Some("vat-1")
        );
    }

    #[test]
    fn host_kubeconfig_rejects_partial_files() {
        assert!(host_kubeconfig("apiVersion: v1\n", 6443).is_err());
    }
}
