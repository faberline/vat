# vat roadmap

## Purpose

This roadmap turns the product owner's direction of 2026-10-07 into ordered
outcomes. vat has three pillars: lightweight Apple-native containers (pure
Apple ecosystem, no Linux, no VM), complete Docker over one shared lightweight
Linux VM with efficiency as the goal, and realistic local GCP with GKE first.
Agent-first operation, CLI plus structured JSON output, permanently no GUI, and
copy-on-write fork/snapshot with `vat state` stay core and are not outcomes
here because they are already shipped.

The milestone order below (M1 through M5) is the **order the product owner
confirmed on 2026-10-07**; the later outcomes after it are uncommitted. The
ordering rationale is lowest risk first (M1 is the closest to the shipped
host-process sandbox), then the substrate every Linux outcome depends on (M2),
then the two user-visible payoffs on that substrate (M3, M4), then GKE realism
on top of M4 (M5). An outcome is complete only when the
completion evidence named in it runs as a gate; until then the matching
[STATUS.md](STATUS.md) row stays `Not supported` or `Limited`.

## Near-term outcomes

### M1 — Darwin native runtime and OCI darwin image format

- ID: VAT-R-M1-NATIVE-RUNTIME
- Outcome: A `vat.toml` service or direct run can name an OCI image whose
  manifest carries `darwin/arm64` layers (for example a Python environment or a
  set of Homebrew bottles); vat pulls it, materializes the layers into an APFS
  `clonefile` copy-on-write rootfs, and runs the workload as a macOS process
  under a dedicated per-container UID with seatbelt confinement and
  process-group lifecycle, with Metal/MPS/MLX/`tensorflow-metal` reaching the
  native GPU. `vat state` reports the image reference, digest, and UID.
- Boundary: No Linux layers, no VM, and no claim of a hostile-code security
  boundary (macOS has no namespaces or cgroups). The chroot-versus-seatbelt
  path-confinement decision is an open spike: chroot needs root and has dyld
  shared-cache issues, so the current lean is seatbelt-only; the spike's
  decision is recorded in [docs/product/architecture.md](docs/product/architecture.md)
  before implementation starts. The darwin image format vat consumes must be
  producible by a documented build step; publishing images is not part of this
  outcome.
- Completion evidence: A `cargo test -p vat` target pulls a fixture
  `darwin/arm64` image from a local OCI layout, runs a command that writes into
  the rootfs under the dedicated UID, observes the write in `vat diff`, proves
  the seatbelt profile denies a write outside the rootfs, and proves
  `vat gpu`/`state.gpu` still report `accessible` inside the container.
  [STATUS.md](STATUS.md) rows `VAT-S-OCI-DARWIN` and `VAT-S-NATIVE-UID` move to
  `Supported`.
- Tracking: Not assigned.

### M2 — Shared lightweight Linux VM with containerd, virtiofs, and networking

- ID: VAT-R-M2-SHARED-VM
- Outcome: vat starts, on demand, exactly one shared lightweight Linux VM on
  Apple Hypervisor.framework via libkrun, running containerd, with virtiofs
  sharing of declared host paths, an in-VM bridge network with service-name
  DNS, and Rosetta so `linux/amd64` images run. The VM is reported in `vat
  state`/`vat capabilities --json` (running, idle, resource use) and can be
  stopped and restarted without losing images or volumes.
- Boundary: One VM per host, never one per container. No Apple GPU inside the
  VM; Vulkan (Venus) is deferred and not a commitment. Startup time, idle
  memory, and file-sharing throughput are goals to be measured by this outcome's
  evidence, not promises; the measured values become `Limits` in
  [STATUS.md](STATUS.md) only once the owner confirms them as budgets. This
  outcome does not expose a Docker API or Kubernetes; it is the substrate.
- Completion evidence: A `cargo test -p vat` target (opt-in real-host E2E,
  gated by an `*_E2E_REQUIRED=1` variable) boots the VM from cold, runs a
  `linux/arm64` and a `linux/amd64` container through containerd, resolves one
  container from another by service name, reads and writes a virtiofs-shared
  directory, stops and restarts the VM, and proves the image survived. The same
  target records cold-start seconds, idle RSS, and a file-sharing throughput
  sample into `vat state` as measurements. [STATUS.md](STATUS.md) row
  `VAT-S-SHARED-VM` moves to `Supported`.
- Tracking: Not assigned.

### M3 — Docker Engine API and retirement of the CLI-subset shim

