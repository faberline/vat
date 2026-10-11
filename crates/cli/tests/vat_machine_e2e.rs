//! End-to-end coverage for the shared Linux machine (ROADMAP M2) and the
//! Docker Engine socket it serves (M3).
//!
//! Opt-in: boots a real Virtualization.framework VM, so it needs macOS on
//! Apple Silicon, network access on first boot, and an upstream `docker` CLI.
//!
//! ```text
//! VAT_MACHINE_E2E_REQUIRED=1 cargo test --test vat_machine_e2e -- --ignored --nocapture --test-threads=1
//! ```
//!
//! By default the machine lives in a throwaway `VAT_MACHINE_HOME` (a true
//! cold boot including provisioning). Set `VAT_MACHINE_E2E_HOME` to reuse a
//! provisioned home across runs.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use serde_json::Value;

fn vat_bin() -> &'static str {
    env!("CARGO_BIN_EXE_vat")
}

fn required() -> bool {
    std::env::var("VAT_MACHINE_E2E_REQUIRED").as_deref() == Ok("1")
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
            .prefix("vatm")
            .tempdir_in("/private/tmp")
            .expect("tempdir");
        Self {
            home: tmp.path().to_path_buf(),
            _tmp: Some(tmp),
        }
    }

    fn vat(&self, args: &[&str]) -> Output {
        self.vat_with(Path::new(vat_bin()), args)
    }

    /// Run another vat binary (a different build) against this machine.
    fn vat_with(&self, bin: &Path, args: &[&str]) -> Output {
        Command::new(bin)
            .args(args)
            .env("VAT_MACHINE_HOME", &self.home)
            .output()
            .expect("spawn vat")
    }

    fn vat_json(&self, args: &[&str]) -> Value {
        self.vat_json_with(Path::new(vat_bin()), args)
    }

    fn vat_json_with(&self, bin: &Path, args: &[&str]) -> Value {
        let out = self.vat_with(bin, args);
        assert!(
            out.status.success(),
            "{} {args:?} failed\nstdout:\n{}\nstderr:\n{}",
            bin.display(),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).expect("vat json")
    }

    fn docker_host(&self) -> String {
        format!("unix://{}", self.home.join("run/docker.sock").display())
    }

    fn docker(&self, args: &[&str]) -> Output {
        Command::new(std::env::var("VAT_E2E_DOCKER").unwrap_or_else(|_| "docker".into()))
            .args(args)
            .env("DOCKER_HOST", self.docker_host())
            .env_remove("DOCKER_CONTEXT")
            .output()
            .expect("spawn docker")
    }

    fn docker_ok(&self, args: &[&str]) -> String {
        let out = self.docker(args);
        assert!(
            out.status.success(),
            "docker {args:?} failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// `sh -c script` in the guest; panics on failure.
    fn guest(&self, script: &str) -> String {
        let out = self.vat(&["machine", "exec", "--", "sh", "-c", script]);
        assert!(
            out.status.success(),
            "guest {script:?} failed\n{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// One request line to the guest agent through the control socket.
    fn agent(&self, header: &str) -> String {
        let mut s = std::os::unix::net::UnixStream::connect(self.home.join("run/vat.sock"))
            .expect("connect the control socket");
        s.set_read_timeout(Some(Duration::from_secs(60))).unwrap();
        s.write_all(format!("{header}\n").as_bytes()).unwrap();
        let mut reply = String::new();
        s.read_to_string(&mut reply).unwrap();
        reply.trim().to_string()
    }

    fn docker_stdin(&self, args: &[&str], input: &[u8]) -> String {
        use std::process::Stdio;
        let mut child =
            Command::new(std::env::var("VAT_E2E_DOCKER").unwrap_or_else(|_| "docker".into()))
                .args(args)
                .env("DOCKER_HOST", self.docker_host())
                .env_remove("DOCKER_CONTEXT")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .expect("spawn docker");
        child.stdin.take().unwrap().write_all(input).unwrap();
        let out = child.wait_with_output().expect("docker output");
        assert!(
            out.status.success(),
            "docker {args:?} failed\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }
}

impl Drop for Machine {
    fn drop(&mut self) {
        if self._tmp.is_some() {
            // Also unload the launchd job, so no test socket outlives the run.
            let _ = self.vat(&["machine", "stop", "--no-wake", "--json"]);
        }
    }
}

fn http_get(port: u16, deadline: Duration) -> String {
    let t0 = Instant::now();
    loop {
        if let Ok(mut s) = TcpStream::connect(("127.0.0.1", port)) {
            let _ = s.set_read_timeout(Some(Duration::from_secs(5)));
            let _ = s.write_all(b"GET / HTTP/1.0\r\n\r\n");
            let mut body = String::new();
            let _ = s.read_to_string(&mut body);
            if !body.is_empty() {
                return body;
            }
        }
        assert!(t0.elapsed() < deadline, "no response on 127.0.0.1:{port}");
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn record(path: &Path, value: &Value) {
    std::fs::write(path, serde_json::to_vec_pretty(value).unwrap()).unwrap();
}

#[test]
#[ignore = "boots a real VM; run with VAT_MACHINE_E2E_REQUIRED=1 -- --ignored"]
fn machine_docker_engine_end_to_end() {
    if !required() {
        eprintln!("skipping: set VAT_MACHINE_E2E_REQUIRED=1");
        return;
    }
    let m = Machine::new();

    // Cold start (provisioning on a fresh home), then dockerd answers.
    let start = m.vat_json(&["machine", "start", "--json", "--memory", "2048"]);
    assert_eq!(start["state"], "running");
    let cold = start["timings"].clone();

    // Upstream docker CLI against the vat socket: arm64 and amd64 (Rosetta).
    let arch = m.docker_ok(&["run", "--rm", "alpine:3.22", "uname", "-m"]);
    assert_eq!(arch, "aarch64");
    // A different image so the amd64 pull does not retag alpine:3.22.
    let amd = m.docker_ok(&[
        "run",
        "--rm",
        "--platform",
        "linux/amd64",
        "busybox:1.37",
        "uname",
        "-m",
    ]);
    assert_eq!(amd, "x86_64");

    // Exit codes, and stdin EOF reaching the container while output still
    // streams back (half-close through the vsock relay).
    let code = m.docker(&["run", "--rm", "alpine:3.22", "sh", "-c", "exit 7"]);
    assert_eq!(code.status.code(), Some(7));
    let echoed = m.docker_stdin(
        &[
            "run",
            "-i",
            "--rm",
            "alpine:3.22",
            "sh",
            "-c",
            "cat; echo after-eof",
        ],
        b"from-stdin\n",
    );
    assert_eq!(echoed, "from-stdin\nafter-eof");

    // BuildKit build through the Engine API.
    let ctx = tempfile::tempdir().expect("build context");
    std::fs::write(
        ctx.path().join("Dockerfile"),
        "FROM alpine:3.22\nRUN echo built > /built\nCMD [\"cat\", \"/built\"]\n",
    )
    .unwrap();
    let build_t0 = Instant::now();
    m.docker_ok(&[
        "build",
        "-q",
        "-t",
        "vat-e2e-build:latest",
        &ctx.path().to_string_lossy(),
    ]);
    let build_ms = build_t0.elapsed().as_millis() as u64;
    assert_eq!(
        m.docker_ok(&["run", "--rm", "vat-e2e-build:latest"]),
        "built"
    );

    // Compose: service DNS plus depends_on, app exit code propagated.
    let proj = tempfile::tempdir().expect("compose dir");
    std::fs::write(
        proj.path().join("compose.yaml"),
        r#"services:
  db:
    image: alpine:3.22
    command: ["sh", "-c", "while true; do printf 'HTTP/1.0 200 OK\r\n\r\ndb-ok' | nc -l -p 5000; done"]
  app:
    image: alpine:3.22
    depends_on: [db]
    command: ["sh", "-c", "for i in 1 2 3 4 5 6 7 8 9 10; do wget -qO- http://db:5000/ && exit 0; sleep 0.5; done; exit 1"]
"#,
    )
    .unwrap();
    let compose_file = proj
        .path()
        .join("compose.yaml")
        .to_string_lossy()
        .to_string();
    let up = m.docker(&[
        "compose",
        "-f",
        &compose_file,
        "-p",
        "vate2e",
        "up",
        "--abort-on-container-exit",
        "--exit-code-from",
        "app",
    ]);
    let up_out = String::from_utf8_lossy(&up.stdout).to_string();
    let _ = m.docker(&["compose", "-f", &compose_file, "-p", "vate2e", "down"]);
    assert!(
        up.status.success(),
        "compose up failed:\n{up_out}\n{}",
        String::from_utf8_lossy(&up.stderr)
    );
    assert!(
        up_out.contains("db-ok"),
        "compose app did not reach db:\n{up_out}"
    );

    // Service-name DNS on a user network.
    let _ = m.docker(&["rm", "-f", "vat-e2e-db"]);
    let _ = m.docker(&["network", "rm", "vat-e2e"]);
    m.docker_ok(&["network", "create", "vat-e2e"]);
    m.docker_ok(&[
        "run",
        "-d",
        "--name",
        "vat-e2e-db",
        "--network",
        "vat-e2e",
        "alpine:3.22",
        "sleep",
        "300",
    ]);
    m.docker_ok(&[
        "run",
        "--rm",
        "--network",
        "vat-e2e",
        "alpine:3.22",
        "ping",
        "-c",
        "1",
        "vat-e2e-db",
    ]);

    // virtiofs bind mount, read-write both ways (TMPDIR lives under /var/folders).
    let dir = tempfile::tempdir().expect("bind dir");
    let host_dir = dir.path().to_string_lossy().to_string();
    std::fs::write(dir.path().join("from-host"), "hello-guest").unwrap();
    let seen = m.docker_ok(&[
        "run",
        "--rm",
        "-v",
        &format!("{host_dir}:/w"),
        "alpine:3.22",
        "sh",
        "-c",
        "cat /w/from-host && echo hello-host > /w/from-guest",
    ]);
    assert_eq!(seen, "hello-guest");
    assert_eq!(
        std::fs::read_to_string(dir.path().join("from-guest"))
            .unwrap()
            .trim(),
        "hello-host"
    );

    // Published port reachable on the host.
    let _ = m.docker(&["rm", "-f", "vat-e2e-web"]);
    m.docker_ok(&[
        "run",
        "-d",
        "--name",
        "vat-e2e-web",
        "-p",
        "18089:8080",
        "alpine:3.22",
        "sh",
        "-c",
        "while true; do printf 'HTTP/1.0 200 OK\\r\\n\\r\\nvat-port' | nc -l -p 8080; done",
    ]);
    let body = http_get(18089, Duration::from_secs(20));
    assert!(body.contains("vat-port"), "unexpected body: {body}");

    // Idle footprint before restart: memory macOS pays for the VM process,
    // and its CPU over ten idle seconds.
    std::thread::sleep(Duration::from_secs(3));
    let status = m.vat_json(&["machine", "status", "--json"]);
    std::thread::sleep(Duration::from_secs(10));
    let later = m.vat_json(&["machine", "status", "--json"]);
    let cpu = |s: &Value| s["vm_cpu_ms"].as_u64().expect("vm_cpu_ms in status");
    let idle_cpu_pct = (cpu(&later) - cpu(&status)) as f64 / 100.0;

    // Restart persistence: volume data and pulled images survive.
    m.docker_ok(&["volume", "create", "vat-e2e-vol"]);
    m.docker_ok(&[
        "run",
        "--rm",
        "-v",
        "vat-e2e-vol:/v",
        "alpine:3.22",
        "sh",
        "-c",
        "echo persisted > /v/f",
    ]);
    m.docker_ok(&["rm", "-f", "vat-e2e-db", "vat-e2e-web"]);
    let stop = m.vat_json(&["machine", "stop", "--json"]);
    assert_eq!(stop["forced"], false, "guest did not power off cleanly");
    let warm = m.vat_json(&["machine", "start", "--json", "--memory", "2048"]);
    let images = m.docker_ok(&["images", "-q", "alpine:3.22"]);
    assert!(!images.is_empty(), "image did not survive restart");
    let persisted = m.docker_ok(&[
        "run",
        "--rm",
        "-v",
        "vat-e2e-vol:/v",
        "alpine:3.22",
        "cat",
        "/v/f",
    ]);
    assert_eq!(persisted, "persisted");
    m.docker_ok(&["volume", "rm", "vat-e2e-vol"]);
    let _ = m.docker(&["network", "rm", "vat-e2e"]);

    let guest = &status["guest"];
    let used_kib = guest["mem_total_kib"].as_u64().unwrap_or(0)
        - guest["mem_available_kib"].as_u64().unwrap_or(0);
    let evidence = serde_json::json!({
        "cold_start": cold,
        "warm_start": warm["timings"],
        "idle_guest_mem_used_mib": used_kib / 1024,
        "vmm_rss_mib": status["vmm_rss_kib"].as_u64().unwrap_or(0) / 1024,
        "vm_footprint_mib": status["vm_footprint_kib"].as_u64().unwrap_or(0) / 1024,
        "idle_vm_cpu_pct_of_core": idle_cpu_pct,
        "data_disk_allocated_mib": status["data_disk_allocated_kib"].as_u64().unwrap_or(0) / 1024,
        "stop_ms": stop["stop_ms"],
        "docker_build_ms": build_ms,
    });
    let out = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("vat-machine-e2e.json");
    record(&out, &evidence);
    eprintln!(
        "machine evidence ({}):\n{}",
        out.display(),
        serde_json::to_string_pretty(&evidence).unwrap()
    );
}

/// Disk space freed in the guest goes back to the host's sparse image, and
/// the guest clock is stepped back to the host's after drifting (as after a
/// host sleep; SIGHUP to the VMM forces the same sync).
#[test]
#[ignore = "boots a real VM; run with VAT_MACHINE_E2E_REQUIRED=1 -- --ignored"]
fn machine_returns_disk_space_and_follows_the_host_clock() {
    if !required() {
        eprintln!("skipping: set VAT_MACHINE_E2E_REQUIRED=1");
        return;
    }
    let m = Machine::new();
    m.vat_json(&["machine", "start", "--json", "--memory", "2048"]);
    let allocated = || {
        m.vat_json(&["machine", "status", "--json"])["data_disk_allocated_kib"]
            .as_u64()
            .expect("data_disk_allocated_kib")
            / 1024
    };

    // Disk: write and delete 512 MiB, then the agent's trim gives it back.
    let before = allocated();
    m.guest("dd if=/dev/urandom of=/var/vat-e2e-trim bs=1M count=512 2>/dev/null && sync");
    let written = allocated();
    assert!(
        written >= before + 400,
        "write did not grow data.img: {before} -> {written} MiB"
    );
    m.guest("rm /var/vat-e2e-trim && sync");
    let reply = m.agent("trim");
    let trimmed: u64 = reply
        .strip_prefix("ok ")
        .and_then(|n| n.parse().ok())
        .unwrap_or_else(|| panic!("trim: {reply}"));
    let after = allocated();
    assert!(
        after + 400 <= written,
        "trim ({trimmed} bytes) did not shrink data.img: {written} -> {after} MiB"
    );

    // Clock: set the guest an hour behind, then have the VMM re-sync it.
    let host_now = || {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64
    };
    m.guest(&format!("date -s @{} >/dev/null", host_now() - 3600));
    let pid = m.vat_json(&["machine", "status", "--json"])["pid"]
        .as_i64()
        .expect("vmm pid");
    assert_eq!(unsafe { libc::kill(pid as i32, libc::SIGHUP) }, 0);
    let t0 = Instant::now();
    let skew = loop {
        let guest: i64 = m.guest("date +%s").parse().expect("guest date");
        let skew = guest - host_now();
        if skew.abs() <= 2 || t0.elapsed() > Duration::from_secs(20) {
            break skew;
        }
        std::thread::sleep(Duration::from_millis(500));
    };
    assert!(skew.abs() <= 2, "guest clock still {skew}s off the host's");
    let elastic = &m.vat_json(&["machine", "status", "--json"])["elastic"];
    let recorded = elastic["clock_skew_ms"].as_i64().expect("clock_skew_ms");
    assert!(
        (-3_700_000..=-3_500_000).contains(&recorded),
        "recorded skew {recorded} ms"
    );

    let evidence = serde_json::json!({
        "trim_bytes": trimmed,
        "data_disk_mib": { "before": before, "written": written, "after_trim": after },
        "clock_skew_ms_before_sync": recorded,
        "clock_skew_s_after_sync": skew,
    });
    let out = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("vat-machine-elastic-e2e.json");
    record(&out, &evidence);
    eprintln!(
        "elastic evidence ({}):\n{}",
        out.display(),
        serde_json::to_string_pretty(&evidence).unwrap()
    );
}

fn alive(pid: u64) -> bool {
    pid > 0 && unsafe { libc::kill(pid as i32, 0) } == 0
}

/// An idle machine stops and gives its memory back to macOS; the next Docker
/// client starts it again through the launchd-held socket, and running
/// containers keep it up.
#[test]
#[ignore = "boots a real VM; run with VAT_MACHINE_E2E_REQUIRED=1 -- --ignored"]
fn machine_stops_when_idle_and_wakes_on_docker_sock() {
    if !required() {
        eprintln!("skipping: set VAT_MACHINE_E2E_REQUIRED=1");
        return;
    }
    let m = Machine::new();
    let started = m.vat_json(&[
        "machine",
        "start",
        "--json",
        "--memory",
        "2048",
        "--idle-stop",
        "15s",
    ]);
    let status = |m: &Machine| m.vat_json(&["machine", "status", "--json"]);
    let st = status(&m);
    assert_eq!(
        st["wake_on_socket"], true,
        "launchd does not hold docker.sock: {st}"
    );
    assert_eq!(st["idle_stop_secs"], 15);
    m.docker_ok(&["run", "--rm", "alpine:3.22", "true"]);
    let vm_pid = status(&m)["elastic"]["vm_pid"].as_u64().expect("vm_pid");
    let footprint_mib = status(&m)["vm_footprint_kib"].as_u64().unwrap_or(0) / 1024;
    assert!(alive(vm_pid));

    // Idle: no clients, no containers -> the VM process exits.
    let t0 = Instant::now();
    let stopped = loop {
        let st = status(&m);
        if st["state"] == "stopped" || t0.elapsed() > Duration::from_secs(90) {
            break st;
        }
        std::thread::sleep(Duration::from_secs(1));
    };
    let idle_stop_s = t0.elapsed().as_secs();
    assert_eq!(
        stopped["state"], "stopped",
        "machine did not stop when idle"
    );
    assert_eq!(stopped["elastic"]["stopped_by"], "idle");
    assert_eq!(stopped["wake_on_socket"], true);
    assert!(
        !alive(vm_pid),
        "the VM process (and its memory) outlived the stop"
    );
    assert!(
        m.home.join("run/docker.sock").exists(),
        "docker.sock went away"
    );

    // A plain Docker client wakes it; the first request is held, not refused.
    let t0 = Instant::now();
    m.docker_ok(&["version", "--format", "{{.Server.Version}}"]);
    let wake_ms = t0.elapsed().as_millis() as u64;
    assert_eq!(status(&m)["state"], "running");

    // A running container keeps it up past the idle window.
    m.docker_ok(&[
        "run",
        "-d",
        "--name",
        "vat-e2e-awake",
        "alpine:3.22",
        "sleep",
        "600",
    ]);
    std::thread::sleep(Duration::from_secs(30));
    assert_eq!(
        status(&m)["state"],
        "running",
        "stopped with a container running"
    );
    m.docker_ok(&["rm", "-f", "vat-e2e-awake"]);

    // --no-wake releases the socket: clients fail instead of booting it.
    m.vat_json(&["machine", "stop", "--no-wake", "--json"]);
    assert!(!m.home.join("run/docker.sock").exists());
    assert_eq!(status(&m)["wake_on_socket"], false);
    assert!(!m.docker(&["version"]).status.success());

    let evidence = serde_json::json!({
        "start_timings": started["timings"],
        "vm_footprint_mib_before_idle_stop": footprint_mib,
        "idle_stop_after_s": idle_stop_s,
        "configured_idle_stop_s": 15,
        "wake_ms": wake_ms,
    });
    let out = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("vat-machine-lifecycle-e2e.json");
    record(&out, &evidence);
    eprintln!(
        "lifecycle evidence ({}):\n{}",
        out.display(),
        serde_json::to_string_pretty(&evidence).unwrap()
    );
}

/// A copy of the vat binary that hashes differently: what a vat upgrade
/// looks like to the machine. Re-signing under another identifier changes
/// the bytes and keeps the binary valid (appended bytes would break its
/// signature, and codesign then refuses to re-sign the VMM copy).
fn other_build(dir: &Path, n: u32) -> (PathBuf, String) {
    let bin = dir.join(format!("vat-{n}"));
    std::fs::copy(vat_bin(), &bin).expect("copy vat");
    let out = Command::new("codesign")
        .args(["-s", "-", "-f", "-i", &format!("dev.vat.e2e.upgrade{n}")])
        .arg(&bin)
        .output()
        .expect("run codesign");
    assert!(
        out.status.success(),
        "codesign: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    (bin.clone(), sha256(&bin))
}

fn sha256(path: &Path) -> String {
    let out = Command::new("shasum")
        .args(["-a", "256"])
        .arg(path)
        .output()
        .expect("run shasum");
    let text = String::from_utf8_lossy(&out.stdout);
    text.split_whitespace().next().expect("sha").to_string()
}

/// After a vat upgrade the machine follows the new binary: a stopped machine
/// is re-staged so the next socket wake runs the new VMM, an idle one is
/// restarted onto it, and a busy one keeps its containers until it is idle.
/// `rm` of the last machine leaves nothing behind for launchd to run.
#[test]
#[ignore = "boots a real VM; run with VAT_MACHINE_E2E_REQUIRED=1 -- --ignored"]
fn machine_follows_a_vat_upgrade() {
    if !required() {
        eprintln!("skipping: set VAT_MACHINE_E2E_REQUIRED=1");
        return;
    }
    let m = Machine::new();
    let builds = tempfile::tempdir().expect("tempdir");
    let t_all = Instant::now();
    let original = PathBuf::from(vat_bin());
    let original_sha = sha256(&original);
    let status = |bin: &Path| m.vat_json_with(bin, &["machine", "status", "--json"]);
    // Idle as the VMM sees it, then quiet for a few of its ticks so that
    // the status calls' own connections are not what the next command sees.
    let wait_idle = |bin: &Path| {
        let t0 = Instant::now();
        while status(bin)["elastic"]["idle"] != true {
            assert!(
                t0.elapsed() < Duration::from_secs(120),
                "machine never went idle: {}",
                status(bin)
            );
            std::thread::sleep(Duration::from_millis(500));
        }
        std::thread::sleep(Duration::from_secs(2));
    };

    m.vat_json(&[
        "machine",
        "start",
        "--json",
        "--memory",
        "2048",
        "--idle-stop",
        "0",
    ]);
    let st = status(&original);
    assert_eq!(st["vmm"]["vat_sha256"], original_sha.as_str(), "{st}");
    assert_eq!(st["vmm_outdated"], false, "{st}");

    // An upgrade: status re-stages for the next start but never restarts.
    let (new1, sha1) = other_build(builds.path(), 1);
    assert_ne!(sha1, original_sha);
    let pid0 = status(&original)["pid"].as_u64().expect("pid");
    let st = status(&new1);
    assert_eq!(st["pid"], pid0, "status restarted the machine");
    assert_eq!(st["vmm_outdated"], true, "{st}");
    assert_eq!(st["vmm_staged_sha256"], sha1.as_str(), "{st}");
    assert_eq!(st["vmm_running_sha256"], original_sha.as_str(), "{st}");
    let human = m.vat_with(&new1, &["machine", "status"]);
    assert!(
        String::from_utf8_lossy(&human.stdout).contains("vmm      outdated (restart to update)"),
        "{}",
        String::from_utf8_lossy(&human.stdout)
    );

    // Idle: start restarts it onto the new build.
    wait_idle(&new1);
    let t0 = Instant::now();
    let started = m.vat_json_with(&new1, &["machine", "start", "--json"]);
    let idle_restart_ms = t0.elapsed().as_millis() as u64;
    assert_eq!(started["timings"]["upgraded_from_pid"], pid0, "{started}");
    let st = status(&new1);
    let pid1 = st["pid"].as_u64().expect("pid");
    assert_ne!(pid1, pid0);
    assert!(!alive(pid0), "the old VMM outlived the restart");
    assert_eq!(st["vmm"]["vat_sha256"], sha1.as_str(), "{st}");
    assert_eq!(st["vmm_outdated"], false, "{st}");
    m.docker_ok(&["version", "--format", "{{.Server.Version}}"]);

    // Busy: a running container keeps the old build up; nothing is killed.
    let (new2, sha2) = other_build(builds.path(), 2);
    m.docker_ok(&[
        "run",
        "-d",
        "--name",
        "vat-e2e-upgrade",
        "alpine:3.22",
        "sleep",
        "600",
    ]);
    let busy = m.vat_with(&new2, &["machine", "start"]);
    assert!(busy.status.success());
    assert!(
        String::from_utf8_lossy(&busy.stderr).contains("older vat build"),
        "no restart hint: {}",
        String::from_utf8_lossy(&busy.stderr)
    );
    let started = m.vat_json_with(&new2, &["machine", "start", "--json"]);
    assert_eq!(started["timings"]["vmm_outdated"], true, "{started}");
    let st = status(&new2);
    assert_eq!(st["pid"], pid1, "a busy machine was restarted");
    assert_eq!(st["vmm_outdated"], true, "{st}");
    assert_eq!(st["vmm_staged_sha256"], sha2.as_str(), "{st}");
    assert_eq!(
        m.docker_ok(&["inspect", "-f", "{{.State.Running}}", "vat-e2e-upgrade"]),
        "true"
    );
    m.docker_ok(&["rm", "-f", "vat-e2e-upgrade"]);
    wait_idle(&new2);
    let started = m.vat_json_with(&new2, &["machine", "start", "--json"]);
    assert_eq!(started["timings"]["upgraded_from_pid"], pid1, "{started}");
    let st = status(&new2);
    assert_eq!(st["vmm"]["vat_sha256"], sha2.as_str(), "{st}");
    assert_eq!(st["vmm_outdated"], false, "{st}");

    // Stopped: the upgrade is staged silently, and a plain Docker client
    // wakes the new build through launchd.
    m.vat_json_with(&new2, &["machine", "stop", "--json"]);
    let (new3, sha3) = other_build(builds.path(), 3);
    let st = status(&new3);
    assert_eq!(st["state"], "stopped", "{st}");
    assert_eq!(st["wake_on_socket"], true, "{st}");
    assert_eq!(st["vmm_staged_sha256"], sha3.as_str(), "{st}");
    assert_eq!(st["vmm_outdated"], false, "{st}");
    let t0 = Instant::now();
    m.docker_ok(&["version", "--format", "{{.Server.Version}}"]);
    let wake_ms = t0.elapsed().as_millis() as u64;
    let st = status(&new3);
    assert_eq!(st["state"], "running", "{st}");
    assert_eq!(st["vmm"]["vat_sha256"], sha3.as_str(), "{st}");
    assert_eq!(st["vmm"]["vat_version"], env!("CARGO_PKG_VERSION"), "{st}");
    assert_eq!(st["vmm_outdated"], false, "{st}");

    // The last machine's rm leaves no job, socket, or VMM binary.
    if m._tmp.is_some() {
        m.vat_json_with(&new3, &["machine", "rm", "--yes", "--json"]);
        let st = status(&new3);
        assert_eq!(st["state"], "absent", "{st}");
        assert_eq!(st["wake_on_socket"], false, "{st}");
        assert!(!m.home.join("run/docker.sock").exists());
        assert!(!m.home.join("machine/bin").exists());
        assert!(!m.docker(&["version"]).status.success());
    }

    let evidence = serde_json::json!({
        "builds": [original_sha, sha1, sha2, sha3],
        "idle_restart_ms": idle_restart_ms,
        "wake_ms": wake_ms,
        "total_s": t_all.elapsed().as_secs(),
    });
    let out = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("vat-machine-upgrade-e2e.json");
    record(&out, &evidence);
    eprintln!(
        "upgrade evidence ({}):\n{}",
        out.display(),
        serde_json::to_string_pretty(&evidence).unwrap()
    );
}

/// Testcontainers (the Rust crate, through bollard) drives the machine's
/// Engine API like any Docker host: create with an ephemeral published port,
/// wait on a log line, then talk to the service from the host.
#[test]
#[ignore = "boots a real VM; run with VAT_MACHINE_E2E_REQUIRED=1 -- --ignored"]
fn machine_docker_engine_serves_testcontainers() {
    use testcontainers::core::{IntoContainerPort, WaitFor};
    use testcontainers::runners::SyncRunner;
    use testcontainers::GenericImage;

    if !required() {
        eprintln!("skipping: set VAT_MACHINE_E2E_REQUIRED=1");
        return;
    }
    let m = Machine::new();
    m.vat_json(&["machine", "start", "--json", "--memory", "2048"]);
    // Testcontainers reads DOCKER_HOST once, when it first connects.
    std::env::set_var("DOCKER_HOST", m.docker_host());
    std::env::remove_var("DOCKER_CONTEXT");

    let t0 = Instant::now();
    let redis = GenericImage::new("redis", "7-alpine")
        .with_exposed_port(6379.tcp())
        .with_wait_for(WaitFor::message_on_stdout("Ready to accept connections"))
        .start()
        .expect("start redis through testcontainers");
    let ready_ms = t0.elapsed().as_millis() as u64;
    let port = redis.get_host_port_ipv4(6379).expect("mapped port");

    // No retry: as with Docker, the published port must already be open on
    // the host by the time the container's log says it is ready.
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap_or_else(|e| {
        let ports = std::fs::read_to_string(m.home.join("machine/default/ports.json"))
            .unwrap_or_else(|e| format!("<no ports.json: {e}>"));
        let ps = m.docker(&["ps", "--format", "{{.Names}} {{.Ports}}"]);
        panic!(
            "connect to redis on 127.0.0.1:{port}: {e}\nports.json: {ports}\ndocker ps:\n{}",
            String::from_utf8_lossy(&ps.stdout)
        )
    });
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    s.write_all(b"PING\r\n").unwrap();
    let mut reply = [0u8; 7];
    s.read_exact(&mut reply).expect("redis reply");
    assert_eq!(&reply, b"+PONG\r\n");
    let id = redis.id().to_string();
    drop(redis);
    let gone = m.docker(&["inspect", &id]);
    assert!(!gone.status.success(), "testcontainers did not remove {id}");

    let evidence = serde_json::json!({
        "testcontainers_redis_ready_ms": ready_ms,
        "mapped_port": port,
    });
    let out = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("vat-testcontainers-e2e.json");
    record(&out, &evidence);
    eprintln!(
        "testcontainers evidence ({}):\n{}",
        out.display(),
        serde_json::to_string_pretty(&evidence).unwrap()
    );
}
