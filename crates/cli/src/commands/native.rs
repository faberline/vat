//! `vat image …`, `vat container …`, and `vat native …`: the Darwin native
//! runtime (see [`crate::native`]).

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{bail, Result};
use clap::Subcommand;

use super::print_json;
use crate::native::{build, container, store::ImageStore, users};

#[derive(Subcommand, Debug)]
pub enum ImageCmd {
    /// Build a darwin/arm64 image from a Vatfile (Dockerfile subset).
    Build {
        /// Name to tag the result as, NAME[:TAG].
        #[arg(short = 't', long = "tag")]
        tag: String,
        /// Build file (default: CONTEXT/Vatfile, then CONTEXT/Dockerfile).
        #[arg(short = 'f', long = "file")]
        file: Option<PathBuf>,
        /// Build context directory.
        context: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Pull a darwin/arm64 image from an OCI registry.
    Pull {
        reference: String,
        #[arg(long)]
        json: bool,
    },
    /// Push a local image to an OCI registry (DEST defaults to SOURCE).
    Push {
        source: String,
        destination: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Import images from an OCI image-layout directory.
    Import {
        #[arg(long = "oci-layout")]
        oci_layout: PathBuf,
        /// Tag to apply (required when the layout entry carries no name).
        #[arg(long)]
        tag: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Export images to an OCI image-layout directory.
    Export {
        #[arg(long = "oci-layout")]
        oci_layout: PathBuf,
        #[arg(required = true)]
        names: Vec<String>,
    },
    /// List local native images.
    Ls {
        #[arg(long)]
        json: bool,
    },
    /// Remove image names, then garbage-collect unreferenced blobs/snapshots.
    Rm {
        #[arg(required = true)]
        names: Vec<String>,
    },
    /// Show an image's manifest, config, and relocations (JSON).
    Inspect {
        name: String,
        /// Single-line JSON.
        #[arg(long)]
        json: bool,
    },
    /// Add a name for an existing local image.
    Tag { source: String, target: String },
}

#[derive(Subcommand, Debug)]
pub enum ContainerCmd {
    /// Create and start a container from a native image.
    Run {
        #[arg(long)]
        name: Option<String>,
        /// Run in the background and print the container id.
        #[arg(short = 'd', long)]
        detach: bool,
        /// Environment variable K=V (repeatable).
        #[arg(short = 'e', long = "env")]
        env: Vec<String>,
        /// Bind HOST:CONTAINER[:ro] (symlink + seatbelt write allowance).
        #[arg(short = 'v', long = "volume")]
        volumes: Vec<String>,
        /// none | host.
        #[arg(long, default_value = "host")]
        network: String,
        /// Remove the container when it exits.
        #[arg(long)]
        rm: bool,
        /// Working directory inside the root.
        #[arg(short = 'w', long = "workdir")]
        workdir: Option<String>,
        image: String,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
    },
    /// List containers (running only unless --all).
    Ps {
        #[arg(short = 'a', long)]
        all: bool,
        #[arg(long)]
        json: bool,
    },
    /// Print a container's stdout/stderr.
    Logs {
        container: String,
        #[arg(short = 'f', long)]
        follow: bool,
    },
    /// Run a command inside a running or stopped container's root.
    Exec {
        #[arg(short = 'e', long = "env")]
        env: Vec<String>,
        #[arg(short = 'w', long = "workdir")]
        workdir: Option<String>,
        container: String,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        command: Vec<String>,
    },
    /// SIGTERM the container's process group, SIGKILL after --time seconds.
    Stop {
        #[arg(short = 't', long = "time", default_value_t = container::DEFAULT_STOP_TIMEOUT_S)]
        time: u64,
        #[arg(required = true)]
        containers: Vec<String>,
    },
    /// Remove containers and their roots.
    Rm {
        #[arg(short = 'f', long)]
        force: bool,
        #[arg(required = true)]
        containers: Vec<String>,
    },
    /// Show a container's state (JSON).
    Inspect {
        container: String,
        /// Single-line JSON.
        #[arg(long)]
        json: bool,
    },
    /// Files added/changed/deleted in the root since creation.
    Diff {
        container: String,
        #[arg(long)]
        json: bool,
    },
    #[command(name = "__supervise", hide = true)]
    Supervise { id: String },
}

#[derive(Subcommand, Debug)]
pub enum NativeCmd {
    /// Manage the hidden `_vatN` UID pool used for per-container users.
    Users {
        #[command(subcommand)]
        cmd: UsersCmd,
    },
}

#[derive(Subcommand, Debug)]
pub enum UsersCmd {
    /// Create hidden users `_vat1.._vatN` with dscl (requires root).
    Setup {
        #[arg(long, default_value_t = 8)]
        count: u32,
        #[arg(long, default_value_t = users::DEFAULT_FIRST_ID)]
        first_id: u32,
        /// Print the dscl commands instead of running them.
        #[arg(long)]
        print: bool,
    },
    /// Show the pool and whether UID isolation would be active.
    Ls {
        #[arg(long)]
        json: bool,
    },
}

fn exit(code: i32) -> ExitCode {
    ExitCode::from(code.clamp(0, 255) as u8)
}

fn short(digest: &str) -> &str {
    let hex = digest.trim_start_matches("sha256:");
    &hex[..hex.len().min(12)]
}

pub fn image(cmd: ImageCmd) -> Result<ExitCode> {
    let store = ImageStore::open()?;
    match cmd {
        ImageCmd::Build {
            tag,
            file,
            context,
            json,
        } => {
            let outcome = build::build(&build::BuildOptions { tag, file, context })?;
            if json {
                print_json(&outcome, false)?;
            } else {
                println!("{} {}", outcome.reference, outcome.digest);
            }
        }
        ImageCmd::Pull { reference, json } => {
            #[cfg(feature = "registry")]
            {
                let outcome = crate::native::distribution::pull(&store, &reference)?;
                if json {
                    print_json(&outcome, false)?;
                } else {
                    println!(
                        "{} {}",
                        outcome.tagged.as_deref().unwrap_or(&reference),
                        outcome.digest
                    );
                }
            }
            #[cfg(not(feature = "registry"))]
            {
                let _ = (reference, json);
                bail!("this vat was built without the `registry` feature; `vat image pull` is unavailable (use `vat image import --oci-layout`)");
            }
        }
        ImageCmd::Push {
            source,
            destination,
            json,
        } => {
            #[cfg(feature = "registry")]
            {
                let outcome =
                    crate::native::distribution::push(&store, &source, destination.as_deref())?;
                if json {
                    print_json(&outcome, false)?;
                } else {
                    println!("{} {}", outcome.destination, outcome.digest);
                }
            }
            #[cfg(not(feature = "registry"))]
            {
                let _ = (source, destination, json);
                bail!("this vat was built without the `registry` feature; `vat image push` is unavailable (use `vat image export --oci-layout`)");
            }
        }
        ImageCmd::Import {
            oci_layout,
            tag,
            json,
        } => {
            let imported = store.import(&oci_layout, tag.as_deref())?;
            if json {
                let rows: Vec<_> = imported
                    .iter()
                    .map(|(name, digest)| serde_json::json!({ "name": name, "digest": digest }))
                    .collect();
                print_json(&rows, false)?;
            } else {
                for (name, digest) in imported {
                    println!("{name} {digest}");
                }
            }
        }
        ImageCmd::Export { oci_layout, names } => {
            for digest in store.export(&names, &oci_layout)? {
                println!("{digest}");
            }
        }
        ImageCmd::Ls { json } => {
            let images = store.list()?;
            if json {
                let rows: Vec<_> = images
                    .iter()
                    .map(|(names, image)| image.summary(names.clone()))
                    .collect();
                print_json(&rows, false)?;
            } else {
                println!(
                    "{:<40} {:<12} {:>6} {:>12}",
                    "NAME", "DIGEST", "LAYERS", "SIZE"
                );
                for (names, image) in images {
                    let summary = image.summary(names.clone());
                    for name in names {
                        println!(
                            "{:<40} {:<12} {:>6} {:>12}",
                            name,
                            short(&image.manifest_digest),
                            image.manifest.layers.len(),
                            summary["size"]
                        );
                    }
                }
            }
        }
        ImageCmd::Rm { names } => {
            for name in names {
                let (removed, digest) = store.remove(&name)?;
                println!("untagged {removed} ({})", short(&digest));
            }
            let in_use = container::images_in_use();
            let stats = store.gc(&in_use)?;
            if stats.blobs + stats.snapshots > 0 {
                println!(
                    "deleted {} blobs, {} snapshots ({} bytes)",
                    stats.blobs, stats.snapshots, stats.bytes
                );
            }
        }
        ImageCmd::Inspect { name, json } => {
            let image = store.resolve(&name)?;
            let names = store
                .names_by_digest()?
                .remove(&image.manifest_digest)
                .unwrap_or_default();
            let mut value = image.summary(names);
            value["manifest"] = serde_json::to_value(&image.manifest)?;
            value["config"] = serde_json::to_value(&image.config)?;
            value["relocation_paths"] = serde_json::to_value(image.relocations()?)?;
            print_json(&value, json)?;
        }
        ImageCmd::Tag { source, target } => {
            let image = store.resolve(&source)?;
            let name = store.set_ref(&target, &image.manifest_digest)?;
            println!("{name} {}", image.manifest_digest);
        }
    }
    Ok(ExitCode::SUCCESS)
}

pub fn container(cmd: ContainerCmd) -> Result<ExitCode> {
    match cmd {
        ContainerCmd::Run {
            name,
            detach,
            env,
            volumes,
            network,
            rm,
            workdir,
            image,
            command,
        } => {
            let opts = container::RunOptions {
                name,
                detach,
                env,
                volumes,
                network: container::Network::parse(&network)?,
                auto_remove: rm,
                workdir,
                image,
                command,
            };
            // uid_isolation (and why it is unavailable) is reported by
            // `vat container inspect`, not on the workload's stderr.
            let created = container::create(&opts)?;
            if detach {
                container::run_detached(&created)?;
                println!("{}", created.record.id);
                Ok(ExitCode::SUCCESS)
            } else {
                Ok(exit(container::run_foreground(&created)?))
            }
        }
        ContainerCmd::Ps { all, json } => {
            let rows: Vec<_> = container::list()?
                .iter()
                .filter(|c| all || c.status() == container::Status::Running)
                .map(container::ps_row)
                .collect();
            if json {
                print_json(&rows, false)?;
            } else {
                println!(
                    "{:<16} {:<20} {:<28} {:<10} COMMAND",
                    "ID", "NAME", "IMAGE", "STATUS"
                );
                for row in rows {
                    let status = match (row["status"].as_str(), row["exit_code"].as_i64()) {
                        (Some("exited"), Some(code)) => format!("exited({code})"),
                        (Some(s), _) => s.to_string(),
                        _ => "?".into(),
                    };
                    let command: Vec<String> = row["command"]
                        .as_array()
                        .map(|a| {
                            a.iter()
                                .filter_map(|v| v.as_str().map(str::to_string))
                                .collect()
                        })
                        .unwrap_or_default();
                    println!(
                        "{:<16} {:<20} {:<28} {:<10} {}",
                        row["id"].as_str().unwrap_or(""),
                        row["name"].as_str().unwrap_or(""),
                        row["image"].as_str().unwrap_or(""),
                        status,
                        command.join(" ")
                    );
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        ContainerCmd::Logs {
            container: key,
            follow,
        } => {
            container::logs(&container::find(&key)?, follow)?;
            Ok(ExitCode::SUCCESS)
        }
        ContainerCmd::Exec {
            env,
            workdir,
            container: key,
            command,
        } => {
            let target = container::find(&key)?;
            Ok(exit(container::exec(
                &target,
                &command,
                &env,
                workdir.as_deref(),
            )?))
        }
        ContainerCmd::Stop { time, containers } => {
            for key in containers {
                let target = container::find(&key)?;
                let code = container::stop(&target, Duration::from_secs(time))?;
                match code {
                    Some(code) => println!("{} exited({code})", target.record.id),
                    None => println!("{}", target.record.id),
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        ContainerCmd::Rm { force, containers } => {
            for key in containers {
                let target = container::find(&key)?;
                container::remove(&target, force)?;
                println!("{}", target.record.id);
            }
            Ok(ExitCode::SUCCESS)
        }
        ContainerCmd::Inspect {
            container: key,
            json,
        } => {
            print_json(&container::find(&key)?.inspect()?, json)?;
            Ok(ExitCode::SUCCESS)
        }
        ContainerCmd::Diff {
            container: key,
            json,
        } => {
            let changes = container::find(&key)?.changes()?;
            if json {
                print_json(&changes, false)?;
            } else {
                for path in &changes.added {
                    println!("A {path}");
                }
                for path in &changes.modified {
                    println!("C {path}");
                }
                for path in &changes.deleted {
                    println!("D {path}");
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        ContainerCmd::Supervise { id } => {
            container::supervise(&id)?;
            Ok(ExitCode::SUCCESS)
        }
    }
}

pub fn native(cmd: NativeCmd) -> Result<ExitCode> {
    match cmd {
        NativeCmd::Users { cmd } => match cmd {
            UsersCmd::Setup {
                count,
                first_id,
                print,
            } => {
                if count == 0 {
                    bail!("--count must be at least 1");
                }
                if print {
                    users::check_setup_ids(count, first_id)?;
                    for argv in users::setup_commands(count, first_id) {
                        let line: Vec<String> =
                            argv.iter().map(|a| users::shell_quote(a)).collect();
                        println!("sudo {}", line.join(" "));
                    }
                } else {
                    users::run_setup(count, first_id)?;
                    println!(
                        "created {count} pool users ({}1..{}{count})",
                        users::POOL_PREFIX,
                        users::POOL_PREFIX
                    );
                }
                Ok(ExitCode::SUCCESS)
            }
            UsersCmd::Ls { json } => {
                let pool = users::discover_pool();
                let euid = users::euid();
                let base = crate::native::roots_base()?;
                let decision = users::decide(
                    euid,
                    &pool,
                    &Default::default(),
                    users::world_traversable(&base),
                );
                let (state, reason) = match decision {
                    users::Decision::Active(_) => ("active", None),
                    users::Decision::RunAsInvoker { reason } => ("unavailable", Some(reason)),
                    users::Decision::Refuse { reason } => ("refused", Some(reason)),
                };
                let value = serde_json::json!({
                    "pool": pool,
                    "euid": euid,
                    "roots_base": base,
                    "uid_isolation": state,
                    "reason": reason,
                });
                if json {
                    print_json(&value, false)?;
                } else {
                    println!("pool users: {}", pool.len());
                    for user in &pool {
                        println!("  {} uid={} gid={}", user.name, user.uid, user.gid);
                    }
                    println!("uid_isolation: {state}");
                    if let Some(reason) = value["reason"].as_str() {
                        println!("  {reason}");
                    }
                }
                Ok(ExitCode::SUCCESS)
            }
        },
    }
}