- ID: VAT-R-M3-DOCKER-ENGINE-API
- Outcome: vat serves the Docker Engine API over a unix socket; with
  `DOCKER_HOST` pointed at it, the unmodified upstream `docker` CLI, `docker
  compose` v2, Testcontainers, and the Docker SDKs build, run, network, and
  tear down containers in the M2 shared VM. The `docker` CLI-subset shim over
  Apple Container (`vat docker install-shim`, three fixed Compose profiles) is
  retired once the Engine API passes its gate, with a documented migration note
  for agents that used the shim's VAT-JSON receipts.
- Boundary: The API version and endpoint subset supported are declared in
  [STATUS.md](STATUS.md) as the contract; endpoints outside it return a Docker
  error, never a silent no-op. Efficiency claims remain measurements from M2.
  Docker Desktop features that are not Engine API (extensions, Desktop
  Kubernetes, GUI) are out of scope. Retiring the shim removes `vat docker
  install-shim` and the shim-only `vat.docker.*`/`vat.docker-compose.*` JSON
  schemas; `vat build` and `vat compose` are re-pointed at the Engine API or
  retired in the same change, which the owner decides.
- Completion evidence: A `cargo test -p vat` target (opt-in real-host E2E)
  runs the upstream `docker` CLI against the socket for `build`, `run`, `ps`,
  `logs`, `exec`, `volume`, and `network`; runs a `docker compose up` with two
  services resolving each other by name and a `depends_on`; and runs one
  Testcontainers-based test against the socket. A unit test proves the shim
  entry point is gone. [STATUS.md](STATUS.md) row `VAT-S-DOCKER-ENGINE` moves
  to `Supported` and `VAT-S-DOCKER-SHIM` is removed.
- Tracking: Not assigned.

### M4 — Persistent K3s in the shared VM with shared images and PVCs

- ID: VAT-R-M4-PERSISTENT-K3S
- Outcome: A K3s control plane runs inside the M2 shared VM, using the same
  containerd so an image produced by `docker build` through M3 is usable by a
  pod with `imagePullPolicy: IfNotPresent` and no load step. The cluster, its
  PersistentVolumeClaims, and its kubeconfig survive VM restart and host
  reboot; `vat k8s` exposes the kubeconfig and status through structured JSON.
  The kind/k3d/minikube `cluster` service and the one-boot Apple Container K3s
  session are superseded and retired once this outcome passes its gate.
- Boundary: Single node. No Ingress/GCLB emulation, no Secret Manager, no
  multi-node in this outcome. The independent-`kubectl` requirement carries
  over unless the owner decides vat should vend its own. Retirement of
  `vat cluster` and `vat k8s ephemeral|session` is part of this outcome's
  definition of done, so their [STATUS.md](STATUS.md) rows are removed, not
  left `Limited`.
- Completion evidence: A `cargo test -p vat` target (opt-in real-host E2E)
  builds an image through the Engine API, deploys a pod using it with no load
  step, writes to a PVC, restarts the VM, and proves the pod and PVC data are
  back; a second run after a simulated host reboot (VM stopped and state
  reloaded from disk) proves the same. [STATUS.md](STATUS.md) row
  `VAT-S-K3S-PERSISTENT` moves to `Supported`; `VAT-S-CLUSTER` and
  `VAT-S-K8S-SESSION` are removed.
- Tracking: Not assigned.

### M5 — GKE realism: metadata server, Workload Identity, local Artifact Registry

- ID: VAT-R-M5-GKE-REALISM
- Outcome: Pods in the M4 cluster see a GCE metadata server
  (`metadata.google.internal`) that answers project, zone, and service-account
  token requests; Workload Identity mapping from a Kubernetes service account
  to an emulated GCP service account is honored, so a stock GCP client library
  inside a pod authenticates and resolves `*.googleapis.com` to the vat
  emulators automatically, with no code change. A local Artifact Registry
  serves `docker push`/`pull` and pod image pulls at a GCP-shaped host name.
- Boundary: Tokens are local emulator credentials with no value outside the
  host; IAM policy evaluation is limited to the Workload Identity binding
  itself. Routing inside pods reuses the pillar-3 transparent-routing
  mechanism; services without a vat emulator are not emulated by this outcome.
  Ingress/GCLB, Secret Manager, and multi-node remain later outcomes.
- Completion evidence: A `cargo test -p vat` target (opt-in real-host E2E)
  deploys a pod with a stock Pub/Sub client and a Cloud Storage client, bound
  via Workload Identity, that publishes to the vat Pub/Sub emulator and writes
  to the vat Cloud Storage emulator with no explicit endpoint configuration;
  and pushes an image to the local Artifact Registry then runs a pod from it.
  [STATUS.md](STATUS.md) row `VAT-S-GKE-REALISM` moves to `Supported`.
