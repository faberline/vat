// HANDWRITE-BEGIN gap="missing-generator:cli:compose-lifecycle-orchestration" tracker="#1484" reason="R8-R10 plus #1526/#1529: Cmd dispatch for import/up/down/ps/logs, the locked ComposeRecord registry at <root>/compose/<project>/project.json, atomic import publication/rollback with a fail-closed imported-record service-id gate, up's foreground poll-thread-plus-in-process-run vs. --detach re-exec-plus-poll divergence, and down's VAT-owned stop-request acknowledgement. This process-orchestration shape (in-process call vs. self-re-exec vs. child-owned teardown acknowledgement) is genuinely new -- no existing vat command proxies a long-running run in three different ways -- so the whole file is hand-authored this WI (missing-generator:cli:compose-lifecycle-orchestration, trackers #1484, #1526, and #1529), the same class of gap Phase 2's commands/build.rs recorded for its own dual-mode divergence (missing-generator:cli:streamed-subprocess-dual-mode, tracker #1479)."

//! Compose lifecycle orchestration: import/up/down/ps/logs for docker-compose projects.
//!
//! Manages a registry at `root/compose/<project>/project.json` to track
//! running compose projects, their vat_id, and service list. Up runs in two
//! modes: foreground (poll in-process, then run), or --detach (re-exec self).

use crate::cli::ComposeCmd;
use crate::config::ServiceRuntime;
use crate::spec::{GpuRequest, Isolation};
use crate::state::{ProcessStatus, Status, TestRunEvidence};
use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
#[cfg(unix)]
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, Stdio};
use std::time::{Duration, Instant};

/// Compose project registry entry.
///
/// Registries written while VAT shipped its argv0 Docker shim may still carry
/// `docker_shim_profile`, `launch_generation`, and `launch_ticket` keys; serde
/// ignores them, so they load as ordinary `vat compose` projects.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ComposeRecord {
    project: String,
    vat_id: Option<String>,
    /// Durable provenance for the token-owned compose launch protocol.  It
    /// remains after publication deliberately: the transient token and PID
    /// are cleared once their handoff is complete, but a later missing VAT
    /// metadata file must not make a current binding look like a historical
    /// uncorrelated record that is safe to reclaim.
    #[serde(default, skip_serializing_if = "is_legacy_handoff_protocol")]
    handoff_protocol: u8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    startup_pid: Option<u32>,
    /// Correlates a re-exec'd detached `vat run` with this exact startup so
    /// the child can durably publish its vat id if the parent exits early.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    startup_token: Option<String>,
    /// Time the token-backed detached handoff began. A record with neither a
    /// VAT id nor a launcher PID is only considered abandoned after a small
    /// grace window; this prevents a parent crash before spawn from wedging
    /// the project in `starting` forever.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    startup_started_at: Option<String>,
    service_ids: Vec<String>,
    status: String, // imported, starting, ready
    created_at: String,
}

/// A unique, one-shot ownership proof for a compose startup. Only the holder
/// of this token may publish the VAT id that backs a compose project; a VAT
/// name is deliberately not a correlation key because ordinary `vat run`
/// invocations may use the same name concurrently.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ComposeHandoff {
    project: String,
    token: String,
}

impl ComposeHandoff {
    fn new(project: impl AsRef<str>, token: impl Into<String>) -> Result<Self> {
        let project = sanitize_project_name(project.as_ref());
        if project.is_empty() {
            bail!("compose startup handoff supplied an empty project name");
        }
        let token = token.into();
        if token.is_empty() {
            bail!("compose startup handoff supplied an empty token");
        }
        Ok(Self { project, token })
    }
}

/// The only truthful detached startup states. A discovered vat id alone is
/// not a startup success: its persisted service evidence must say every
/// compose service is Ready.
#[derive(Debug, Clone, PartialEq, Eq)]
enum DetachedStartup {
    Starting,
    Ready,
    /// The VAT parent has begun terminal teardown. Its runner may already be
    /// terminal, but compose must retain the binding until VAT status is
    /// Exited or Interrupted and every tracked service has a confirmed
    /// terminal state.
    Stopping,
    /// Persisted VAT evidence could not be read. This is never terminal: a
    /// concurrent atomic replacement or transient filesystem error cannot
    /// prove services were torn down, so the compose binding stays retained.
    EvidenceUnavailable(String),
    Terminal(String),
    /// The VAT reached a terminal state, but a VAT-owned MicroVM cleanup was
    /// not confirmed. The compose binding must stay retained until a retry
    /// proves the resource is gone.
    CleanupUnconfirmed(String),
}

/// Cross-process claim around every compose registry read-modify-write
/// transition. The persistent lock inode avoids the stale-file race: advisory
/// ownership is released by the OS when its owner crashes or is SIGKILLed.
struct StartupClaim {
    // An advisory lock is released by the OS if `vat compose up` crashes or is
    // SIGKILLed. The lock file intentionally remains as a stable lock inode;
    // deleting it after unlock would let a concurrent opener lock a new inode.
    #[cfg(unix)]
    _file: File,
    #[cfg(not(unix))]
    path: PathBuf,
}

impl StartupClaim {
    fn acquire(registry_dir: &Path, project_name: &str) -> Result<Self> {
        Self::acquire_with_deadline(registry_dir, project_name, None)
    }

    /// Detached children use a blocking claim during the parent-to-child
    /// handoff. The parent holds the claim through spawn and PID persistence,
    /// so a child cannot publish a VAT id that the parent subsequently
    /// overwrites with stale state.
    fn acquire_blocking(registry_dir: &Path, project_name: &str) -> Result<Self> {
        Self::acquire_with_deadline(
            registry_dir,
            project_name,
            Some(Instant::now() + STARTUP_CLAIM_WAIT),
        )
    }

    #[cfg(unix)]
    fn acquire_with_deadline(
        registry_dir: &Path,
        project_name: &str,
        deadline: Option<Instant>,
    ) -> Result<Self> {
        let path = registry_dir.join("startup.lock");
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(&path)
            .with_context(|| format!("open compose startup lock {}", path.display()))?;
        loop {
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                break;
            }
            let err = std::io::Error::last_os_error();
            let busy = matches!(
                err.raw_os_error(),
                Some(code) if code == libc::EWOULDBLOCK || code == libc::EAGAIN
            );
            if busy && deadline.is_some_and(|deadline| Instant::now() < deadline) {
                let remaining = deadline
                    .expect("deadline checked above")
                    .saturating_duration_since(Instant::now());
                std::thread::sleep(remaining.min(Duration::from_millis(10)));
                continue;
            }
            bail!(
                "compose project `{project_name}` has a lifecycle operation in progress; retry `vat compose ps {project_name}` ({err})"
            );
        }
        file.set_len(0)
            .with_context(|| format!("reset compose startup lock {}", path.display()))?;
        writeln!(file, "{}", std::process::id())
            .with_context(|| format!("write {}", path.display()))?;
        Ok(Self { _file: file })
    }

    #[cfg(not(unix))]
    fn acquire_with_deadline(
        registry_dir: &Path,
        project_name: &str,
        deadline: Option<Instant>,
    ) -> Result<Self> {
        let path = registry_dir.join("startup.lock");
        loop {
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(mut file) => {
                    if let Err(err) = writeln!(file, "{}", std::process::id()) {
                        let _ = fs::remove_file(&path);
                        return Err(err).with_context(|| format!("write {}", path.display()));
                    }
                    return Ok(Self { path });
                }
                Err(err)
                    if deadline.is_some() && err.kind() == std::io::ErrorKind::AlreadyExists =>
                {
                    if Instant::now() >= deadline.expect("checked above") {
                        bail!(
                            "compose project `{project_name}` has a lifecycle operation in progress; retry `vat compose ps {project_name}`"
                        );
                    }
                    let remaining = deadline
                        .expect("deadline checked above")
                        .saturating_duration_since(Instant::now());
                    std::thread::sleep(remaining.min(Duration::from_millis(10)));
                }
                Err(err) => {
                    return Err(err).with_context(|| {
                        format!(
                            "compose project `{project_name}` has a lifecycle operation in progress; retry `vat compose ps {project_name}`"
                        )
                    });
                }
            }
        }
    }
}

impl Drop for StartupClaim {
    fn drop(&mut self) {
        #[cfg(not(unix))]
        let _ = fs::remove_file(&self.path);
    }
}

/// Id of the synthesized runner every imported project gets (see `compose::materialize`).
const RUNNER_ID: &str = "project.up";
/// Version of the durable token-owned compose launch protocol.  Records that
/// predate it deserialize as zero and retain the narrowly scoped legacy
/// recovery behavior.
const HANDOFF_PROTOCOL: u8 = 1;

fn is_legacy_handoff_protocol(protocol: &u8) -> bool {
    *protocol == 0
}

