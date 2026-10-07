//! Native container lifecycle: create (clone + relocate), run (foreground or
//! detached under a supervisor), ps, logs, exec, stop, rm, inspect, diff.
//!
//! There is no daemon. A container is a directory under
//! `<native home>/containers/<id>/` (record, base manifest, logs) plus a
//! fixed-length root. Liveness is the recorded workload pid **and** its
//! kernel start time, so a recycled pid is never mistaken for the workload.
//! A detached container is supervised by a hidden `vat container __supervise`
//! process (its own session) that owns the log files and records the exit
//! code; the workload runs in its own process group so `stop` can signal the
//! whole tree with `killpg`.

use std::collections::{BTreeMap, BTreeSet};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use super::layer::{self, ScanOptions, Tree};
use super::oci::ContainerConfig;
use super::root::{self, RelocStats, RootKind};
use super::store::{ImageStore, LocalImage};
use super::users::{self, Decision};
use crate::sandbox::seatbelt;
use crate::spec::EgressPolicy;

/// Host system directories appended to every container `PATH`.
pub const HOST_SYSTEM_PATH: &str = "/usr/bin:/bin:/usr/sbin:/sbin";
const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";
/// Default grace period between SIGTERM and SIGKILL for `stop`.
pub const DEFAULT_STOP_TIMEOUT_S: u64 = 10;

/// Network mode (`--network`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Network {
    /// Host network stack (the process is a host process).
    #[default]
    Host,
    /// Seatbelt denies all network operations.
    None,
}

impl Network {
    pub fn parse(raw: &str) -> Result<Self> {
        match raw {
            "host" => Ok(Network::Host),
            "none" => Ok(Network::None),
            other => bail!("unsupported --network {other:?} (native containers support `host` and `none`)"),
        }
    }

    fn egress(self) -> EgressPolicy {
        match self {
            Network::Host => EgressPolicy::Open,
            Network::None => EgressPolicy::Deny,
        }
    }
}

/// A `-v HOST:CTR[:ro]` bind, realized as a symlink at `$VAT_ROOT/CTR`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MountRecord {
    /// Canonical host path.
    pub host: PathBuf,
    /// In-container absolute path.
    pub container: String,
    /// The symlink created inside the root.
    pub link: PathBuf,
    pub read_only: bool,
}

/// Parse `HOST:CTR[:ro|rw]`.
pub fn parse_mount(spec: &str) -> Result<(PathBuf, String, bool)> {
    let parts: Vec<&str> = spec.split(':').collect();
    let (host, ctr, ro) = match parts.as_slice() {
        [host, ctr] => (*host, *ctr, false),
        [host, ctr, "ro"] => (*host, *ctr, true),
        [host, ctr, "rw"] => (*host, *ctr, false),
        _ => bail!("invalid -v {spec:?}: expected HOST:CONTAINER[:ro]"),
    };
    if !ctr.starts_with('/') {
        bail!("invalid -v {spec:?}: the container path must be absolute");
    }
    let host = std::fs::canonicalize(host)
        .with_context(|| format!("-v {spec:?}: host path {host:?} does not exist"))?;
    root::sanitize_rel(ctr).with_context(|| format!("invalid -v {spec:?}"))?;
    Ok((host, ctr.to_string(), ro))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageRef {
    pub reference: String,
    pub digest: String,
    pub config_digest: String,
}

/// Summary of the seatbelt confinement actually applied.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxSummary {
    pub backend: String,
    /// sha256 of the exact profile text passed to `sandbox-exec -p`.
    pub profile_sha256: String,
    /// Subpaths where writes are allowed (the root first).
    pub writable: Vec<PathBuf>,
    /// Reads are not confined (dyld, frameworks, Metal need host reads).
    pub reads: String,
    pub network: Network,
}

/// Who the workload runs as.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunAs {
    pub uid: u32,
    pub gid: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
}

/// Persistent container record (`container.json`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContainerRecord {
    pub id: String,
    pub name: String,
    pub image: ImageRef,
    pub root: PathBuf,
    /// Requested command (after image Entrypoint/Cmd resolution, before
    /// root-prefixing of argv0).
    pub command: Vec<String>,
    /// argv actually executed under `sandbox-exec`.
    pub argv: Vec<String>,
    pub workdir: PathBuf,
    pub env: Vec<String>,
    pub mounts: Vec<MountRecord>,
    pub network: Network,
    pub detached: bool,
    pub auto_remove: bool,
    pub created_at: String,
    /// `"active"` or `"unavailable"`.
    pub uid_isolation: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uid_isolation_reason: Option<String>,
    pub run_as: RunAs,
    pub sandbox: SandboxSummary,
    pub relocation: RelocStats,
    /// Relocation entries recorded in the image.
    pub image_relocations: usize,
}

/// Runtime state written once the workload has started (`started.json`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Started {
    pub pid: i32,
    pub pgid: i32,
    /// Kernel start time of `pid` (µs since epoch) for pid-reuse-safe liveness.
    pub start_time_us: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supervisor_pid: Option<i32>,
    pub started_at: String,
}

/// Exit state (`exit.json`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Exited {
    /// Shell-style code: the exit status, or 128+signal.
    pub exit_code: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal: Option<i32>,
    pub finished_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Created,
    Running,
    Exited,
}

/// A container as loaded from disk with its derived status.
#[derive(Debug, Clone)]
pub struct Container {
    pub dir: PathBuf,
    pub record: ContainerRecord,
    pub started: Option<Started>,
    pub exited: Option<Exited>,
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(
            serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))?,
        )),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err).with_context(|| format!("read {}", path.display())),
    }
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    super::write_atomic(path, &serde_json::to_vec_pretty(value)?)
}

