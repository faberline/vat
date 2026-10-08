# vat architecture

This document explains how the three pillars stated in the root
[README.md](../../README.md) fit together as components, which of them exist in
the current tree, and which decisions are still open. It adds no promises:
what is shipped is in [STATUS.md](../../STATUS.md); what is committed is in
[ROADMAP.md](../../ROADMAP.md). Milestone references (M1 through M5) use the
order the product owner confirmed in the roadmap; outcomes after M5 are
uncommitted.

## Shape in one picture

```
agent ──► vat CLI (structured JSON, vat.toml, vat state / diff / fork / snapshot)
              │
              ├── pillar 1: native macOS runtime ─────────── macOS process
              │     APFS clonefile rootfs, seatbelt, dedicated UID,     │
              │     OCI darwin/arm64 layers                      Metal / MPS / MLX
              │
              ├── pillar 2: ONE shared Linux machine ──────── dockerd
              │     Virtualization.framework (vat-vmm), Alpine guest     │
              │     ~/.vat/run/docker.sock over vsock, virtiofs,         ├── containers
              │     Rosetta, published ports                             └── K3s --docker (pillar 3)
              │                                                              │
              │     VMM built-ins on guest link-local uplinks:               │ pods
              │       metadata.google.internal + Workload Identity ◄─────────┤
              │       <region>-docker.pkg.dev (Artifact Registry, TLS) ◄─────┤
              │       shared Pub/Sub + Storage emulators, webhook ◄───────────┘
              │
              └── pillar 3: local GCP ─────────────────────── emulators + routing
                    Pub/Sub, Auth, Tasks, Scheduler, Workflows, GCS,
                    http-mock/OpenAPI, gcloud-wrapped Firestore/Datastore/
                    Bigtable/Spanner (host-side, per run)
```

The agent-facing core (left) is shared by every pillar and already ships. The
native runtime is the GPU path and has no VM. The Docker and GKE pillars share
exactly one Linux machine. The GCP pillar is reachable from host processes
through the per-run presets and transparent routing, and from pods through the
machine's built-in metadata server, registry, and shared emulators.

### Crate layout

The code follows the same layers as a Cargo workspace under `crates/`. It
still builds one `vat` binary.

| crate | dir | layer |
|---|---|---|
| `vat-core` | `crates/core` | shared model: spec, `vat.toml`, overlay, event log, state, store, sandbox backends, GPU probe |
| `vat-native` | `crates/native` | pillar 1: Apple-native containers and the OCI distribution client |
| `vat-docker` | `crates/docker` | pillar 2: the shared machine (VMM, vsock bridges, guest files) and its Docker Engine |
| `vat-k8s` | `crates/k8s` | pillar 3: K3s, the local GCP services, the Rust emulators, and the registry |
| `vat` | `crates/cli` | the CLI and the `vat` binary |

Dependencies point down only: `vat` → `vat-k8s` → `vat-docker` → `vat-core`,
and `vat` → `vat-native` → `vat-core`. The machine does not depend on the
layer above it. `vat-k8s` plugs K3s and local GCP into the machine through
`vat_docker::addon::MachineAddon`, which the CLI installs at startup. The K3s
and GCP settings are kept in the machine config under their original keys.

## Shared core: the agent-facing model

A *vat* is a copy-on-write workspace plus a declarative `EnvSpec`, an
append-only event log, and a projected `VatState` document
(`crates/core/src/overlay.rs`, `crates/core/src/spec.rs`, `crates/core/src/event.rs`, `crates/core/src/state.rs`). `vat run`
clones a base, runs the workload in the selected `Sandbox` backend
(`crates/core/src/sandbox/`), records the run, recomputes the diff, and cleans up by
policy. `vat fork` and `vat snapshot` are `clonefile` operations, so branching
a running environment is cheap. `vat.toml` is the run protocol: setup steps,
services, readiness, runner, artifacts, scenarios, retention.

Every pillar plugs into this core at two points: a **sandbox backend** (where
the workload process runs) and a **service kind** (what a `[[services]]` entry
can start). Nothing in a pillar is allowed to bypass `vat state`; if an agent
needs a fact about a container, a VM, or a cluster, it belongs in that
document.

## Pillar 1: lightweight Apple-native containers