/// An internal parent/child handoff may briefly contend on the same registry
/// immediately after the child publishes its VAT id.  Wait only for that
/// bounded transition; external lifecycle commands remain non-blocking.
const STARTUP_CLAIM_WAIT: Duration = Duration::from_secs(10);

/// A detached MicroVM service can take several seconds to acknowledge a
/// force-delete after its runner consumes the stop request. This outer wait
/// must exceed the bounded runtime teardown rather than racing it and making a
/// healthy Apple Container lifecycle look unacknowledged.
const COMPOSE_SHUTDOWN_WAIT: Duration = Duration::from_secs(60);

/// A real re-exec child records its PID immediately on entry. This small
/// window lets a parent crash before spawn be reclaimed without confusing the
/// normal spawn-to-exec handoff for a failed launch.
const DETACHED_HANDOFF_GRACE: Duration = Duration::from_secs(2);

/// Historic non-wait `compose up -d` behavior waits briefly for a
/// token-owned child to publish its VAT id. `--wait` supplies its own bounded
/// readiness deadline instead of adding this interval after it.
const DEFAULT_DETACHED_HANDOFF_WAIT: Duration = Duration::from_secs(10);

/// Main dispatch for compose subcommands.
pub fn exec(cmd: ComposeCmd) -> Result<ExitCode> {
    match cmd {
        ComposeCmd::Import {
            file,
            project,
            runtime,
        } => import_cmd(file, project, runtime),
        ComposeCmd::Up { project, detach } => up_cmd(project, detach),
        ComposeCmd::Down { project } => down_cmd(project),
        ComposeCmd::Ps { project } => ps_cmd(project),
        ComposeCmd::Logs { project, service } => logs_cmd(project, service),
    }
}

/// Import a compose file as a vat.toml project.
fn import_cmd(file: PathBuf, project: Option<String>, runtime: ServiceRuntime) -> Result<ExitCode> {
    let compose_file = crate::compose::parse(&file)?;
    let project_name = if let Some(p) = project {
        sanitize_project_name(&p)
    } else {
        compose_file
            .source_path()
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .map(sanitize_project_name)
            .ok_or_else(|| anyhow::anyhow!("cannot infer project name from compose file path"))?
    };
    if project_name.is_empty() {
        bail!("compose project name must contain at least one letter, number, dash, or underscore");
    }

    let registry_dir = registry_dir_for_project(&project_name)?;
    fs::create_dir_all(&registry_dir)
        .with_context(|| format!("create registry dir {}", registry_dir.display()))?;
    let _claim = StartupClaim::acquire(&registry_dir, &project_name)?;
    if registry_dir.join("project.json").exists() {
        let existing = read_registry(&registry_dir)?;
        if existing.vat_id.is_some() || matches!(existing.status.as_str(), "starting" | "ready") {
            bail!(
                "compose project `{project_name}` has an active lifecycle; run `vat compose down {project_name}` before re-importing"
            );
        }
    }
    // Expand (and therefore preflight/build) only while this project claim is
    // held. A failing runtime leaves the previous materialized import intact,
    // and a concurrent import cannot replace its registry in the meantime.
    let services = crate::compose::expand(&compose_file, &project_name, runtime)?;
    let vat_toml = registry_dir.join("vat.toml");
    let previous_vat_toml = match fs::read(&vat_toml) {
        Ok(contents) => Some(contents),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(error).with_context(|| {
                format!("read existing materialized config {}", vat_toml.display())
            });
        }
    };
    crate::compose::materialize(&services, &vat_toml)?;
    let service_ids = match compose_service_ids(&vat_toml) {
        Ok(service_ids) => service_ids,
        Err(error) => {
            return Err(rollback_failed_import(
                &vat_toml,
                previous_vat_toml.as_deref(),
                "validate newly materialized vat.toml",
                error,
            ));
        }
    };

    let record = ComposeRecord {
        project: project_name.clone(),
        vat_id: None,
        handoff_protocol: HANDOFF_PROTOCOL,
        startup_pid: None,
        startup_token: None,
        startup_started_at: None,
        service_ids,
        status: "imported".to_string(),
        created_at: chrono::Utc::now().to_rfc3339(),
    };
    if let Err(error) = write_registry(&registry_dir, &record) {
        return Err(rollback_failed_import(
            &vat_toml,
            previous_vat_toml.as_deref(),
            "publish compose registry",
            error,
        ));
    }

    println!(
        "Imported compose project `{project_name}` -> {}",
        vat_toml.display()
    );
    Ok(ExitCode::SUCCESS)
}