/// Kernel start time of `pid` in µs since the epoch.
pub fn process_start_time(pid: i32) -> Option<u64> {
    #[cfg(target_os = "macos")]
    {
        let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
        let rc = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDTBSDINFO,
                0,
                &mut info as *mut _ as *mut libc::c_void,
                size,
            )
        };
        if rc != size {
            return None;
        }
        Some(info.pbi_start_tvsec * 1_000_000 + info.pbi_start_tvusec)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = pid;
        None
    }
}

fn pid_exists(pid: i32) -> bool {
    let rc = unsafe { libc::kill(pid, 0) };
    rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

impl Container {
    pub fn status(&self) -> Status {
        if self.exited.is_some() {
            return Status::Exited;
        }
        match &self.started {
            None => Status::Created,
            Some(started) => {
                if self.workload_alive(started) {
                    Status::Running
                } else {
                    Status::Exited
                }
            }
        }
    }

    fn workload_alive(&self, started: &Started) -> bool {
        match (started.start_time_us, process_start_time(started.pid)) {
            (Some(recorded), Some(actual)) => recorded == actual,
            // Could not read the start time (e.g. another user's process):
            // fall back to existence.
            (_, None) => pid_exists(started.pid),
            (None, Some(_)) => true,
        }
    }

    pub fn exit_code(&self) -> Option<i32> {
        self.exited.as_ref().map(|e| e.exit_code)
    }

    pub fn stdout_log(&self) -> PathBuf {
        self.dir.join("stdout.log")
    }

    pub fn stderr_log(&self) -> PathBuf {
        self.dir.join("stderr.log")
    }

    fn base_manifest_path(&self) -> PathBuf {
        self.dir.join("base-manifest.json")
    }

    /// Filesystem changes in the root since create (after relocation).
    pub fn changes(&self) -> Result<crate::state::ChangeSet> {
        let base: Tree = read_json(&self.base_manifest_path())?.unwrap_or_default();
        let now = layer::scan_tree(&self.record.root, &ScanOptions::default())?;
        let diff = layer::diff(&base, &now);
        let keep = |path: &String, tree: &Tree| {
            tree.get(path).is_some_and(|e| e.kind != layer::EntryKind::Dir)
        };
        Ok(crate::state::ChangeSet {
            added: diff.added.into_iter().filter(|p| keep(p, &now)).collect(),
            modified: diff.modified.into_iter().filter(|p| keep(p, &now)).collect(),
            deleted: diff.deleted,
        })
    }

    /// The full agent-facing JSON projection (`container inspect`, `vat state`).
    pub fn inspect(&self) -> Result<serde_json::Value> {
        let r = &self.record;
        let changes = self.changes().ok();
        Ok(serde_json::json!({
            "kind": "native-container",
            "id": r.id,
            "name": r.name,
            "status": self.status(),
            "image": r.image,
            "root": r.root,
            "root_length": r.root.as_os_str().len(),
            "command": r.command,
            "argv": r.argv,
            "workdir": r.workdir,
            "env": r.env,
            "mounts": r.mounts,
            "network": r.network,
            "detached": r.detached,
            "auto_remove": r.auto_remove,
            "created_at": r.created_at,
            "started": self.started,
            "exited": self.exited,
            "exit_code": self.exit_code(),
            "uid_isolation": r.uid_isolation,
            "uid_isolation_reason": r.uid_isolation_reason,
            "run_as": r.run_as,
            "sandbox": r.sandbox,
            "relocation": {
                "image_entries": r.image_relocations,
                "applied": r.relocation,
            },
            "changes": changes.as_ref().map(|c| serde_json::json!({
                "total": c.total(),
                "added": c.added,
                "modified": c.modified,
                "deleted": c.deleted,
            })),
            "gpu": crate::gpu::detect(),
            "logs": {
                "stdout": self.stdout_log(),
                "stderr": self.stderr_log(),
            },
        }))
    }
}

/// Load one container directory.
fn load_dir(dir: &Path) -> Result<Container> {
    let record: ContainerRecord = read_json(&dir.join("container.json"))?
        .with_context(|| format!("{} has no container.json", dir.display()))?;
    Ok(Container {
        dir: dir.to_path_buf(),
        record,
        started: read_json(&dir.join("started.json"))?,
        exited: read_json(&dir.join("exit.json"))?,
    })
}

/// All containers (unreadable records are skipped with a warning).
pub fn list() -> Result<Vec<Container>> {
    let dir = super::containers_dir()?;
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Ok(out);
    };
    for entry in entries.flatten() {
        if !entry.path().join("container.json").is_file() {
            continue;
        }
        match load_dir(&entry.path()) {
            Ok(container) => out.push(container),
            Err(err) => eprintln!("vat: skipping {}: {err:#}", entry.path().display()),
        }
    }
    out.sort_by(|a, b| a.record.created_at.cmp(&b.record.created_at));
    Ok(out)
}

/// Find a container by id, unique id prefix, or name.
pub fn find(key: &str) -> Result<Container> {
    let all = list()?;
    if let Some(found) = all.iter().find(|c| c.record.id == key || c.record.name == key) {
        return Ok(found.clone());
    }
    let matches: Vec<&Container> = all
        .iter()
        .filter(|c| key.len() >= 4 && c.record.id.starts_with(key))
        .collect();
    match matches.len() {
        1 => Ok(matches[0].clone()),
        0 => bail!("no native container {key:?} (see `vat container ps --all`)"),
        _ => bail!("container id prefix {key:?} is ambiguous"),
    }
}

/// Is `id` shaped like a native container id (`ctr-…`)?
pub fn looks_like_id(id: &str) -> bool {
    id.starts_with("ctr-")
}