**Today.** The `process` and `seatbelt` backends run the workload as a macOS
process whose working directory is the copy-on-write rootfs. Seatbelt wraps
it in a generated `sandbox-exec` profile that allows broad reads (so
toolchains resolve) and confines writes to the rootfs and temp dirs; the
`[network].egress` policy is enforced there and fails closed on a backend that
cannot enforce it. Because nothing is virtualized, Metal, PyTorch MPS, MLX,
and `tensorflow-metal` see the native GPU; `vat gpu` reports it.

**Direction (M1).** The runtime becomes a container runtime in the OCI sense
while staying a macOS process model:

- *Image format.* An OCI image whose manifest carries `darwin/arm64` layers.
  Layers are ordinary tar layers of macOS filesystem content: a Python
  environment, a set of Homebrew bottles, a project checkout. vat pulls the
  image into a local store and materializes the layer stack into a base
  directory once; each container is a `clonefile` of that base, so N
  containers from one image share blocks until written.
- *Identity.* A dedicated UID per container, allocated by vat, so file
  ownership inside the rootfs and process ownership in `ps` are attributable
  and two containers cannot trample each other's files by accident.
- *Lifecycle.* Every container is a process group vat owns; the interrupt
  cleanup that already ships (TERM, grace, KILL, PGID-absence check) is the
  lifecycle primitive, extended to start/stop/exec verbs.
- *Confinement.* Seatbelt path confinement scoped to the rootfs, temp dirs,
  and declared mounts.

**What this pillar is not.** macOS has no namespaces or cgroups. A dedicated
UID plus seatbelt plus a private rootfs is resource isolation for cooperative
workloads; it is weaker than a VM and is not a boundary for hostile code.
There is no Linux in this pillar: a Linux image or a Linux-only tool goes to
pillar 2.

**Open spike: chroot versus seatbelt path confinement.** chroot would give a
real root-directory boundary, but it needs root privileges and macOS binaries
depend on the dyld shared cache and system frameworks outside any chroot, which
makes a self-contained darwin rootfs impractical. Seatbelt-only confinement
keeps the system paths readable and confines writes, at the cost of being a
policy rather than a namespace.

### Decision: seatbelt + fixed-length relocation, no chroot

Decided for M1 and implemented in `crates/native/src/` (gate:
`cargo test -p vat --test vat_native_runtime`).

- *No chroot.* A container's `/` is the host's `/`. dyld, the shared cache,
  system frameworks, Metal, and `/usr/bin` tools resolve exactly as on the
  host, and no root privilege is needed to start a container. Image content
  lives under the container root and is reached through `$VAT_ROOT`. `PATH`
  is rewritten root-relative, with the host system dirs appended.
- *Seatbelt confinement.* Each workload (and each `vat image build` `RUN`
  step) runs under a generated `sandbox-exec` profile. Writes are allowed
  only under the container root (whose `tmp/` is `TMPDIR`), read-write `-v`
  mounts, the invoking user's per-user cache dir (Metal shader caches), and
  the usual character devices. There is no blanket `/tmp` or
  `/private/var/folders` write allowance. Reads stay broad, since a macOS
  process cannot run without them. `--network none` reuses the egress
  `deny` rules.
- *Fixed-length relocation instead of a fixed mount point.* Without chroot,
  a tool that bakes its install path into files (a Python venv, shebangs,
  `.pc` files, Mach-O load commands) would break when the image runs from a
  different directory. Every build root and container root therefore has
  the same length, 128 bytes, padded with a fixed filler. At layer commit,
  occurrences of the build root are replaced byte-for-byte by a placeholder
  of the same length, and each affected path is recorded in the
  `vat.relocations` file of the image. At container creation, the
  placeholder is replaced by the container root of identical length.
  Offsets never shift, so binary files stay valid. Rewritten Mach-O files
  are re-signed ad hoc, because a changed page invalidates their signature.
  The root base directory must be at most 109 bytes for the padded root to
  fit; `VAT_NATIVE_ROOT_BASE` overrides it.
- *Identity is separate.* A dedicated UID per container needs a root-created
  hidden user pool (`vat native users setup`) and vat itself running as
  root. Otherwise the workload runs as the invoking user, and the container
  reports `uid_isolation: "unavailable"` with the reason.

Consequences: this is a cooperative-workload boundary, not a hostile-code
one. Reads of the host filesystem are unrestricted, and a workload that
writes an absolute host path such as `/opt/x` is denied rather than
redirected into its root. `sandbox-exec` is Apple-deprecated; if it is
removed, the confinement layer has to be replaced, but the relocation layer
does not.

