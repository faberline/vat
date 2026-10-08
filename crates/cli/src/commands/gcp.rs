//! `vat gcp` — the machine's local GCP services.

use std::process::ExitCode;

use anyhow::{bail, Result};
use serde_json::{json, Value};

use crate::commands::machine;
use crate::gcp::{self, GcpConfig};
use crate::vm::{MachineConfig, MachinePaths, DEFAULT_MACHINE};

fn load() -> Result<(MachinePaths, Option<MachineConfig>)> {
    let paths = MachinePaths::new(DEFAULT_MACHINE)?;
    let cfg = MachineConfig::load(&paths.config)?;
    Ok((paths, cfg))
}

fn endpoints(cfg: &GcpConfig) -> Value {
    json!({
        "metadata": format!("http://{}", gcp::METADATA_IP),
        "registry": format!("https://{}", cfg.registry_host()),
        "pubsub": format!("{}:{}", gcp::EMULATOR_IP, gcp::PUBSUB_PORT),
        "storage": format!("http://{}:{}", gcp::EMULATOR_IP, gcp::STORAGE_PORT),
    })
}

pub fn status(json_out: bool) -> Result<ExitCode> {
    let (paths, cfg) = load()?;
    let gcp_cfg = cfg
        .as_ref()
        .map(GcpConfig::of)
        .transpose()?
        .unwrap_or_default();
    let running = machine::running_pid(&paths).is_some();
    let state = running
        .then(|| machine::read_json(&paths.dir.join("gcp.json")))
        .flatten();
    let services = state.as_ref().is_some_and(|s| s["running"] == true);
    let report = json!({
        "enabled": gcp_cfg.enabled,
        "machine": if running { "running" } else if cfg.is_some() { "stopped" } else { "absent" },
        "running": services,
        // A machine started by an older vat has no services until restarted.
        "restart_required": running && gcp_cfg.enabled && state.is_none(),
        "project": gcp_cfg.project,
        "project_number": gcp_cfg.project_number(),
        "zone": gcp_cfg.zone,
        "region": gcp_cfg.region(),
        "node_service_account": gcp_cfg.node_service_account(),
        "workload_pool": gcp_cfg.workload_pool(),
        "in_machine": endpoints(&gcp_cfg),
        "host": state.as_ref().map(|s| s["host"].clone()),
        "error": state.as_ref().and_then(|s| s.get("error").cloned()),
        "registry_root": state.as_ref().map(|s| s["registry"]["root"].clone()),
        "ca": state.as_ref().map(|s| s["ca"].clone()),
    });
    if json_out {
        crate::commands::print_json(&report, false)?;
    } else {
        let what = match (gcp_cfg.enabled, running, services) {
            (false, _, _) => "disabled",
            (true, false, _) => "stopped",
            (true, true, false) => "not running (restart the machine)",
            (true, true, true) => "running",
        };
        println!("gcp       {what}");
        println!(
            "project   {} ({})",
            gcp_cfg.project,
            gcp_cfg.project_number()
        );
        println!("zone      {}", gcp_cfg.zone);
        println!("registry  {}", gcp_cfg.registry_host());
        println!("metadata  http://metadata.google.internal (pods)");
    }
    Ok(ExitCode::SUCCESS)
}

pub fn env(json_out: bool) -> Result<ExitCode> {
    let (_, cfg) = load()?;
    let gcp_cfg = cfg
        .as_ref()
        .map(GcpConfig::of)
        .transpose()?
        .unwrap_or_default();
    if !gcp_cfg.enabled {
        bail!("local GCP is disabled; run `vat gcp config --enable`");
    }
    let host = gcp_cfg.host_env();
    if json_out {
        let to_map = |vars: &[(&str, String)]| -> Value {
            vars.iter()
                .map(|(k, v)| (k.to_string(), json!(v)))
                .collect::<serde_json::Map<_, _>>()
                .into()
        };
        crate::commands::print_json(
            &json!({
                "host": to_map(&host),
                "pods": to_map(&gcp_cfg.pod_env()),
                "registry": gcp_cfg.registry_host(),
            }),
            false,
        )?;
    } else {
        for (k, v) in host {
            println!("export {k}={v}");
        }
    }
    Ok(ExitCode::SUCCESS)
}

pub struct ConfigArgs {
    pub project: Option<String>,
    pub zone: Option<String>,
    pub host_pubsub_port: Option<u16>,
    pub host_storage_port: Option<u16>,
    pub enabled: Option<bool>,
    pub json: bool,
}

fn valid_project(id: &str) -> bool {
    (6..=30).contains(&id.len())
        && id.starts_with(|c: char| c.is_ascii_lowercase())
        && !id.ends_with('-')
        && id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

pub fn config(args: ConfigArgs) -> Result<ExitCode> {
    let (paths, cfg) = load()?;
    let mut cfg = cfg.unwrap_or_default();
    let mut gcp_cfg = GcpConfig::of(&cfg)?;
    let before = gcp_cfg.clone();
    if let Some(project) = args.project {
        if !valid_project(&project) {
            bail!("invalid project id {project:?}: 6-30 lowercase letters, digits, or hyphens, starting with a letter");
        }
        gcp_cfg.project = project;
    }
    if let Some(zone) = args.zone {
        if zone.split('-').count() < 2 {
            bail!("invalid zone {zone:?}; expected e.g. us-central1-a");
        }
        gcp_cfg.zone = zone;
    }
    if let Some(port) = args.host_pubsub_port {
        gcp_cfg.host_pubsub_port = port;
    }
    if let Some(port) = args.host_storage_port {
        gcp_cfg.host_storage_port = port;
    }
    if let Some(enabled) = args.enabled {
        gcp_cfg.enabled = enabled;
    }
    let changed = gcp_cfg != before;
    if changed {
        gcp_cfg.store(&mut cfg);
        std::fs::create_dir_all(&paths.dir)?;
        cfg.save(&paths.config)?;
    }
    let restart_required = changed && machine::running_pid(&paths).is_some();
    if args.json {
        crate::commands::print_json(
            &json!({ "gcp": gcp_cfg, "changed": changed, "restart_required": restart_required }),
            false,
        )?;
    } else {
        println!(
            "gcp {} (project {}, zone {})",
            if gcp_cfg.enabled {
                "enabled"
            } else {
                "disabled"
            },
            gcp_cfg.project,
            gcp_cfg.zone
        );
        if restart_required {
            println!("restart the machine to apply: vat machine stop && vat machine start");
        }
    }
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::valid_project;

    #[test]
    fn project_ids_follow_gcp_rules() {
        assert!(valid_project("vat-local"));
        assert!(valid_project("my-proj-123"));
        assert!(!valid_project("short"));
        assert!(!valid_project("Upper-case"));
        assert!(!valid_project("1starts-digit"));
        assert!(!valid_project("ends-with-"));
    }
}
