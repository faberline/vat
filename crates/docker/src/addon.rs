//! Extension points for the layers built on the machine.
//!
//! The machine itself knows Linux, `dockerd`, and the host sockets. K3s and
//! local GCP (`vat-k8s`) also need to stage guest files, add uplinks and
//! `/etc/hosts` names, forward host ports, and serve builtin services from the
//! VMM process. They do that through a [`MachineAddon`] the CLI installs once
//! at startup, so this crate never depends on the layers above it. Their
//! settings ride along in [`MachineConfig::layers`].

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, OnceLock};

use anyhow::Result;

use crate::{MachineConfig, MachinePaths, Uplink};

/// Runs `sh -c <script>` in the guest; returns (exit code, combined output).
pub type GuestExec = Arc<
    dyn Fn(String) -> Pin<Box<dyn Future<Output = Result<(i32, String)>> + Send>> + Send + Sync,
>;

/// Builtin uplink services by name (an uplink whose target is
/// `builtin:<name>` is served in the VMM process instead of dialed).
#[cfg(feature = "machine")]
pub type Builtins = std::collections::HashMap<String, crate::bridge::Builtin>;

/// A host loopback listener relayed to a guest port.
#[derive(Debug, Clone, PartialEq)]
pub struct HostForward {
    /// What the listener serves, for log lines.
    pub name: &'static str,
    /// State file under the machine dir that records the listener's outcome.
    pub record: &'static str,
    /// `(host port, guest port)` to listen on, or `None` to stop listening
    /// and drop the record.
    pub ports: Option<(u16, u16)>,
}

/// A layer built on the machine. Every hook has a no-op default.
pub trait MachineAddon: Send + Sync {
    /// Extra guest -> host uplinks.
    fn uplinks(&self, _cfg: &MachineConfig) -> Vec<Uplink> {
        Vec::new()
    }

    /// Extra guest `/etc/hosts` lines.
    fn hosts(&self, _cfg: &MachineConfig) -> Vec<String> {
        Vec::new()
    }

    /// Write this layer's files into the guest share before boot, and remove
    /// the ones a disabled layer left behind.
    fn stage(&self, _paths: &MachinePaths, _cfg: &MachineConfig) -> Result<()> {
        Ok(())
    }

    /// Host listeners the VMM keeps in sync with the config (re-read on
    /// SIGHUP).
    fn forwards(&self, _cfg: &MachineConfig) -> Vec<HostForward> {
        Vec::new()
    }

    /// Why the machine must not stop when idle (e.g. a cluster runs in it),
    /// or `None` to let it.
    fn keeps_awake(&self, _cfg: &MachineConfig) -> Option<&'static str> {
        None
    }

    /// Start the services this layer serves from the VMM process.
    #[cfg(feature = "machine")]
    fn builtins<'a>(
        &'a self,
        _paths: &'a MachinePaths,
        _cfg: &'a MachineConfig,
        _exec: GuestExec,
    ) -> Pin<Box<dyn Future<Output = Builtins> + Send + 'a>> {
        Box::pin(async { Builtins::new() })
    }
}

static INSTALLED: OnceLock<Vec<Box<dyn MachineAddon>>> = OnceLock::new();

/// Install the addons for this process. Only the first call takes effect.
pub fn install(addons: Vec<Box<dyn MachineAddon>>) {
    let _ = INSTALLED.set(addons);
}

/// The installed addons (none until [`install`]).
pub fn installed() -> &'static [Box<dyn MachineAddon>] {
    INSTALLED.get().map(Vec::as_slice).unwrap_or(&[])
}
