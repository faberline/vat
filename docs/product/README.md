# vat product documents

Documents in this directory explain the product direction stated in the root
[README.md](../../README.md) and ordered in [ROADMAP.md](../../ROADMAP.md).
They do not add promises: a capability is claimed only in the README, its state
only in [STATUS.md](../../STATUS.md). Each area file ends with its non-goals.

| File | Area | Summary |
|---|---|---|
| [architecture.md](architecture.md) | Architecture | The three pillars as components: native macOS runtime, one shared Linux machine on Virtualization.framework running dockerd, the Docker Engine socket, persistent K3s on that dockerd, the machine's local GCP services (metadata server with Workload Identity, Artifact Registry, shared emulators and webhook), the shared agent-facing core, and the decided and open spikes. |