/// Prove the registry is bound to this exact project while holding the
/// registry claim. This is checked before any reconciliation, state
/// transition, or log read.
fn require_compose_access(record: &ComposeRecord, project_name: &str) -> Result<()> {
    if record.project != project_name {
        bail!(
            "compose project `{project_name}` registry belongs to `{}`; refuse lifecycle access without an exact project binding",
            record.project
        );
    }
    Ok(())
}
// <HANDWRITE gap="vat-compose-detached-readiness-reconciliation" tracker="#1526" reason="Reconcile persisted VAT service records for detached compose so starting, ready, and terminal startup failure are truthful and diagnosable.">
/// Start a compose project (foreground or detached).
fn up_cmd(project: Option<String>, detach: bool) -> Result<ExitCode> {
    let project_name = sanitize_project_name(
        &project.ok_or_else(|| anyhow::anyhow!("--project required for up"))?,
    );
    let registry_dir = registry_dir_for_project(&project_name)?;
    let vat_toml = registry_dir.join("vat.toml");
    if !vat_toml.exists() {
        bail!("no imported compose project `{project_name}` -- run `vat compose import` first");
    }
    let claim = StartupClaim::acquire(&registry_dir, &project_name)?;
    let mut record = load_and_validate_registry(&registry_dir, &project_name, &vat_toml)?;
    if matches!(record.status.as_str(), "started" | "running") {
        // Records written by older VAT versions used these intermediate labels.
        // Treat them as active instead of clearing their vat id and spawning a
        // second run.
        record.status = "starting".to_string();
        write_registry(&registry_dir, &record)?;
    }
    if record.vat_id.is_some()
        || matches!(record.status.as_str(), "starting" | "ready" | "stopping")
    {
        match reconcile_detached_startup(&record)? {
            DetachedStartup::Terminal(_) => reset_active_run(&registry_dir, &mut record)?,
            DetachedStartup::CleanupUnconfirmed(message) => {
                bail!(compose_cleanup_unconfirmed_error(
                    &project_name,
                    record.vat_id.as_deref(),
                    &message
                ));
            }
            DetachedStartup::Starting | DetachedStartup::Ready | DetachedStartup::Stopping => {
                bail!(
                    "compose project `{project_name}` is already {}; use `vat compose ps {project_name}` or `vat compose down {project_name}`",
                    record.status
                );
            }
            DetachedStartup::EvidenceUnavailable(message) => {
                bail!(
                    "compose project `{project_name}` VAT evidence is temporarily unavailable: {message}; registry retained to avoid overlapping services; retry `vat compose ps {project_name}`"
                );
            }
        }
    }
    record.status = "starting".to_string();
    record.vat_id = None;
    // A re-launch through this binary is now token-owned even when the
    // imported registry originated before the protocol existed.  Keep this
    // marker after publish/reset so missing evidence never frees a current
    // service binding.
    record.handoff_protocol = HANDOFF_PROTOCOL;
    record.startup_pid = None;
    record.startup_token = None;
    record.startup_started_at = None;
    write_registry(&registry_dir, &record)?;

    if detach {
        // Persist this correlation token before spawn. The re-exec'd child uses
        // it to write its VAT id itself, so a slow clone or a parent crash
        // cannot strand the project in `starting`. The token, not the VAT
        // name or creation time, is the only authority allowed to bind this
        // compose project to a VAT.
        let handoff = ComposeHandoff::new(&project_name, crate::id::fresh())?;
        record.startup_token = Some(handoff.token.clone());
        record.startup_started_at = Some(Utc::now().to_rfc3339());
        write_registry(&registry_dir, &record)?;
        // Re-exec the canonical VAT executable rather than whatever path this
        // process was invoked through.
        let runner_executable = std::env::current_exe()
            .context("resolve current VAT executable for detached compose run")?
            .canonicalize()
            .context("canonicalize VAT executable for detached compose run")?;
        let mut child = match Command::new(&runner_executable)
            .arg("run")
            .arg(RUNNER_ID)
            .arg("--name")
            .arg(&project_name)
            // Compose must preserve failed startup evidence even when a
            // user-edited imported vat.toml changes its normal retention.
            .args(["--keep", "always"])
            .current_dir(&registry_dir)
            .env("VAT_COMPOSE_PROJECT", &handoff.project)
            .env("VAT_COMPOSE_STARTUP_TOKEN", &handoff.token)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(child) => child,
            Err(err) => {
                reset_active_run(&registry_dir, &mut record)?;
                return Err(err).context("spawn detached `vat run`");
            }
        };
        record.startup_pid = Some(child.id());
        write_registry(&registry_dir, &record)?;
        drop(claim);

        let handoff_deadline = Instant::now() + DEFAULT_DETACHED_HANDOFF_WAIT;
        let observed_vat_id =
            poll_for_detached_handoff(&registry_dir, &handoff, handoff_deadline, &mut child);
        // The detached child takes this same claim before it records its PID
        // or publishes the VAT id. Reacquiring before every post-poll update
        // prevents parent/child/ps lost updates and keeps project.json
        // serialized across processes.
        let mut final_claim = StartupClaim::acquire_blocking(&registry_dir, &project_name)?;
        let mut record = read_registry(&registry_dir)?;
        let observed_vat_id = match observed_vat_id {
            Ok(vat_id) => vat_id,
            Err(err) => {
                // Reset only the startup this parent still owns. A child that
                // published before exiting (or a newer lifecycle) must never
                // be clobbered by this parent's stale in-memory record.
                if record.vat_id.is_none()
                    && record.startup_token.as_deref() == Some(handoff.token.as_str())
                {
                    reset_active_run(&registry_dir, &mut record)?;
                }
                return Err(err);
            }
        };
        // The child writes the registry itself while proving this exact token.
        // A parent must never infer a VAT id from the global store: a normal
        // `vat run --name <project>` can otherwise win the same-name race and
        // redirect compose cleanup to an unrelated service set.
        let token_matches = record.startup_token.as_deref() == Some(handoff.token.as_str());
        let child_already_published = match (record.vat_id.as_deref(), observed_vat_id.as_deref()) {
            (Some(actual), Some(observed)) => actual == observed && record.startup_token.is_none(),
            (None, None) => token_matches,
            _ => false,
        };
        if !child_already_published {
            bail!(
                "detached compose startup for `{project_name}` lost registry ownership; inspect `vat compose ps {project_name}`"
            );
        }
        // Within the same bounded handoff window, keep watching a service set
        // that is still starting: one that fails fast is reported as a
        // startup failure instead of a false `starting` success. The claim is
        // released meanwhile so the child and `compose ps` are not blocked.
        let mut startup = reconcile_detached_startup(&record)?;
        if matches!(startup, DetachedStartup::Starting) && Instant::now() < handoff_deadline {
            drop(final_claim);
            while matches!(startup, DetachedStartup::Starting) && Instant::now() < handoff_deadline
            {
                std::thread::sleep(Duration::from_millis(200));
                startup = reconcile_detached_startup(&record)?;
            }
            final_claim = StartupClaim::acquire_blocking(&registry_dir, &project_name)?;
            let current = read_registry(&registry_dir)?;
            if current.vat_id != record.vat_id {
                bail!(
                    "detached compose startup for `{project_name}` lost registry ownership; inspect `vat compose ps {project_name}`"
                );
            }
            record = current;
            startup = reconcile_detached_startup(&record)?;
        }
        let _final_claim = final_claim;
        match startup {
            DetachedStartup::Starting => record.status = "starting".to_string(),
            DetachedStartup::Ready => record.status = "ready".to_string(),
            DetachedStartup::Stopping => record.status = "stopping".to_string(),
            DetachedStartup::EvidenceUnavailable(message) => {
                return Err(anyhow::anyhow!(
                    "detached compose startup for `{project_name}` VAT evidence is temporarily unavailable: {message}; registry retained to avoid overlapping services"
                ));
            }
            DetachedStartup::Terminal(message) => {
                let vat_id = record.vat_id.clone();
                reset_active_run(&registry_dir, &mut record)?;
                return Err(compose_terminal_startup_error(
                    &project_name,
                    vat_id.as_deref(),
                    &message,
                ));
            }
            DetachedStartup::CleanupUnconfirmed(message) => {
                return Err(compose_cleanup_unconfirmed_error(
                    &project_name,
                    record.vat_id.as_deref(),
                    &message,
                ));
            }
        }
        write_registry(&registry_dir, &record)?;

        crate::commands::print_json(
            &serde_json::json!({
                "project": project_name,
                "vat_id": record.vat_id,
                "status": record.status,
            }),
            true,
        )?;
        return Ok(ExitCode::SUCCESS);
    }

    // Foreground uses the same token-owned handoff as a detached child. This
    // eliminates the former background name/time store poll, which could bind
    // an unrelated ordinary `vat run --name <project>` to this compose run.
    let handoff = ComposeHandoff::new(&project_name, crate::id::fresh())?;
    record.startup_pid = Some(std::process::id());
    record.startup_token = Some(handoff.token.clone());
    record.startup_started_at = Some(Utc::now().to_rfc3339());
    write_registry(&registry_dir, &record)?;
    drop(claim);

    std::env::set_current_dir(&registry_dir)
        .with_context(|| format!("cd into {}", registry_dir.display()))?;
    crate::commands::run::exec(crate::commands::run::Args {
        target: crate::commands::run::Target::Runner {
            runner_ids: vec![RUNNER_ID.to_string()],
        },
        base: None,
        from: None,
        name: Some(project_name),
        isolation: Isolation::default(),
        gpu: GpuRequest::default(),
        microvm_image: None,
        json: false,
        plan: None,
        keep: None,
        compose_handoff: Some(handoff),
    })
}
// </HANDWRITE>

/// Stop a running compose project.
fn down_cmd(project: String) -> Result<ExitCode> {
    let project_name = sanitize_project_name(&project);
    let registry_dir = registry_dir_for_project(&project_name)?;
    // Hold the registry claim through acknowledgement and final reset. A
    // concurrent `up` must not bind a second service set while this request
    // is still waiting for the old VAT parent to finish cleanup.
    let _claim = StartupClaim::acquire(&registry_dir, &project_name)?;
    let mut record = read_registry(&registry_dir)
        .with_context(|| format!("no compose project `{project_name}` in registry"))?;
    require_compose_access(&record, &project_name)?;

    if record.vat_id.is_none() && record.status == "imported" {
        bail!("compose project `{project_name}` is imported but has no active vat run");
    }
    match reconcile_detached_startup(&record)? {
        DetachedStartup::Starting => bail!(
            "compose project `{project_name}` is still starting; retry `vat compose down {project_name}` once the runner PID is persisted"
        ),
        DetachedStartup::EvidenceUnavailable(message) => {
            bail!(
                "compose project `{project_name}` VAT evidence is temporarily unavailable: {message}; registry retained to avoid overlapping services; retry `vat compose down {project_name}`"
            );
        }
        DetachedStartup::Stopping => {
            let vat_id = record
                .vat_id
                .as_deref()
                .context("stopping compose project is missing its VAT id")?;
            wait_for_compose_shutdown(vat_id, &record.service_ids, COMPOSE_SHUTDOWN_WAIT)?;
            reset_active_run(&registry_dir, &mut record)?;
            println!("Stopped compose project `{project_name}` after VAT cleanup");
            return Ok(ExitCode::SUCCESS);
        }
        DetachedStartup::Terminal(message) => {
            reset_active_run(&registry_dir, &mut record)?;
            println!("compose project `{project_name}` already terminated: {message}");
            return Ok(ExitCode::SUCCESS);
        }
        DetachedStartup::CleanupUnconfirmed(message) => {
            let vat_id = record
                .vat_id
                .as_deref()
                .context("cleanup-unconfirmed compose project is missing its VAT id")?;
            let mut vat = crate::store::load(vat_id).with_context(|| {
                format!("load VAT {vat_id} for compose project `{project_name}` cleanup retry")
            })?;
            if let Err(error) = crate::commands::run::retry_unconfirmed_service_cleanup(&mut vat) {
                return Err(compose_cleanup_unconfirmed_error(
                    &project_name,
                    Some(vat_id),
                    &format!("{message}; retry failed: {error}"),
                ));
            }
            match reconcile_detached_startup(&record)? {
                DetachedStartup::Terminal(recovered) => {
                    reset_active_run(&registry_dir, &mut record)?;
                    println!(
                        "Stopped compose project `{project_name}` after confirming prior MicroVM cleanup: {recovered}"
                    );
                    return Ok(ExitCode::SUCCESS);
                }
                DetachedStartup::CleanupUnconfirmed(retry_message) => {
                    return Err(compose_cleanup_unconfirmed_error(
                        &project_name,
                        Some(vat_id),
                        &retry_message,
                    ));
                }
                DetachedStartup::Starting | DetachedStartup::Ready | DetachedStartup::Stopping => {
                    bail!(
                        "compose project `{project_name}` changed while retrying cleanup; inspect `vat state {vat_id}` before retrying `vat compose down {project_name}`"
                    );
                }
                DetachedStartup::EvidenceUnavailable(message) => {
                    bail!(
                        "compose project `{project_name}` VAT evidence is temporarily unavailable after cleanup retry: {message}; registry retained to avoid overlapping services"
                    );
                }
            }
        }
        DetachedStartup::Ready => {}
    }

    let vat_id = record
        .vat_id
        .as_deref()
        .context("ready compose project is missing its vat id")?;

    let vat = crate::store::load(vat_id)
        .with_context(|| format!("load vat {vat_id} for compose project `{project_name}`"))?;

    // Do not signal a persisted OS PID directly: it can be stale or reused,
    // and resetting the registry before the VAT parent owns teardown creates
    // a port-collision window. The live VAT process consumes this request,
    // stops its own runner/services, and persists Status::Exited first.
    crate::commands::run::request_detached_compose_stop(&vat)?;
    wait_for_compose_shutdown(vat_id, &record.service_ids, COMPOSE_SHUTDOWN_WAIT)?;
    reset_active_run(&registry_dir, &mut record)?;
    println!("Stopped compose project `{project_name}` after VAT cleanup");
    Ok(ExitCode::SUCCESS)
}

