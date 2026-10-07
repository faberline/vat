//! M1 evidence for the Darwin native runtime (`vat image …`, `vat container …`).
//!
//! No network and no root: every test uses a private `VAT_HOME` and a short
//! `VAT_NATIVE_ROOT_BASE` under the temp dir, and the registry test talks to
//! an in-process `vat::registry` server on 127.0.0.1.
#![cfg(target_os = "macos")]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use serde_json::Value;

const VAT: &str = env!("CARGO_BIN_EXE_vat");

struct Env {
    tmp: tempfile::TempDir,
}

impl Env {
    fn new() -> Self {
        let tmp = tempfile::Builder::new().prefix("vnr").tempdir().expect("tempdir");
        std::fs::create_dir_all(tmp.path().join("r")).unwrap();
        Env { tmp }
    }

    fn path(&self) -> PathBuf {
        self.tmp.path().canonicalize().unwrap()
    }

    fn cmd(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(VAT);
        cmd.args(args)
            .env("VAT_HOME", self.path().join("home"))
            .env("VAT_NATIVE_ROOT_BASE", self.path().join("r"))
            .env_remove("VAT_NATIVE_HOME")
            .env("DOCKER_CONFIG", self.path().join("docker"))
            .stdin(std::process::Stdio::null());
        cmd
    }

    fn run(&self, args: &[&str]) -> Output {
        self.cmd(args).output().expect("spawn vat")
    }

    /// Run and require success; returns stdout.
    fn ok(&self, args: &[&str]) -> String {
        let out = self.run(args);
        assert!(
            out.status.success(),
            "vat {args:?} failed ({:?})\nstdout:\n{}\nstderr:\n{}",
            out.status.code(),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).to_string()
    }

    fn json(&self, args: &[&str]) -> Value {
        let stdout = self.ok(args);
        serde_json::from_str(&stdout).unwrap_or_else(|e| panic!("vat {args:?}: bad JSON ({e}): {stdout}"))
    }

    /// Write a build context from `(relative path, contents, mode)` triples.
    fn context(&self, name: &str, files: &[(&str, &str, u32)]) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let dir = self.path().join(name);
        for (rel, contents, mode) in files {
            let path = dir.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, contents).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(*mode)).unwrap();
        }
        dir
    }
}

