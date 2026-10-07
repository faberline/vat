# vat — agent-native local runtime for macOS: native containers, Docker, and local GCP

## Brief

`vat` is a headless local runtime for the one operator Docker was never
designed for: a **coding/ML agent**. GUI and Desktop surfaces are permanently
out of scope; agents use the CLI and structured JSON output. An agent writes
`vat.toml`; vat prepares an ephemeral copy-on-write workspace, starts
run-scoped services, waits for readiness, runs the named runner, captures
logs/artifacts/diff/state, and cleans up according to the run policy. One
[`vat state`](#vat-state) JSON document, git-like copy-on-write
[fork/snapshot](#the-model), and forwarded exit codes are the **unflagged**
path and stay core across everything below.

vat is built on three pillars. What each pillar can do **today** is the support
matrix in [STATUS.md](STATUS.md); what it commits to **next** is
[ROADMAP.md](ROADMAP.md); how the pieces fit is
[docs/product/architecture.md](docs/product/architecture.md).

1. **Lightweight Apple-native containers — pure Apple ecosystem.** A
   macOS-native container runtime: workloads are macOS processes over an APFS
   `clonefile` copy-on-write rootfs, confined by seatbelt, with a dedicated UID
   per container and process-group lifecycle, fed by OCI images carrying
   `darwin/arm64` layers (Python environments, Homebrew bottles, and the like).
   Because the workload never leaves macOS, the **Apple GPU just works**:
   Metal, PyTorch MPS, MLX, and `tensorflow-metal` see the native device, with
   no VM in the path and nothing to bridge. There is **no Linux in this pillar**.
   Honesty clause: macOS has no namespaces or cgroups, so this isolation is
   weaker than a VM and is **not** a security boundary for hostile code. Today
   this pillar ships two parts. The first is the sandboxed host-process runtime
   (`vat run`, `--isolation none|seatbelt`). The second is the native container
   runtime: `vat image` builds, imports, exports, pushes, and pulls OCI
   `darwin/arm64` images, and `vat container` runs them in seatbelt-confined
   copy-on-write roots, with no chroot and no VM. The dedicated UID works only
   when vat runs as root over a root-created user pool; otherwise every
   container reports `uid_isolation: "unavailable"`.

2. **Complete Docker, with efficiency as the goal.** The direction is a Docker
   Engine API served over a unix socket (`DOCKER_HOST`) so the real `docker`
   CLI, `docker compose` v2, Testcontainers, and the Docker SDKs work
   unmodified, backed by **one shared lightweight Linux VM** (libkrun on Apple
   Hypervisor.framework) running containerd, with virtiofs file sharing, an
   in-VM bridge network with service-name DNS, and Rosetta for `amd64` images.
   Every Linux need goes here. Startup time, idle memory, and file-sharing
   throughput are **goals to be measured**, not achieved claims. Today
   `vat machine start` boots that VM and serves its Docker Engine at
   `~/.vat/run/docker.sock`; vat points its own child processes there via
   `DOCKER_HOST`, and you export `DOCKER_HOST=unix://$HOME/.vat/run/docker.sock`
   yourself to use the stock `docker` CLI.

3. **Realistic local GCP, especially GKE.** The built-in emulators (Pub/Sub,
   Firebase Auth, Cloud Tasks, Cloud Scheduler, Workflows, Cloud Storage,
   http-mock/OpenAPI) and the gcloud-wrapped family (Firestore, Datastore,
   Bigtable, Spanner), plus transparent REST and gRPC routing of real
   `*.googleapis.com` hosts to those emulators, are shipped and stay. The
   direction for GKE, in priority order: persistent K3s inside the pillar-2
   shared VM sharing its containerd so a `docker build` image is usable by a
   pod with no load step, with PVCs and survival across sessions; a GCE
   metadata server plus Workload Identity emulation so GCP clients inside pods
   resolve to vat emulators automatically; a local Artifact Registry; and later
   Ingress/GCLB behavior, Secret Manager, and multi-node. The existing
   kind/k3d/minikube wrapping and the one-boot Apple Container K3s session are
   legacy paths scheduled to be superseded.

The operating surface faces the agent, not a human developer. Docker's
ergonomics (a daemon, a desktop app, `ps`/`inspect`/`logs`/`diff` as separate
human-readable text dumps) are tradeoffs *for developers*. vat's tradeoffs are
*for agents*: one structured `vat state` JSON that answers "what is this
environment right now", `--json` on every inventory verb, forwarded exit codes,
copy-on-write disposability, and fork/snapshot.

## Capabilities

A promise with no gate under it is not claimed. The three pillars in the Brief
are a direction; the capabilities below are what the current tree ships and
gates. Anything a pillar promises beyond these rows is a
[ROADMAP.md](ROADMAP.md) outcome, not a capability, and its current state is
recorded in [STATUS.md](STATUS.md).

Nothing reads the tables below. The capability gate that validated their
shape was deleted with the `aw` binary, so the shape is convention now and
the commands named in each row are the only part that runs.

### Capability Index

| Capability | Root WI | Notes |
|---|---:|---|
| Agent-Native State and Copy-on-Write Lifecycle | #4152 | The core every pillar shares: `vat.toml` run protocol, one structured `vat state`/`vat diff` document, copy-on-write fork/snapshot over APFS `clonefile`, interrupt-safe cleanup, and production-like scenarios. |
| Native macOS Runtime (pillar 1, shipped part) | - | Sandboxed host-process execution with host GPU visibility, opt-in seatbelt isolation, and the fail-closed egress policy. Also native containers: OCI `darwin/arm64` images (`vat image`), seatbelt-confined copy-on-write roots with fixed-length path relocation and a process-group lifecycle (`vat container`), and `image` services with `runtime = "native"`. No VM, no Linux, no chroot. The dedicated UID requires root and is not verified by the gate. |
| Local GCP Emulation and Transparent Routing (pillar 3, shipped part) | - | Built-in Rust emulators (REST + gRPC), gcloud-wrapped emulator presets, the http-mock/OpenAPI proxy, and transparent HTTP/gRPC routing of real GCP hosts to local emulators. GKE realism (persistent K3s, metadata server, Workload Identity, Artifact Registry) is roadmap. |
| Container and Kubernetes Paths Scheduled for Supersession | - | Shipped, bounded, still gated: `vat build`/`vat compose`, the MicroVM service backend, Docker-backed `vat cluster` (kind/k3d/minikube), and the one-boot Apple Container K3s session. ROADMAP outcomes for pillar 2 and GKE replace them; nothing here is removed before its replacement passes its gate. |
| Developer & Agent Experience | #1819 | Offline command contracts, task-scoped onboarding, and host preflight evidence for local agents. |

### Agent-Native State and Copy-on-Write Lifecycle

vat runs a workload over a copy-on-write workspace and projects everything an
agent needs to know into one structured document. This is the differentiator
that every pillar builds on: `vat run` clones a base, runs the runner, records
the run, recomputes the filesystem diff, and cleans up by policy; `vat fork`
and `vat snapshot` branch a running environment like git.

- Root WI: #4152
- Surfaces: CLI: `vat run` + `vat state/diff/ls/logs` + `vat fork/snapshot/gc/rm` -
  Agent-facing copy-on-write run with structured state/diff, fork/snapshot, and
  interrupt-safe cleanup.
- Gate — behavior: `cargo test -p vat` - vat.toml run protocol, scenario
  topology, interrupt cleanup, and the state/diff projection.
- Gate: `cargo test -p vat`
- Gate:
  `rg -n -e 'vat state' -e 'vat diff' -e '--json' -e structured README.md`
- Gate:
  `rg -n -e copy-on-write -e fork -e snapshot -e clonefile -e APFS README.md`

| Work Root | Kind | WI | Gate / Evidence |
|---|---|---:|---|
| Agent-legible state and diff surface | epic | - | `rg -n -e 'vat state' -e 'vat diff' -e '--json' -e structured README.md` |
| Copy-on-write fork and snapshot lifecycle | epic | - | `rg -n -e copy-on-write -e fork -e snapshot -e clonefile -e APFS README.md` |
| Local agent test runner protocol | epic | #4152 | `cargo test -p vat --test behavior_vat_toml_runner_local_service_smoke --test vat_toml_runner -- --nocapture` |
| Interrupt-safe owned process cleanup | change | #2394 | `cargo test -p vat --test vat_signal_cleanup -- --test-threads=1` proves real SIGINT/SIGTERM cleanup for configured and direct runs. |
| Production-like integration scenarios | change | #701 | `cargo test -p vat --test vat_toml_runner --test behavior_scenario_failure_keeps_topology_and_logs --test behavior_scenario_hermetic_requires_http_mock_service --test behavior_scenario_run_starts_app_dependency_and_runner -- --nocapture` |

### Native macOS Runtime (pillar 1, shipped part)

The workload is a macOS process over the copy-on-write rootfs, so the Apple GPU
(Metal, MPS, MLX, `tensorflow-metal`) is simply present. Isolation is a
pluggable [`Sandbox`](src/sandbox/mod.rs) backend: `none` is a plain host
process; `seatbelt` wraps it in a `sandbox-exec` profile that confines writes to
the rootfs and enforces the `[network].egress` policy, failing closed when the
selected backend cannot enforce it. This is resource isolation for cooperative
workloads, not a security boundary for hostile code: macOS has no namespaces or
cgroups.

Native containers add an image and container lifecycle on the same
foundations, without a VM:

- *Images.* `vat image build -t NAME[:TAG] [-f Vatfile] CONTEXT` builds an
  OCI image (`darwin/arm64`, gzip tar layers) from a Dockerfile-like `Vatfile`
  (`FROM scratch|<image>`, `WORKDIR`, `ENV`, `LABEL`, `EXPOSE`, `COPY`, `RUN`,
  `CMD`, `ENTRYPOINT`). Each `RUN` step executes on the host under seatbelt,
  with `$VAT_ROOT` pointing at the build root. Images live in a local store
  at `~/.vat/native` (`VAT_HOME` respected). They move between hosts as OCI
  image layouts (`vat image export|import --oci-layout DIR`) or through any
  OCI Distribution registry (`vat image push|pull`, with Basic or Bearer auth
  from the Docker config `auths`; plain HTTP for loopback and
  `VAT_INSECURE_REGISTRIES`).
- *Containers.* `vat container run` clones the image's unpacked base with
  APFS `clonefile` into a root whose absolute path is always 128 bytes long.
  Build-time root paths baked into files (venvs, shebangs, Mach-O strings)
  were replaced at build time by a same-length placeholder recorded in
  `vat.relocations`; they are rewritten to the container root, and rewritten
  Mach-O files are re-signed ad hoc. There is no chroot: the workload sees the
  host `/`, gets `VAT_ROOT` set, and gets a root-relative `PATH` with the host
  system dirs appended. Seatbelt confines writes to the root, read-write `-v`
  mounts, and the per-user cache dir. The workload has its own process group:
  a foreground exit code is forwarded, and detached containers (`-d`) are
  observed with `ps`, `logs`, `exec`, `inspect`, and `diff`, then stopped
  with TERM followed by KILL. `vat state ctr-…` and `vat diff ctr-…` read the
  same records.
- *Identity.* `vat native users setup --count N` (as root, or `--print` for
  the `dscl` commands) creates a hidden `_vat` user pool. A workload runs as
  a pool user only when vat itself runs as root. Otherwise `uid_isolation` is
  `unavailable`, and `vat container inspect` says why.
- *Limits.* Host network only, with no port mapping. Reads of the host
  filesystem are unrestricted. Builds do not support `COPY` wildcards,
  `.dockerignore`, or multi-stage builds, and are not reproducible
  byte-for-byte. The design decision is recorded in
  [docs/product/architecture.md](docs/product/architecture.md#decision-seatbelt--fixed-length-relocation-no-chroot).

- Root WI: -
- Surfaces: CLI: `vat run -- <cmd>` with `--isolation none|seatbelt` and
  `--gpu auto|required|none`, `vat gpu`, `[network].egress`, and
  `vat run --scenario` hermetic mode; `vat image`, `vat container`,
  `vat native users`, and `vat.toml` `image` services with
  `runtime = "native"`.
- Gate — behavior: `cargo test -p vat` - host-process execution, GPU
  visibility, seatbelt egress and hermetic conformance, and the native
  container runtime.
- Gate: `cargo test -p vat`
- Gate:
  `rg -n -e 'Apple GPU' -e Metal -e MPS -e MLX -e tensorflow-metal README.md src/gpu.rs`
- Gate:
  `rg -n -e sandbox -e isolation -e seatbelt README.md src/sandbox`

| Work Root | Kind | WI | Gate / Evidence |
|---|---|---:|---|
| Host-process execution and GPU visibility | epic | - | `rg -n -e 'Apple GPU' -e Metal -e MPS -e MLX -e tensorflow-metal README.md src/gpu.rs` |
| Resource isolation boundary | epic | - | `rg -n -e sandbox -e isolation -e seatbelt README.md src/sandbox` |
| Network sandbox v3 — seatbelt egress policy | change | #518 | `cargo test -p vat --test vat_sandbox_egress -- --nocapture` |
| Sandbox applied to runner-mode commands | change | #527 | `cargo test -p vat --test vat_runner_sandbox -- --nocapture` |
| Sandbox egress policy fails closed when isolation cannot enforce it | change | #1300 | `cargo test -p vat --test vat_sandbox_egress_fail_closed -- --nocapture` |
| Native containers: OCI `darwin/arm64` images, seatbelt + fixed-length relocation, process-group lifecycle | change | - | `cargo test -p vat --test vat_native_runtime -- --nocapture` (build/export/import/run with a relocated script and venv, in-root writes visible to diff, seatbelt denial outside the root, host-equal GPU, detached lifecycle and exit codes, push/pull against an in-test registry, a native `vat.toml` image service). Dedicated UID: not verified without root. |

### Local GCP Emulation and Transparent Routing (pillar 3, shipped part)

Pure-Rust in-process emulators start instantly with no Java, gcloud, or Docker
and are reached through the standard `*_EMULATOR_HOST` variables; the
gcloud-wrapped family covers the services Google ships an emulator for. With an
`http-mock` service and a `[network]` route, a runner's calls to the real
`*.googleapis.com` host — REST and gRPC — are routed to the local emulator with
no app code change.

- Root WI: -
- Surfaces: CLI: `vat emulator` (hidden, run by presets) + `vat.toml`
  `[[services]] preset = gcloud-pubsub|firebase-auth|gcloud-cloud-tasks|cloud-scheduler|cloud-workflows|cloud-storage|http-mock|openapi|gcloud-firestore|gcloud-datastore|gcloud-bigtable|gcloud-spanner|firebase`
  + `[[network.routes]]`.
- Gate — behavior: `cargo test -p vat` - built-in emulators (REST + gRPC),
  transparent routing, and hermetic no-forward conformance.
- Gate: `cargo test -p vat`
- Gate: `cargo test -p vat --test vat_emulators -- --nocapture`

| Work Root | Kind | WI | Gate / Evidence |
|---|---|---:|---|
| GCP / Firebase emulator service presets | change | #143 | `cargo test -p vat --test vat_emulators -- --nocapture` |
| Built-in Rust emulators (Pub/Sub gRPC + Firebase Auth REST) | change | #145 | `cargo test -p vat --test vat_emulator_auth --test vat_emulator_pubsub -- --nocapture` |
| Built-in Rust emulators (Cloud Tasks + Cloud Scheduler) | change | #146 | `cargo test -p vat --test vat_emulator_tasks --test vat_emulator_scheduler -- --nocapture` |
| Built-in Rust emulator (Cloud Workflows subset interpreter) | change | #147 | `cargo test -p vat --test vat_emulator_workflows -- --nocapture` |
| Built-in Rust emulator (Cloud Storage / GCS) | change | #148 | `cargo test -p vat --test vat_emulator_storage -- --nocapture` |
| Built-in HTTP mock + record/replay proxy (HTTPS MITM) | change | #149 | `cargo test -p vat --test vat_emulator_httpmock -- --nocapture` |
| OpenAPI-driven mock HTTP service (spec → responses) | change | #150 | `cargo test -p vat --test vat_emulator_openapi -- --nocapture` |
| Dual-protocol emulators (Cloud Tasks + Scheduler gRPC alongside REST) | change | #499 | `cargo test -p vat --test vat_emulator_tasks_grpc --test vat_emulator_scheduler_grpc -- --nocapture` |
| Network sandbox v1 — transparent HTTP host-routing | change | #503 | `cargo test -p vat --test vat_emulator_httpmock_routing -- --nocapture` |
| Network sandbox v2 — transparent gRPC routing (h2 MITM) | change | #509 | `cargo test -p vat --test vat_emulator_grpc_mitm_routing -- --nocapture` |
| gRPC reverse-proxy h2c connection pool | change | #516 | `cargo test -p vat --test vat_emulator_grpc_mitm_routing -- --nocapture` |
| Full-hermetic http-mock no-forward mode | change | #530 | `cargo test -p vat --test vat_emulator_httpmock_hermetic -- --nocapture` |

### Container and Kubernetes Paths Scheduled for Supersession

These rows are shipped and still gated; they are the Linux-workload and
Kubernetes paths the current tree can keep. The product direction replaces them
with the shared Linux VM, the Docker Engine API, and persistent K3s (see
[ROADMAP.md](ROADMAP.md)). Until a replacement passes its own gate, each row
below stays supported exactly as bounded in the [CLI](#cli) table and in
[STATUS.md](STATUS.md). `vat compose` is not general Compose, and the Apple
Container K3s session is not persistent Kubernetes.

- Root WI: -
- Surfaces: CLI: `vat build`, `vat compose`, `vat.toml` `runtime = "micro_vm"`
  and `cluster = ...` services, `vat cluster`, and `vat k8s ephemeral|session`.
- Gate — behavior: `cargo test -p vat` - deterministic fake coverage for
  Compose, the MicroVM backend, cluster drivers, and K3s session;
  real-host E2Es are opt-in `--ignored` runs named per row.
- Gate: `cargo test -p vat`
- Gate:
  `cargo test -p vat --test vat_build --test vat_compose --test vat_compose_import --test vat_compose_build --test vat_cluster --test vat_sandbox_microvm --test vat_k8s_ephemeral -- --nocapture`

| Work Root | Kind | WI | Gate / Evidence |
|---|---|---:|---|
| Local Kubernetes cluster service and `vat cluster` | change | #141 | `cargo test -p vat --test vat_cluster -- --nocapture` |
| MicroVm sandbox backend for vat run | change | #1474 | `cargo test -p vat --test vat_sandbox_microvm --test vat_sandbox_microvm_fail_closed -- --nocapture` |
| vat build: Dockerfile build via container CLI | change | #1479 | `cargo test -p vat --test vat_build -- --nocapture` |
| vat compose: bounded compose subset, up/down/ps/logs | change | #1484 | `cargo test -p vat --test vat_compose --test vat_compose_import -- --nocapture` |
| Compose runtime-local build artifacts | change | #1529 | `cargo test -p vat --test vat_compose_build -- --nocapture` |
| Headless Apple Container K3s one-shot, lease, local-image delivery, and loopback Service port-forward | change | #1693 | deterministic fake regression passed, including bounded session-exec lifecycle/marker coverage; independent-kubectl one-shot E2E passed 1/1 (36 filtered, 28.38s), leased E2E passed 1/1 (36 filtered, 29.97s), local-image E2E passed 1/1 (36 filtered, 49.73s), and Service-forward E2E passed 1/1 (36 filtered, 49.57s). Requires an independently installed PATH `kubectl`; VAT rejects OrbStack-provided kubectl. Evidence is bounded to text commands, strict one-document JSON exec with explicit `--timeout 30`, one already-local Apple `alpine:3.20` pod with `imagePullPolicy=Never` and a marker log, and one Service-only loopback JSON tunnel; it does not claim registry-pull generality, persistent Kubernetes, GUI, Docker Engine/API, or OS-sandbox behavior. Gate: `RUST_TEST_THREADS=1 VAT_K8S_LOCAL_IMAGE_E2E_REQUIRED=1 cargo test -p vat --test vat_k8s_ephemeral -- --ignored --nocapture` |
| Apple Container k3s local Kubernetes | epic | #1537 | one-shot, leased, local-image, and Service-forward independent-kubectl real-host E2Es passed; each remains bounded. Phase 0 is a bounded Docker-free path: `vat k8s ephemeral` runs one foreground host command and cleans up, while `vat k8s session create/exec/port-forward/image/status/delete` keeps one running guest and private credentials across explicit agent calls until its bounded lease is deleted or reclaimed. Every K3s command requires an independently installed `kubectl` first on PATH and rejects an OrbStack-provided binary. Persistent/reboot-safe kubeconfig, storage/PVC, ingress/LB, multi-node networking, and `microvm-k3s` remain blocked. |

### Developer & Agent Experience

An agent can discover VAT's supported command surface and boundaries offline,
select concise task-specific guidance, and inspect the host substrate before a
project has a `vat.toml`.

- Root WI: #1819
- Surfaces: CLI: `vat llm`, `vat --help`, `vat doctor --host-only`, and
  machine-readable command output.
- Gate — behavior:
  `cargo test -p vat --test vat_cli_convention --test vat_toml_runner` -
  offline onboarding, documented command inventory, and configuration-free host
  preflight.
- Gate: `cargo test -p vat --test vat_cli_convention -- --nocapture`
- Gate:
  `cargo test -p vat --test vat_toml_runner -- --nocapture`

| Work Root | Kind | WI | Gate / Evidence |
|---|---|---:|---|
| Offline command contract | change | #1817 | `cargo test -p vat --test vat_cli_convention -- --nocapture` |
| Agent onboarding topics | change | #1818 | `cargo test -p vat --test vat_cli_convention -- --nocapture` |
| Interactive tooling | n/a | - | VAT is a local CLI and intentionally has no GUI, daemon dashboard, or remote control plane. |
| Integration contract | change | #701 | `cargo test -p vat --test vat_toml_runner --test behavior_scenario_failure_keeps_topology_and_logs --test behavior_scenario_hermetic_requires_http_mock_service --test behavior_scenario_run_starts_app_dependency_and_runner -- --nocapture` |
| Configuration-free host preflight | change | #1820 | `cargo test -p vat --test vat_toml_runner -- --nocapture` |

## AW Verification Snapshot

| Field | Value |
|---|---|
| Last verified | 2026-06-20 |
| Production readiness | ready |
| Tech design root | `tech-design` |
| TD lock | `tech-design/td.lock` |
| External-contract inventory | `aw.toml` (`aw.ec.generated`) |
| Source ownership | full codegen, 100.0% (65/65) |
| Semantic coverage | 100.0% |
| Traceability coverage | 95.6% |
| External-contract gate | passed, 6/6 |
| Test gate | `cargo test -p vat` passed |

## What vat is *not*

- **Not a GUI or Desktop application — permanently.** vat is operated through
  its CLI and machine-readable output for agents. Do not add graphical controls,
  dashboards, tray/menu-bar UI, or a Desktop lifecycle surface.
- **Not a hostile-code security boundary on the native runtime.** Pillar 1
  runs macOS processes. macOS has no namespaces or cgroups, so seatbelt
  confinement, a copy-on-write rootfs, and (on the roadmap) a dedicated UID per
  container give resource isolation for cooperative workloads — weaker than a
  VM. A workload that must be contained as untrusted belongs in the shared Linux
  VM of pillar 2, where the kernel is the boundary. The same-UID host-child
  hygiene in `vat k8s session port-forward` and the `micro_vm` service path are
  likewise not adversarial-child boundaries.
- **Not a Linux runtime on the native pillar.** Pillar 1 is pure Apple
  ecosystem: `darwin/arm64` layers, Homebrew bottles, macOS processes. Linux
  images and Linux-only workloads go through the shared Linux VM of pillar 2.
  Besides `vat machine`, today's Linux routes are the bounded Apple Container
  paths (`runtime = "micro_vm"` services and the K3s session); they are
  scheduled to be superseded, not extended.
- **Not "no VM at all".** The native runtime has no VM, which is why the host
  GPU is reachable. The Docker and GKE pillars use exactly **one shared Linux
  VM**; vat never starts one VM per container. Metal does not pass into that VM,
  so a Linux container has no Apple GPU. Vulkan (Venus) GPU inside the VM is
  explicitly deferred and is not a commitment.
- **Not a `docker` command shim.** vat no longer installs a `docker`
  symlink or translates Docker argv. Docker work goes through `vat machine
  start`, which serves a Docker Engine at `~/.vat/run/docker.sock`; vat sets
  `DOCKER_HOST` to it for its own child processes, and you export
  `DOCKER_HOST=unix://$HOME/.vat/run/docker.sock` yourself to use the stock
  `docker` CLI from your shell.
- **Not persistent Kubernetes today.** `vat cluster` and the `cluster`
  service wrap kind/k3d/minikube, which need a Docker daemon on Apple Silicon.
  `vat k8s ephemeral` is a one-boot Apple Container K3s guest for one foreground
  command; `vat k8s session` adds a bounded lease so an agent can make several
  explicit calls with the same private kubeconfig. Neither is a daemon nor
  restart-safe, and neither promises reboot-safe kubeconfig, storage/PVC,
  ingress or load balancer, multi-node networking, or registry-pull generality.
  Every `vat k8s` command requires an independently installed `kubectl` first
  on `PATH` and rejects an OrbStack-provided binary. Persistent K3s inside the
  shared VM, with shared images and PVCs, is a [ROADMAP.md](ROADMAP.md)
  outcome.
- **Not a GCP account.** The emulators reproduce the API behavior local tests
  depend on (the common client operations, REST and gRPC where both exist).
  They do not reproduce IAM, quotas, billing, regional behavior, or every
  method; fidelity gaps are listed per emulator in [STATUS.md](STATUS.md). The
  official emulators remain reachable as `runtime = native|docker` fallbacks
  where Google ships one.
- **Not a resource scheduler.** vat owns resource isolation: copy-on-write
  workspaces, sandbox backends, and agent-readable state. It does not decide
  admission, throttling, pausing, or kill policy. That is cap's job. Compose
  them explicitly when scheduling is needed, for example
  `cap run --label "vat train" -- vat run -- python train.py`.
- **Not a long-lived process manager.** Services in `vat.toml` are dependencies
  of one runner invocation. vat starts them, waits for readiness, runs the
  runner, captures evidence, and terminates them. Standalone `vat cluster`
  clusters and `vat k8s session` leases outlive a run as a convenience, but vat
  does not *supervise* them (no daemon, no restart, no health monitoring) — it
  creates/lists/deletes/reports only on explicit command. The shared Linux VM of
  pillar 2 will be the one exception: it is a long-lived substrate vat starts on
  demand and reports on, and its lifecycle contract is a roadmap outcome.
- **Not a shared Apple Container builder manager.** `vat capabilities --json`
  reports `apple_container.builder` as a bounded, read-only advisory
  (`ownership="shared_unknown"`, `automatic_cleanup=false`; optional
  `container system df` evidence is host-global, never VAT-attributed). VAT
  never starts, stops, deletes, or prunes the shared builder or its cache.
- **Not an image registry or remote image-build service.** `vat build` and a
  compose `build:` service
  build a Dockerfile into the selected local image store (Docker or Apple
  Container). A local Artifact Registry for the GKE pillar is a roadmap
  outcome; a hosted or remote registry is not a goal. A vat's environment is a
  declarative [`EnvSpec`](src/spec.rs) an agent reads and rewrites. A
  `vat.toml` *service* may run as an ephemeral container, but the runner is
  always a host process — vat never containerizes your workload on the native
  pillar.

## Quick start

```bash
./build.sh debug         # build + install ~/.cargo/bin/vat

# run a command in a fresh copy-on-write clone of the current dir
vat run -- python train.py

# run the default local test protocol from vat.toml
vat capabilities --json  # full host probe, including Docker and shared-builder advisory
vat plan --json          # inspect selected runner/services without side effects
vat doctor --json        # selected-plan preflight; Apple-only plans skip Docker
vat run
vat logs <id> runner

# let an upstream planner/TIA tool choose tests; vat only injects the plan
vat run --plan impact.json impacted

# give an LLM/tool agent the compact vat usage contract
vat llm

# Docker: boot vat's shared Linux VM, then use the stock docker CLI against it
vat machine start
export DOCKER_HOST="unix://$HOME/.vat/run/docker.sock"
docker run --rm alpine:3.20 uname -a

# Keep one Docker-free local K3s guest across explicit agent steps (bounded lease).
# Prerequisite: an independently installed kubectl must be first on PATH; VAT rejects
# an OrbStack-provided kubectl. On this host Homebrew kubectl is at /opt/homebrew/bin.
# Independent-kubectl one-shot, leased, local-image, and Service-forward E2Es
# passed. The local-image proof is one already-local Apple alpine:3.20 pod with
# imagePullPolicy=Never and a marker log, followed by exact session cleanup; it
# is not registry-pull generality. All remain bounded one-guest evidence rather
# than a durable cluster claim.
vat k8s ephemeral image build
vat k8s session create --ttl 30m
# stdout returns id; use it in subsequent tool calls
vat k8s session status --verify-api <id>
vat k8s session exec --timeout 30 <id> -- kubectl get nodes
vat k8s session exec <id> -- kubectl get namespaces
# Text exec is unchanged. For one agent document rather than raw child streams,
# use JSON exec; its process exit remains the child exit code.
vat k8s session exec --format json --timeout 30 <id> -- kubectl get nodes -o json
# move a pre-existing Apple Container image into this active K3s lease only
vat k8s session image load <id> alpine:3.20
# prove the workload cannot fall back to a registry pull
vat k8s session exec <id> -- kubectl run local-alpine --image=alpine:3.20 --restart=Never --image-pull-policy=Never --command -- /bin/sh -ec 'echo local'
# Test one already-created ClusterIP Service through a loopback-only tunnel.
# VAT strips KUBECONFIG, VAT_K8S_CACHE_DIR, VAT_K8S_API_SERVER, and VAT_HOME
# from the child environment. The child shares kubectl's tracked process group,
# so keep it cooperative and non-daemonizing; this hygiene is not a same-UID OS sandbox.
# Text preserves direct child streams. JSON waits for verified tunnel cleanup,
# then emits one bounded agent document with no raw child-stream replay.
vat k8s session port-forward run --format json <id> service/api 8080 -- /bin/sh -ec 'curl -fsS "http://$VAT_K8S_PORT_FORWARD_ADDR/healthz"'
vat k8s session delete <id>

# what GPU can my vats see? (the headline claim, in one command)
vat gpu
#   vendor   apple
#   chip     Apple M1 Pro
#   backends metal, mps, mlx
#   status   ✓ accessible

# what happened / what changed — one JSON doc, for an agent
vat state <id>
vat diff  <id>

# branch a running environment, git-style
vat fork <id>          # new runnable working copy
vat snapshot <id>      # frozen restore point
```

## The model

A **vat** =
copy-on-write workspace ([`overlay`](src/overlay.rs))
+ declarative [`EnvSpec`](src/spec.rs)
+ append-only [`event`](src/event.rs) log
+ projected [`VatState`](src/state.rs).

`vat run` clones a base (a host dir, or another vat via `--from`) into a fresh
rootfs, runs your command in the chosen [`sandbox`](src/sandbox/) backend with
live stdio, then records the run and recomputes the filesystem diff. Because
clones are APFS `clonefile(2)` (near-instant, block-shared until written),
fork/snapshot are cheap — an agent can try two approaches, fail, and roll back
without rebuilding.

Vat state is repo-local by default: the store root is `<repo>/.vat` (ignored by
git). Set `VAT_HOME` only when an external runner intentionally wants a
different store root.

### vat state

The command an agent calls to understand a vat. One document, no log-scraping:

```jsonc
{
  "id": "vat-5oyh3vc",
  "status": { "state": "exited", "code": 0 },
  "spec":   { "isolation": "none", "gpu": "auto", ... },
  "lineage": ["vat-..."],            // the fork tree this vat sits in
  "last_run": { "command": [...], "exit_code": 0, "duration_ms": 30 },
  "plan": { "source_path": "impact.json", "rootfs_path": ".../.vat-plan/impact.json",
            "digest": "fnv1a64:..." },
  "test_run": { "topology": { "runners": ["e2e"], "services": ["pg"] },
                "plan": { "...": "..." }, ... },
  "workspace": { "rootfs": "...", "file_count": 12, "size_bytes": 4096 },
  "changes": { "added": 1, "deleted": 1, "sample_added": ["made.txt"], ... },
  "gpu": { "chip": "Apple M1 Pro", "accessible": true,
           "backends": ["metal","mps","mlx"] },
  "events_tail": [ ... ]
}
```

## CLI

| Verb | Purpose |
|------|---------|
| `vat run` | Load `vat.toml`, select `default_runner` or the only runner, emit sparse JSONL checkpoints, run setup/services/readiness/runner, capture logs/artifacts/diff/state, and cleanup. |
| `vat run <runner-id>` | Run a specific `vat.toml` runner. |
| `vat run --scenario <id>` | Run a named app-under-test scenario: app service + scenario deps + runner deps, with topology evidence in `vat state`. |
| `vat run --keep always\|failed\|never [runner-id]` | Override `[workspace].keep` for one configured run, e.g. retain logs for a passing probe without editing `vat.toml`. |
| `vat run --plan <path> [runner-id]` | Copy an opaque upstream plan (for example TIA output) into the vat, expose it as `VAT_PLAN_PATH` / `VAT_PLAN_DIGEST`, and record it in `vat state`. |
| `vat run -- <cmd>` | Clone a base, run one direct command, record the result. `--base DIR`, `--from VAT`, `--isolation none\|seatbelt`, `--gpu auto\|required\|none`, `--json`. |
| `vat capabilities --json` | Full host capability discovery: report COW clone method, isolation backends, Docker provider/daemon state, service-provider capabilities, and an Apple Container shared-builder advisory. It retains the normal Docker daemon probe regardless of a later selected plan. `services.docker_services` is an explicit availability string: a full Docker probe yields `available` or `unavailable`. The advisory is bounded and read-only: `builder status` yields `ownership=shared_unknown` and `automatic_cleanup=false`; parseable configuration is distinct from optional live `observed_stats`, and optional `system df` is `global_apple_container` host evidence rather than VAT-owned disk. Unsupported, malformed, or timed-out status/stats/df appear as advisory unknown/probe errors without failing capability discovery; VAT never starts, stops, deletes, or prunes the builder/cache. A live builder state is reported only when the installed Apple Container CLI supports and returns it. |
| `vat plan [runner-id...] --json` | Print the selected configured run topology without creating a vat, starting services, or running tests. |
| `vat doctor [runner-id...] --json` | Run cheap read-only preflight checks with capability discovery scoped to the selected topology. A selected explicit MicroVm/Apple-Container-only plan performs exactly one read-only `container system status` probe per doctor invocation and projects that result to its selected MicroVm services; it never executes Docker even when it is on `PATH`. In that deliberate no-probe state, `services.docker_services` is `not_probed`, while `docker.daemon_probe.state=skipped` with `Docker daemon probe skipped for Apple-Container-only selected plan` supplies provenance. `docker.daemon=false` is not Docker-unavailable evidence because no Docker command ran. An unselected Docker service cannot poison that runner. The selected plan also reports the shared-builder advisory, but its unknown/timeout/probe errors never change runtime success. Doctor neither autostarts Apple Container nor falls back to Docker: unsupported MicroVm presets with no declared OCI route and MicroVm preset named volumes fail closed. Docker-runtime services, Auto image services, eligible Auto preset Docker fallbacks, and selected clusters retain the normal Docker daemon probe, yielding `services.docker_services=available|unavailable`; a cluster requires its Docker backend. |
| `vat doctor --host-only [--json]` | Configuration-free read-only host preflight. It does not read `vat.toml`, select a runner, create a workspace, or start services. It reports copy-on-write, requested isolation availability, host GPU visibility, Apple Container, Docker daemon, and independent (non-OrbStack) `kubectl` evidence. Missing optional substrates are reported as `unavailable` observations; the command itself completes successfully with `next: vat capabilities --json`. |
| `vat llm [--topic <t>] [--format md\|json]` | Print offline agent-facing docs. Default `outline`; use `--topic guide` for the detailed vat.toml/service/evidence/boundary guide. |
| `vat upgrade` | Self-update to the latest `vat@*` GitHub release (`--check` to report only, `--version <tag>` to pin). One of the three mandatory CLI-convention verbs (`llm`/`upgrade`/`issue`), via the shared `cli-std` crate. |
| `vat issue search\|view\|create` | Search, read, and file diagnostics-rich GitHub issues under `app:vat`; `issue create --dry-run --title <t>` previews version + target + OS/arch diagnostics without submitting. |
| `vat machine start` | Boot the shared Linux machine (creating it on first use) and wait for its Docker Engine at `~/.vat/run/docker.sock`. vat sets `DOCKER_HOST` to that socket for its own child processes; export `DOCKER_HOST=unix://$HOME/.vat/run/docker.sock` yourself to use the stock `docker` CLI. `vat machine --help` lists the other verbs. |
| K3s host CLI prerequisite | Every `vat k8s` command requires an independently installed `kubectl` first on `PATH`; VAT rejects an OrbStack-provided binary before K3s use. This is host-tool provenance, not a GUI or Docker Engine requirement. Homebrew `kubernetes-cli` at `/opt/homebrew/bin/kubectl` is installed on this host. Independent-kubectl one-shot, leased, local-image, and Service-forward E2Es each passed 1/1 (36 filtered) in 28.38s, 29.97s, 49.73s, and 49.57s respectively; the local-image proof is one already-local Apple `alpine:3.20` pod with `imagePullPolicy=Never`, a marker log, and exact session cleanup—not registry-pull generality. |
| `vat k8s ephemeral image build` | Explicitly build VAT's embedded systemd image into the Apple Container image store. Its local tag identifies the embedded build-asset revision, not a verified supply-chain image digest. It never starts a cluster. |
| `vat k8s ephemeral run [--image <ref>] -- <command...>` | Start exactly one disposable Apple K3s guest, prove host API access through a private kubeconfig, run one foreground command, then delete credentials and the exact owned machine. The child receives `KUBECONFIG`, `VAT_K8S_CACHE_DIR`, `VAT_K8S_API_SERVER`, and an isolated `HOME`; direct kubectl keeps its normal cache under that private HOME. Its final stdout line is a `vat_k8s_ephemeral_result` terminal JSON record. The independent-kubectl one-shot real-host E2E passed 1/1 (36 filtered) in 28.38s. On bootstrap failure, VAT renders the root error first, then a best-effort 6-second total / 1-second-per-probe read-only diagnostic block with exactly `guest_install_log`, `guest_k3s_system`, `backing_container_logs`, `machine_boot_log`, `machine_inspect`, and `container_system_status`; staged installer evidence is non-sensitive, private kubeconfig/cache and host credentials are excluded, and the existing exact cleanup still runs. This does not fix or retry the existing 300-second bootstrap behavior, rerun `k3s --version`, or add a wrapper/recovery command. `vat k8s ephemeral cleanup` reconciles interrupted sessions only after the recorded PID is gone; an interrupted create retains its marker until Apple Container can prove a terminal create/cancellation state. |
| `vat k8s session create [--ttl 30m]` | Create one bounded, one-boot Apple K3s lease with private 0600 credentials. The result includes an opaque id and runnable `next`; it never exposes the kubeconfig path. TTL accepts whole seconds or `s`/`m`/`h`, from 1 minute through 4 hours. Its shared K3s bootstrap path uses the same primary-error-first, fixed read-only diagnostic block and exact cleanup on failure; advisory diagnostics never make this a persistent Kubernetes backend. |
| `vat k8s session exec [--format json] [--timeout <seconds>] <id> -- <command...>` | Both text and JSON exec prove the active lease, exact Apple backing-ID/API endpoint, private credentials, and owned host API under the private operation lock. Omit `--timeout` to use the remaining lease TTL; an explicit timeout is 1..=14400 seconds and cannot exceed that remaining TTL. VAT rechecks expiry immediately before spawn, puts every credentialed host command in an owned process group, and holds the lock through its cleanup. Normal exit, deadline, or SIGINT/SIGTERM stops and reaps that group; the private exec marker is removed only after the group is absent. If VAT crashes after marker creation, a starting or live exec marker makes later exec, delete, or cleanup fail closed rather than signal an arbitrary recovered command; this is not a crash-safe termination guarantee. `--format json` then emits exactly one `schema="vat.k8s.session.exec.v1"`, `format="vat_json"` document with separate stdout/stderr, child exit code, stream truncation/lossy flags, `api_verified=true`, `runtime_invoked=true`, `session_record_mutated=false`, and a `status --verify-api` next step; raw child streams are not replayed. Both streams drain concurrently and retain only a latest suffix whose serialized JSON string is at most 64 KiB. JSON errors mask private credential/cache paths. The child intentionally receives credentials, so this is not an untrusted-child security boundary. Deterministic fake/unit coverage exists. The independent-kubectl leased real-host E2E passed 1/1 (36 filtered) in 29.97s and proved text commands, strict one-document JSON exec with `--timeout 30`, `status --verify-api`, and exact delete; it does not establish crash recovery termination or persistent Kubernetes. |
| `vat k8s session port-forward run [--format json] <id> service/<name> <remote-port> [--namespace <ns>] [--local-port <port>] -- <command...>` | Requires an independently installed `kubectl` first on `PATH`; VAT rejects an OrbStack-provided binary before K3s use. Text forwards exactly one literal Service port to `127.0.0.1` for one foreground host child and writes its terminal record on a new line after child output. `--format json` is the only JSON form. It remains Service-only, loopback-only, and credential-free for the host child: `--local-port 0` (the default) lets kubectl choose a loopback port; the child receives only `VAT_K8S_PORT_FORWARD_{HOST,PORT,ADDR,RESOURCE,NAMESPACE}` and a private `HOME`, while VAT strips `KUBECONFIG`, `VAT_K8S_CACHE_DIR`, `VAT_K8S_API_SERVER`, `VAT_K8S_EPHEMERAL`, and `VAT_HOME`. This is credential hygiene rather than a same-UID OS sandbox or adversarial-child security boundary. The child joins kubectl's authenticated process group, and VAT holds the private operation lock through group cleanup: normal cleanup reaps the leader and confirms ordinary cooperative, non-daemonizing descendants are gone before `cleanup=confirmed`; children that daemonize or escape the group are outside the contract. JSON emits exactly one `schema="vat.k8s.session.port-forward.v1"`, `format="vat_json"` document only after cleanup is confirmed, with separate 64 KiB serialized-capped stdout/stderr, truncation/lossy flags, child exit, and a `status --verify-api` next step; it never replays raw child streams. VAT-owned lease/setup/API/tunnel/cleanup failures are masked, while opaque credential-free child output is preserved in a successful document. It rechecks the lease silently after API proof and immediately before exact kubectl and host-child spawns, so expiry prevents a tunnel; a partial reader setup reaps the direct child and completes outer-group cleanup before joining readers. The independent-kubectl Service-forward E2E passed 1/1 (36 filtered) in 49.57s, including one loopback Service text and strict one-document JSON tunnel with a credential-free child, confirmed cleanup, and closed local ports. This is not ingress/LB, a public listener, a background tunnel, arbitrary resource forwarding, persistent Kubernetes, or a same-UID OS sandbox. |
| `vat k8s session image load <id> <local-ref> [--platform linux/arm64]` | Deliver one already-local Apple Container image into the active lease's K3s `k8s.io` namespace without Docker or a registry pull. VAT requires exactly one inspected `linux/arm64` variant, uses a private 2 GiB-bounded OCI archive, verifies the canonical reference after import, then removes archive copies from host and guest. The opt-in real-host local-image E2E passed 1/1 (36 filtered) in 49.73s: one already-local Apple `alpine:3.20` loaded into one lease, a pod ran it with `imagePullPolicy=Never` and emitted its marker log, then exact session cleanup completed. This is not registry-pull generality, persistence, GUI, or Docker Engine/API evidence. Arbitrary tar files and cross-platform delivery fail closed. |
| `vat k8s session status [--verify-api] <id> \| delete <id> \| cleanup` | No-flag `status` is unchanged: it reports only non-secret lease and exact-machine state. `status --verify-api <id>` only probes an active, unexpired session with no retained port-forward or exec marker. Under the private operation lock it rechecks expiry, proves the exact backing identity/endpoint and private credentials, rechecks expiry immediately before one bounded API probe, then reports `api_checked=true`, `api_state="reachable"` on success. Expired/recovery-marker paths do not probe and report `api_checked=false`, `api_state="not_checked"`; busy, unavailable, and identity-mismatched sessions fail closed without mutating the lease or credentials. A live or starting exec marker similarly blocks exec, delete, and cleanup rather than claiming it safely terminated a prior credentialed group. Focused fake status coverage passed 4/4; the precise status unit passed 1/1. The independent-kubectl leased E2E passed 1/1 (36 filtered) in 29.97s and includes `status --verify-api` after text and strict JSON exec; this is bounded active-lease evidence, not persistence or a general API-status guarantee. This remains a one-boot, nonpersistent Apple Container lease with no GUI or Docker Engine/API. `delete` confirms removal of the exact machine before removing credentials. `cleanup` reclaims expired leases and abandoned creates; there is no background cleanup daemon. |
| `vat ls` | List vats (one line each, or `--json` array of full states). |
| `vat state <id>` | Full agent-legible state as JSON (`--compact` for one line). |
| `vat diff <id>` | Every filesystem change vs. the vat's base (`--json`). |
| `vat logs <id> [service-id\|runner]` | Print captured logs from a retained vat.toml runner invocation. |
| `vat fork <id>` | Copy-on-write a new **runnable** working copy. |
| `vat snapshot <id>` | Copy-on-write a **frozen** restore point. |
| `vat rm <id>` | Delete a vat and its workspace. |
| `vat gc [--execute]` | Report retained vat disk usage and prune old workspaces. Dry-run by default; protects running/snapshot/failed/interrupted/newest vats unless explicit flags opt in. |
| `vat gpu` | Report the GPU every vat on this host can reach. |
| `vat image build -t NAME[:TAG] [-f FILE] CONTEXT [--json]` | Build a native OCI `darwin/arm64` image from a `Vatfile` (`FROM scratch\|<image>`, `WORKDIR`, `ENV`, `LABEL`, `EXPOSE`, `COPY`, `RUN`, `CMD`, `ENTRYPOINT`; no `COPY` wildcards, `.dockerignore`, or multi-stage builds). `RUN` executes on the host under seatbelt with `$VAT_ROOT` set to the build root; baked root paths are recorded in `vat.relocations`. |
| `vat image pull REF [--json]` / `vat image push SRC [DEST] [--json]` | Pull or push a `darwin/arm64` image over the OCI Distribution API. Pull picks `darwin/arm64` from an index and verifies every digest. Credentials are Basic or Bearer, from the Docker config `auths`, with no credential helpers. Plain HTTP is used only for loopback hosts and `VAT_INSECURE_REGISTRIES`. Requires the default-on `registry` feature; a lean build bails cleanly. |
| `vat image import --oci-layout DIR [--tag REF] [--json]` / `vat image export --oci-layout DIR NAME...` | Move images as OCI image layouts with no registry. |
| `vat image ls [--json]` / `vat image inspect NAME [--json]` / `vat image tag SRC DEST` / `vat image rm NAME...` | Inspect and manage the native image store at `~/.vat/native` (`VAT_HOME` respected). `rm` garbage-collects blobs no tag or container uses. |
| `vat container run [--name N] [-d] [--rm] [-e K=V] [-v HOST:PATH[:ro]] [-w DIR] [--network host\|none] IMAGE [CMD...]` | Run a native container: an APFS `clonefile` copy-on-write root of a fixed 128-byte path, relocated for that root, under a seatbelt profile that confines writes to the root, read-write mounts, and the per-user cache dir. No chroot, so `VAT_ROOT` is set and `PATH` is root-relative. Foreground forwards the exit code (127 if the command is not found); `-d` prints the `ctr-…` id. Host network only. |
| `vat container ps [-a] [--json]` / `logs [-f]` / `exec [-e] [-w] ID CMD...` / `stop [-t SECS]` / `rm [-f]` / `inspect [--json]` / `diff [--json]` | Detached lifecycle over the container's process group. `stop` sends TERM, then KILL after the timeout (exit 143 or 137 unless the workload exits itself). `inspect` reports image, root, sandbox paths, relocation, changes, GPU, and `uid_isolation`. `vat state ctr-…` and `vat diff ctr-…` accept the same ids. |
| `vat native users setup [--count N] [--first-id ID] [--print]` / `vat native users ls [--json]` | Create (as root) or list the hidden `_vat` user pool for dedicated per-container UIDs. Workloads use the pool only when vat runs as root; otherwise containers report `uid_isolation: "unavailable"`. |
| `vat cluster create\|ls\|delete\|kubeconfig` | Manage standalone local Kubernetes clusters (kind/k3d/minikube), independent of a run. |

### Disk cleanup

Retained vats can accumulate large copy-on-write workspaces. Use `vat gc` to
inspect disk pressure before deleting anything:

```bash
vat gc --json                         # dry-run, machine-readable metadata report
vat gc --measure --json               # also run du -sk for disk sizes
vat gc --keep-last 5                  # dry-run: keep the newest 5 vats
vat gc --execute --keep-last 5        # delete non-running, non-snapshot,
                                      # non-failed candidates
vat gc --execute --include-failed --keep-last 5
                                      # also prune failed/interrupted retained runs
vat gc --apparent --json              # also walk files for apparent size
```

The default GC report avoids walking large rootfs trees, so it stays usable when
hundreds of vats exist. Add `--measure` when you need `disk_size_bytes` from
`du -sk`. Add `--apparent` only when you need file-length totals; it walks every
retained rootfs and is slower on large stores. APFS/reflink clones can make
apparent size much larger than physical blocks.

### Interrupt cleanup

`vat run` installs scoped SIGINT/SIGTERM cancellation before it owns children.
The first signal wins; the handler only records it, while the run thread stops
runner process groups first and VAT-owned services in reverse start order. Each
group receives TERM, a bounded grace period, KILL when still present, leader
reaping, and an explicit PGID-absence check before terminal metadata is written.
Direct and configured runs then persist `status.state = "interrupted"` with the
signal and reason, clear child PIDs, retain the VAT as failure evidence, and
exit 130 for SIGINT or 143 for SIGTERM. Explicit `external` services and other
unrelated listeners are observed only and are never signalled by this cleanup.

## vat.toml

`vat.toml` is the project-local protocol an agent edits when it needs vat to
prepare and run a real local test environment:

```toml
version = 1
name = "local-e2e"
default_runner = "e2e"

[workspace]
base = "."
workdir = "."
keep = "failed" # failed | always | never

[env]
NODE_ENV = "test"

[[setup]]
id = "install"
cmd = ["pnpm", "install", "--frozen-lockfile"]
when = "missing:node_modules/.modules.yaml"

[[services]]
id = "pg"
preset = "postgres"        # native binary preferred, Docker image fallback
# runtime = "auto"         # auto (default) | native | docker | micro_vm (explicit Apple Container)
seed = ["schema.sql", "fixtures.sql"]
export = { DATABASE_URL = "DATABASE_URL" }

[[services]]
id = "alloy"               # OCI image dependency (no native binary)
image = "google/alloydbomni:latest"
runtime = "micro_vm"       # explicit Apple Container route; never silently falls back to Docker
container_port = 5432
image_env = { POSTGRES_PASSWORD = "pw" }
export = { ALLOY_URL = "postgres://postgres:pw@{host}:{port}/postgres" }

[[services]]
id = "ci-pg"               # already started by GitLab CI services / Docker Compose
external = { host = "postgres", port = 5432 }
export = { DATABASE_URL = "postgres://postgres@{host}:{port}/app" }

[[services]]
id = "k8s"                 # ephemeral local Kubernetes cluster
cluster = "auto"           # auto (kind→k3d→minikube) | kind | k3d | minikube
# k8s_version = "1.30"
# nodes = 1
export = { KUBECONFIG = "{kubeconfig}" }

[[services]]
id = "web"                 # app under test; {port} is auto-allocated
cmd = ["pnpm", "run", "dev", "--", "--host", "127.0.0.1", "--port", "{port}"]
ready_http = "http://127.0.0.1:{port}/"
export = { APP_URL = "APP_URL" }
timeout_s = 30

[[services]]
id = "http"
preset = "http-mock"       # required for hermetic scenario routing/no-forward

[[runners]]
id = "e2e"
requires = ["pg", "k8s"]
cmd = ["pnpm", "run", "test:e2e"]
timeout_s = 300
artifacts = ["test-results/**", "playwright-report/**"]

[[scenarios]]
id = "prod-like"
app = "web"
requires = ["pg", "k8s", "http"]
runner = "e2e"
network = "hermetic"       # open | hermetic
```

`[[scenarios]]` is the production-like path: the app-under-test service starts
with its declared dependencies, runner dependencies are deduped into the same
service set, and `vat state <id>` records `test_run.scenario` with app, runner,
services, routes, and whether hermetic mode was active. `network = "hermetic"`
requires a participating `preset = "http-mock"` service, sets localhost-only
egress, defaults the run to seatbelt isolation, runs direct-start services under
that sandbox, and starts the proxy in no-forward mode. Docker/image, cluster, and
native preset service backends remain external local services; the app and runner
are still host processes, not containers or VMs.

A service is provided in one of five ways, and **native (Homebrew) is
preferred**:

- `preset` — a built-in service. With the default `runtime = "auto"` vat uses
  the native binary when it is installed and falls back to the preset's official
  Docker image when it is not; `runtime = "native"` / `"docker"` force one path.
  `runtime = "micro_vm"` is explicit — `auto` never selects it and it never
  falls back to Docker. For presets with a declared OCI route (for example
  Redis/Postgres/NATS) it uses Apple `container`, checks the local image store,
  performs a bounded pull if missing, verifies it again, then runs it through a
  loopback-only published port. Presets without a declared OCI route and
  MicroVM preset named volumes fail closed.
  Datastore/broker presets: `postgres`, `redis`, `nats`, `rabbitmq`, `mysql`,
  `mongo`.
- `preset` (built-in Rust emulators) — `gcloud-pubsub`, `firebase-auth`, `gcloud-cloud-tasks`,
  `cloud-scheduler`, and `cloud-workflows` run vat's **own** in-process emulator
  under `runtime = auto`: pure Rust, instant start, **no gcloud / Java /
  firebase-tools / Docker**. `pubsub` is a google.pubsub.v1 gRPC server
  (topics/subscriptions, Publish, Pull, StreamingPull, Acknowledge);
  `firebase-auth` is a Firebase Auth (Identity Toolkit) REST server;
  `gcloud-cloud-tasks` serves **both the Cloud Tasks v2 gRPC service and the v2 REST API
  on one port** and delivers each task's httpRequest to its target at scheduleTime
  (or `tasks/{t}:run`); `cloud-scheduler` likewise serves **gRPC + v1 REST** and
  fires a job's httpTarget on its cron schedule (or `jobs/{j}:run`); `cloud-workflows` is a
  Cloud Workflows v1 REST server (createWorkflow → createExecution →
  getExecution) running a **subset Workflows interpreter** (assign / call http.* /
  switch / for / try-retry-except / subworkflow + `${...}` expressions) whose
  `call: http.*` steps **orchestrate the other emulators** or any HTTP endpoint;
  `cloud-storage` is a GCS JSON API v1 server over an in-memory object store
  (bucket CRUD, media/multipart upload, `alt=media` download, list with prefix,
  delete; reports size + md5Hash); `http-mock` is a **transparent HTTP stub +
  record/replay proxy with HTTPS MITM** — the mock-killer for third-party APIs.
  Each exports its host var (`PUBSUB_EMULATOR_HOST`, `FIREBASE_AUTH_EMULATOR_HOST`,
  `CLOUD_TASKS_EMULATOR_HOST`, `CLOUD_SCHEDULER_EMULATOR_HOST`,
  `CLOUD_WORKFLOWS_EMULATOR_HOST`, `STORAGE_EMULATOR_HOST` — point your client's
  base URL at `http://$HOST` for host:port vars; `STORAGE_EMULATOR_HOST` is
  exported as `http://127.0.0.1:<port>` because the GCS REST SDK expects a
  schemed endpoint). `http-mock` instead exports `HTTP(S)_PROXY` + a CA-trust bundle
  (`SSL_CERT_FILE`, `NODE_EXTRA_CA_CERTS`, `REQUESTS_CA_BUNDLE`, …) so the runner's
  outbound HTTP/HTTPS — even hardcoded `https://api.example.com` — is intercepted
  with **no app code change**: register stubs at `$VAT_HTTP_MOCK_HOST/__admin/stubs`,
  and unstubbed calls record to a cassette once then replay offline forever.
  `openapi` (`preset = "openapi"`, `spec = "api.yaml"`) reads an **OpenAPI
  document and serves spec-derived responses** (the response `example`, else a
  schema-synthesized body; path templating like `/users/{id}` and `$ref`) — a
  working fake of a documented API with no stubs or recording. It runs standalone
  (point your base URL at `$OPENAPI_MOCK_HOST`) and also backs the http-mock proxy:
  `POST $VAT_HTTP_MOCK_HOST/__admin/openapi` registers a spec for a host, so a
  proxied `https://` call is answered from the contract (resolution order **stub >
  openapi > cassette > forward**). `pubsub` still accepts `runtime = native`
  (gcloud) / `runtime = docker` (the cloud-cli image) as a full-fidelity fallback;
  the others are built-in only (no official emulator exists). The async emulator
  stack sits behind a default-on `emulator` Cargo feature (`--no-default-features`
  drops it). **Wiring a `gcloud-cloud-tasks` / `cloud-scheduler` client:** these SDKs don't
  read `CLOUD_TASKS_EMULATOR_HOST` / `CLOUD_SCHEDULER_EMULATOR_HOST` (Google ships no
  emulator). Since the emulators now serve **both gRPC and REST**, point the stock
  gRPC client at the host var with an insecure endpoint override (Python:
  `CloudTasksClient(client_options={"api_endpoint": host})`), or use `transport="rest"`
  + `http://$HOST`, or POST the v2 REST API directly. For **zero app config**, add an
  `http-mock` service + a `[network]` route (see *Network sandbox* below): vat then
  transparently routes the real `cloudtasks.googleapis.com` host — REST *and* gRPC —
  to the local emulator.
  ```toml
  [[services]]
  id = "ps"
  preset = "gcloud-pubsub"   # built-in gRPC emulator → PUBSUB_EMULATOR_HOST
  ```
- `preset` (external emulators) — `gcloud-firestore`, `gcloud-datastore`,
  `gcloud-bigtable`, and `gcloud-spanner` wrap the GCP `gcloud beta emulators`
  family. Native needs gcloud +
  Java + the gcloud component; `runtime = auto` falls back to the cloud-cli
  Docker image (Spanner uses its own image) when the component is missing.
  Each exports the well-known host var (e.g. `FIRESTORE_EMULATOR_HOST`).
  `preset = "firebase"` is the Firebase Emulator Suite bundle: it requires a
  `firebase.json`, runs `firebase emulators:start`, and exports each configured
  emulator's `*_EMULATOR_HOST` (native-only — no Docker fallback).
- `preset = "lumen"` — a versioned native Lumen service. Set
  `version = "lumen@X.Y.Z"` to pin a release; omit it to resolve the newest
  `lumen@*` release. VAT downloads the target-native archive into its own cache,
  verifies a published checksum when present, starts `lumen serve` on loopback,
  waits for `/readyz`, and exports `LUMEN_URL`. It never replaces a global
  `lumen` installation and rejects Docker/MicroVM runtimes. Lumen state is
  ephemeral for the VAT run: no source build, import, seed, or persistence is implied.
- `image` — an OCI image dependency that has no native equivalent (e.g.
  AlloyDB). Requires `container_port`; `image_env` is passed into the container;
  `runtime = "docker"` uses Docker, while explicit `runtime = "micro_vm"` uses
  Apple Container with the same bounded inspect/pull/verify preflight and no
  Docker fallback. Explicit `runtime = "native"` runs a `darwin/arm64` image
  from the native store (pulled first if missing) as
  `vat container run --rm` on the host network. There is no port mapping, so
  the workload must listen on `container_port` (also passed as `PORT`), and a
  fixed `port` must equal it. In `export`, `{host}`/`{port}` resolve to the
  mapped host endpoint and `VAT_SERVICE_<ID>_{HOST,PORT}` are always exported.
- `external` — an already provisioned endpoint, such as a GitLab CI `services:`
  sidecar, GitHub Actions service container, local Docker Compose service, or
  host daemon. vat does not start or stop it; it waits for readiness, substitutes
  `{host}`/`{port}` in `ready_http`, `ready_cmd`, and `export`, injects
  `VAT_SERVICE_<ID>_{HOST,PORT}`, and records `owned_by_vat = false` in
  `vat state`.
- `cluster` — an ephemeral local Kubernetes cluster, for testing K8s-native
  targets. `auto` picks the first installed of kind → k3d → minikube (all need
  Docker on Apple Silicon); `kind`/`k3d`/`minikube` force one. Optional
  `k8s_version` and `nodes`. vat creates the cluster before the runner with an
  isolated kubeconfig (it never touches `~/.kube/config`), exports `KUBECONFIG`
  (the `{kubeconfig}` token) and `VAT_SERVICE_<ID>_KUBECONFIG`, probes readiness
  with `kubectl get nodes`, and deletes it at teardown per the `keep` policy. A
  missing backend fails with a structured `cluster_backend_unavailable` error
  (never a panic). `vat cluster` manages clusters standalone, outside a run.
- `cmd` — an explicit native command. When the command owns an IPv4 endpoint on
  literal `127.0.0.1` (through `port`, `{host}`/`{port}`, or a fixed
  `127.0.0.1` `ready_http`), vat reserves that exact endpoint for the run
  through preparation and releases it only at the child-spawn boundary.
  `localhost`, `::1`, and other loopback spellings are rejected because they
  cannot be proven by the same exact IPv4 reservation. An already occupied
  endpoint fails closed with the exact service and endpoint; vat never treats
  the existing listener as the owned child's readiness. After spawn, both the
  owned child and the endpoint's unavailable-to-ready transition must remain
  valid before a dependent runner starts. Declare
  `external = { host = "...", port = ... }` when attaching to an intentionally
  pre-existing listener.

Env export contract:

| Service backing | Default exports | `export` map semantics | Raw service vars |
|---|---|---|---|
| `preset` datastore/broker | postgres/mysql → `DATABASE_URL`; redis → `REDIS_URL`; nats → `NATS_URL`; rabbitmq → `AMQP_URL`; mongo → `MONGODB_URI`; opensearch → `OPENSEARCH_URL` | Value containing `{host}`/`{port}` uses the map key as the env var name; otherwise the value is a legacy alias name receiving the default URL. | `VAT_SERVICE_<ID>_HOST`, `VAT_SERVICE_<ID>_PORT` |
| `preset` built-in emulator | `PUBSUB_EMULATOR_HOST`, `FIREBASE_AUTH_EMULATOR_HOST`, `CLOUD_TASKS_EMULATOR_HOST`, `CLOUD_SCHEDULER_EMULATOR_HOST`, `CLOUD_WORKFLOWS_EMULATOR_HOST`, `STORAGE_EMULATOR_HOST`, `VAT_HTTP_MOCK_HOST`, or `OPENAPI_MOCK_HOST` | Same template/alias rule as other presets. `STORAGE_EMULATOR_HOST` includes `http://`; the others are host:port unless documented by the service. | `VAT_SERVICE_<ID>_HOST`, `VAT_SERVICE_<ID>_PORT` |
| `preset` Lumen | `LUMEN_URL=http://127.0.0.1:<port>` | Same template/alias rule as other presets. | `VAT_SERVICE_<ID>_HOST`, `VAT_SERVICE_<ID>_PORT` |
| `image` | none | Key is always the env var name; value may use `{host}`/`{port}`. | `VAT_SERVICE_<ID>_HOST`, `VAT_SERVICE_<ID>_PORT` |
| `external` | none | Key is always the env var name; value may use `{host}`/`{port}` from the attached endpoint. | `VAT_SERVICE_<ID>_HOST`, `VAT_SERVICE_<ID>_PORT`; state records `owned_by_vat = false` |
| `cmd` | `VAT_SERVICE_<ID>_URL` when `ready_http` exists and no custom export is set | Value containing `{host}`/`{port}` uses the map key as the env var name; otherwise the value aliases `ready_http`. | `VAT_SERVICE_<ID>_HOST`, `VAT_SERVICE_<ID>_PORT` only when the command needs/allocates a port |
| `cluster` | `KUBECONFIG` | `{kubeconfig}` expands to the isolated kubeconfig path. | `VAT_SERVICE_<ID>_KUBECONFIG` |

Runner scripts can detect a configured vat run with `VAT_WORKSPACE_BASE`; it is
set for `vat.toml` runner and scenario modes and points at the source workspace
that vat cloned. When `vat run --plan <path>` is used, vat copies that opaque
plan into the rootfs, injects `VAT_PLAN_PATH` and `VAT_PLAN_DIGEST`, and records
the plan evidence in `vat state`; vat never interprets the plan semantics.

For the native path vat checks for required binaries, cold-prepares cached
service data when needed, and clones it on later runs. Native preset and
endpoint-bearing `cmd` services reserve their exact `127.0.0.1` endpoints until
spawn, then require the owned child to stay live while the endpoint becomes
ready; a probe response from a pre-existing listener cannot certify ownership.
The Docker path runs an
ephemeral `docker run --rm` container bound to loopback; the explicit MicroVM
path runs an ephemeral Apple `container run --rm` service after the bounded
image preflight, with stricter host-port readiness evidence. Both are removed at
teardown. For the `external` path vat treats the surrounding environment as the
lifecycle owner and only records/probes the endpoint. The **runner itself is
never containerized**, so the host GPU is untouched. Managed paths auto-allocate
ports, every path exports runner env vars, and vat reports only a few JSONL
checkpoints unless the agent asks for logs/state/diff.
A Docker-backed service with no reachable daemon fails with a structured
`docker_unavailable` error rather than a panic.

On macOS, connection-heavy runners against native TCP presets can hit the host
accept-backlog ceiling (`kern.ipc.somaxconn`, often 128) and see intermittent
`ECONNREFUSED` even while Redis/Postgres/etc. are still running. vat surfaces the
Redis startup warning as a structured `hint` event. Prefer connection pooling in
the app under test, or raise the host limit for the session, for example
`sudo sysctl -w kern.ipc.somaxconn=1024`, then rerun vat.

## Network sandbox

An optional `[network]` block turns a run into a confined, hermetic environment —
on macOS with **no VM** (Apple Seatbelt + the http-mock proxy), so the host GPU
stays untouched.

```toml
[network]
egress = "localhost-only"   # open (default) | localhost-only | deny

# Transparent service routing: a real host → a local target. Auto-derived for
# declared GCP emulator presets, so you usually don't write these by hand.
[[network.routes]]
host = "cloudtasks.googleapis.com"
target = "http://127.0.0.1:8123"   # or a local emulator's host:port
```

- **Transparent routing** (`[network].routes`, needs an `http-mock` service):
  an outbound request to a known host is served by a local emulator/mock instead
  of the real service, with **zero app code change**. Works for **HTTP/REST**
  (resolution `route > stub > openapi > cassette > forward`) **and gRPC** (the
  CONNECT MITM negotiates ALPN h2 and stream-reverse-proxies routed gRPC, trailers
  preserved, to the emulator's h2c port). Declaring a GCP emulator preset
  (`cloud-tasks`, `cloud-scheduler`, …) plus an `http-mock` service auto-adds the
  route from its real `*.googleapis.com` host to the local emulator.
- **Egress policy** (`[network].egress`, enforced under `--isolation seatbelt`):
  `localhost-only` denies outbound network except loopback (so the run reaches
  only vat's local emulators/proxy); `deny` blocks all outbound; `open` (default)
  is unrestricted. Reads stay open and the GPU is untouched. Applies to both
  direct (`vat run -- cmd`) and runner (`vat run <runner>`) commands. Regular
  runner-mode services keep their network; `vat run --scenario <id>` with
  `network = "hermetic"` also wraps direct-start app/dependency services while
  leaving Docker/image, cluster, and preset service backends on their native
  local-service path. With `--isolation none` a non-`open` policy warns that
  confinement needs seatbelt.
- **Fully hermetic**: when `egress` is `localhost-only`/`deny`, vat also runs the
  `http-mock` proxy in **no-forward** mode — an unmatched request returns
  `502 hermetic: … forwarding disabled` instead of reaching the internet. Net:
  the runner is confined to localhost *and* the proxy refuses upstream, so the run
  is fail-closed (routes/stubs/OpenAPI/cassette-replays still serve).

> Seatbelt enforcement uses `sandbox-exec` (Apple-deprecated but functional; the
> [`Sandbox`] trait keeps a future Endpoint Security backend local). Routing/egress
> only catch proxy-honoring / loopback-confined clients — non-cooperating egress is
> *blocked* (fail-closed), not transparently rerouted.

[`Sandbox`]: src/sandbox/mod.rs

## Supporting documents

| Document | What it answers |
|---|---|
| [STATUS.md](STATUS.md) | Which surfaces are Supported, Limited, or Not supported today, with the gate or E2E that proves each row. |
| [ROADMAP.md](ROADMAP.md) | The owner-confirmed milestone order for the three pillars (M1–M5), the uncommitted later outcomes, and the non-goals. |
| [docs/product/architecture.md](docs/product/architecture.md) | How the native runtime, the shared Linux VM, the Docker Engine API, and local GKE fit together, including the open spikes. |
| [CONTRIBUTING.md](CONTRIBUTING.md) | How to change vat and which gate a change must pass. |