// <HANDWRITE gap="vat-compose-detached-readiness-projection" tracker="#1526" reason="Project reconciled detached compose state instead of treating a discovered VAT id as a successful startup.">
/// Lifecycle phase of one compose project as observed by `ps`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ComposePhase {
    Inactive,
    Starting,
    Ready,
    Stopping,
}

/// A complete `ps` observation assembled under one compose registry claim.
/// The runner evidence belongs to the same reconciliation read that selected
/// `phase`, so the printed services cannot pair a fresh registry with a
/// different generation of VAT metadata.
#[derive(Debug)]
struct ComposePsSnapshot {
    project: String,
    phase: ComposePhase,
    service_ids: Vec<String>,
    test_run: Option<TestRunEvidence>,
}

impl ComposePsSnapshot {
    fn from_record(
        record: &ComposeRecord,
        phase: ComposePhase,
        test_run: Option<TestRunEvidence>,
    ) -> Self {
        Self {
            project: record.project.clone(),
            phase,
            service_ids: record.service_ids.clone(),
            test_run,
        }
    }
}

/// Return exactly one service evidence record. Duplicate IDs in persisted
/// runner metadata are not a safe ownership proof.
fn unique_service_evidence<'a>(
    test_run: &'a TestRunEvidence,
    service_id: &str,
) -> Option<&'a crate::state::ServiceRunRecord> {
    let mut matching = test_run
        .services
        .iter()
        .filter(|service| service.id == service_id);
    let service = matching.next()?;
    matching.next().is_none().then_some(service)
}

/// Gather one `ps` observation while holding the compose claim.
fn collect_compose_ps_snapshot(project: String) -> Result<ComposePsSnapshot> {
    let project_name = sanitize_project_name(&project);
    let registry_dir = registry_dir_for_project(&project_name)?;
    let _claim = StartupClaim::acquire(&registry_dir, &project_name)?;
    let mut record = read_registry(&registry_dir)
        .with_context(|| format!("no compose project `{project_name}` in registry"))?;
    require_compose_access(&record, &project_name)?;

    if record.vat_id.is_none() && record.status == "imported" {
        return Ok(ComposePsSnapshot::from_record(
            &record,
            ComposePhase::Inactive,
            None,
        ));
    }

    let ReconciledStartupEvidence { state, test_run } =
        reconcile_detached_startup_with_evidence(&record)?;
    match state {
        DetachedStartup::Starting => {
            record.status = "starting".to_string();
            write_registry(&registry_dir, &record)?;
            Ok(ComposePsSnapshot::from_record(
                &record,
                ComposePhase::Starting,
                test_run,
            ))
        }
        DetachedStartup::Ready => {
            record.status = "ready".to_string();
            write_registry(&registry_dir, &record)?;
            Ok(ComposePsSnapshot::from_record(
                &record,
                ComposePhase::Ready,
                test_run,
            ))
        }
        DetachedStartup::Stopping => {
            record.status = "stopping".to_string();
            write_registry(&registry_dir, &record)?;
            Ok(ComposePsSnapshot::from_record(
                &record,
                ComposePhase::Stopping,
                test_run,
            ))
        }
        DetachedStartup::EvidenceUnavailable(message) => Err(anyhow::anyhow!(
            "compose project `{project_name}` VAT evidence is temporarily unavailable: {message}; registry retained to avoid overlapping services; retry `vat compose ps {project_name}`"
        )),
        DetachedStartup::Terminal(message) => {
            let vat_id = record.vat_id.clone();
            reset_active_run(&registry_dir, &mut record)?;
            Err(compose_terminal_startup_error(
                &project_name,
                vat_id.as_deref(),
                &message,
            ))
        }
        DetachedStartup::CleanupUnconfirmed(message) => Err(compose_cleanup_unconfirmed_error(
            &project_name,
            record.vat_id.as_deref(),
            &message,
        )),
    }
}

/// Print the human-readable `vat compose ps` text surface.
fn print_compose_ps_snapshot(snapshot: &ComposePsSnapshot) {
    match snapshot.phase {
        ComposePhase::Inactive => {
            println!(
                "compose project `{}` is imported; run `vat compose up --project {}`",
                snapshot.project, snapshot.project
            );
        }
        ComposePhase::Starting => {
            println!("compose project `{}` is starting", snapshot.project);
        }
        ComposePhase::Stopping => println!(
            "compose project `{}` is stopping; registry remains bound until VAT cleanup is confirmed",
            snapshot.project
        ),
        ComposePhase::Ready => {
            println!("compose project `{}` is ready", snapshot.project);
            if let Some(test_run) = snapshot.test_run.as_ref() {
                for service in &test_run.services {
                    if snapshot.service_ids.contains(&service.id) {
                        println!(
                            "{}\t{:?}\t{}",
                            service.id,
                            service.status,
                            service
                                .port
                                .map(|port| port.to_string())
                                .unwrap_or_else(|| "-".to_string())
                        );
                    }
                }
            }
        }
    }
}

/// List services in a compose project.
fn ps_cmd(project: String) -> Result<ExitCode> {
    let snapshot = collect_compose_ps_snapshot(project)?;
    print_compose_ps_snapshot(&snapshot);
    Ok(ExitCode::SUCCESS)
}
// </HANDWRITE>

/// Print logs from a service in a compose project.
fn logs_cmd(project: String, service: String) -> Result<ExitCode> {
    let project_name = sanitize_project_name(&project);
    let registry_dir = registry_dir_for_project(&project_name)?;
    let claim = StartupClaim::acquire(&registry_dir, &project_name)?;
    let record = read_registry(&registry_dir)
        .with_context(|| format!("no compose project `{project_name}` in registry"))?;
    require_compose_access(&record, &project_name)?;

    let Some(vat_id) = record.vat_id.clone() else {
        bail!("compose project `{project_name}` is still starting (no vat_id yet)");
    };

    if !record.service_ids.iter().any(|id| id == &service) {
        bail!("service `{service}` is not part of compose project `{project_name}`");
    }

    let vat = crate::store::load(&vat_id)
        .with_context(|| format!("load vat {vat_id} for compose project `{project_name}`"))?;

    let Some(test_run) = vat.meta.test_run.as_ref() else {
        bail!("compose project `{project_name}` has no runner evidence yet");
    };

    let Some(svc) = test_run.services.iter().find(|s| s.id == service) else {
        bail!("no log source `{service}` in compose project `{project_name}`");
    };

    print_file(&svc.stdout_log)?;
    print_file(&svc.stderr_log)?;
    // Keep the registry lock through the log read/print so log paths cannot
    // come from a later re-import or teardown that reused the project name.
    drop(claim);
    Ok(ExitCode::SUCCESS)
}
fn print_file(path: &str) -> Result<()> {
    match fs::read_to_string(path) {
        Ok(content) => {
            print!("{content}");
            Ok(())
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err).with_context(|| format!("read log {path}")),
    }
}

