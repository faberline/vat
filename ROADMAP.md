# vat roadmap

## Purpose

This roadmap turns the product owner's direction of 2026-10-07 into ordered
outcomes. vat has three pillars: lightweight Apple-native containers (pure
Apple ecosystem, no Linux, no VM), complete Docker over one shared lightweight
Linux machine with efficiency as the goal, and realistic local GCP with GKE
first. Agent-first operation, CLI plus structured JSON output, permanently no
GUI, and copy-on-write fork/snapshot with `vat state` stay core and are not
outcomes here because they are already shipped.

The milestones M1 through M5 are the **order the product owner confirmed on
2026-10-07**, and all five have landed on this tree; each entry below keeps
its original outcome and boundary and records the evidence that closed it, so
the shipped shape can be checked against what was promised. Where the shipped
substrate differs from the plan (Virtualization.framework and dockerd instead
of libkrun and containerd), the entry says so. The later outcomes after them
are uncommitted. An outcome is complete only when the completion evidence named
in it runs as a gate; the matching [STATUS.md](STATUS.md) rows are now
`Supported`.

## Near-term outcomes

### M1 — Darwin native runtime and OCI darwin image format (done)

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
  decision was taken as seatbelt plus fixed-length relocation, no chroot, and
  is recorded in [docs/product/architecture.md](docs/product/architecture.md).
  The dedicated UID requires a root-created user pool and vat running as root,
  so it stays `Limited` in STATUS. Publishing images is not part of this
  outcome.
- Completion evidence: Landed. `cargo test -p vat --test vat_native_runtime`
  builds, exports, imports, and runs a relocated `darwin/arm64` image with a
  venv, observes in-root writes in `vat diff`, proves the seatbelt denial
  outside the root, proves host-equal GPU, exercises the detached lifecycle
  and exit codes, and pushes and pulls against an in-test registry.
  [STATUS.md](STATUS.md) `VAT-S-OCI-DARWIN` is `Supported`;
  `VAT-S-NATIVE-UID` is `Limited` (not verified without root).
- Tracking: Not assigned.

### M2 — Shared lightweight Linux machine with dockerd, virtiofs, and networking (done)

- ID: VAT-R-M2-SHARED-VM
- Outcome: vat starts, on demand, exactly one shared lightweight Linux machine
  per host (`vat machine start|stop|status|exec|env|logs|rm`): an Alpine guest
  on Apple's Virtualization.framework booted by a codesigned helper
  (`vat-vmm`), with a persistent ext4 data disk, virtiofs host shares at the
  same absolute paths, Rosetta so `linux/amd64` images run, a vsock guest
  agent, and published ports. The machine is reported by `vat machine status
  --json` (state, guest health, memory, VMM RSS, disk, ports) and can be
  stopped and restarted without losing images or volumes.
- Boundary: One machine per host, never one per container. No Apple GPU inside
  the machine; Vulkan (Venus) is deferred and not a commitment. Startup time,
  idle memory, and file-sharing throughput are recorded by this outcome's
  evidence as measurements, not promises; a measured value becomes a `Limits`
  cell in [STATUS.md](STATUS.md) only once the owner confirms it as a budget.
  The plan named libkrun on Hypervisor.framework running containerd; what
  shipped is Virtualization.framework running upstream dockerd, because Docker
  compatibility then comes from Docker itself rather than a translation layer.
- Completion evidence: Landed. `VAT_MACHINE_E2E_REQUIRED=1 cargo test -p vat
  --test vat_machine_e2e -- --ignored --nocapture --test-threads=1` boots the
  machine from cold, runs `linux/arm64` and `linux/amd64` containers, resolves
  containers by name on a user network, reads and writes a virtiofs bind mount
  both ways, reaches a published port, stops cleanly, restarts, and proves the
  image and a volume survived; it records cold and warm start timings, idle
  guest memory, VMM RSS, and disk allocation to `vat-machine-e2e.json`.
  [STATUS.md](STATUS.md) `VAT-S-SHARED-VM` is `Supported`.