/// The invoking user's per-user cache dir (`getconf DARWIN_USER_CACHE_DIR`),
/// canonicalized. Metal and other system frameworks write shader/compile
/// caches there, so the seatbelt profile allows it.
pub fn user_cache_dir() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        let mut buf = vec![0u8; 1024];
        let n = unsafe {
            libc::confstr(
                libc::_CS_DARWIN_USER_CACHE_DIR,
                buf.as_mut_ptr() as *mut libc::c_char,
                buf.len(),
            )
        };
        if n == 0 || n > buf.len() {
            return None;
        }
        buf.truncate(n - 1);
        let path = PathBuf::from(String::from_utf8(buf).ok()?);
        std::fs::canonicalize(path).ok()
    }
    #[cfg(not(target_os = "macos"))]
    {
        None
    }
}

/// Replace `$VAT_ROOT` / `${VAT_ROOT}` (not `$VAT_ROOTX`) with `root`.
pub fn substitute_root(value: &str, root: &str) -> String {
    let value = value.replace("${VAT_ROOT}", root);
    let mut out = String::with_capacity(value.len());
    let mut rest = value.as_str();
    while let Some(idx) = rest.find("$VAT_ROOT") {
        out.push_str(&rest[..idx]);
        let after = &rest[idx + "$VAT_ROOT".len()..];
        let continues = after
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_');
        if continues {
            out.push_str("$VAT_ROOT");
        } else {
            out.push_str(root);
        }
        rest = after;
    }
    out.push_str(rest);
    out
}

fn root_join(root: &Path, in_image: &str) -> PathBuf {
    let rel = in_image.trim_start_matches('/');
    if rel.is_empty() {
        root.to_path_buf()
    } else {
        root.join(rel)
    }
}

fn split_env(entry: &str) -> (String, String) {
    match entry.split_once('=') {
        Some((k, v)) => (k.to_string(), v.to_string()),
        None => (entry.to_string(), String::new()),
    }
}

/// Build the workload environment for `root` from the image env plus `-e`
/// overrides, following the native env contract.
pub fn runtime_env(root: &Path, image_env: &[String], overrides: &[String]) -> Result<Vec<(String, String)>> {
    let root_str = root.to_str().context("root must be UTF-8")?;
    let mut env: BTreeMap<String, String> = BTreeMap::new();
    for key in ["TERM", "LANG", "USER", "LOGNAME"] {
        if let Ok(value) = std::env::var(key) {
            env.insert(key.into(), value);
        }
    }
    for (key, value) in std::env::vars() {
        if key.starts_with("LC_") {
            env.insert(key, value);
        }
    }
    for entry in image_env {
        let (key, value) = split_env(entry);
        let value = match key.as_str() {
            // Absolute PATH entries are root-relative.
            "PATH" => value
                .split(':')
                .filter(|p| !p.is_empty())
                .map(|p| {
                    if p.starts_with('/') {
                        root_join(root, p).display().to_string()
                    } else {
                        p.to_string()
                    }
                })
                .collect::<Vec<_>>()
                .join(":"),
            "HOME" if value.starts_with('/') && !value.starts_with(root_str) => {
                root_join(root, &value).display().to_string()
            }
            _ => value,
        };
        env.insert(key, substitute_root(&value, root_str));
    }
    env.entry("HOME".into()).or_insert_with(|| root.join("root").display().to_string());
    env.insert("TMPDIR".into(), root.join("tmp").display().to_string());
    for entry in overrides {
        let (key, value) = match entry.split_once('=') {
            Some((k, v)) => (k.to_string(), v.to_string()),
            None => match std::env::var(entry) {
                Ok(v) => (entry.clone(), v),
                Err(_) => continue,
            },
        };
        if key.is_empty() {
            bail!("invalid -e {entry:?}: empty variable name");
        }
        if key == "VAT_ROOT" {
            bail!("-e VAT_ROOT is reserved: vat sets it to the container root");
        }
        env.insert(key, substitute_root(&value, root_str));
    }
    let path = match env.get("PATH").filter(|p| !p.is_empty()) {
        Some(path) => format!("{path}:{HOST_SYSTEM_PATH}"),
        None => HOST_SYSTEM_PATH.to_string(),
    };
    env.insert("PATH".into(), path);
    env.insert("VAT_ROOT".into(), root_str.to_string());
    Ok(env.into_iter().collect())
}

/// Resolve argv from Entrypoint + (override or Cmd), mapping an absolute
/// argv0 that exists inside the root to its root path.
pub fn resolve_argv(root: &Path, config: &ContainerConfig, command: &[String]) -> Result<(Vec<String>, Vec<String>)> {
    let mut requested: Vec<String> = config.entrypoint.clone().unwrap_or_default();
    if command.is_empty() {
        requested.extend(config.cmd.clone().unwrap_or_default());
    } else {
        requested.extend(command.iter().cloned());
    }
    if requested.is_empty() {
        bail!("no command: the image has no Entrypoint/Cmd and none was given");
    }
    let mut argv = requested.clone();
    if argv[0].starts_with('/') {
        let in_root = root_join(root, &argv[0]);
        if std::fs::symlink_metadata(&in_root).is_ok() {
            argv[0] = in_root.display().to_string();
        }
    }
    let root_str = root.to_str().context("root must be UTF-8")?;
    for arg in argv.iter_mut().skip(1) {
        *arg = substitute_root(arg, root_str);
    }
    Ok((requested, argv))
}

