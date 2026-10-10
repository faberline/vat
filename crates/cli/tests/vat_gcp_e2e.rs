//! End-to-end coverage for local GCP inside the machine (ROADMAP M5): an
//! unmodified workload using Google's own client libraries runs in K3s with
//! Workload Identity, pulls its image from the local Artifact Registry, and
//! talks to Pub/Sub and Cloud Storage through env the cluster injected.
//!
//! Opt-in: boots a real Virtualization.framework VM with K3s, so it needs
//! macOS on Apple Silicon, network access (base image + pip), and an upstream
//! `docker` CLI.
//!
//! ```text
//! VAT_GCP_E2E_REQUIRED=1 cargo test --test vat_gcp_e2e -- --ignored --nocapture --test-threads=1
//! ```
//!
//! The machine lives in a throwaway `VAT_MACHINE_HOME` seeded from the cached
//! downloads under `~/.vat`; set `VAT_MACHINE_E2E_HOME` to reuse a home.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use serde_json::Value;

fn vat_bin() -> &'static str {
    env!("CARGO_BIN_EXE_vat")
}

fn required() -> bool {
    std::env::var("VAT_GCP_E2E_REQUIRED").as_deref() == Ok("1")
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
            .prefix("vatg")
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

    fn docker_cmd(&self, args: &[&str]) -> Output {
        let docker = std::env::var("VAT_E2E_DOCKER").unwrap_or_else(|_| "docker".into());
        self.cmd(&docker, args)
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
}

impl Drop for Machine {
    fn drop(&mut self) {
        if self._tmp.is_some() {
            // Also unload the launchd job, so no test socket outlives the run.
            let _ = self.cmd(vat_bin(), &["machine", "stop", "--no-wake", "--json"]);
        }
    }
}

/// Hard-link the user's cached downloads (kernel, rootfs, K3s, kubectl) into
/// a fresh home so the run measures vat, not the network.
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

/// Off the defaults so the test can run beside a developer machine.
const API_PORT: &str = "16443";
const HOST_PUBSUB: &str = "28085";
const HOST_STORAGE: &str = "29023";
const PROJECT: &str = "vat-e2e-proj";
const GSA: &str = "probe@vat-e2e-proj.iam.gserviceaccount.com";
const IMAGE: &str = "us-central1-docker.pkg.dev/vat-e2e-proj/e2e/gcp-probe:v1";

const DOCKERFILE: &str = "FROM python:3.12-slim
RUN pip install --no-cache-dir --quiet google-auth requests google-cloud-pubsub google-cloud-storage
COPY probe.py /probe.py
CMD [\"python\", \"/probe.py\"]
";

/// Stock client-library calls only: nothing here knows it is not on GKE.
const PROBE: &str = r#"import base64, json, os
import google.auth
from google.auth.transport.requests import Request
from google.oauth2 import id_token
from google.cloud import pubsub_v1, storage

out = {"env": {k: os.environ.get(k) for k in ("PUBSUB_EMULATOR_HOST", "STORAGE_EMULATOR_HOST")}}
creds, project = google.auth.default()
creds.refresh(Request())
out["project"] = project
out["email"] = creds.service_account_email
out["token_prefix"] = creds.token[:9]
tok = id_token.fetch_id_token(Request(), "https://e2e.example")
claims = json.loads(base64.urlsafe_b64decode(tok.split(".")[1] + "=="))
out["id_token"] = {"aud": claims["aud"], "email": claims.get("email")}

pub = pubsub_v1.PublisherClient()
sub = pubsub_v1.SubscriberClient()
topic = pub.topic_path(project, "e2e-topic")
subscription = sub.subscription_path(project, "e2e-sub")
pub.create_topic(name=topic)
sub.create_subscription(name=subscription, topic=topic)
pub.publish(topic, b"hello from a pod").result(timeout=30)
resp = sub.pull(subscription=subscription, max_messages=1, timeout=30)
msg = resp.received_messages[0]
sub.acknowledge(subscription=subscription, ack_ids=[msg.ack_id])
out["pubsub"] = msg.message.data.decode()

gcs = storage.Client(project=project)
bucket = gcs.create_bucket("e2e-bucket")
blob = bucket.blob("dir/hello.txt")
blob.upload_from_string("stored from a pod", content_type="text/plain")
out["gcs"] = blob.download_as_bytes().decode()
out["gcs_list"] = [b.name for b in gcs.list_blobs("e2e-bucket")]
print(json.dumps(out), flush=True)
"#;

fn manifest() -> String {
    format!(
        r#"apiVersion: v1
kind: ServiceAccount
metadata:
  name: probe
  annotations:
    iam.gke.io/gcp-service-account: {GSA}
---
apiVersion: v1
kind: Pod
metadata:
  name: gcp-probe
spec:
  serviceAccountName: probe
  restartPolicy: Never
  containers:
  - name: probe
    image: {IMAGE}
    # Always: the local tag is gone, so the pod starting proves the pull
    # came from the in-machine Artifact Registry.
    imagePullPolicy: Always
"#
    )
}

fn wait_pod(m: &Machine, deadline: Duration) -> String {
    let t0 = Instant::now();
    loop {
        let phase = m.kubectl(&["get", "pod", "gcp-probe", "-o", "jsonpath={.status.phase}"]);
        match phase.as_str() {
            "Succeeded" => return phase,
            "Failed" => {
                let logs = m.kubectl(&["logs", "gcp-probe"]);
                panic!("probe pod failed:\n{logs}");
            }
            _ => {}
        }
        assert!(
            t0.elapsed() < deadline,
            "probe pod stuck in {phase:?}:\n{}",
            m.kubectl(&["describe", "pod", "gcp-probe"])
        );
        std::thread::sleep(Duration::from_secs(1));
    }
}

#[test]
#[ignore = "boots a real VM with K3s and local GCP; run with VAT_GCP_E2E_REQUIRED=1 -- --ignored"]
fn gcp_workload_identity_registry_and_emulators_end_to_end() {
    if !required() {
        eprintln!("skipping: set VAT_GCP_E2E_REQUIRED=1");
        return;
    }
    let m = Machine::new();

    let cfg = m.vat_json(&[
        "gcp",
        "config",
        "--project",
        PROJECT,
        "--host-pubsub-port",
        HOST_PUBSUB,
        "--host-storage-port",
        HOST_STORAGE,
        "--json",
    ]);
    assert_eq!(cfg["gcp"]["project"], PROJECT);

    let up = m.vat_json(&["k8s", "up", "--json", "--api-port", API_PORT]);
    assert_eq!(up["k8s"], "ready");
    let status = m.vat_json(&["gcp", "status", "--json"]);
    assert_eq!(status["running"], true, "gcp status: {status}");
    assert_eq!(status["project"], PROJECT);
    assert_eq!(status["host"]["storage"]["listening"], true);

    // Build, push to the local Artifact Registry, and drop the local tag.
    let ctx = tempfile::tempdir().expect("build context");
    std::fs::write(ctx.path().join("Dockerfile"), DOCKERFILE).unwrap();
    std::fs::write(ctx.path().join("probe.py"), PROBE).unwrap();
    let build_t0 = Instant::now();
    m.docker(&["build", "-q", "-t", IMAGE, &ctx.path().to_string_lossy()]);
    let build_ms = build_t0.elapsed().as_millis() as u64;
    let push_t0 = Instant::now();
    m.docker(&["push", "-q", IMAGE]);
    let push_ms = push_t0.elapsed().as_millis() as u64;
    m.docker(&["image", "rm", IMAGE]);
    assert!(
        !m.docker_cmd(&["image", "inspect", IMAGE]).status.success(),
        "local tag still present"
    );

    let manifest_path = ctx.path().join("probe.yaml");
    std::fs::write(&manifest_path, manifest()).unwrap();
    let run_t0 = Instant::now();
    m.kubectl(&["apply", "-f", &manifest_path.to_string_lossy()]);
    wait_pod(&m, Duration::from_secs(300));
    let run_ms = run_t0.elapsed().as_millis() as u64;

    let logs = m.kubectl(&["logs", "gcp-probe"]);
    let line = logs.lines().last().unwrap_or_default();
    let probe: Value =
        serde_json::from_str(line).unwrap_or_else(|e| panic!("probe output ({e}):\n{logs}"));
    assert_eq!(probe["env"]["PUBSUB_EMULATOR_HOST"], "169.254.169.252:8085");
    assert_eq!(
        probe["env"]["STORAGE_EMULATOR_HOST"],
        "http://169.254.169.252:9023"
    );
    assert_eq!(probe["project"], PROJECT);
    assert_eq!(probe["email"], GSA, "Workload Identity: {probe}");
    assert_eq!(probe["token_prefix"], "ya29.vat.");
    assert_eq!(probe["id_token"]["aud"], "https://e2e.example");
    assert_eq!(probe["id_token"]["email"], GSA);
    assert_eq!(probe["pubsub"], "hello from a pod");
    assert_eq!(probe["gcs"], "stored from a pod");
    assert_eq!(probe["gcs_list"], serde_json::json!(["dir/hello.txt"]));

    let events = m.kubectl(&[
        "get",
        "events",
        "--field-selector",
        "involvedObject.name=gcp-probe,reason=Pulled",
        "-o",
        "jsonpath={.items[*].message}",
    ]);
    assert!(
        events.contains("Successfully pulled image") && events.contains(IMAGE),
        "no registry pull recorded: {events:?}"
    );

    // The host shares the same emulator state as the pods.
    let url = format!(
        "http://127.0.0.1:{HOST_STORAGE}/download/storage/v1/b/e2e-bucket/o/dir%2Fhello.txt?alt=media"
    );
    let from_host = m.ok("curl", &["-sf", &url]);
    assert_eq!(from_host, "stored from a pod");

    let env = m.vat_json(&["gcp", "env", "--json"]);
    assert_eq!(
        env["host"]["STORAGE_EMULATOR_HOST"],
        format!("http://127.0.0.1:{HOST_STORAGE}")
    );

    m.kubectl(&[
        "delete",
        "-f",
        &manifest_path.to_string_lossy(),
        "--wait=false",
    ]);

    let evidence = serde_json::json!({
        "cold_k8s_ready_ms": up["ready_ms"],
        "docker_build_ms": build_ms,
        "registry_push_ms": push_ms,
        "pull_to_probe_done_ms": run_ms,
        "project": PROJECT,
        "workload_identity": probe["email"],
        "probe": probe,
    });
    let out = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("vat-gcp-e2e.json");
    std::fs::write(&out, serde_json::to_vec_pretty(&evidence).unwrap()).unwrap();
    eprintln!(
        "gcp evidence ({}):\n{}",
        out.display(),
        serde_json::to_string_pretty(&evidence).unwrap()
    );
}
