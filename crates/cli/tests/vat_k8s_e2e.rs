//! End-to-end coverage for the persistent K3s cluster (ROADMAP M4).
//!
//! Opt-in: boots a real Virtualization.framework VM and runs K3s inside it, so
//! it needs macOS on Apple Silicon, network access on first boot, and an
//! upstream `docker` CLI.
//!
//! ```text
//! VAT_K8S_E2E_REQUIRED=1 cargo test --test vat_k8s_e2e -- --ignored --nocapture --test-threads=1
//! ```
//!
//! By default the machine lives in a throwaway `VAT_MACHINE_HOME`; downloaded
//! downloads already cached under `~/.vat` (K3s, kubectl, kernel) are
//! hard-linked in so a cold run does not re-fetch them. Set
//! `VAT_MACHINE_E2E_HOME` to reuse a home.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use serde_json::Value;

fn vat_bin() -> &'static str {
    env!("CARGO_BIN_EXE_vat")
}

fn required() -> bool {
    std::env::var("VAT_K8S_E2E_REQUIRED").as_deref() == Ok("1")
}

struct Machine {
    home: PathBuf,
    _tmp: Option<tempfile::TempDir>,
}

impl Machine {
    fn new() -> Self {
        if let Some(home) = std::env::var_os("VAT_MACHINE_E2E_HOME") {
            return Self {
                home: PathBuf::from(home),
                _tmp: None,
            };
        }
        // Short path: unix socket names must stay under ~104 bytes.
        let tmp = tempfile::Builder::new()
            .prefix("vatk")
            .tempdir_in("/private/tmp")
            .expect("tempdir");
        seed_cache(tmp.path());
        Self {
            home: tmp.path().to_path_buf(),
            _tmp: Some(tmp),
        }
    }

    fn cmd(&self, program: &str, args: &[&str]) -> Output {
        Command::new(program)
            .args(args)
            .env("VAT_MACHINE_HOME", &self.home)
            .env(
                "DOCKER_HOST",
                format!("unix://{}", self.home.join("run/docker.sock").display()),
            )
            .env_remove("DOCKER_CONTEXT")
            .env_remove("KUBECONFIG")
            .output()
            .unwrap_or_else(|e| panic!("spawn {program}: {e}"))
    }