/// Find `argv0` the way `execvp` would (relative to `workdir` when it has a
/// slash, else on the env's PATH). `None` means the exec would fail with
/// ENOENT; callers report exit code 127 like a shell.
pub fn find_executable(argv0: &str, env: &[String], workdir: &Path) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let executable = |p: &Path| {
        std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    };
    if argv0.contains('/') {
        let path = workdir.join(argv0);
        return executable(&path).then_some(path);
    }
    let path_var = env
        .iter()
        .find_map(|e| e.strip_prefix("PATH="))
        .unwrap_or(HOST_SYSTEM_PATH);
    path_var
        .split(':')
        .filter(|dir| !dir.is_empty())
        .map(|dir| workdir.join(dir).join(argv0))
        .find(|candidate| executable(candidate))
}

fn not_found_message(argv0: &str) -> String {
    format!("{argv0}: executable not found (checked the container PATH, which is root-relative plus the host system dirs)")
}

/// Working directory: image `WorkingDir` (or `-w`) relative to the root.
pub fn resolve_workdir(root: &Path, config: &ContainerConfig, override_dir: Option<&str>) -> PathBuf {
    match override_dir.or(config.working_dir.as_deref()) {
        Some(dir) if !dir.is_empty() => root_join(root, dir),
        _ => root.to_path_buf(),
    }
}

/// Options for `vat container run`.
#[derive(Debug, Clone, Default)]
pub struct RunOptions {
    pub name: Option<String>,
    pub detach: bool,
    pub env: Vec<String>,
    pub volumes: Vec<String>,
    pub network: Network,
    pub auto_remove: bool,
    pub workdir: Option<String>,
    pub image: String,
    pub command: Vec<String>,
}

fn validate_name(name: &str) -> Result<()> {
    let ok = name
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphanumeric())
        && name.chars().all(|c| c.is_ascii_alphanumeric() || "_.-".contains(c))
        && name.len() <= 128;
    if !ok {
        bail!("invalid container name {name:?} ([a-zA-Z0-9][a-zA-Z0-9_.-]*)");
    }
    Ok(())
}

/// The seatbelt profile for a container (or an exec into it).
fn profile_for(record: &ContainerRecord) -> String {
    let writable: Vec<PathBuf> = record.sandbox.writable.iter().skip(1).cloned().collect();
    seatbelt::native_container_profile(&record.root, &writable, record.network.egress())
}

/// UIDs of pool users held by live containers.
fn pool_uids_in_use() -> BTreeSet<u32> {
    list()
        .unwrap_or_default()
        .into_iter()
        .filter(|c| c.record.uid_isolation == "active" && c.status() != Status::Exited)
        .map(|c| c.record.run_as.uid)
        .collect()
}

/// Create a container: materialize the image snapshot, clone it into a fresh
/// fixed-length root, relocate, link mounts, decide the UID, and record it.
pub fn create(opts: &RunOptions) -> Result<Container> {
    if !seatbelt::available() && !Path::new(SANDBOX_EXEC).exists() {
        bail!("sandbox-exec is unavailable on this host; native containers fail closed without seatbelt");
    }
    let store = ImageStore::open()?;
    let image: LocalImage = store.resolve(&opts.image)?;
    let existing = list()?;
    let id = format!("ctr-{}", super::random_hex(6));
    let name = match &opts.name {
        Some(name) => {
            validate_name(name)?;
            if existing.iter().any(|c| &c.record.name == name) {
                bail!("a native container named {name:?} already exists (remove it with `vat container rm {name}`)");
            }
            name.clone()
        }
        None => id.clone(),
    };
    let mut mounts_spec = Vec::new();
    for spec in &opts.volumes {
        mounts_spec.push(parse_mount(spec)?);
    }

    let snapshot = store.snapshot(&image)?;
    let base = super::roots_base()?;
    let (_root_id, root_path) = root::allocate(&base, RootKind::Container)?;
    let dir = super::containers_dir()?.join(&id);
    std::fs::create_dir_all(&dir)?;
    let result = (|| -> Result<Container> {
        super::clone_tree(&snapshot, &root_path)
            .with_context(|| format!("clone snapshot into {}", root_path.display()))?;
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&root_path, std::fs::Permissions::from_mode(0o755))?;
        }
        let relocations = image.relocations()?;
        let stats = root::relocate(
            &root_path,
            &relocations,
            root::placeholder().as_bytes(),
            root_path.as_os_str().as_encoded_bytes(),
        )?;
        std::fs::create_dir_all(root_path.join("tmp"))?;

        let mut mounts = Vec::new();
        for (host, ctr, read_only) in mounts_spec {
            let link = root_join(&root_path, &ctr);
            if let Ok(meta) = std::fs::symlink_metadata(&link) {
                if meta.is_dir() && std::fs::read_dir(&link)?.next().is_none() {
                    std::fs::remove_dir(&link)?;
                } else if meta.file_type().is_symlink() {
                    std::fs::remove_file(&link)?;
                } else {
                    bail!("-v target {ctr} already exists in the image and is not an empty directory");
                }
            }
            if let Some(parent) = link.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::os::unix::fs::symlink(&host, &link)
                .with_context(|| format!("link {} -> {}", link.display(), host.display()))?;
            mounts.push(MountRecord { host, container: ctr, link, read_only });
        }

        let decision = users::decide(
            users::euid(),
            &users::discover_pool(),
            &pool_uids_in_use(),
            users::world_traversable(&root_path),
        );
        let (uid_isolation, reason, run_as) = match decision {
            Decision::Active(user) => {
                ("active", None, RunAs { uid: user.uid, gid: user.gid, user: Some(user.name) })
            }
            Decision::RunAsInvoker { reason } => (
                "unavailable",
                Some(reason),
                RunAs { uid: unsafe { libc::getuid() }, gid: unsafe { libc::getgid() }, user: std::env::var("USER").ok() },
            ),
            Decision::Refuse { reason } => bail!("{reason}"),
        };

        let mut writable = vec![root_path.clone()];
        writable.extend(mounts.iter().filter(|m| !m.read_only).map(|m| m.host.clone()));
        if uid_isolation != "active" {
            if let Some(cache) = user_cache_dir() {
                writable.push(cache);
            }
        }
        let profile = seatbelt::native_container_profile(
            &root_path,
            &writable[1..],
            opts.network.egress(),
        );
        let config = &image.config.config;
        let env = runtime_env(&root_path, &config.env, &opts.env)?;
        let (command, argv) = resolve_argv(&root_path, config, &opts.command)?;
        let workdir = resolve_workdir(&root_path, config, opts.workdir.as_deref());
        std::fs::create_dir_all(&workdir)
            .with_context(|| format!("create working dir {}", workdir.display()))?;
        if let Some((_, home)) = env.iter().find(|(k, _)| k == "HOME") {
            if Path::new(home).starts_with(&root_path) {
                let _ = std::fs::create_dir_all(home);
            }
        }
        if uid_isolation == "active" {
            users::chown_tree(&root_path, run_as.uid, run_as.gid)?;
        }

        let record = ContainerRecord {
            id: id.clone(),
            name,
            image: ImageRef {
                reference: image.reference.clone().unwrap_or_else(|| opts.image.clone()),
                digest: image.manifest_digest.clone(),
                config_digest: image.manifest.config.digest.clone(),
            },
            root: root_path.clone(),
            command,
            argv,
            workdir,
            env: env.iter().map(|(k, v)| format!("{k}={v}")).collect(),
            mounts,
            network: opts.network,
            detached: opts.detach,
            auto_remove: opts.auto_remove,
            created_at: now(),
            uid_isolation: uid_isolation.to_string(),
            uid_isolation_reason: reason,
            run_as,
            sandbox: SandboxSummary {
                backend: "seatbelt".into(),
                profile_sha256: super::oci::sha256_digest(profile.as_bytes()),
                writable,
                reads: "unrestricted".into(),
                network: opts.network,
            },
            relocation: stats,
            image_relocations: relocations.len(),
        };
        let tree = layer::scan_tree(&root_path, &ScanOptions::default())?;
        write_json(&dir.join("base-manifest.json"), &tree)?;
        std::fs::write(dir.join("profile.sb"), &profile)?;
        write_json(&dir.join("container.json"), &record)?;
        Ok(Container { dir: dir.clone(), record, started: None, exited: None })
    })();
    if result.is_err() {
        let _ = super::remove_tree(&root_path);
        let _ = super::remove_tree(&dir);
    }
    result
}

