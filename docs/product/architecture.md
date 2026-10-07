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
              ├── pillar 2: ONE shared Linux VM (libkrun) ─── containerd
              │     Docker Engine API over DOCKER_HOST socket        │
              │     virtiofs, bridge + service-name DNS, Rosetta      ├── containers
              │                                                       └── K3s (pillar 3)
              │
              └── pillar 3: local GCP ─────────────────────── emulators + routing
                    Pub/Sub, Auth, Tasks, Scheduler, Workflows, GCS,
                    http-mock/OpenAPI, gcloud-wrapped Firestore/Datastore/
                    Bigtable/Spanner; metadata server + Workload Identity;
                    local Artifact Registry
```

The agent-facing core (left) is shared by every pillar and already ships. The
native runtime is the GPU path and has no VM. The Docker and GKE pillars share
exactly one Linux VM. The GCP pillar is reachable from host processes today and
from pods once M4 and M5 land.

## Shared core: the agent-facing model

A *vat* is a copy-on-write workspace plus a declarative `EnvSpec`, an
append-only event log, and a projected `VatState` document
(`src/overlay.rs`, `src/spec.rs`, `src/event.rs`, `src/state.rs`). `vat run`
clones a base, runs the workload in the selected `Sandbox` backend
(`src/sandbox/`), records the run, recomputes the diff, and cleans up by
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

Decided for M1 and implemented in `src/native/` (gate:
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

## Pillar 2: complete Docker over one shared Linux VM

**Today.** The Linux routes in the tree are bounded Apple Container paths: the
`micro_vm` service runtime and `MicroVmBackend` (`src/sandbox/microvm.rs`),
and `vat build` and `vat compose` over the `container` CLI. Apple Container
runs one VM per container and exposes no Engine API. The opt-in `docker`
CLI-subset shim is retired: Docker work goes through `vat machine start` and
its Docker Engine socket. The remaining paths stay
`Limited` in [STATUS.md](../../STATUS.md) until their replacement passes its
gate, and are then retired.

**Direction (M2, M3).** Two layers:

- *Substrate (M2).* One shared lightweight Linux VM, started on demand, built
  on libkrun over Apple Hypervisor.framework. Inside: containerd as the single
  image and container store, virtiofs for host directory sharing, an in-VM
  bridge network with a DNS resolver that answers service names, and Rosetta
  so `linux/amd64` images run on Apple Silicon. The VM is a long-lived
  substrate vat reports in `vat state` and `vat capabilities`; it is the one
  exception to "vat is not a process manager", and it is managed only on
  explicit command.
- *API (M3).* A Docker Engine API server on a unix socket. `DOCKER_HOST`
  points the unmodified upstream `docker` CLI, `docker compose` v2,
  Testcontainers, and the Docker SDKs at vat; vat translates to containerd
  inside the VM. The supported API version and endpoint subset are declared in
  STATUS as the contract; unsupported endpoints return a Docker-shaped error.

**Efficiency as the goal.** Startup time from cold, idle memory of the VM,
and file-sharing throughput through virtiofs are the three numbers this pillar
is judged on. They are measured by M2's completion evidence and recorded in
`vat state`; a number becomes a budget in STATUS only when the owner confirms
it. No efficiency figure is claimed before that.

**GPU.** Metal does not pass into a Linux guest, so a Linux container has no
Apple GPU. A Vulkan (Venus) path inside the VM is explicitly deferred and is
not a commitment; the native pillar remains the GPU path.

## Pillar 3: realistic local GCP, especially GKE

**Today.** Built-in pure-Rust emulators (`src/emulator/`): Pub/Sub (gRPC),
Firebase Auth (REST), Cloud Tasks and Cloud Scheduler (gRPC and REST on one
port, with dispatch to targets), Cloud Workflows (REST, subset interpreter
that can orchestrate sibling emulators), Cloud Storage (JSON API v1), the
`http-mock` stub and record/replay proxy with HTTPS MITM, and the `openapi`
spec-driven mock. gcloud-wrapped presets cover Firestore, Datastore, Bigtable,
and Spanner, and `firebase` wraps the Emulator Suite. Transparent routing
(`[[network.routes]]`, auto-derived for declared GCP presets) sends a runner's
calls to the real `*.googleapis.com` host, REST or gRPC, to the local emulator
with no code change.

**Direction, in priority order.**

1. *Persistent K3s in the shared VM (M4).* K3s runs inside the M2 VM and uses
   the same containerd, so an image built through the M3 Engine API is visible
   to pods immediately with no load step. The cluster, its PVCs, and its
   kubeconfig persist across VM restart and host reboot. This supersedes both
   the kind/k3d/minikube `cluster` service (which needs a Docker daemon) and
   the one-boot Apple Container K3s session (`src/commands/k8s.rs`).
2. *GCE metadata server and Workload Identity (M5).* Pods resolve
   `metadata.google.internal` to a vat-served metadata endpoint that answers
   project, zone, and service-account token requests; a Kubernetes service
   account annotated for Workload Identity maps to an emulated GCP service
   account. Stock GCP client libraries inside a pod therefore authenticate and,
   through the same transparent-routing mechanism, reach the vat emulators
   with no endpoint configuration.
3. *Local Artifact Registry (M5).* A registry at a GCP-shaped host name that
   accepts `docker push` from the Engine API and serves pod image pulls.
4. *Later.* Ingress/GCLB behavior, a Secret Manager emulator, and multi-node
   clusters.

**Fidelity.** Emulators reproduce the API behavior local tests depend on. IAM
beyond the Workload Identity binding, quotas, billing, and regional behavior
are not reproduced, and each emulator's gaps are listed in STATUS.

## How the pillars compose in one run

A `vat.toml` run can mix all three: the runner is a native macOS process
(pillar 1) with the GPU; its `[[services]]` may be native Homebrew presets,
built-in emulators (pillar 3), or Linux containers in the shared VM reached
through loopback-published ports (pillar 2); a `cluster`-kind service can be
the persistent K3s (pillar 3 on pillar 2). `vat state` reports the topology
of the whole run in one document, and hermetic scenarios still confine the
native runner to loopback so every external call lands on an emulator.

## Open spikes

| Spike | Question | Current lean | Decides |
|---|---|---|---|
| chroot vs seatbelt | Does the native runtime confine the rootfs with chroot or with seatbelt path rules? | Decided: seatbelt + fixed-length relocation, no chroot (see Pillar 1). | M1 |
| Engine API subset | Which Docker API version and endpoints are the M3 contract? | The set the upstream CLI, Compose v2, and Testcontainers need for build/run/exec/logs/network/volume. | M3 |
| Shim retirement shape | Are `vat build` and `vat compose` re-pointed at the Engine API or retired? | The shim itself is retired; `vat build` and `vat compose` are unchanged. Re-pointing or retiring them is still an owner call. | M3 |
| kubectl provenance | Does vat keep requiring an independent `kubectl` or vend one? | Keep requiring, unless the owner decides otherwise. | M4 |
| Vulkan (Venus) | Is GPU inside the VM worth pursuing after M2? | Deferred; not a commitment. | Later |

Each spike's `## Decision` is appended to this file when made; no decision is
implied before then.

## Non-goals

- GUI or Desktop application, dashboard, tray or menu-bar surface: permanently
  out of scope.
- Linux inside the native pillar: rejected by the product owner; Linux goes to
  the shared VM.
- A hostile-code security boundary on the native runtime: macOS has no
  namespaces or cgroups; the Linux VM is the boundary when one is needed.
- One VM per container: the Apple Container model is the path being
  superseded, not extended.
- Resource scheduling and long-lived supervision: `cap` schedules; vat
  manages only the shared VM, and only on explicit command.
- Hosted or remote registry, proxying a real GCP project, or IAM, quota, and
  billing fidelity.
- Vulkan (Venus) GPU inside the Linux VM as a commitment: deferred to a
  decision after M2.
