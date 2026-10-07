// CODEGEN-BEGIN
//! vat — agent-native, GPU-native dev containers.
//!
//! ## What vat is
//!
//! A headless runtime for the one user who never gets a say in Docker's design:
//! a coding/ML **agent**. vat never ships a GUI or Desktop surface; agents use
//! its CLI and structured output. Two things make it different from
//! developer-oriented Docker tooling:
//!
//! 1. **Agent-legible state.** Every vat projects its full current state as
//!    one compact, structured [`state::VatState`] JSON value — what's
//!    installed, what changed on disk vs. its base, the last run, recent
//!    events, the GPU it can see, its fork lineage. An agent reads *one*
//!    document to understand "what is this environment right now" instead of
//!    parsing the scrollback of `docker ps/inspect/diff/logs`.
//!
//! 2. **GPU-native where it runs on macOS.** On Apple Silicon, Linux
//!    containers run inside a Linux VM, and Metal has no compute passthrough
//!    into that guest — so the M-series GPU is invisible to them. A vat and an
//!    Apple-native container ([`native`]) are **sandboxed host processes** over
//!    a copy-on-write root, so the workload runs natively on macOS and the
//!    Apple GPU (Metal / MPS / MLX) is simply present. See [`gpu`].
//!
//! Linux workloads go to one shared machine ([`vm`], Virtualization.framework)
//! that serves a Docker Engine, a persistent K3s cluster, and local GCP
//! services ([`gcp`]: metadata server with Workload Identity, Artifact
//! Registry, emulators). That machine has no GPU.
//!
//! ## The model
//!
//! A *vat* = a copy-on-write workspace ([`overlay`]) + a declarative
//! [`spec::EnvSpec`] + an append-only [`event`] log + projected
//! [`state::VatState`]. Vats are cheap to [`snapshot`](commands::snapshot) and
//! to **fork** (try two approaches from one starting point), like git for a
//! running environment. Isolation is a pluggable [`sandbox::Sandbox`] backend;
//! v1 ships a host-process backend with an opt-in macOS seatbelt profile.

pub mod commands;
pub mod compose;
pub mod config;
#[cfg(feature = "emulator")]
pub mod emulator;
pub mod event;
pub mod gcp;
pub mod gpu;
pub mod id;
pub mod lumen_release;
pub mod native;
pub mod overlay;
pub mod paths;
#[cfg(feature = "registry")]
pub mod registry;
pub mod sandbox;
pub mod spec;
pub mod state;
pub mod store;
pub mod vm;

pub mod cli;

/// Crate version, surfaced by `vat --version`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
// CODEGEN-END
// CODEGEN-BEGIN
// TODO: Implement src/lib.rs
// CODEGEN-END