/// How the workload's stdio is wired.
enum Stdio3 {
    Inherit,
    Files { stdout: std::fs::File, stderr: std::fs::File },
}

/// Build the `sandbox-exec` command for a container workload or exec.
fn workload_command(
    record: &ContainerRecord,
    argv: &[String],
    env: &[String],
    workdir: &Path,
    stdio: Stdio3,
    foreground_tty: bool,
) -> Command {
    let profile = profile_for(record);
    let mut cmd = Command::new(SANDBOX_EXEC);
    cmd.arg("-p").arg(&profile).args(argv);
    cmd.env_clear();
    for entry in env {
        let (k, v) = split_env(entry);
        cmd.env(k, v);
    }
    cmd.current_dir(workdir);
    match stdio {
        Stdio3::Inherit => {}
        Stdio3::Files { stdout, stderr } => {
            cmd.stdin(Stdio::null()).stdout(stdout).stderr(stderr);
        }
    }
    let switch_to = (record.uid_isolation == "active").then_some((record.run_as.uid, record.run_as.gid));
    unsafe {
        cmd.pre_exec(move || {
            // Own process group so `stop` can killpg the workload tree.
            if libc::setpgid(0, 0) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            if foreground_tty {
                libc::signal(libc::SIGTTOU, libc::SIG_IGN);
                libc::tcsetpgrp(0, libc::getpid());
            }
            for sig in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP, libc::SIGQUIT, libc::SIGPIPE, libc::SIGTTOU, libc::SIGTTIN] {
                libc::signal(sig, libc::SIG_DFL);
            }
            if let Some((uid, gid)) = switch_to {
                let groups = [gid as libc::gid_t];
                if libc::setgroups(1, groups.as_ptr()) != 0
                    || libc::setgid(gid) != 0
                    || libc::setuid(uid) != 0
                {
                    return Err(std::io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
    cmd
}

fn exit_code_of(status: std::process::ExitStatus) -> (i32, Option<i32>) {
    match (status.code(), status.signal()) {
        (Some(code), _) => (code, None),
        (None, Some(sig)) => (128 + sig, Some(sig)),
        _ => (255, None),
    }
}

fn record_exit(dir: &Path, code: i32, signal: Option<i32>, error: Option<String>) -> Result<()> {
    let path = dir.join("exit.json");
    if path.exists() {
        return Ok(());
    }
    write_json(&path, &Exited { exit_code: code, signal, finished_at: now(), error })
}

fn record_started(dir: &Path, pid: i32, supervisor_pid: Option<i32>) -> Result<()> {
    write_json(
        &dir.join("started.json"),
        &Started {
            pid,
            pgid: pid,
            start_time_us: process_start_time(pid),
            supervisor_pid,
            started_at: now(),
        },
    )
}

/// Run a created container in the foreground; returns its exit code.
pub fn run_foreground(container: &Container) -> Result<i32> {
    let record = &container.record;
    if find_executable(&record.argv[0], &record.env, &record.workdir).is_none() {
        let message = not_found_message(&record.command[0]);
        eprintln!("vat: {message}");
        record_exit(&container.dir, 127, None, Some(message))?;
        if record.auto_remove {
            remove(&load_dir(&container.dir)?, true)?;
        }
        return Ok(127);
    }
    let tty = unsafe { libc::isatty(0) } == 1 && unsafe { libc::tcgetpgrp(0) } == unsafe { libc::getpgrp() };
    let mut cmd = workload_command(record, &record.argv, &record.env, &record.workdir, Stdio3::Inherit, tty);
    let flags: Vec<(i32, Arc<AtomicBool>)> = [libc::SIGTERM, libc::SIGINT, libc::SIGHUP, libc::SIGQUIT]
        .into_iter()
        .map(|sig| (sig, Arc::new(AtomicBool::new(false))))
        .collect();
    let mut handles = Vec::new();
    for (sig, flag) in &flags {
        handles.push(signal_hook::flag::register(*sig, Arc::clone(flag))?);
    }
    let spawn = cmd.spawn();
    let mut child = match spawn {
        Ok(child) => child,
        Err(err) => {
            for h in handles {
                signal_hook::low_level::unregister(h);
            }
            record_exit(&container.dir, 127, None, Some(err.to_string()))?;
            return Err(err).context("spawn sandbox-exec");
        }
    };
    let pid = child.id() as i32;
    if tty {
        unsafe {
            libc::signal(libc::SIGTTOU, libc::SIG_IGN);
            libc::setpgid(pid, pid);
            libc::tcsetpgrp(0, pid);
        }
    }
    record_started(&container.dir, pid, None)?;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        for (sig, flag) in &flags {
            if flag.swap(false, Ordering::SeqCst) {
                unsafe { libc::killpg(pid, *sig) };
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    if tty {
        unsafe {
            libc::tcsetpgrp(0, libc::getpgrp());
            libc::signal(libc::SIGTTOU, libc::SIG_DFL);
        }
    }
    for h in handles {
        signal_hook::low_level::unregister(h);
    }
    let (code, signal) = exit_code_of(status);
    record_exit(&container.dir, code, signal, None)?;
    if record.auto_remove {
        remove(&load_dir(&container.dir)?, true)?;
    }
    Ok(code)
}

/// Start a created container detached under a supervisor process. Returns
/// once the workload is running (or failed to start).
pub fn run_detached(container: &Container) -> Result<()> {
    let exe = std::env::current_exe().context("locate the vat executable")?;
    let mut cmd = Command::new(exe);
    cmd.args(["container", "__supervise", &container.record.id])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(container.dir.join("supervisor.log"))?);
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut supervisor = cmd.spawn().context("spawn the native container supervisor")?;
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if container.dir.join("started.json").exists() || container.dir.join("exit.json").exists() {
            break;
        }
        if let Some(status) = supervisor.try_wait()? {
            if status.success() && container.record.auto_remove && !container.dir.exists() {
                // A `--rm` workload that already ran, exited, and was removed.
                std::mem::forget(supervisor);
                return Ok(());
            }
            if !container.dir.join("started.json").exists() {
                let log = std::fs::read_to_string(container.dir.join("supervisor.log")).unwrap_or_default();
                bail!("native container supervisor exited ({status}) before starting the workload: {}", log.trim());
            }
            break;
        }
        if Instant::now() > deadline {
            bail!("timed out waiting for container {} to start", container.record.id);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    if let Some(exited) = read_json::<Exited>(&container.dir.join("exit.json"))? {
        if let Some(error) = exited.error {
            bail!("container {} failed to start: {error}", container.record.id);
        }
    }
    // The supervisor is reparented to launchd once we exit; don't wait on it.
    std::mem::forget(supervisor);
    Ok(())
}

/// Body of the hidden `vat container __supervise <id>` process.
pub fn supervise(id: &str) -> Result<()> {
    let container = find(id)?;
    let open_log = |path: PathBuf| {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("open {}", path.display()))
    };
    let stdout = open_log(container.stdout_log())?;
    let stderr = open_log(container.stderr_log())?;
    let record = &container.record;
    if find_executable(&record.argv[0], &record.env, &record.workdir).is_none() {
        record_exit(&container.dir, 127, None, Some(not_found_message(&record.command[0])))?;
        return Ok(());
    }
    let mut cmd = workload_command(
        record,
        &record.argv,
        &record.env,
        &record.workdir,
        Stdio3::Files { stdout, stderr },
        false,
    );
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(err) => {
            record_exit(&container.dir, 127, None, Some(format!("spawn sandbox-exec: {err}")))?;
            return Ok(());
        }
    };
    let pid = child.id() as i32;
    unsafe {
        libc::setpgid(pid, pid);
    }
    record_started(&container.dir, pid, Some(std::process::id() as i32))?;
    unsafe {
        libc::signal(libc::SIGTERM, libc::SIG_IGN);
        libc::signal(libc::SIGINT, libc::SIG_IGN);
        libc::signal(libc::SIGHUP, libc::SIG_IGN);
    }
    let status = child.wait()?;
    let (code, signal) = exit_code_of(status);
    record_exit(&container.dir, code, signal, None)?;
    if record.auto_remove {
        remove(&load_dir(&container.dir)?, true)?;
    }
    Ok(())
}

/// Stop: SIGTERM the workload's process group, SIGKILL after `timeout`.
pub fn stop(container: &Container, timeout: Duration) -> Result<Option<i32>> {
    if container.status() != Status::Running {
        return Ok(container.exit_code());
    }
    let started = container.started.as_ref().expect("running implies started");
    unsafe { libc::killpg(started.pgid, libc::SIGTERM) };
    let deadline = Instant::now() + timeout;
    let mut killed = false;
    loop {
        let current = load_dir(&container.dir);
        let current = match current {
            Ok(c) => c,
            // --rm containers disappear once the supervisor records the exit.
            Err(_) => return Ok(Some(143)),
        };
        if current.exited.is_some() {
            return Ok(current.exit_code());
        }
        if current.status() == Status::Exited {
            // Workload is gone; give a supervisor (bounded) time to record
            // the real exit code before falling back to a signal code.
            let grace = Instant::now() + Duration::from_secs(5);
            loop {
                match load_dir(&container.dir) {
                    Ok(again) => {
                        if let Some(code) = again.exit_code() {
                            return Ok(Some(code));
                        }
                    }
                    Err(_) => return Ok(Some(143)),
                }
                let supervisor_alive = started
                    .supervisor_pid
                    .is_some_and(|pid| unsafe { libc::kill(pid, 0) } == 0);
                if !supervisor_alive || Instant::now() > grace {
                    break;
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            let (code, sig) = if killed { (137, libc::SIGKILL) } else { (143, libc::SIGTERM) };
            record_exit(&container.dir, code, Some(sig), None)?;
            return Ok(Some(code));
        }
        if !killed && Instant::now() > deadline {
            unsafe { libc::killpg(started.pgid, libc::SIGKILL) };
            killed = true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Remove a container and its root. Running containers need `force`
/// (which kills them first).
pub fn remove(container: &Container, force: bool) -> Result<()> {
    if container.status() == Status::Running {
        if !force {
            bail!(
                "container {} is running; stop it first or pass --force",
                container.record.id
            );
        }
        stop(container, Duration::from_secs(0))?;
    }
    for mount in &container.record.mounts {
        // Remove links first so tree removal can never reach host data.
        let _ = std::fs::remove_file(&mount.link);
    }
    super::remove_tree(&container.record.root).with_context(|| {
        format!("remove root {} (if it was chowned to a pool user, remove it as root)", container.record.root.display())
    })?;
    super::remove_tree(&container.dir)?;
    Ok(())
}

/// Run a command inside an existing container root (same env, profile, and
/// user); returns the exit code.
pub fn exec(container: &Container, command: &[String], extra_env: &[String], workdir: Option<&str>) -> Result<i32> {
    let record = &container.record;
    if !record.root.is_dir() {
        bail!("container {} has no root on disk", record.id);
    }
    if command.is_empty() {
        bail!("vat container exec needs a command");
    }
    let mut env: BTreeMap<String, String> = record.env.iter().map(|e| split_env(e)).collect();
    let root_str = record.root.to_str().context("root must be UTF-8")?;
    for entry in extra_env {
        let (k, v) = split_env(entry);
        if k == "VAT_ROOT" {
            bail!("-e VAT_ROOT is reserved");
        }
        env.insert(k, substitute_root(&v, root_str));
    }
    let env: Vec<String> = env.into_iter().map(|(k, v)| format!("{k}={v}")).collect();
    let config = ContainerConfig { cmd: Some(command.to_vec()), ..Default::default() };
    let (_, argv) = resolve_argv(&record.root, &config, &[])?;
    let workdir = match workdir {
        Some(dir) => root_join(&record.root, dir),
        None => record.workdir.clone(),
    };
    if find_executable(&argv[0], &env, &workdir).is_none() {
        eprintln!("vat: {}", not_found_message(&command[0]));
        return Ok(127);
    }
    let tty = unsafe { libc::isatty(0) } == 1 && unsafe { libc::tcgetpgrp(0) } == unsafe { libc::getpgrp() };
    let mut cmd = workload_command(record, &argv, &env, &workdir, Stdio3::Inherit, tty);
    let flags: Vec<(i32, Arc<AtomicBool>)> = [libc::SIGTERM, libc::SIGINT, libc::SIGHUP]
        .into_iter()
        .map(|sig| (sig, Arc::new(AtomicBool::new(false))))
        .collect();
    let mut handles = Vec::new();
    for (sig, flag) in &flags {
        handles.push(signal_hook::flag::register(*sig, Arc::clone(flag))?);
    }
    let mut child = cmd.spawn().context("spawn sandbox-exec")?;
    let pid = child.id() as i32;
    if tty {
        unsafe {
            libc::signal(libc::SIGTTOU, libc::SIG_IGN);
            libc::setpgid(pid, pid);
            libc::tcsetpgrp(0, pid);
        }
    }
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        for (sig, flag) in &flags {
            if flag.swap(false, Ordering::SeqCst) {
                unsafe { libc::killpg(pid, *sig) };
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    if tty {
        unsafe {
            libc::tcsetpgrp(0, libc::getpgrp());
            libc::signal(libc::SIGTTOU, libc::SIG_DFL);
        }
    }
    for h in handles {
        signal_hook::low_level::unregister(h);
    }
    Ok(exit_code_of(status).0)
}

/// One `ps` row.
pub fn ps_row(container: &Container) -> serde_json::Value {
    let r = &container.record;
    serde_json::json!({
        "id": r.id,
        "name": r.name,
        "image": r.image.reference,
        "image_digest": r.image.digest,
        "status": container.status(),
        "exit_code": container.exit_code(),
        "pid": container.started.as_ref().map(|s| s.pid),
        "command": r.command,
        "created_at": r.created_at,
        "root": r.root,
        "network": r.network,
        "uid_isolation": r.uid_isolation,
    })
}

/// Stream logs; with `follow`, keep reading until the container exits.
pub fn logs(container: &Container, follow: bool) -> Result<()> {
    use std::io::{Read, Seek, SeekFrom, Write};
    let mut offsets = [0u64, 0u64];
    let paths = [container.stdout_log(), container.stderr_log()];
    loop {
        for (i, path) in paths.iter().enumerate() {
            let Ok(mut file) = std::fs::File::open(path) else { continue };
            file.seek(SeekFrom::Start(offsets[i]))?;
            let mut buf = Vec::new();
            file.read_to_end(&mut buf)?;
            offsets[i] += buf.len() as u64;
            if i == 0 {
                std::io::stdout().write_all(&buf)?;
                std::io::stdout().flush()?;
            } else {
                std::io::stderr().write_all(&buf)?;
            }
        }
        if !follow {
            return Ok(());
        }
        let Ok(current) = load_dir(&container.dir) else { return Ok(()) };
        if current.status() != Status::Running {
            follow_final_flush(&paths, &mut offsets)?;
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn follow_final_flush(paths: &[PathBuf; 2], offsets: &mut [u64; 2]) -> Result<()> {
    use std::io::{Read, Seek, SeekFrom, Write};
    for (i, path) in paths.iter().enumerate() {
        let Ok(mut file) = std::fs::File::open(path) else { continue };
        file.seek(SeekFrom::Start(offsets[i]))?;
        let mut buf = Vec::new();
        file.read_to_end(&mut buf)?;
        offsets[i] += buf.len() as u64;
        if i == 0 {
            std::io::stdout().write_all(&buf)?;
        } else {
            std::io::stderr().write_all(&buf)?;
        }
    }
    Ok(())
}

/// Manifest digests of images used by existing containers (kept by GC).
pub fn images_in_use() -> Vec<String> {
    list()
        .unwrap_or_default()
        .into_iter()
        .map(|c| c.record.image.digest)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn substitute_root_respects_identifier_boundaries() {
        assert_eq!(substitute_root("$VAT_ROOT/app", "/r"), "/r/app");
        assert_eq!(substitute_root("${VAT_ROOT}/a:$VAT_ROOT", "/r"), "/r/a:/r");
        assert_eq!(substitute_root("$VAT_ROOTS", "/r"), "$VAT_ROOTS");
    }

    #[test]
    fn env_contract() {
        let root = Path::new("/x/c-1");
        let env = runtime_env(
            root,
            &[
                "PATH=/app/bin:$VAT_ROOT/venv/bin:relative".into(),
                "DATA=$VAT_ROOT/data".into(),
                "PLAIN=/etc/hosts".into(),
            ],
            &["EXTRA=${VAT_ROOT}/e".into()],
        )
        .unwrap();
        let get = |k: &str| env.iter().find(|(key, _)| key == k).map(|(_, v)| v.clone());
        assert_eq!(
            get("PATH").unwrap(),
            "/x/c-1/app/bin:/x/c-1/venv/bin:relative:/usr/bin:/bin:/usr/sbin:/sbin"
        );
        assert_eq!(get("VAT_ROOT").unwrap(), "/x/c-1");
        assert_eq!(get("DATA").unwrap(), "/x/c-1/data");
        assert_eq!(get("PLAIN").unwrap(), "/etc/hosts");
        assert_eq!(get("TMPDIR").unwrap(), "/x/c-1/tmp");
        assert_eq!(get("HOME").unwrap(), "/x/c-1/root");
        assert_eq!(get("EXTRA").unwrap(), "/x/c-1/e");
        assert!(runtime_env(root, &[], &["VAT_ROOT=/".into()]).is_err());
        let bare = runtime_env(root, &[], &[]).unwrap();
        assert!(bare.iter().any(|(k, v)| k == "PATH" && v == HOST_SYSTEM_PATH));
    }

    #[test]
    fn argv_and_workdir_resolution() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("app")).unwrap();
        std::fs::write(root.join("app/run.sh"), "").unwrap();
        let config = ContainerConfig {
            entrypoint: Some(vec!["/app/run.sh".into()]),
            cmd: Some(vec!["$VAT_ROOT/data".into()]),
            working_dir: Some("/app".into()),
            ..Default::default()
        };
        let (requested, argv) = resolve_argv(root, &config, &[]).unwrap();
        assert_eq!(requested, vec!["/app/run.sh", "$VAT_ROOT/data"]);
        assert_eq!(argv[0], root.join("app/run.sh").display().to_string());
        assert_eq!(argv[1], format!("{}/data", root.display()));
        let (_, host) = resolve_argv(root, &ContainerConfig::default(), &["/bin/echo".into()]).unwrap();
        assert_eq!(host[0], "/bin/echo");
        assert!(resolve_argv(root, &ContainerConfig::default(), &[]).is_err());
        assert_eq!(resolve_workdir(root, &config, None), root.join("app"));
        assert_eq!(resolve_workdir(root, &config, Some("/w")), root.join("w"));
        assert_eq!(resolve_workdir(root, &ContainerConfig::default(), None), root.to_path_buf());
    }

    #[test]
    fn mount_parsing() {
        let dir = tempfile::tempdir().unwrap();
        let host = dir.path().display().to_string();
        let (h, c, ro) = parse_mount(&format!("{host}:/data:ro")).unwrap();
        assert_eq!(h, std::fs::canonicalize(dir.path()).unwrap());
        assert_eq!(c, "/data");
        assert!(ro);
        assert!(parse_mount(&format!("{host}:data")).is_err());
        assert!(parse_mount(&format!("{host}:/../x")).is_err());
        assert!(parse_mount("/definitely/not/here:/d").is_err());
        assert!(Network::parse("bridge").is_err());
    }

    #[test]
    fn own_process_start_time_is_stable() {
        let pid = std::process::id() as i32;
        let a = process_start_time(pid);
        assert!(a.is_some());
        assert_eq!(a, process_start_time(pid));
    }
}