## Pillar 2: complete Docker over one shared Linux machine

**Today (M2, M3 landed).** Two layers, both in `crates/docker/src/`:

- *Substrate (M2).* One shared lightweight Linux machine per host, named
  `default`, started on demand by `vat machine start`. It runs on Apple's
  Virtualization.framework: the VMM is a codesigned copy of the vat binary
  (`~/.vat/machine/bin/vat-vmm`) holding the `com.apple.security.virtualization`
  entitlement, spawned detached as `vat machine __vmm` (`crates/docker/src/vmm.rs`).
  The guest is Alpine on a sparse persistent ext4 data disk (`data.img`),
  booted from a downloaded kernel and a vat-generated initramfs
  (`crates/docker/src/assets.rs`); guest scripts live in a
  virtiofs share (`crates/docker/src/guest/`) copied in on every start, so guest
  behavior changes without rebuilding images. Host directories are shared
  over virtiofs at the same absolute paths, Rosetta runs `linux/amd64`
  images, and a vsock guest agent dials guest sockets for the host
  (`crates/docker/src/bridge.rs`): the Docker socket, a control socket, guest-to-host
  uplinks, and published container ports. The plan named libkrun on
  Hypervisor.framework; Virtualization.framework shipped instead because it
  gives virtiofs, vsock, Rosetta, and a signed-helper model without a
  third-party hypervisor library. The machine is a long-lived substrate vat
  reports through `vat machine status --json`; it is the one exception to
  "vat is not a process manager", and it is managed only on explicit command.
- *Engine (M3).* The guest runs upstream dockerd; the VMM forwards its socket
  to `~/.vat/run/docker.sock` over vsock. There is no translation layer and
  no declared endpoint subset: Docker compatibility is Docker's own, so the
  unmodified `docker` CLI, `docker compose` v2, Testcontainers, and the SDKs
  see a real Engine. `crates/docker/src/engine.rs` makes that socket the default
  `DOCKER_HOST` for everything vat itself runs (`vat build`, image services,
  compose runners, probes), booting the machine on demand; an explicit
  `DOCKER_HOST`/`DOCKER_CONTEXT` always wins and `VAT_ENGINE=external` opts
  out. The argv0 `docker` shim over Apple Container is retired.

**Remaining Apple Container route.** The explicit `runtime = "micro_vm"`
service runtime and `MicroVmBackend` (`crates/core/src/sandbox/microvm.rs`) still use the
`container` CLI, one VM per container. It stays `Limited` in
[STATUS.md](../../STATUS.md) as the superseded path and is not extended.

**Efficiency as the goal.** Startup time from cold, idle guest memory, the
memory macOS pays for the VM (the physical footprint of
Virtualization.framework's VM process, not the VMM's RSS), idle VM CPU, disk
allocation, and build time are recorded by the machine E2E
(`crates/cli/tests/vat_machine_e2e.rs`) into `vat-machine-e2e.json`; a number becomes a
budget in STATUS only when the owner confirms it. No efficiency figure is
promised before that.

**Host resources** (`crates/docker/src/elastic.rs`):

- *Disk.* The guest agent discards free blocks every ten minutes (FITRIM),
  and Virtualization.framework punches the matching holes in the sparse
  `data.img`, so deleted images and volumes return their space to macOS.
- *Clock.* The guest clock stands still while the host sleeps. The VMM
  notices the wall/monotonic gap after a wake and steps the guest clock to the
  host's. It also does this 30 s after boot, every 15 minutes, and on SIGHUP.
- *Memory.* The VM's footprint is the high-water mark of what the guest has
  ever touched. Virtualization.framework does not return guest pages while
  the VM runs. Its virtio balloon takes pages from the guest (measured: 2816
  MiB inflated) but leaves the VM process footprint unchanged. So the VMM
  does not drive the balloon, and only stopping the VM gives the memory back.

**GPU.** Metal does not pass into a Linux guest, so a Linux container has no
Apple GPU. A Vulkan (Venus) path inside the machine is explicitly deferred and
is not a commitment; the native pillar remains the GPU path.

## Pillar 3: realistic local GCP, especially GKE