    fn ok(&self, program: &str, args: &[&str]) -> String {
        let out = self.cmd(program, args);
        assert!(
            out.status.success(),
            "{program} {args:?} failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn vat_json(&self, args: &[&str]) -> Value {
        serde_json::from_str(&self.ok(vat_bin(), args)).expect("vat json")
    }

    fn docker(&self, args: &[&str]) -> String {
        let docker = std::env::var("VAT_E2E_DOCKER").unwrap_or_else(|_| "docker".into());
        self.ok(&docker, args)
    }

    fn kubectl(&self, args: &[&str]) -> String {
        let mut full = vec!["k8s", "kubectl", "--"];
        full.extend_from_slice(args);
        self.ok(vat_bin(), &full)
    }

    /// Retry `kubectl` until it succeeds: right after a restart the API can
    /// answer before the pod's container is back.
    fn kubectl_eventually(&self, args: &[&str], deadline: Duration) -> String {
        let t0 = Instant::now();
        let mut full = vec!["k8s", "kubectl", "--"];
        full.extend_from_slice(args);
        loop {
            let out = self.cmd(vat_bin(), &full);
            if out.status.success() {
                return String::from_utf8_lossy(&out.stdout).trim().to_string();
            }
            assert!(
                t0.elapsed() < deadline,
                "kubectl {args:?} never succeeded:\n{}",
                String::from_utf8_lossy(&out.stderr)
            );
            std::thread::sleep(Duration::from_secs(1));
        }
    }
}

impl Drop for Machine {
    fn drop(&mut self) {
        if self._tmp.is_some() {
            let _ = self.cmd(vat_bin(), &["machine", "stop", "--json"]);
        }
    }
}

/// Hard-link the user's cached downloads (kernel, rootfs, K3s, kubectl) into
/// a fresh home so a cold run measures boot, not the network.
fn seed_cache(home: &Path) {
    let Some(user) = std::env::var_os("HOME") else {
        return;
    };
    let user = PathBuf::from(user).join(".vat");
    for sub in ["machine/assets", "bin"] {
        let Ok(entries) = std::fs::read_dir(user.join(sub)) else {
            continue;
        };
        let dest = home.join(sub);
        std::fs::create_dir_all(&dest).expect("cache dir");
        for entry in entries.flatten() {
            let name = entry.file_name();
            if name.to_string_lossy().ends_with(".part") {
                continue;
            }
            let _ = std::fs::hard_link(entry.path(), dest.join(&name));
        }
    }
}

/// Off the default 6443 so the test can run beside a developer cluster.
const API_PORT: &str = "16443";

const MANIFEST: &str = r#"apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: vat-e2e-data
spec:
  accessModes: [ReadWriteOnce]
  resources:
    requests:
      storage: 64Mi
---
apiVersion: apps/v1
kind: Deployment
metadata:
  name: vat-e2e
spec:
  replicas: 1
  selector:
    matchLabels: {app: vat-e2e}
  template:
    metadata:
      labels: {app: vat-e2e}
    spec:
      containers:
      - name: app
        image: vat-k8s-e2e:v1
        # Never: the image exists only in the machine's Docker store, so the
        # pod starting at all proves there is no load or push step.
        imagePullPolicy: Never
        volumeMounts:
        - {name: data, mountPath: /data}
      volumes:
      - name: data
        persistentVolumeClaim: {claimName: vat-e2e-data}
"#;

fn wait_rollout(m: &Machine) {
    m.kubectl_eventually(
        &["rollout", "status", "deployment/vat-e2e", "--timeout=180s"],
        Duration::from_secs(300),
    );
}

fn read_data(m: &Machine) -> String {
    m.kubectl_eventually(
        &["exec", "deploy/vat-e2e", "--", "cat", "/data/f", "/baked"],
        Duration::from_secs(120),
    )
}

/// A `cluster = "machine"` vat.toml service on the running cluster: the
/// runner gets a namespaced `KUBECONFIG`, its objects land in
/// `VAT_K8S_NAMESPACE`, and a passing run deletes that namespace.
/// Returns the namespace and how long `vat run` took.
fn machine_cluster_service_run(m: &Machine) -> (String, u64) {
    let project = tempfile::tempdir().expect("project");
    let vat_home = tempfile::tempdir().expect("vat home");
    let seen = project.path().join("namespace.out");
    let kubectl = m.home.join("bin/kubectl");
    let script = format!(
        "set -eu; test -n \"$VAT_K8S_NAMESPACE\"; \
         {k} create configmap vat-e2e-probe --from-literal=k=v; \
         got=$({k} get configmap vat-e2e-probe -o jsonpath='{{.metadata.namespace}}'); \
         [ \"$got\" = \"$VAT_K8S_NAMESPACE\" ]; printf %s \"$got\" > {out}",
        k = kubectl.display(),
        out = seen.display(),
    );
    std::fs::write(
        project.path().join("vat.toml"),
        format!(
            r#"version = 1
default_runner = "e2e"

[network]
egress = "open"

[[services]]
id = "k8s"
cluster = "machine"

[[runners]]
id = "e2e"
requires = ["k8s"]
cmd = ["/bin/sh", "-c", {script:?}]
"#
        ),
    )
    .unwrap();

    let t0 = Instant::now();
    let out = Command::new(vat_bin())
        .args(["run", "e2e"])
        .current_dir(project.path())
        .env("VAT_HOME", vat_home.path())
        .env("VAT_MACHINE_HOME", &m.home)
        .env_remove("KUBECONFIG")
        .output()
        .expect("spawn vat run");
    let run_ms = t0.elapsed().as_millis() as u64;
    assert!(
        out.status.success(),
        "vat run with a machine cluster service failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let namespace = std::fs::read_to_string(&seen).expect("runner recorded its namespace");
    assert!(
        namespace.ends_with("-k8s"),
        "namespace is <run-id>-<service-id>: {namespace}"
    );

    // Teardown deletes with --wait=false; the namespace drains shortly after.
    let t0 = Instant::now();
    loop {
        let get = m.cmd(
            vat_bin(),
            &["k8s", "kubectl", "--", "get", "ns", &namespace],
        );
        if !get.status.success() {
            let stderr = String::from_utf8_lossy(&get.stderr);
            assert!(stderr.contains("NotFound"), "get ns failed: {stderr}");
            break;
        }
        assert!(
            t0.elapsed() < Duration::from_secs(120),
            "run namespace {namespace} was not deleted"
        );
        std::thread::sleep(Duration::from_secs(1));
    }
    (namespace, run_ms)
}

#[test]
#[ignore = "boots a real VM with K3s; run with VAT_K8S_E2E_REQUIRED=1 -- --ignored"]
fn k8s_persistent_cluster_end_to_end() {
    if !required() {
        eprintln!("skipping: set VAT_K8S_E2E_REQUIRED=1");
        return;
    }
    let m = Machine::new();

    // Cold: boot the machine with K3s, API forwarded, kubeconfig + kubectl.
    let up = m.vat_json(&["k8s", "up", "--json", "--api-port", API_PORT]);
    assert_eq!(up["k8s"], "ready");
    assert_eq!(up["context"], "vat");
    let kubeconfig = PathBuf::from(up["kubeconfig"].as_str().unwrap());
    assert!(kubeconfig.is_file());
    let version = m.kubectl(&["version", "-o", "json"]);
    let version: Value = serde_json::from_str(&version).unwrap();
    assert_eq!(
        version["serverVersion"]["gitVersion"]
            .as_str()
            .unwrap()
            .split('+')
            .next(),
        version["clientVersion"]["gitVersion"].as_str(),
        "vended kubectl must match the server"
    );
    let node = m.kubectl(&["get", "nodes", "-o", "jsonpath={.items[0].metadata.name}"]);
    assert_eq!(node, "vat");

    // Build through the Engine API; the kubelet sees the image directly.
    let ctx = tempfile::tempdir().expect("build context");
    std::fs::write(
        ctx.path().join("Dockerfile"),
        "FROM alpine:3.22\nRUN echo baked > /baked\nCMD [\"sleep\", \"infinity\"]\n",
    )
    .unwrap();
    let build_t0 = Instant::now();
    m.docker(&[
        "build",
        "-q",
        "-t",
        "vat-k8s-e2e:v1",
        &ctx.path().to_string_lossy(),
    ]);
    let build_ms = build_t0.elapsed().as_millis() as u64;

    let manifest = ctx.path().join("app.yaml");
    std::fs::write(&manifest, MANIFEST).unwrap();
    let deploy_t0 = Instant::now();
    m.kubectl(&["apply", "-f", &manifest.to_string_lossy()]);
    wait_rollout(&m);
    let deploy_ms = deploy_t0.elapsed().as_millis() as u64;
    m.kubectl(&[
        "exec",
        "deploy/vat-e2e",
        "--",
        "sh",
        "-c",
        "echo persisted > /data/f",
    ]);
    assert_eq!(read_data(&m), "persisted\nbaked");

    // Clean restart: cluster state, the pod, and PVC data come back.
    let stop = m.vat_json(&["machine", "stop", "--json"]);
    assert_eq!(stop["forced"], false, "guest did not power off cleanly");
    let warm_t0 = Instant::now();
    let warm = m.vat_json(&["k8s", "up", "--json", "--api-port", API_PORT]);
    assert_eq!(warm["enabled_live"], false);
    wait_rollout(&m);
    let warm_ms = warm_t0.elapsed().as_millis() as u64;
    assert_eq!(read_data(&m), "persisted\nbaked");

    // Simulated host reboot: the VMM dies without a guest shutdown.
    let status = m.vat_json(&["machine", "status", "--json"]);
    let pid = status["pid"].as_i64().expect("running vmm pid");
    let killed = Command::new("kill")
        .args(["-9", &pid.to_string()])
        .status()
        .unwrap();
    assert!(killed.success());
    let t0 = Instant::now();
    while m.vat_json(&["machine", "status", "--json"])["state"] == "running" {
        assert!(t0.elapsed() < Duration::from_secs(30), "vmm did not exit");
        std::thread::sleep(Duration::from_millis(200));
    }
    let crash_t0 = Instant::now();
    m.vat_json(&["k8s", "up", "--json", "--api-port", API_PORT]);
    wait_rollout(&m);
    let crash_ms = crash_t0.elapsed().as_millis() as u64;
    assert_eq!(read_data(&m), "persisted\nbaked");

    let k8s_status = m.vat_json(&["k8s", "status", "--json"]);
    assert_eq!(k8s_status["ready"], true);
    assert_eq!(k8s_status["kubectl"]["installed"], true);

    m.kubectl(&["delete", "-f", &manifest.to_string_lossy(), "--wait=false"]);

    let (namespace, run_ms) = machine_cluster_service_run(&m);

    let evidence = serde_json::json!({
        "cold_k8s_ready_ms": up["ready_ms"],
        "docker_build_ms": build_ms,
        "deploy_ready_ms": deploy_ms,
        "restart_to_pod_ready_ms": warm_ms,
        "crash_to_pod_ready_ms": crash_ms,
        "machine_service_run_ms": run_ms,
        "machine_service_namespace": namespace,
        "k3s_version": up["k3s_version"],
    });
    let out = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("vat-k8s-e2e.json");
    std::fs::write(&out, serde_json::to_vec_pretty(&evidence).unwrap()).unwrap();
    eprintln!(
        "k8s evidence ({}):\n{}",
        out.display(),
        serde_json::to_string_pretty(&evidence).unwrap()
    );
}