/// Wait only for a token-owned child to publish into its compose registry.
/// Global VAT-store name/time discovery is intentionally forbidden here: an
/// unrelated `vat run --name <project>` can be created in the same interval.
fn poll_for_detached_handoff(
    registry_dir: &Path,
    handoff: &ComposeHandoff,
    deadline: Instant,
    child: &mut Child,
) -> Result<Option<String>> {
    loop {
        if Instant::now() >= deadline {
            return Ok(None);
        }
        let record = read_registry(registry_dir).with_context(|| {
            format!(
                "read compose registry for `{}` while waiting for token-owned VAT publication",
                handoff.project
            )
        })?;
        if record.project != handoff.project {
            bail!(
                "detached compose startup for `{}` lost its registry project binding",
                handoff.project
            );
        }
        if let Some(vat_id) = record.vat_id {
            if record.startup_token.is_none() {
                return Ok(Some(vat_id));
            }
            bail!(
                "detached compose startup for `{}` published a VAT id without completing its token handoff",
                handoff.project
            );
        }
        if record.status != "starting"
            || record.startup_token.as_deref() != Some(handoff.token.as_str())
        {
            bail!(
                "detached compose startup for `{}` lost token ownership before VAT publication",
                handoff.project
            );
        }
        if let Some(status) = child.try_wait()? {
            bail!(
                "detached vat run for compose project `{}` exited {:?} before creating VAT evidence",
                handoff.project,
                status.code()
            );
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok(None);
        }
        std::thread::sleep(remaining.min(Duration::from_millis(200)));
    }
}

/// Reconciliation plus the exact evidence revision used to make that
/// lifecycle decision. `vat compose ps` retains this value under its registry
/// claim instead of reconciling one VAT metadata revision and printing
/// services from a later reread.
#[derive(Debug)]
struct ReconciledStartupEvidence {
    state: DetachedStartup,
    test_run: Option<TestRunEvidence>,
}

fn reconcile_detached_startup(record: &ComposeRecord) -> Result<DetachedStartup> {
    Ok(reconcile_detached_startup_with_evidence(record)?.state)
}

fn reconcile_detached_startup_with_evidence(
    record: &ComposeRecord,
) -> Result<ReconciledStartupEvidence> {
    let Some(vat_id) = record.vat_id.as_deref() else {
        // Only new detached records carry a token. Older `started`/`running`
        // records without one retain their conservative legacy behavior, but
        // a token-backed launch can be classified once its re-exec child dies
        // rather than remaining in `starting` forever.
        if record.startup_token.is_some() {
            let state = match record.startup_pid {
                Some(pid) if detached_child_is_alive(pid) => DetachedStartup::Starting,
                Some(pid) => DetachedStartup::Terminal(format!(
                    "detached vat launcher pid {pid} exited before publishing VAT evidence"
                )),
                // The child records its own PID at the top of `vat run`; a
                // small spawn-to-exec window must remain recoverable, but a
                // token with no launcher forever is an abandoned parent crash
                // and must not wedge later up/down operations.
                None if detached_handoff_expired(record) => DetachedStartup::Terminal(format!(
                    "detached startup token never published a launcher pid within {}s",
                    DETACHED_HANDOFF_GRACE.as_secs()
                )),
                None => DetachedStartup::Starting,
            };
            return Ok(ReconciledStartupEvidence {
                state,
                test_run: None,
            });
        }
        if let Some(pid) = record.startup_pid {
            let state = if detached_child_is_alive(pid) {
                DetachedStartup::Starting
            } else {
                DetachedStartup::Terminal(format!(
                    "compose foreground launcher pid {pid} exited before publishing VAT evidence"
                ))
            };
            return Ok(ReconciledStartupEvidence {
                state,
                test_run: None,
            });
        }
        return Ok(ReconciledStartupEvidence {
            state: DetachedStartup::Starting,
            test_run: None,
        });
    };
    let vat = match crate::store::load(vat_id) {
        Ok(vat) => vat,
        Err(err) => {
            // Current compose launches retain their durable protocol marker
            // after the transient token and PID are cleared.  Their missing
            // metadata is not enough to establish that a service stopped:
            // retain the binding so a temporary read/delete race cannot
            // permit another run to reuse its published port.  Only
            // pre-protocol legacy records retain the historical recovery
            // path, and only when the metadata path is definitively absent
            // (not malformed or merely unreadable).
            if record.handoff_protocol == 0 && legacy_vat_metadata_is_definitively_absent(vat_id) {
                return Ok(ReconciledStartupEvidence {
                    state: DetachedStartup::Terminal(format!(
                        "legacy VAT evidence `{vat_id}` is absent"
                    )),
                    test_run: None,
                });
            }
            return Ok(ReconciledStartupEvidence {
                state: DetachedStartup::EvidenceUnavailable(format!(
                    "VAT evidence `{vat_id}` could not be read: {err}"
                )),
                test_run: None,
            });
        }
    };
    let crate::store::Vat { meta, .. } = vat;
    let status = meta.status;
    let test_run = meta.test_run;
    if !matches!(&status, Status::Exited { .. } | Status::Interrupted { .. }) {
        let state = detached_startup_while_active(&record.service_ids, test_run.as_ref());
        return Ok(ReconciledStartupEvidence { state, test_run });
    }
    let terminal_state = vat_terminal_state_label(&status);

    let Some(test_run_ref) = test_run.as_ref() else {
        return Ok(ReconciledStartupEvidence {
            state: DetachedStartup::Terminal(format!(
                "VAT reached {terminal_state} without compose run evidence"
            )),
            test_run: None,
        });
    };
    if let Some(message) = compose_cleanup_error(Some(test_run_ref), &record.service_ids) {
        return Ok(ReconciledStartupEvidence {
            state: DetachedStartup::CleanupUnconfirmed(message),
            test_run,
        });
    }
    if !compose_services_are_terminal(Some(test_run_ref), &record.service_ids) {
        return Ok(ReconciledStartupEvidence {
            state: DetachedStartup::Stopping,
            test_run,
        });
    }
    let outcome = detached_startup_from_evidence(&record.service_ids, Some(test_run_ref));
    let state = match outcome {
        // A terminal VAT cannot truthfully be starting or ready. Preserve the
        // binding until this point, then make malformed/incomplete evidence a
        // resettable terminal failure instead of wedging the project forever.
        DetachedStartup::Starting | DetachedStartup::Ready | DetachedStartup::Stopping => {
            DetachedStartup::Terminal(format!(
                "VAT reached {terminal_state} without terminal compose runner evidence"
            ))
        }
        terminal => terminal,
    };
    Ok(ReconciledStartupEvidence { state, test_run })
}

fn vat_terminal_state_label(status: &Status) -> &'static str {
    match status {
        Status::Interrupted { .. } => "Interrupted",
        Status::Exited { .. } => "Exited",
        Status::Created | Status::Running | Status::Snapshot => "nonterminal",
    }
}