fn host_python() -> bool {
    Command::new("/usr/bin/python3")
        .args(["-c", "import venv"])
        .stdin(std::process::Stdio::null())
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// A small image: a COPYed script plus a RUN-generated script that bakes the
/// build root (`$VAT_ROOT`) into its bytes, so it must be relocated.
const DEMO_VATFILE: &str = r#"FROM scratch
WORKDIR /app
ENV GREETING=hello
COPY hello.sh /app/bin/hello.sh
RUN printf '#!/bin/sh\necho "baked=%s"\n' "$VAT_ROOT" > $VAT_ROOT/app/bin/where.sh && chmod +x $VAT_ROOT/app/bin/where.sh
ENV PATH=/app/bin
CMD ["hello.sh"]
"#;

const HELLO_SH: &str = "#!/bin/sh\necho \"$GREETING from $PWD root=$VAT_ROOT\"\n";

fn build_demo(env: &Env, tag: &str) -> String {
    let ctx = env.context(
        "demo-ctx",
        &[("Vatfile", DEMO_VATFILE, 0o644), ("hello.sh", HELLO_SH, 0o755)],
    );
    let out = env.json(&["image", "build", "-t", tag, "--json", ctx.to_str().unwrap()]);
    assert!(out["relocations"].as_u64().unwrap() >= 1, "where.sh must be recorded for relocation: {out}");
    out["digest"].as_str().unwrap().to_string()
}

fn inspect(env: &Env, id: &str) -> Value {
    env.json(&["container", "inspect", id])
}

fn root_of(env: &Env, id: &str) -> PathBuf {
    PathBuf::from(inspect(env, id)["root"].as_str().unwrap())
}

fn wait_exited(env: &Env, id: &str) -> Value {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let state = inspect(env, id);
        if state["status"] == "exited" && !state["exit_code"].is_null() {
            return state;
        }
        assert!(Instant::now() < deadline, "container {id} did not exit: {state}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Evidence 1: build → export → rm → import → run. The relocated script
/// reports the *container* root, and a python venv created at build time runs
/// from the relocated root.
#[test]
fn build_export_import_run_relocates_script_and_venv() {
    let env = Env::new();
    let python = host_python();
    let mut vatfile = DEMO_VATFILE.to_string();
    if python {
        vatfile.push_str(
            "RUN /usr/bin/python3 -m venv $VAT_ROOT/opt/venv && $VAT_ROOT/opt/venv/bin/python -c 'import sys; print(sys.prefix)'\n",
        );
    } else {
        eprintln!("skipping the venv half: /usr/bin/python3 is not usable on this host");
    }
    let ctx = env.context("ctx", &[("Vatfile", &vatfile, 0o644), ("hello.sh", HELLO_SH, 0o755)]);
    let built = env.json(&["image", "build", "-t", "demo:1", "--json", ctx.to_str().unwrap()]);
    let digest = built["digest"].as_str().unwrap().to_string();

    let layout = env.path().join("layout");
    env.ok(&["image", "export", "--oci-layout", layout.to_str().unwrap(), "demo:1"]);
    assert!(layout.join("index.json").is_file() && layout.join("oci-layout").is_file());
    let index: Value = serde_json::from_slice(&std::fs::read(layout.join("index.json")).unwrap()).unwrap();
    assert_eq!(index["manifests"][0]["platform"]["os"], "darwin");
    assert_eq!(index["manifests"][0]["platform"]["architecture"], "arm64");

    env.ok(&["image", "rm", "demo:1"]);
    let images = env.json(&["image", "ls", "--json"]);
    assert_eq!(images.as_array().unwrap().len(), 0, "image removed: {images}");
    assert!(!env.run(&["image", "inspect", "demo:1"]).status.success());

    let imported = env.json(&["image", "import", "--oci-layout", layout.to_str().unwrap(), "--json"]);
    assert_eq!(imported[0]["name"], "demo:1");
    assert_eq!(imported[0]["digest"], digest.as_str(), "import restores the same digest");

    let image = env.json(&["image", "inspect", "demo:1"]);
    let reloc_paths: Vec<&str> = image["relocation_paths"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["path"].as_str().unwrap())
        .collect();
    assert!(reloc_paths.contains(&"app/bin/where.sh"), "{reloc_paths:?}");

    // Default CMD via a root-relative PATH entry.
    let out = env.ok(&["container", "run", "--name", "c1", "demo:1"]);
    let root = root_of(&env, "c1");
    assert_eq!(root.as_os_str().len(), 128, "fixed-length root: {}", root.display());
    assert_eq!(out.trim(), format!("hello from {}/app root={}", root.display(), root.display()));

    // The RUN-baked build root was rewritten to this container's root.
    let out = env.ok(&["container", "run", "--rm", "demo:1", "where.sh"]);
    let baked = out.trim().strip_prefix("baked=").expect("where.sh output");
    assert_eq!(baked.len(), 128);
    assert!(!baked.contains("@@VAT_ROOT@@"), "placeholder leaked: {baked}");
    assert!(Path::new(baked).is_absolute());
    // `--rm` removed that container, so its root is gone again.
    assert!(!Path::new(baked).exists(), "--rm container root removed");

    if python {
        let out = env.ok(&[
            "container", "run", "--name", "py", "demo:1", "/opt/venv/bin/python", "-c",
            "import sys, os; print(sys.prefix); print(os.environ['VAT_ROOT'])",
        ]);
        let root = root_of(&env, "py");
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines[0], format!("{}/opt/venv", root.display()), "venv prefix is the container root");
        assert_eq!(lines[1], root.display().to_string());
        let activate = std::fs::read_to_string(root.join("opt/venv/bin/activate")).unwrap();
        assert!(activate.contains(&format!("{}/opt/venv", root.display())), "activate relocated");
        assert!(!activate.contains("@@VAT_ROOT@@"));
        // A console script whose shebang was relocated runs.
        let pip = env.ok(&["container", "run", "--rm", "demo:1", "/opt/venv/bin/pip", "--version"]);
        assert!(pip.starts_with("pip "), "{pip}");
        let state = inspect(&env, "py");
        assert!(state["relocation"]["applied"]["files"].as_u64().unwrap() >= 2, "{state}");
    }
    env.ok(&["container", "rm", "c1"]);
    if python {
        env.ok(&["container", "rm", "py"]);
    }
}

/// Evidence 2: a write inside the root is visible through inspect / diff /
/// `vat state` / `vat diff`.
#[test]
fn write_inside_root_is_visible_in_inspect_and_diff() {
    let env = Env::new();
    build_demo(&env, "demo:2");
    env.ok(&[
        "container", "run", "--name", "w", "demo:2", "sh", "-c",
        "echo data > \"$VAT_ROOT/app/out.txt\" && echo more >> \"$VAT_ROOT/app/bin/hello.sh\"",
    ]);
    let root = root_of(&env, "w");
    assert_eq!(std::fs::read_to_string(root.join("app/out.txt")).unwrap(), "data\n");

    let diff = env.json(&["container", "diff", "--json", "w"]);
    assert!(diff["added"].as_array().unwrap().iter().any(|p| p == "app/out.txt"), "{diff}");
    assert!(diff["modified"].as_array().unwrap().iter().any(|p| p == "app/bin/hello.sh"), "{diff}");

    let state = inspect(&env, "w");
    let id = state["id"].as_str().unwrap().to_string();
    assert!(state["changes"]["added"].as_array().unwrap().iter().any(|p| p == "app/out.txt"));
    assert_eq!(state["exit_code"], 0);

    let via_state = env.json(&["state", &id]);
    assert_eq!(via_state["id"], id.as_str());
    let via_diff = env.json(&["diff", &id, "--json"]);
    assert!(via_diff["added"].as_array().unwrap().iter().any(|p| p == "app/out.txt"));
    env.ok(&["container", "rm", "w"]);
    assert!(!root.exists());
}

/// Evidence 3: seatbelt denies writes outside the root (and outside rw
/// mounts); a read-only mount is not writable; a rw mount is.
#[test]
fn seatbelt_denies_writes_outside_the_root() {
    let env = Env::new();
    build_demo(&env, "demo:3");
    let outside = env.path().join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    let rw = env.path().join("rw");
    let ro = env.path().join("ro");
    std::fs::create_dir_all(&rw).unwrap();
    std::fs::create_dir_all(&ro).unwrap();
    std::fs::write(ro.join("input.txt"), "readable\n").unwrap();

    let target = outside.join("escape.txt");
    let script = format!("echo pwned > '{}'", target.display());
    let out = env.run(&["container", "run", "--rm", "demo:3", "sh", "-c", &script]);
    assert_ne!(out.status.code(), Some(0), "write outside the root must fail");
    assert!(String::from_utf8_lossy(&out.stderr).contains("Operation not permitted"), "{out:?}");
    assert!(!target.exists(), "nothing escaped the root");

    let mount_rw = format!("{}:/data", rw.display());
    let mount_ro = format!("{}:/input:ro", ro.display());
    let out = env.run(&[
        "container", "run", "--name", "m", "-v", &mount_rw, "-v", &mount_ro, "demo:3", "sh", "-c",
        "cat \"$VAT_ROOT/input/input.txt\" && echo ok > \"$VAT_ROOT/data/result.txt\" && ! echo no 2>/dev/null > \"$VAT_ROOT/input/new.txt\"",
    ]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(String::from_utf8_lossy(&out.stdout), "readable\n");
    assert_eq!(std::fs::read_to_string(rw.join("result.txt")).unwrap(), "ok\n");
    assert!(!ro.join("new.txt").exists(), "read-only mount stayed read-only");

    let state = inspect(&env, "m");
    assert_eq!(state["sandbox"]["backend"], "seatbelt");
    let writable: Vec<&str> = state["sandbox"]["writable"].as_array().unwrap().iter().map(|v| v.as_str().unwrap()).collect();
    assert_eq!(writable[0], state["root"].as_str().unwrap(), "root is the first writable path");
    assert!(writable.contains(&rw.to_str().unwrap()));
    assert!(!writable.contains(&ro.to_str().unwrap()));
    // Not root and no pool on CI hosts: isolation is reported, not claimed.
    if unsafe_euid() != 0 {
        assert_eq!(state["uid_isolation"], "unavailable");
        assert!(state["uid_isolation_reason"].as_str().unwrap().contains("not running as root"));
    }
    env.ok(&["container", "rm", "m"]);
    assert!(rw.join("result.txt").exists(), "rm never follows mount links into host data");
}

fn unsafe_euid() -> u32 {
    let out = Command::new("/usr/bin/id").arg("-u").output().unwrap();
    String::from_utf8_lossy(&out.stdout).trim().parse().unwrap()
}

const METAL_PROBE: &str = r#"
import ctypes
metal = ctypes.CDLL('/System/Library/Frameworks/Metal.framework/Metal')
objc = ctypes.CDLL('/usr/lib/libobjc.A.dylib')
metal.MTLCreateSystemDefaultDevice.restype = ctypes.c_void_p
dev = metal.MTLCreateSystemDefaultDevice()
if not dev:
    print('no-device'); raise SystemExit(0)
objc.sel_registerName.restype = ctypes.c_void_p
objc.sel_registerName.argtypes = [ctypes.c_char_p]
send = objc.objc_msgSend
send.restype = ctypes.c_void_p
send.argtypes = [ctypes.c_void_p, ctypes.c_void_p]
name = send(dev, objc.sel_registerName(b'name'))
send.restype = ctypes.c_char_p
print(send(name, objc.sel_registerName(b'UTF8String')).decode())
"#;

/// Evidence 4: GPU visibility inside a container equals the host's — both
/// vat's own report and a real Metal device query.
#[test]
fn gpu_visibility_equals_host() {
    let env = Env::new();
    build_demo(&env, "demo:4");
    let host_report = env.json(&["gpu", "--json"]);
    let inside = env.ok(&["container", "run", "--name", "g", "demo:4", VAT, "gpu", "--json"]);
    let inside: Value = serde_json::from_str(&inside).unwrap();
    assert_eq!(inside, host_report, "vat gpu inside == outside");
    assert_eq!(inspect(&env, "g")["gpu"], host_report, "inspect reports the host GPU");
    env.ok(&["container", "rm", "g"]);

    if !host_python() {
        eprintln!("skipping the Metal probe: /usr/bin/python3 is not usable on this host");
        return;
    }
    let host = Command::new("/usr/bin/python3").args(["-c", METAL_PROBE]).output().unwrap();
    assert!(host.status.success(), "{}", String::from_utf8_lossy(&host.stderr));
    let inside = env.ok(&["container", "run", "--rm", "demo:4", "/usr/bin/python3", "-c", METAL_PROBE]);
    assert_eq!(inside.trim(), String::from_utf8_lossy(&host.stdout).trim(), "same Metal device");
    if host_report["vendor"] == "apple" {
        assert_ne!(inside.trim(), "no-device");
    }
}

/// Evidence 5: detached lifecycle (ps, logs, exec, stop, rm) and exit codes.
#[test]
fn detached_lifecycle_and_exit_codes() {
    let env = Env::new();
    build_demo(&env, "demo:5");

    let out = env.run(&["container", "run", "--rm", "demo:5", "sh", "-c", "echo out; echo err >&2; exit 42"]);
    assert_eq!(out.status.code(), Some(42), "foreground exit code forwarded");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "out\n");
    assert!(String::from_utf8_lossy(&out.stderr).contains("err"));
    let out = env.run(&["container", "run", "--rm", "demo:5", "no-such-binary"]);
    assert_eq!(out.status.code(), Some(127));

    let id = env
        .ok(&["container", "run", "-d", "--name", "svc", "demo:5", "sh", "-c", "trap 'echo bye; exit 0' TERM; echo up; while :; do sleep 0.1; done"])
        .trim()
        .to_string();
    assert!(id.starts_with("ctr-"), "{id}");
    let ps = env.json(&["container", "ps", "--json"]);
    let row = ps.as_array().unwrap().iter().find(|r| r["id"] == id.as_str()).expect("running in ps");
    assert_eq!(row["status"], "running");
    let pid = row["pid"].as_i64().unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !env.ok(&["container", "logs", "svc"]).contains("up") {
        assert!(Instant::now() < deadline, "log never arrived");
        std::thread::sleep(Duration::from_millis(50));
    }
    let code = env.run(&["container", "exec", "svc", "sh", "-c", "exit 5"]).status.code();
    assert_eq!(code, Some(5), "exec forwards its exit code");
    env.ok(&["container", "exec", "-e", "X=1", "svc", "sh", "-c", "echo $X > \"$VAT_ROOT/exec.txt\""]);
    assert_eq!(std::fs::read_to_string(root_of(&env, "svc").join("exec.txt")).unwrap(), "1\n");

    let stop = env.ok(&["container", "stop", "-t", "5", "svc"]);
    let logs = env.ok(&["container", "logs", "svc"]);
    assert!(stop.contains("exited(0)"), "trapped TERM exits 0: {stop} logs={logs} state={}", inspect(&env, &id));
    let state = wait_exited(&env, &id);
    assert_eq!(state["exit_code"], 0);
    assert!(Command::new("/bin/kill").args(["-0", &pid.to_string()]).output().map(|o| !o.status.success()).unwrap());
    assert!(env.ok(&["container", "logs", "svc"]).contains("bye"));
    let ps = env.json(&["container", "ps", "--json"]);
    assert!(ps.as_array().unwrap().iter().all(|r| r["id"] != id.as_str()), "not listed as running");
    let all = env.json(&["container", "ps", "--all", "--json"]);
    assert!(all.as_array().unwrap().iter().any(|r| r["id"] == id.as_str() && r["status"] == "exited"));

    // A workload that ignores TERM is killed after the timeout (137).
    env.ok(&["container", "run", "-d", "--name", "stubborn", "demo:5", "sh", "-c", "trap '' TERM; echo ready; while :; do sleep 0.1; done"]);
    let deadline = Instant::now() + Duration::from_secs(10);
    while !env.ok(&["container", "logs", "stubborn"]).contains("ready") {
        assert!(Instant::now() < deadline, "stubborn never became ready");
        std::thread::sleep(Duration::from_millis(50));
    }
    let stop = env.ok(&["container", "stop", "-t", "1", "stubborn"]);
    assert!(stop.contains("exited(137)"), "{stop}");

    // A detached workload's own exit code is recorded.
    env.ok(&["container", "run", "-d", "--name", "quick", "demo:5", "sh", "-c", "exit 3"]);
    assert_eq!(wait_exited(&env, "quick")["exit_code"], 3);

    // rm refuses a running container without --force.
    env.ok(&["container", "run", "-d", "--name", "live", "demo:5", "sh", "-c", "while :; do sleep 0.1; done"]);
    assert!(!env.run(&["container", "rm", "live"]).status.success());
    env.ok(&["container", "rm", "--force", "live", "svc", "stubborn", "quick"]);
    let all = env.json(&["container", "ps", "--all", "--json"]);
    assert_eq!(all.as_array().unwrap().len(), 0, "{all}");

    // Detached --rm removes itself after exit.
    env.ok(&["container", "run", "-d", "--rm", "--name", "gone", "demo:5", "true"]);
    let deadline = Instant::now() + Duration::from_secs(10);
    while env.json(&["container", "ps", "--all", "--json"]).as_array().unwrap().iter().any(|r| r["name"] == "gone") {
        assert!(Instant::now() < deadline, "--rm container lingered");
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[cfg(feature = "registry")]
mod registry {
    use super::*;
    use base64_encode::encode as b64;

    mod base64_encode {
        /// Tiny standard base64 encoder (keeps the test free of extra deps).
        pub fn encode(input: &[u8]) -> String {
            const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
            let mut out = String::new();
            for chunk in input.chunks(3) {
                let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
                let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
                for i in 0..4 {
                    if i <= chunk.len() {
                        out.push(T[((n >> (18 - 6 * i)) & 63) as usize] as char);
                    } else {
                        out.push('=');
                    }
                }
            }
            out
        }
    }

    /// Start an in-process registry on 127.0.0.1; returns `host:port`.
    fn start(root: PathBuf, auth: vat::registry::Auth) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            rt.block_on(async move {
                let listener = tokio::net::TcpListener::from_std(listener).unwrap();
                vat::registry::serve(listener, vat::registry::Config { root, auth }).await.unwrap();
            });
        });
        format!("127.0.0.1:{}", addr.port())
    }

    fn sha256(bytes: &[u8]) -> String {
        // The vat binary verifies digests; here we only need a reference value.
        let out = Command::new("/usr/bin/shasum")
            .args(["-a", "256"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .and_then(|mut child| {
                use std::io::Write;
                child.stdin.take().unwrap().write_all(bytes)?;
                child.wait_with_output()
            })
            .unwrap();
        format!("sha256:{}", String::from_utf8_lossy(&out.stdout).split_whitespace().next().unwrap())
    }

    /// Evidence 6: push → rm → pull → run against an in-test registry with
    /// Basic auth from the Docker config, plus the registry protocol surface
    /// (chunked uploads, HEAD/GET by tag and digest, tags/list, _catalog).
    #[test]
    fn push_pull_round_trip_against_in_test_registry() {
        let env = Env::new();
        let digest = build_demo(&env, "demo:6");
        let host = start(
            env.path().join("registry"),
            vat::registry::Auth::Basic { user: "alice".into(), password: "s3cret".into() },
        );
        let remote = format!("{host}/team/demo:v1");

        // Without credentials the registry refuses.
        let out = env.run(&["image", "push", "demo:6", &remote]);
        assert!(!out.status.success());
        assert!(String::from_utf8_lossy(&out.stderr).contains("Basic credentials"), "{out:?}");

        std::fs::create_dir_all(env.path().join("docker")).unwrap();
        let config = serde_json::json!({ "auths": { host.clone(): { "auth": b64(b"alice:s3cret") } } });
        std::fs::write(env.path().join("docker/config.json"), config.to_string()).unwrap();

        let pushed = env.json(&["image", "push", "demo:6", &remote, "--json"]);
        assert_eq!(pushed["digest"], digest.as_str());
        assert!(pushed["uploaded_blobs"].as_u64().unwrap() >= 2);
        let again = env.json(&["image", "push", "demo:6", &remote, "--json"]);
        assert_eq!(again["uploaded_blobs"], 0, "second push only HEADs blobs");

        env.ok(&["image", "rm", "demo:6"]);
        assert_eq!(env.json(&["image", "ls", "--json"]).as_array().unwrap().len(), 0);
        let pulled = env.json(&["image", "pull", &remote, "--json"]);
        assert_eq!(pulled["digest"], digest.as_str(), "pull restores the pushed manifest");
        assert_eq!(pulled["tagged"], remote.as_str());
        let out = env.ok(&["container", "run", "--rm", &remote, "where.sh"]);
        assert!(out.starts_with("baked=") && !out.contains("@@VAT_ROOT@@"), "{out}");
        let by_digest = format!("{host}/team/demo@{digest}");
        let pulled = env.json(&["image", "pull", &by_digest, "--json"]);
        assert_eq!(pulled["digest"], digest.as_str());

        // Protocol surface, spoken directly.
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let http = reqwest::Client::new();
            let base = format!("http://{host}");
            let auth = format!("Basic {}", b64(b"alice:s3cret"));
            let v2 = http.get(format!("{base}/v2/")).send().await.unwrap();
            assert_eq!(v2.status(), 401);
            assert!(v2.headers()["www-authenticate"].to_str().unwrap().starts_with("Basic"));
            let v2 = http.get(format!("{base}/v2/")).header("authorization", &auth).send().await.unwrap();
            assert_eq!(v2.status(), 200);
            assert_eq!(v2.headers()["docker-distribution-api-version"], "registry/2.0");

            // Chunked upload: POST, PATCH x2 with Content-Range, PUT ?digest=.
            let blob = b"hello chunked world".to_vec();
            let blob_digest = sha256(&blob);
            let start = http.post(format!("{base}/v2/team/other/blobs/uploads/")).header("authorization", &auth).send().await.unwrap();
            assert_eq!(start.status(), 202);
            let location = start.headers()["location"].to_str().unwrap().to_string();
            let (a, b) = blob.split_at(5);
            let patch = http
                .patch(format!("{base}{location}"))
                .header("authorization", &auth)
                .header("content-range", "0-4")
                .body(a.to_vec())
                .send()
                .await
                .unwrap();
            assert_eq!(patch.status(), 202);
            assert_eq!(patch.headers()["range"], "0-4");
            let bad = http
                .patch(format!("{base}{location}"))
                .header("authorization", &auth)
                .header("content-range", "0-4")
                .body(a.to_vec())
                .send()
                .await
                .unwrap();
            assert_eq!(bad.status(), 416, "out-of-order chunk rejected");
            let patch = http
                .patch(format!("{base}{location}"))
                .header("authorization", &auth)
                .header("content-range", format!("5-{}", blob.len() - 1))
                .body(b.to_vec())
                .send()
                .await
                .unwrap();
            assert_eq!(patch.status(), 202);
            let wrong = format!("sha256:{}", "0".repeat(64));
            let status = http.get(format!("{base}{location}")).header("authorization", &auth).send().await.unwrap();
            assert_eq!(status.status(), 204);
            let put = http
                .put(format!("{base}{location}?digest={}", blob_digest.replace(':', "%3A")))
                .header("authorization", &auth)
                .send()
                .await
                .unwrap();
            assert_eq!(put.status(), 201);
            assert_eq!(put.headers()["docker-content-digest"], blob_digest.as_str());
            let head = http.head(format!("{base}/v2/team/other/blobs/{blob_digest}")).header("authorization", &auth).send().await.unwrap();
            assert_eq!(head.status(), 200);
            assert_eq!(head.headers()["content-length"], blob.len().to_string().as_str());
            let get = http.get(format!("{base}/v2/team/other/blobs/{blob_digest}")).header("authorization", &auth).send().await.unwrap();
            assert_eq!(get.bytes().await.unwrap().to_vec(), blob);
            let missing = http.head(format!("{base}/v2/team/other/blobs/{wrong}")).header("authorization", &auth).send().await.unwrap();
            assert_eq!(missing.status(), 404);

            // Monolithic upload with a wrong digest is rejected.
            let mono = http
                .post(format!("{base}/v2/team/other/blobs/uploads/?digest={wrong}"))
                .header("authorization", &auth)
                .body(b"x".to_vec())
                .send()
                .await
                .unwrap();
            assert_eq!(mono.status(), 400);

            // Manifests by tag and digest; tags/list; _catalog.
            let manifest = http
                .get(format!("{base}/v2/team/demo/manifests/v1"))
                .header("authorization", &auth)
                .send()
                .await
                .unwrap();
            assert_eq!(manifest.status(), 200);
            assert_eq!(manifest.headers()["content-type"], "application/vnd.oci.image.manifest.v1+json");
            assert_eq!(manifest.headers()["docker-content-digest"], digest.as_str());
            let head = http
                .head(format!("{base}/v2/team/demo/manifests/{digest}"))
                .header("authorization", &auth)
                .send()
                .await
                .unwrap();
            assert_eq!(head.status(), 200);
            let tags: Value = http
                .get(format!("{base}/v2/team/demo/tags/list"))
                .header("authorization", &auth)
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            assert_eq!(tags["tags"], serde_json::json!(["v1"]));
            let catalog: Value = http
                .get(format!("{base}/v2/_catalog"))
                .header("authorization", &auth)
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            assert_eq!(catalog["repositories"], serde_json::json!(["team/demo"]));
            // A manifest referencing an unknown blob is refused.
            let bogus = serde_json::json!({
                "schemaVersion": 2,
                "mediaType": "application/vnd.oci.image.manifest.v1+json",
                "config": { "mediaType": "application/vnd.oci.image.config.v1+json", "digest": wrong, "size": 1 },
                "layers": []
            });
            let put = http
                .put(format!("{base}/v2/team/demo/manifests/bogus"))
                .header("authorization", &auth)
                .header("content-type", "application/vnd.oci.image.manifest.v1+json")
                .body(bogus.to_string())
                .send()
                .await
                .unwrap();
            assert_eq!(put.status(), 400);
        });
    }

    /// Bearer token flow: the client answers the challenge via the realm.
    #[test]
    fn push_pull_with_bearer_token_auth() {
        let env = Env::new();
        let digest = build_demo(&env, "demo:7");
        let host = start(
            env.path().join("registry"),
            vat::registry::Auth::Bearer { user: "bob".into(), password: "pw".into() },
        );
        std::fs::create_dir_all(env.path().join("docker")).unwrap();
        let config = serde_json::json!({ "auths": { format!("http://{host}"): { "username": "bob", "password": "pw" } } });
        std::fs::write(env.path().join("docker/config.json"), config.to_string()).unwrap();
        let remote = format!("{host}/demo:latest");
        env.ok(&["image", "tag", "demo:7", &remote]);
        let pushed = env.json(&["image", "push", &remote, "--json"]);
        assert_eq!(pushed["digest"], digest.as_str());
        env.ok(&["image", "rm", "demo:7", &remote]);
        let pulled = env.json(&["image", "pull", &format!("{host}/demo"), "--json"]);
        assert_eq!(pulled["digest"], digest.as_str());
    }
}