**Today.** Built-in pure-Rust emulators (`crates/k8s/src/emulator/`): Pub/Sub (gRPC),
Firebase Auth (REST), Cloud Tasks and Cloud Scheduler (gRPC and REST on one
port, with dispatch to targets), Cloud Workflows (REST, subset interpreter
that can orchestrate sibling emulators), Cloud Storage (JSON API v1), the
`http-mock` stub and record/replay proxy with HTTPS MITM, and the `openapi`
spec-driven mock. gcloud-wrapped presets cover Firestore, Datastore, Bigtable,
and Spanner, and `firebase` wraps the Emulator Suite. Transparent routing
(`[[network.routes]]`, auto-derived for declared GCP presets) sends a runner's
calls to the real `*.googleapis.com` host, REST or gRPC, to the local emulator
with no code change.

**Persistent K3s in the shared machine (M4, landed).** `vat k8s up` enables
K3s (one pinned release, `v1.36.5+k3s1`, `crates/k8s/src/k3s.rs`) inside the M2
machine. K3s is started with `--docker`, so pods run on the machine's dockerd
rather than on K3s's embedded containerd: an image built through the M3
socket is visible to pods immediately with no load or push step. The cluster
state and PVC data live on the machine's data disk and the kubeconfig at
`~/.vat/kube/config` (context `vat`) survives machine restart and a VMM crash;
vat vends a pinned kubectl at `~/.vat/bin/kubectl`. It replaced both the
kind/k3d/minikube `cluster` service (which needed a host Docker daemon) and
the one-boot Apple Container K3s session; a `cluster = "machine"` service
gets a per-run namespace on it with an isolated kubeconfig.

### Local GCP services in the machine (M5, landed)

Everything GKE-shaped that a pod sees is served by the VMM process on the host
(`crates/k8s/src/gcp/`), not by containers in the guest. The guest agent binds link-local
addresses on its loopback and relays each connection over vsock to a
`builtin:<service>` uplink target, so pods and containers reach the services at
the addresses real GKE code expects:

```
169.254.169.254:80    metadata.google.internal (metadata server, Workload Identity)
169.254.169.251:443   <region>-docker.pkg.dev  (local Artifact Registry, TLS)
169.254.169.252:8085  Pub/Sub emulator         (PUBSUB_EMULATOR_HOST)
169.254.169.252:9023  Cloud Storage emulator   (STORAGE_EMULATOR_HOST)
169.254.169.253:443   mutating admission webhook
```

- *Metadata server and Workload Identity* (`crates/k8s/src/gcp/metadata.rs`). Answers
  project, project number, zone, instance attributes (`cluster-name`),
  service accounts, access tokens (`ya29.vat.…`), and identity JWTs; it
  enforces `Metadata-Flavor: Google` and rejects `X-Forwarded-For`, as the
  real server does. The caller is identified by source IP, which the guest
  relay preserves: a pod IP is looked up in the cluster to find its
  Kubernetes service account; a KSA annotated
  `iam.gke.io/gcp-service-account: <gsa>` acts as that GSA, an unannotated
  pod acts as the workload pool principal `<project>.svc.id.goog`, and any
  other caller (a plain Docker container, the node) acts as the node's
  `<number>-compute@developer.gserviceaccount.com`. A CoreDNS stub deployed
  with K3s resolves `metadata.google.internal`. Tokens are local fakes the
  emulators accept; no IAM is evaluated.
- *Local Artifact Registry* (`crates/k8s/src/registry/` behind `crates/k8s/src/gcp/services.rs`).
  The minimal OCI registry serves `<region>-docker.pkg.dev` (region follows
  the configured zone; default `us-central1-docker.pkg.dev`) over TLS, with
  no auth, from the machine directory. Its certificate is minted by a
  persistent per-machine CA (`crates/k8s/src/gcp/ca.rs`); the guest installs the CA for
  dockerd under `/etc/docker/certs.d/<host>/ca.crt`, so `docker push` and pod
  pulls both trust it, and the same CA signs the webhook's `caBundle` so both
  stay valid across restarts.