/// A compatibility-only absence proof for records from before compose
/// handoffs existed.  `metadata` distinguishes a missing path from malformed
/// JSON, permission errors, and other I/O failures; only the former permits
/// legacy recovery.  Modern records never use this escape hatch.
fn legacy_vat_metadata_is_definitively_absent(vat_id: &str) -> bool {
    let Ok(vat_dir) = crate::paths::vat_dir(vat_id) else {
        return false;
    };
    matches!(
        fs::metadata(vat_dir.join(crate::paths::file::META)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound
    )
}

fn detached_handoff_expired(record: &ComposeRecord) -> bool {
    let Some(started_at) = record.startup_started_at.as_deref() else {
        // Token-bearing records from before this field existed cannot prove a
        // live launcher. Treat them as reclaimable; a delayed current child
        // checks token ownership before it creates VAT state.
        return true;
    };
    let Ok(started_at) = DateTime::parse_from_rfc3339(started_at) else {
        return true;
    };
    match Utc::now()
        .signed_duration_since(started_at.with_timezone(&Utc))
        .to_std()
    {
        Ok(age) => age >= DETACHED_HANDOFF_GRACE,
        // A clock moving backwards should prefer the safe, still-starting
        // interpretation rather than reclaiming a potentially live child.
        Err(_) => false,
    }
}

#[cfg(unix)]
fn detached_child_is_alive(pid: u32) -> bool {
    let result = unsafe { libc::kill(pid as i32, 0) };
    result == 0
        || std::io::Error::last_os_error()
            .raw_os_error()
            .is_some_and(|code| code == libc::EPERM)
}

#[cfg(not(unix))]
fn detached_child_is_alive(_pid: u32) -> bool {
    // The detached launcher handoff is still safe because the token can be
    // published by the child; a conservative fallback avoids declaring a
    // live process terminal on platforms without POSIX liveness probing.
    true
}

/// Wait for the VAT parent, not an arbitrary persisted PID, to acknowledge a
/// compose stop request and finish service teardown. The registry stays bound
/// on timeout so a subsequent command cannot start a second service set while
/// the first may still own published ports.
fn wait_for_compose_shutdown(
    vat_id: &str,
    service_ids: &[String],
    timeout: Duration,
) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        let vat = crate::store::load(vat_id)
            .with_context(|| format!("load VAT `{vat_id}` while waiting for compose shutdown"))?;
        if matches!(
            &vat.meta.status,
            Status::Exited { .. } | Status::Interrupted { .. }
        ) {
            if let Some(message) = compose_cleanup_error(vat.meta.test_run.as_ref(), service_ids) {
                bail!(
                    "compose stop request for VAT `{vat_id}` reached a terminal state but cleanup is unconfirmed: {message}; registry retained to avoid overlapping services; inspect with `vat state {vat_id}` and retry `vat compose down`"
                );
            }
            if compose_services_are_terminal(vat.meta.test_run.as_ref(), service_ids) {
                return Ok(());
            }
        }
        if Instant::now() >= deadline {
            bail!(
                "compose stop request for VAT `{vat_id}` was not acknowledged within {}s; registry retained to avoid overlapping services; inspect with `vat state {vat_id}` and retry `vat compose down`",
                timeout.as_secs()
            );
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn compose_services_are_terminal(
    test_run: Option<&TestRunEvidence>,
    service_ids: &[String],
) -> bool {
    let Some(test_run) = test_run else {
        return true;
    };
    service_ids.iter().all(|service_id| {
        test_run
            .services
            .iter()
            .find(|service| service.id == *service_id)
            .map(|service| {
                service.owned_by_vat == Some(false)
                    || (matches!(
                        service.status,
                        ProcessStatus::Interrupted
                            | ProcessStatus::Exited
                            | ProcessStatus::Failed
                            | ProcessStatus::Timeout
                    ) && service.cleanup_error.is_none())
            })
            // An exited VAT that never recorded this service did not have a
            // live process for it in the first place.
            .unwrap_or(true)
    })
}

fn detached_startup_from_evidence(
    service_ids: &[String],
    test_run: Option<&TestRunEvidence>,
) -> DetachedStartup {
    let Some(test_run) = test_run else {
        return DetachedStartup::Starting;
    };

    if let Some(message) = compose_cleanup_error(Some(test_run), service_ids) {
        return DetachedStartup::CleanupUnconfirmed(message);
    }

    for service_id in service_ids {
        if let Some(service) = test_run
            .services
            .iter()
            .find(|service| service.id == *service_id)
        {
            match service.status {
                ProcessStatus::Interrupted => {
                    return DetachedStartup::Terminal(format!(
                        "service `{service_id}` was interrupted before compose startup completed"
                    ));
                }
                ProcessStatus::Failed | ProcessStatus::Timeout => {
                    let detail = service
                        .readiness_error
                        .as_deref()
                        .unwrap_or("service failed before becoming ready");
                    return DetachedStartup::Terminal(format!(
                        "service `{service_id}` is {:?}: {detail}",
                        service.status
                    ));
                }
                ProcessStatus::Exited => {
                    return DetachedStartup::Terminal(format!(
                        "service `{service_id}` exited before compose startup completed"
                    ));
                }
                ProcessStatus::Created | ProcessStatus::Running | ProcessStatus::Ready => {}
            }
        }
    }

    if let Some(runner) = test_run
        .runner
        .iter()
        .chain(test_run.runners.iter())
        .find(|runner| {
            matches!(
                runner.status,
                ProcessStatus::Interrupted
                    | ProcessStatus::Exited
                    | ProcessStatus::Failed
                    | ProcessStatus::Timeout
            )
        })
    {
        return DetachedStartup::Terminal(format!(
            "runner `{}` is {:?} before compose startup completed",
            runner.id, runner.status
        ));
    }

    let runner_is_live = test_run
        .runner
        .iter()
        .chain(test_run.runners.iter())
        .any(|runner| {
            runner.id == RUNNER_ID
                && runner.status == ProcessStatus::Running
                && runner.pid.is_some()
        });

    if runner_is_live && all_registered_services_are_uniquely_ready(test_run, service_ids) {
        return DetachedStartup::Ready;
    }

    DetachedStartup::Starting
}

/// Reconcile a VAT that has not yet reached its durable terminal status. An
/// exited runner is not enough to release compose ownership: run_configured
/// persists that runner record before it tears down services. While teardown
/// is in flight, return `Stopping` so ps/down/up retain the binding and cannot
/// start a second host-port owner.
fn detached_startup_while_active(
    service_ids: &[String],
    test_run: Option<&TestRunEvidence>,
) -> DetachedStartup {
    let Some(test_run) = test_run else {
        return DetachedStartup::Starting;
    };

    let runner_is_live = test_run
        .runner
        .iter()
        .chain(test_run.runners.iter())
        .any(|runner| {
            runner.id == RUNNER_ID
                && runner.status == ProcessStatus::Running
                && runner.pid.is_some()
        });
    if runner_is_live && all_registered_services_are_uniquely_ready(test_run, service_ids) {
        return DetachedStartup::Ready;
    }

    let runner_is_terminal = test_run
        .runner
        .iter()
        .chain(test_run.runners.iter())
        .any(|runner| {
            matches!(
                runner.status,
                ProcessStatus::Interrupted
                    | ProcessStatus::Exited
                    | ProcessStatus::Failed
                    | ProcessStatus::Timeout
            )
        });
    let service_is_terminal = service_ids.iter().any(|service_id| {
        test_run
            .services
            .iter()
            .find(|service| service.id == *service_id)
            .is_some_and(|service| {
                matches!(
                    service.status,
                    ProcessStatus::Interrupted
                        | ProcessStatus::Exited
                        | ProcessStatus::Failed
                        | ProcessStatus::Timeout
                )
            })
    });
    if runner_is_terminal || service_is_terminal {
        DetachedStartup::Stopping
    } else {
        DetachedStartup::Starting
    }
}

/// `Ready` is an ownership claim, not a best-effort summary. A duplicated
/// service id can contain contradictory lifecycle records, so both compose
/// reconciliation paths require exactly one Ready record per service.
fn all_registered_services_are_uniquely_ready(
    test_run: &TestRunEvidence,
    service_ids: &[String],
) -> bool {
    !service_ids.is_empty()
        && service_ids.iter().all(|service_id| {
            unique_service_evidence(test_run, service_id)
                .is_some_and(|service| service.status == ProcessStatus::Ready)
        })
}

fn compose_cleanup_error(
    test_run: Option<&TestRunEvidence>,
    service_ids: &[String],
) -> Option<String> {
    let test_run = test_run?;
    service_ids.iter().find_map(|service_id| {
        let service = test_run
            .services
            .iter()
            .find(|service| service.id == *service_id)?;
        let error = service.cleanup_error.as_deref()?;
        Some(format!("service `{service_id}`: {error}"))
    })
}

fn compose_terminal_startup_error(
    project_name: &str,
    vat_id: Option<&str>,
    message: &str,
) -> anyhow::Error {
    let state = vat_id
        .map(|id| format!("vat state {id}"))
        .unwrap_or_else(|| "the detached vat could not be identified".to_string());
    anyhow::anyhow!(
        "compose project `{project_name}` startup failed: {message}; registry reset to imported; diagnose with `{state}`"
    )
}

fn compose_cleanup_unconfirmed_error(
    project_name: &str,
    vat_id: Option<&str>,
    message: &str,
) -> anyhow::Error {
    let state = vat_id
        .map(|id| format!("vat state {id}"))
        .unwrap_or_else(|| "the retained VAT state".to_string());
    anyhow::anyhow!(
        "compose project `{project_name}` cleanup is unconfirmed: {message}; registry retained to prevent published-port reuse; inspect with `{state}`, repair the runtime resource, then retry `vat compose down {project_name}`"
    )
}

/// Get or create the registry directory for a project.
fn registry_dir_for_project(project: &str) -> Result<PathBuf> {
    let root = crate::paths::root()?;
    let dir = root.join("compose").join(project);
    Ok(dir)
}

/// Read the compose registry entry for a project.
fn read_registry(registry_dir: &Path) -> Result<ComposeRecord> {
    let path = registry_dir.join("project.json");
    let content = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    let record =
        serde_json::from_str(&content).with_context(|| format!("parse {}", path.display()))?;
    Ok(record)
}

/// Load the registry that commits an import, then prove it tracks the current
/// generated config's service ownership before any detached handoff can run.
/// A prior process may have crashed after atomically replacing vat.toml but
/// before it atomically replaced project.json; treating that split state as a
/// successful import could leave a newly added service untracked at teardown.
fn load_and_validate_registry(
    registry_dir: &Path,
    project_name: &str,
    vat_toml: &Path,
) -> Result<ComposeRecord> {
    let record = read_registry(registry_dir).with_context(|| {
        format!(
            "compose project `{project_name}` has vat.toml but no committed registry; re-import with `vat compose import <compose-file> --project {project_name}`"
        )
    })?;
    if record.project != project_name {
        bail!(
            "compose project `{project_name}` registry belongs to `{}`; re-import with `vat compose import <compose-file> --project {project_name}`",
            record.project
        );
    }
    // A bound record describes a VAT that was already launched from an earlier
    // config. Its current vat.toml may legitimately have been edited since
    // launch; reconciliation must use the durable registry and VAT evidence,
    // never block cleanup on a later config edit. Validate only immediately
    // before a new imported project could create a fresh runtime service set.
    if record.vat_id.is_some() || record.status != "imported" {
        return Ok(record);
    }
    // An invalid or unreadable config cannot start a service: `vat run` will
    // reject it before preparation. Preserve that established lifecycle error
    // path instead of masking it with ownership bookkeeping. A parseable
    // replacement config, however, must agree on its service identity set.
    if let Ok(actual_service_ids) = compose_service_ids(vat_toml) {
        if !compose_service_ids_match(&record.service_ids, &actual_service_ids) {
            bail!(
                "compose project `{project_name}` registry/config mismatch: project.json tracks {:?}, but vat.toml declares {:?}; refuse to launch because cleanup ownership would be incomplete. Re-import with `vat compose import <compose-file> --project {project_name}`",
                record.service_ids,
                actual_service_ids,
            );
        }
    }
    Ok(record)
}

fn compose_service_ids(vat_toml: &Path) -> Result<Vec<String>> {
    // This gate proves registry ownership, not full runner readiness. Keep
    // compose import's established behavior: it may materialize a bounded
    // image service before the user fills in a required runtime detail such as
    // container_port. `vat run` performs the complete config validation before
    // it can launch anything.
    let content = fs::read_to_string(vat_toml)
        .with_context(|| format!("read materialized config {}", vat_toml.display()))?;
    let config: crate::config::VatConfig = toml::from_str(&content)
        .with_context(|| format!("parse materialized config {}", vat_toml.display()))?;
    Ok(config
        .services
        .into_iter()
        .map(|service| service.id)
        .collect())
}

/// Service declaration order is not lifecycle ownership. Users may edit the
/// generated vat.toml, including reordering its service tables, so validate
/// the exact identity set rather than a serialization-order artifact.
fn compose_service_ids_match(recorded: &[String], actual: &[String]) -> bool {
    let mut recorded = recorded.to_vec();
    let mut actual = actual.to_vec();
    recorded.sort_unstable();
    actual.sort_unstable();
    recorded == actual
}

fn rollback_failed_import(
    vat_toml: &Path,
    previous_vat_toml: Option<&[u8]>,
    stage: &str,
    error: anyhow::Error,
) -> anyhow::Error {
    match crate::compose::restore_materialized_config(vat_toml, previous_vat_toml) {
        Ok(()) => error.context(format!(
            "{stage} failed; restored the previous materialized vat.toml"
        )),
        Err(rollback_error) => anyhow::anyhow!(
            "{stage} failed: {error}; also failed to restore the previous materialized vat.toml: {rollback_error}. The registry/config gate will refuse a later compose up; re-import before retrying."
        ),
    }
}

/// Write the compose registry entry for a project.
fn write_registry(registry_dir: &Path, record: &ComposeRecord) -> Result<()> {
    fs::create_dir_all(registry_dir)?;
    let path = registry_dir.join("project.json");
    let json = serde_json::to_string_pretty(record)?;
    // Registry readers run in other compose processes. Write to a unique
    // sibling and atomically rename it into place so readers observe either a
    // complete old JSON record or a complete new one, never a truncation.
    let temporary = registry_dir.join(format!(".project.{}.json.tmp", crate::id::fresh()));
    let write_result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .with_context(|| format!("create compose registry temp {}", temporary.display()))?;
        file.write_all(json.as_bytes())
            .with_context(|| format!("write compose registry temp {}", temporary.display()))?;
        file.sync_all()
            .with_context(|| format!("sync compose registry temp {}", temporary.display()))?;
        drop(file);
        fs::rename(&temporary, &path).with_context(|| {
            format!(
                "replace compose registry {} from {}",
                path.display(),
                temporary.display()
            )
        })?;
        Ok(())
    })();
    if write_result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    write_result?;
    Ok(())
}

