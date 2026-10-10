//! vat-core — the model every vat layer shares.
//!
//! vat is a layered container system: Apple-native containers that run
//! directly on the machine (`vat-native`), a Docker Engine on one shared Linux
//! machine (`vat-docker`), and K3s with local GCP/GKE on that machine
//! (`vat-k8s`). This crate holds what they and the `vat` CLI have in common:
//!
//! - [`spec`] and [`config`]: the declarative env spec and `vat.toml`;
//! - [`overlay`], [`event`], [`state`], [`store`]: a vat's copy-on-write
//!   workspace, its append-only event log, and the projected state an agent
//!   reads as one JSON document;
//! - [`sandbox`]: host-process isolation (seatbelt) and the MicroVM backend;
//! - [`gpu`]: what Apple GPU a host process can see;
//! - [`paths`], [`id`], [`lumen_release`]: shared plumbing.

pub mod config;
pub mod event;
pub mod gpu;
pub mod id;
pub mod lumen_release;
pub mod overlay;
pub mod paths;
pub mod sandbox;
pub mod spec;
pub mod state;
pub mod store;

/// The vat release every crate in the workspace shares.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