- Tracking: Not assigned.

## Later outcomes

Everything in this horizon is uncommitted: the owner confirmed M1 through M5
only. These entries record intent and boundaries so they are not mistaken for
near-term promises.

### Ingress and GCLB behavior in the local cluster

- ID: VAT-R-L1-INGRESS-GCLB
- Outcome: Ingress resources in the M4 cluster behave like a GKE external
  HTTP(S) load balancer for the cases local tests depend on (path and host
  routing, health checks, a stable loopback endpoint reported in `vat state`).
- Boundary: Behavioral fidelity, not GCLB API or billing fidelity; no public
  listener.
- Completion evidence: A real-host E2E applies an Ingress with two backends
  and proves routing and health-check gating through the reported endpoint.
- Tracking: Not assigned.

### Secret Manager emulator

- ID: VAT-R-L2-SECRET-MANAGER
- Outcome: A built-in Rust Secret Manager emulator (REST and gRPC) joins the
  pillar-3 set with its own `*_EMULATOR_HOST` export and transparent route, and
  is reachable from pods through M5 Workload Identity.
- Boundary: Common client operations (create, add version, access, list,
  destroy); no IAM beyond the Workload Identity binding; no CMEK or
  replication policy fidelity.
- Completion evidence: A new `cargo test -p vat` integration target, added
  with the emulator and declared in `Cargo.toml` with
  `required-features = ["emulator"]` like its siblings, exercises the common
  operations from a stock client over both protocols and through a routed
  `secretmanager.googleapis.com` host.
- Tracking: Not assigned.

### Multi-node local cluster

- ID: VAT-R-L3-MULTI-NODE
- Outcome: The M4 cluster can run with more than one node inside the shared
  VM so scheduling, affinity, and node-failure tests are possible locally.
- Boundary: Nodes are K3s agents in the same VM, not separate VMs; no node-level
  resource isolation claim.
- Completion evidence: A real-host E2E creates a two-node cluster, schedules
  pods with node affinity, drains one node, and proves rescheduling.
- Tracking: Not assigned.

### Decision on Vulkan (Venus) GPU inside the Linux VM

- ID: VAT-R-L4-VULKAN-DECISION
- Outcome: A written decision, after M2 ships, on whether a Vulkan (Venus)
  GPU path inside the shared VM is worth pursuing, with measurements of what
  it would and would not give Linux containers.
- Boundary: This is explicitly deferred and is not a commitment to ship GPU
  access inside the VM. The native pillar remains the GPU path.
- Completion evidence: A `type:spike` decision recorded in
  [docs/product/architecture.md](docs/product/architecture.md) with the
  measurements that informed it.
- Tracking: Not assigned.

## Non-goals

### GUI or Desktop application

- ID: VAT-N-GUI
- Reason: vat is agent-first and permanently headless; every surface is the
  CLI and structured JSON. Dashboards, tray or menu-bar UI, and a Desktop
  lifecycle are out of scope forever.

### Linux inside the native pillar

- ID: VAT-N-NATIVE-LINUX
- Reason: The product owner rejected a Linux runtime in pillar 1. The native
  pillar is pure Apple ecosystem so the GPU and the host toolchain stay native;
  every Linux need goes to the shared VM of pillar 2.

### Hostile-code security boundary on the native runtime

- ID: VAT-N-NATIVE-SECURITY-BOUNDARY
- Reason: macOS has no namespaces or cgroups; seatbelt, a dedicated UID, and a
  copy-on-write rootfs isolate resources for cooperative workloads but cannot
  be promised as a boundary against malicious code. The Linux VM is the
  boundary when one is needed.

### One VM per container

- ID: VAT-N-VM-PER-CONTAINER
- Reason: Efficiency is the pillar-2 goal. The Docker and GKE pillars share
  exactly one lightweight VM; per-container VMs (the Apple Container model)
  are the path being superseded, not extended.

### Resource scheduling and supervision

- ID: VAT-N-SCHEDULER
- Reason: Admission, throttling, pausing, and kill policy belong to `cap`;
  long-lived supervision (restart, health monitoring) is not vat's job. The
  shared VM is the only long-lived substrate vat manages, and only on explicit
  command.

### Hosted or remote registry and cloud services

- ID: VAT-N-REMOTE-SERVICES
- Reason: vat emulates GCP locally for tests; it does not host a remote
  registry, proxy a real GCP project, or reproduce IAM, quotas, or billing.