/// Decode an optional detached-child handoff. Both variables must be present:
/// accepting only one would turn malformed inherited environment into an
/// uncorrelated VAT run.
pub(crate) fn compose_handoff_from_env() -> Result<Option<ComposeHandoff>> {
    match (
        std::env::var("VAT_COMPOSE_PROJECT").ok(),
        std::env::var("VAT_COMPOSE_STARTUP_TOKEN").ok(),
    ) {
        (None, None) => Ok(None),
        (Some(project), Some(token)) => ComposeHandoff::new(project, token).map(Some),
        _ => bail!(
            "detached compose launcher must provide both VAT_COMPOSE_PROJECT and VAT_COMPOSE_STARTUP_TOKEN"
        ),
    }
}

/// Record the token owner's PID before it loads configuration or clones a
/// workspace. The explicit foreground path and a detached re-exec child use
/// this same operation; both fail before VAT creation if a newer lifecycle
/// reclaimed the registry.
pub(crate) fn register_compose_handoff(handoff: &ComposeHandoff) -> Result<bool> {
    let registry_dir = registry_dir_for_project(&handoff.project)?;
    // The detached parent intentionally holds this claim across spawn and its
    // initial PID write. Blocking here serializes the child handoff instead of
    // letting either side overwrite the other's JSON transition.
    let _claim = StartupClaim::acquire_blocking(&registry_dir, &handoff.project)?;
    let mut record = read_registry(&registry_dir)?;
    if record.project == handoff.project
        && record.status == "starting"
        && record.vat_id.is_none()
        && record.startup_token.as_deref() == Some(handoff.token.as_str())
    {
        record.startup_pid = Some(std::process::id());
        write_registry(&registry_dir, &record)?;
        return Ok(true);
    }
    Ok(false)
}

/// Let the token owner publish its VAT id immediately after durable VAT
/// creation. This is the sole path that can set `ComposeRecord.vat_id` during
/// startup; neither foreground nor detached parents infer it from VAT names.
pub(crate) fn publish_compose_handoff(handoff: &ComposeHandoff, vat_id: &str) -> Result<()> {
    let registry_dir = registry_dir_for_project(&handoff.project)?;
    let _claim = StartupClaim::acquire_blocking(&registry_dir, &handoff.project)?;
    let mut record = read_registry(&registry_dir).with_context(|| {
        format!(
            "read compose registry for project `{}` while publishing VAT `{vat_id}`",
            handoff.project
        )
    })?;
    // Publishing the same ID is idempotent. Any other mismatch is an ownership
    // loss, not a harmless no-op: continuing would create an untracked live
    // service set after a newer lifecycle reclaimed the project.
    if record.project == handoff.project
        && record.vat_id.as_deref() == Some(vat_id)
        && record.startup_token.is_none()
    {
        return Ok(());
    }
    if record.project != handoff.project
        || record.status != "starting"
        || record.vat_id.is_some()
        || record.startup_token.as_deref() != Some(handoff.token.as_str())
    {
        bail!(
            "compose startup for `{}` lost token ownership before publishing VAT `{vat_id}`",
            handoff.project
        );
    }
    record.vat_id = Some(vat_id.to_string());
    record.startup_pid = None;
    record.startup_token = None;
    record.startup_started_at = None;
    write_registry(&registry_dir, &record)
}

/// Clear only the active run binding. Keeping imported service metadata makes
/// a project immediately reusable after `down` or a terminal startup failure.
fn reset_active_run(registry_dir: &Path, record: &mut ComposeRecord) -> Result<()> {
    record.vat_id = None;
    record.startup_pid = None;
    record.startup_token = None;
    record.startup_started_at = None;
    record.status = "imported".to_string();
    write_registry(registry_dir, record)
}

