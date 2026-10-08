//! vat — a layered container system for Apple Silicon, shipped as one `vat`
//! binary. This crate is the CLI; each layer is its own crate:
//!
//! 1. [`native`] (`vat-native`): Apple-native containers that run directly on
//!    the machine as sandboxed host processes, so the Apple GPU is present;
//! 2. [`vm`] (`vat-docker`): one shared Linux machine serving a Docker Engine;
//! 3. [`k3s`] and [`gcp`] (`vat-k8s`): K3s on that machine and a local GKE
//!    with GCP services.
//!
//! The shared model (spec, overlay, event log, state, sandbox, GPU probe)
//! lives in `vat-core` and is re-exported here under its old paths.

pub use vat_core::{
    config, event, gpu, id, lumen_release, overlay, paths, sandbox, spec, state, store, VERSION,
};
pub use vat_docker as vm;
#[cfg(feature = "emulator")]
pub use vat_k8s::emulator;
#[cfg(feature = "registry")]
pub use vat_k8s::registry;
pub use vat_k8s::{gcp, k3s};
pub use vat_native as native;

pub mod cli;
pub mod commands;
pub mod compose;
