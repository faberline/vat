//! vat-k8s — layer 3: Kubernetes on the shared machine, aiming at a local GKE
//! that behaves like the real one.
//!
//! - [`k3s`]: persistent K3s inside the `vat-docker` machine, and the host
//!   kubeconfig;
//! - [`gcp`]: the local GCP surface pods see (metadata server with Workload
//!   Identity, Artifact Registry, Pub/Sub and Storage emulators, the
//!   admission webhook);
//! - [`emulator`], [`registry`]: the Rust GCP emulators and the minimal OCI
//!   registry those services are built from, usable from the host too;
//! - [`addon`]: how this layer plugs into the machine without the machine
//!   depending on it.

pub mod addon;
#[cfg(feature = "emulator")]
pub mod emulator;
pub mod gcp;
pub mod k3s;
#[cfg(feature = "registry")]
pub mod registry;