- Tracking: Not assigned.

### M3 — Docker Engine socket and retirement of the docker shim (done)

- ID: VAT-R-M3-DOCKER-ENGINE-API
- Outcome: The machine's dockerd is forwarded to `~/.vat/run/docker.sock`;
  with `DOCKER_HOST` pointed at it, the unmodified upstream `docker` CLI,
  `docker compose` v2, Testcontainers, and the Docker SDKs build, run,
  network, and tear down containers in the M2 machine. vat's own Docker users
  (`vat build`, `vat run` and compose image services, probes) default to the
  same socket; an explicit `DOCKER_HOST`/`DOCKER_CONTEXT` wins and
  `VAT_ENGINE=external` opts out. The argv0 `docker` shim over Apple
  Container is retired.
- Boundary: The API is upstream dockerd's own, not a declared subset, so no
  endpoint list is maintained in STATUS; what the pinned Alpine guest's dockerd
  supports is what works. Efficiency claims remain measurements from M2.
  Docker Desktop features that are not Engine API (extensions, Desktop
  Kubernetes, GUI) are out of scope. `vat build` and compose image services
  were re-pointed at the Engine (the owner's call); `vat compose` stays a
  bounded subset and the explicit `micro_vm` service route stays the one
  Apple Container path.
- Completion evidence: Landed. The M2 E2E runs the upstream `docker` CLI
  against the socket for `run` (exit codes, stdin half-close), BuildKit
  `build`, `compose up` with two services resolving each other by name and a
  `depends_on`, `network`, and `volume`; `cargo test -p vat --test vat_build`
  proves `vat build` lands its tag in the Engine's image store (Docker-gated).
  The shim entry point and its schemas are gone from the tree.
  [STATUS.md](STATUS.md) `VAT-S-DOCKER-ENGINE` is `Supported` and
  `VAT-S-DOCKER-SHIM` is removed. The same E2E's
  `machine_docker_engine_serves_testcontainers` starts `redis:7-alpine`
  through the Rust `testcontainers` crate (bollard, Engine API) with an
  ephemeral published port, waits on its log line, and gets `+PONG` from the
  host on the first connect, so a published port is open by the time the
  container reports ready.
- Tracking: Not assigned.

### M4 — Persistent K3s in the shared machine with shared images and PVCs (done)

- ID: VAT-R-M4-PERSISTENT-K3S
- Outcome: A K3s control plane (`v1.36.5+k3s1`) runs inside the M2 machine,
  started with `--docker` so pods run on the same dockerd and an image
  produced by `docker build` through M3 is usable by a pod with no load or
  push step. The cluster, its PersistentVolumeClaims, and its kubeconfig
  (`~/.vat/kube/config`, context `vat`) survive machine restart and a VMM
  crash; `vat k8s up|status|kubeconfig|kubectl|down` exposes them through
  structured JSON and vends a pinned kubectl. The kind/k3d/minikube `cluster`
  service and the one-boot Apple Container K3s session are retired;
  `cluster = "machine"` services run on this cluster in a per-run namespace.
- Boundary: Single node. No Ingress/GCLB emulation, no Secret Manager, no
  multi-node in this outcome. The retired commands' [STATUS.md](STATUS.md)
  rows are removed, not left `Limited`.
- Completion evidence: Landed. `VAT_K8S_E2E_REQUIRED=1 cargo test -p vat
  --test vat_k8s_e2e -- --ignored --nocapture --test-threads=1` builds an
  image through the Engine, runs a pod from it with no push, writes a PVC,
  restarts the machine cleanly and proves the pod and PVC data are back, then
  kills the VMM and proves recovery; observed on one host, cold about 15 s,
  restart about 7 s, crash recovery about 15 s. The same E2E then runs a
  `vat.toml` with a `cluster = "machine"` service: the runner's kubectl lands
  a ConfigMap in `VAT_K8S_NAMESPACE`, the run exits 0, and the namespace is
  gone afterwards. Plan, doctor, and validation coverage stays in
  `cargo test -p vat --test vat_toml_runner`.
  [STATUS.md](STATUS.md) `VAT-S-K3S-PERSISTENT` is `Supported`;
  `VAT-S-CLUSTER` and `VAT-S-K8S-SESSION` are removed.
- Tracking: Not assigned.

### M5 — GKE realism: metadata server, Workload Identity, local Artifact Registry (done)

- ID: VAT-R-M5-GKE-REALISM
- Outcome: Pods in the M4 cluster see a GCE/GKE metadata server at
  `metadata.google.internal` (`169.254.169.254`, served by the VMM over a
  guest link-local uplink, resolved through a CoreDNS stub) that answers
  project, zone, instance attributes, service accounts, access tokens, and
  identity JWTs; Workload Identity is resolved by caller pod IP, so a KSA
  annotated `iam.gke.io/gcp-service-account` acts as that GSA and a stock GCP
  client library inside a pod authenticates with no code change. Shared
  Pub/Sub and Cloud Storage emulators are injected into pods by a mutating
  admission webhook (`PUBSUB_EMULATOR_HOST`, `STORAGE_EMULATOR_HOST`) and
  mirrored on host loopback. A local Artifact Registry at
  `<region>-docker.pkg.dev` serves `docker push` and pod pulls over TLS from a
  per-machine CA. `vat gcp status|env|config` reports and configures it.
- Boundary: Tokens are local fakes (`ya29.vat.…`) with no value outside the
  host; there is no IAM evaluation, and the Workload Identity binding is an
  annotation lookup. Instead of routing `*.googleapis.com` inside pods, the
  shipped shape injects the two `*_EMULATOR_HOST` variables the stock clients
  honor; only Pub/Sub and Cloud Storage are shared machine-wide, and the
  other emulators stay per-run host presets. Ingress/GCLB, Secret Manager,
  and multi-node remain later outcomes.
- Completion evidence: Landed. `VAT_GCP_E2E_REQUIRED=1 cargo test -p vat
  --test vat_gcp_e2e -- --ignored --nocapture --test-threads=1` pushes an
  image to the local registry, runs a pod from it with the official
  google-auth, google-cloud-pubsub, and google-cloud-storage libraries bound
  through Workload Identity, and proves the WI email, token prefix, identity
  JWT, publish/pull, upload/download, and that the host reads the pod's object
  through the mirrored emulator; observed on one host, cold k8s ready 16.0 s,
  build 20.2 s, push 2.5 s, pull-to-probe-done 2.9 s.
  [STATUS.md](STATUS.md) `VAT-S-GKE-REALISM` and `VAT-S-GCP-CLI` are
  `Supported`.
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
  machine so scheduling, affinity, and node-failure tests are possible locally.
- Boundary: Nodes are K3s agents in the same machine, not separate VMs; no
  node-level resource isolation claim.
- Completion evidence: A real-host E2E creates a two-node cluster, schedules
  pods with node affinity, drains one node, and proves rescheduling.
- Tracking: Not assigned.

### Decision on Vulkan (Venus) GPU inside the Linux machine

- ID: VAT-R-L4-VULKAN-DECISION
- Outcome: A written decision, now that M2 has shipped on
  Virtualization.framework, on whether a Vulkan (Venus) GPU path inside the
  shared machine is worth pursuing, with measurements of what it would and
  would not give Linux containers.
- Boundary: This is explicitly deferred and is not a commitment to ship GPU
  access inside the machine. The native pillar remains the GPU path.
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
  exactly one lightweight machine; per-container VMs (the Apple Container
  model behind the explicit `micro_vm` service route) are the path being
  superseded, not extended.

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