- *Shared emulators and the webhook* (`crates/k8s/src/gcp/webhook.rs`). The built-in
  Pub/Sub and Cloud Storage emulators from `crates/k8s/src/emulator/` run inside the VMM
  with one state per machine, reachable from the guest at the link-local
  ports above and mirrored on host loopback (`127.0.0.1:18085` and
  `127.0.0.1:19023` by default), so host processes and pods share topics and
  buckets. A mutating admission webhook, registered by a K3s auto-deploy
  manifest with a namespace selector that excludes `kube-system`,
  `kube-public`, and `kube-node-lease`, injects `PUBSUB_EMULATOR_HOST` and
  `STORAGE_EMULATOR_HOST` into new pods without overriding a variable the
  container already sets; the namespace label `vat.dev/gcp-emulators=disabled`
  or the pod annotation `vat.dev/gcp-emulators: "false"` opts out. The plan
  reused transparent `*.googleapis.com` routing inside pods; the shipped shape
  injects the environment variables the stock clients already honor, which
  needs no proxy or CA in the pod.
- *Configuration* (`crates/cli/src/commands/gcp.rs`). `vat gcp config` writes project,
  zone, host ports, and enabled state into the machine's `config.json`; the
  VMM reads it at start and records the live endpoints in `gcp.json`, which
  `vat gcp status` and `vat gcp env` read.

**Later.** Ingress/GCLB behavior, a Secret Manager emulator, and multi-node
clusters.

**Fidelity.** Emulators reproduce the API behavior local tests depend on. IAM
beyond the Workload Identity annotation lookup, quotas, billing, and regional
behavior are not reproduced, and each emulator's gaps are listed in STATUS.

## How the pillars compose in one run

A `vat.toml` run can mix all three: the runner is a native macOS process
(pillar 1) with the GPU; its `[[services]]` may be native Homebrew presets,
built-in emulators (pillar 3), or Linux containers in the shared machine
reached through loopback-published ports (pillar 2); a `cluster = "machine"`
service is a per-run namespace on the persistent K3s (pillar 3 on pillar 2),
where pods get the machine's metadata server, registry, and shared emulators.
`vat state` reports the topology of the whole run in one document, and hermetic
scenarios still confine the native runner to loopback so every external call
lands on an emulator.

## Open spikes

| Spike | Question | Current lean | Decides |
|---|---|---|---|
| chroot vs seatbelt | Does the native runtime confine the rootfs with chroot or with seatbelt path rules? | Decided: seatbelt + fixed-length relocation, no chroot (see Pillar 1). | M1 |
| Hypervisor and container store | libkrun on Hypervisor.framework with containerd, or something else? | Decided by what shipped: Virtualization.framework through a codesigned `vat-vmm`, upstream dockerd in the guest (see Pillar 2). | M2 |
| Engine API subset | Which Docker API version and endpoints are the M3 contract? | Moot: the guest runs upstream dockerd, so the contract is Docker's own API; no subset is declared. | M3 |
| Shim retirement shape | Are `vat build` and `vat compose` re-pointed at the Engine or retired? | Decided: the shim is retired; `vat build` and compose image services build through the Engine; `vat compose` stays a bounded subset; only explicit `micro_vm` keeps Apple Container. | M3 |
| kubectl provenance | Does vat keep requiring an independent `kubectl` or vend one? | Decided: vat vends a pinned kubectl at `~/.vat/bin/kubectl`; `vat k8s kubectl` runs it. | M4 |
| Routing in pods | Do pods reach emulators through transparent `*.googleapis.com` routing or injected `*_EMULATOR_HOST`? | Decided: an admission webhook injects the host variables; no proxy or CA in the pod. | M5 |
| Vulkan (Venus) | Is GPU inside the machine worth pursuing now that M2 has shipped? | Deferred; not a commitment. | Later |

Each spike's `## Decision` is appended to this file when made; no decision is
implied before then. The M2 through M5 rows above record the shape that
shipped and is gated, not a separate spike write-up.

## Non-goals

- GUI or Desktop application, dashboard, tray or menu-bar surface: permanently
  out of scope.
- Linux inside the native pillar: rejected by the product owner; Linux goes to
  the shared machine.
- A hostile-code security boundary on the native runtime: macOS has no
  namespaces or cgroups; the Linux machine is the boundary when one is needed.
- One VM per container: the Apple Container model (the explicit `micro_vm`
  route) is the path being superseded, not extended.
- Resource scheduling and long-lived supervision: `cap` schedules; vat
  manages only the shared machine, and only on explicit command.
- Hosted or remote registry, proxying a real GCP project, or IAM, quota, and
  billing fidelity; the local Artifact Registry and the Workload Identity
  tokens are local-only.
- Vulkan (Venus) GPU inside the Linux machine as a commitment: deferred to a
  decision now that M2 has shipped.