/// Sanitize a project name (simple alphanumeric + dash/underscore).
fn sanitize_project_name(name: &str) -> String {
    name.chars()
        .filter(|c| c.is_alphanumeric() || *c == '-' || *c == '_')
        .collect::<String>()
        .to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::config::RetentionPolicy;
    use crate::state::{ConfigRef, RunnerRunRecord, ServiceRunRecord};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    fn service(id: &str, status: ProcessStatus, readiness_error: Option<&str>) -> ServiceRunRecord {
        ServiceRunRecord {
            id: id.to_string(),
            command: Vec::new(),
            status,
            preset: None,
            host: Some("127.0.0.1".to_string()),
            port: Some(8080),
            owned_by_vat: Some(true),
            prepare_mode: Some("container_run".to_string()),
            cache_key: None,
            prepare_duration_ms: Some(0),
            ready_duration_ms: None,
            exported_env: Vec::new(),
            pid: None,
            exit_code: None,
            ready_http: None,
            docker_name: None,
            microvm_name: None,
            readiness_error: readiness_error.map(str::to_string),
            cleanup_error: None,
            cluster: None,
            stdout_log: String::new(),
            stderr_log: String::new(),
        }
    }

    fn evidence(services: Vec<ServiceRunRecord>, runner: Option<ProcessStatus>) -> TestRunEvidence {
        TestRunEvidence {
            config: ConfigRef {
                path: "vat.toml".to_string(),
                digest: "test".to_string(),
            },
            runner_id: RUNNER_ID.to_string(),
            retention: RetentionPolicy::Always,
            services,
            scenario: None,
            runner: runner.map(|status| RunnerRunRecord {
                id: RUNNER_ID.to_string(),
                command: Vec::new(),
                status,
                exit_code: None,
                duration_ms: None,
                pid: (status == ProcessStatus::Running).then_some(42),
                cleanup_error: None,
                stdout_log: String::new(),
                stderr_log: String::new(),
            }),
            runners: Vec::new(),
            artifacts: Vec::new(),
            plan: None,
            topology: None,
        }
    }

    #[test]
    fn detached_startup_waits_for_all_services_and_surfaces_terminal_evidence() {
        let ids = vec!["web".to_string(), "db".to_string()];
        assert_eq!(
            detached_startup_from_evidence(
                &ids,
                Some(&evidence(
                    vec![
                        service("web", ProcessStatus::Ready, None),
                        service("db", ProcessStatus::Running, None),
                    ],
                    Some(ProcessStatus::Running),
                )),
            ),
            DetachedStartup::Starting
        );

        assert_eq!(
            detached_startup_from_evidence(
                &ids,
                Some(&evidence(
                    vec![
                        service("web", ProcessStatus::Ready, None),
                        service("db", ProcessStatus::Ready, None),
                    ],
                    Some(ProcessStatus::Running),
                )),
            ),
            DetachedStartup::Ready
        );

        // Contradictory duplicate evidence must never turn into a readiness
        // proof: both active/exited reconciliation paths must agree.
        let duplicate_same_id = evidence(
            vec![
                service("web", ProcessStatus::Ready, None),
                service(
                    "web",
                    ProcessStatus::Failed,
                    Some("conflicting duplicate evidence"),
                ),
                service("db", ProcessStatus::Ready, None),
            ],
            Some(ProcessStatus::Running),
        );
        assert_eq!(
            detached_startup_from_evidence(&ids, Some(&duplicate_same_id)),
            DetachedStartup::Starting,
            "duplicate service evidence must reject the exited-run readiness proof"
        );
        assert_eq!(
            detached_startup_while_active(&ids, Some(&duplicate_same_id)),
            DetachedStartup::Starting,
            "duplicate service evidence must reject the active-run readiness proof before exec can spawn"
        );

        assert_eq!(
            detached_startup_from_evidence(
                &ids,
                Some(&evidence(
                    vec![
                        service("web", ProcessStatus::Ready, None),
                        service("db", ProcessStatus::Ready, None),
                    ],
                    None,
                )),
            ),
            DetachedStartup::Starting
        );

        let state = detached_startup_from_evidence(
            &ids,
            Some(&evidence(
                vec![
                    service("web", ProcessStatus::Failed, Some("host endpoint reset")),
                    service("db", ProcessStatus::Ready, None),
                ],
                Some(ProcessStatus::Failed),
            )),
        );
        assert!(matches!(
            state,
            DetachedStartup::Terminal(message)
                if message.contains("web") && message.contains("host endpoint reset")
        ));
    }

    #[test]
    fn detached_startup_treats_terminal_runner_before_readiness_as_failure() {
        let ids = vec!["web".to_string()];
        let state = detached_startup_from_evidence(
            &ids,
            Some(&evidence(
                vec![service("web", ProcessStatus::Running, None)],
                Some(ProcessStatus::Failed),
            )),
        );
        assert!(matches!(
            state,
            DetachedStartup::Terminal(message) if message.contains("project.up")
        ));

        assert_eq!(
            vat_terminal_state_label(&Status::Interrupted {
                signal: libc::SIGTERM,
                reason: "received SIGTERM (15)".to_string(),
            }),
            "Interrupted"
        );
        assert_eq!(
            vat_terminal_state_label(&Status::Exited { code: 0 }),
            "Exited"
        );
    }

    #[test]
    fn cleanup_unconfirmed_blocks_compose_reuse_until_retry_succeeds() {
        let ids = vec!["web".to_string()];
        let mut run = evidence(
            vec![service(
                "web",
                ProcessStatus::Exited,
                Some("endpoint reset"),
            )],
            Some(ProcessStatus::Exited),
        );
        run.services[0].cleanup_error = Some("container rm -f web timed out".to_string());

        assert!(matches!(
            detached_startup_from_evidence(&ids, Some(&run)),
            DetachedStartup::CleanupUnconfirmed(message) if message.contains("container rm")
        ));
        assert!(!compose_services_are_terminal(Some(&run), &ids));

        run.services[0].cleanup_error = None;
        assert!(compose_services_are_terminal(Some(&run), &ids));
    }

    #[test]
    fn registry_ignores_retired_docker_shim_fields() {
        // Registries written while the argv0 Docker shim existed may still
        // carry its provenance key. It must not stop them from loading.
        let record: ComposeRecord = serde_json::from_str(
            r#"{
                "project": "legacy",
                "vat_id": null,
                "docker_shim_profile": "strict-single-image-v1",
                "launch_generation": 3,
                "launch_ticket": "ticket",
                "service_ids": ["web"],
                "status": "imported",
                "created_at": "2026-01-01T00:00:00Z"
            }"#,
        )
        .expect("legacy shim registry must still deserialize");
        assert_eq!(record.project, "legacy");
        assert_eq!(record.service_ids, vec!["web".to_string()]);
        let rewritten = serde_json::to_string(&record).expect("serialize registry");
        assert!(!rewritten.contains("docker_shim_profile"));
        assert!(!rewritten.contains("launch_ticket"));
    }
    #[test]
    fn token_without_pid_is_reclaimed_only_after_handoff_grace() {
        let old = ComposeRecord {
            project: "example".to_string(),
            vat_id: None,
            handoff_protocol: HANDOFF_PROTOCOL,
            startup_pid: None,
            startup_token: Some("old-token".to_string()),
            startup_started_at: Some((Utc::now() - chrono::Duration::seconds(10)).to_rfc3339()),
            service_ids: Vec::new(),
            status: "starting".to_string(),
            created_at: Utc::now().to_rfc3339(),
        };
        assert!(matches!(
            reconcile_detached_startup(&old).expect("reconcile old token"),
            DetachedStartup::Terminal(message) if message.contains("never published")
        ));

        let fresh = ComposeRecord {
            startup_token: Some("fresh-token".to_string()),
            startup_started_at: Some(Utc::now().to_rfc3339()),
            ..old
        };
        assert_eq!(
            reconcile_detached_startup(&fresh).expect("reconcile fresh token"),
            DetachedStartup::Starting
        );
    }

    #[test]
    fn atomic_registry_replacement_never_exposes_torn_json() {
        let temp = tempfile::tempdir().expect("registry tempdir");
        let mut record = ComposeRecord {
            project: "atomic".to_string(),
            vat_id: None,
            handoff_protocol: HANDOFF_PROTOCOL,
            startup_pid: None,
            startup_token: None,
            startup_started_at: None,
            service_ids: vec!["web".to_string()],
            status: "imported".to_string(),
            created_at: Utc::now().to_rfc3339(),
        };
        write_registry(temp.path(), &record).expect("seed registry");

        let done = Arc::new(AtomicBool::new(false));
        let reader_done = Arc::clone(&done);
        let reader_dir = temp.path().to_path_buf();
        let reader = std::thread::spawn(move || {
            while !reader_done.load(Ordering::Acquire) {
                read_registry(&reader_dir)
                    .expect("reader must never observe partial registry JSON");
            }
        });

        for index in 0..128 {
            record.status = if index % 2 == 0 {
                "starting".to_string()
            } else {
                "imported".to_string()
            };
            write_registry(temp.path(), &record).expect("atomic registry replacement");
        }
        done.store(true, Ordering::Release);
        reader.join().expect("registry reader");
    }

    #[test]
    fn compose_access_requires_exact_registry_project_binding() {
        let record = ComposeRecord {
            project: "other-project".to_string(),
            vat_id: None,
            handoff_protocol: HANDOFF_PROTOCOL,
            startup_pid: None,
            startup_token: None,
            startup_started_at: None,
            service_ids: vec!["web".to_string()],
            status: "imported".to_string(),
            created_at: Utc::now().to_rfc3339(),
        };
        let error = require_compose_access(&record, "expected-project")
            .expect_err("mismatched registry project must not be accessible");
        assert!(error
            .to_string()
            .contains("registry belongs to `other-project`"));
    }
}
// HANDWRITE-END
