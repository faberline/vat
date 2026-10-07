// CODEGEN-BEGIN
//! `vat llm` — compact agent-facing usage contract.

use std::process::ExitCode;

use anyhow::Result;

/// Stable guide text intended for LLM/tool agents.
const GUIDE: &str = r#"# vat LLM Guide

vat is a local, ephemeral agent test runner. Use it to prepare a real local
workspace, run one command or one named vat.toml runner, and inspect structured
evidence afterward.

## First Choice

- If the project has `vat.toml`, prefer plain `vat run`.
- If `vat.toml` declares `[[scenarios]]` for an app-under-test, use
  `vat run --scenario <id>`.
- Use `vat run <runner-id>` only when you need a non-default runner.
- Use `vat capabilities --json` to inspect this host's effective substrate:
  COW clone method, isolation backends, Docker provider/daemon state,
  service-provider capabilities, and the Apple Container shared-builder
  advisory. This is a full host probe and retains its normal Docker daemon
  probe: `services.docker_services` is `available` or `unavailable` from that
  conclusive full probe. The builder record is bounded and read-only: `container builder
  status` reports `ownership=shared_unknown` and `automatic_cleanup=false`;
  parseable configured resources are distinct from optional live
  `observed_stats`, and optional `container system df` is global host evidence,
  not VAT-owned disk. Unsupported, malformed, or timed-out status/stats/df is
  nonfatal advisory `unknown`/`probe_errors`; VAT never starts, stops, deletes,
  or prunes the shared builder/cache. A real live state appears only when the
  installed Apple Container CLI supports and returns it.
- Use `vat plan --json` to inspect selected runners, services, env keys, and
  artifacts without creating a vat or starting services.
- Use `vat doctor --json` for cheap selected-plan preflight before a CI/local
  run. An explicit MicroVm/Apple-Container-only selected plan performs exactly
  one read-only `container system status` probe per invocation and projects it
  to its selected MicroVm services; it does not execute Docker even when
  Docker is on `PATH`, and JSON reports
  `docker.daemon_probe.state=skipped` with the truthful reason
  `Docker daemon probe skipped for Apple-Container-only selected plan`; in that
  deliberate no-probe state `services.docker_services=not_probed`.
  `docker.daemon=false` is not Docker-unavailable evidence because no Docker
  command ran. Unselected Docker services do not affect that
  runner. Docker runtime, Auto image, and eligible Auto preset fallback plans
  retain the Docker probe; a `cluster = "machine"` service instead reports the
  machine K3s state (as `vat k8s status` does).
  Doctor neither autostarts Apple Container nor falls back to Docker;
  unsupported MicroVm presets without a declared OCI route and MicroVm preset
  named volumes fail closed. Its shared-builder result is advisory only, so a
  builder timeout/unknown/probe error never changes the runtime success result.
- If an upstream planner/TIA tool selected tests, pass the opaque file with
  `vat run --plan impact.json <runner-id>`; vat copies and records it but does
  not interpret test-selection semantics.
- If you only need one ad-hoc command, use `vat run -- <command>`.
- `vat run` prints sparse JSONL checkpoints; the final line has
  `"type":"result"`.
- SIGINT/SIGTERM cleanup is owned by the running VAT process. The first signal
  wins; VAT stops runners first, then VAT-owned services in reverse order with
  bounded TERM/grace/KILL/reap/PGID-absence proof. It persists terminal
  `interrupted` state with no child PID and exits 130/143. Interrupted evidence
  is retained for `vat state`/`vat gc`; explicit `external` services and
  unrelated listeners are never signalled.
- After a retained run, inspect `vat state <id>`, `vat diff <id>`, and
  `vat logs <id> [runner|service-id]`.
- Use `vat fork <id> [--name N]` to branch a retained vat into a new runnable
  copy that carries its lineage, and `vat snapshot <id> [--name N]` to freeze
  one into an immutable, non-runnable point-in-time copy.
- Use `vat gpu --json` to report the GPU every vat on this host can reach.
- For an Apple-native container (darwin/arm64 Mach-O workloads, no Linux VM,
  full Metal), use `vat image build|pull|push|import|export|ls|inspect|tag|rm`
  and `vat container run|ps|logs|exec|stop|rm|inspect|diff`. Images live in an
  OCI store under `~/.vat/native`; each container gets a clonefile copy-on-write
  root under a fixed 128-byte path, with writes confined by seatbelt. There is
  no chroot: the workload sees host paths and resolves its own files through
  `$VAT_ROOT`. It does not use Docker or a Linux VM.
- For Kubernetes, use the persistent K3s cluster in VAT's machine:
  `vat k8s up [--api-port P] [--timeout S] [--json]` enables K3s (booting the
  machine when it is stopped), waits for the API, writes `~/.vat/kube/config`
  (context `vat`), and installs a pinned kubectl at `~/.vat/bin/kubectl`. Then
  run `vat k8s kubectl -- get nodes` (or point any kubectl at that
  kubeconfig). `vat k8s status|kubeconfig|down [--json]` report, refresh, or
  disable it. Cluster state, PVCs, and the kubeconfig survive machine
  restarts. K3s uses the machine's Docker Engine as its container runtime, so
  images built with `docker build` against VAT's engine are visible to pods
  without a registry push.
- For Docker, use VAT's Docker Engine: run `vat machine start`, then the
  stock `docker` CLI (including `docker compose`) works against it. VAT sets
  `DOCKER_HOST=unix://~/.vat/run/docker.sock` for its own child processes
  (`vat run`, `vat build`, compose runners, capability probes); to use the
  engine from your own shell, export that variable yourself. An explicit
  `DOCKER_HOST` or `DOCKER_CONTEXT` wins, and `VAT_ENGINE=external` opts out.
- Use `vat --help` for flag syntax and `vat <command> --help` for command flags.

## vat.toml Contract

```toml
version = 1
default_runner = "e2e"

[workspace]
base = "."
workdir = "."
keep = "failed" # failed | always | never

[network]
egress = "open" # open | localhost-only | deny

[[services]]
id = "pg"
preset = "postgres"        # native binary preferred; Docker image fallback
# runtime = "auto"         # auto (default) | native | docker | micro_vm
seed = ["schema.sql", "fixtures.sql"]
export = { DATABASE_URL = "DATABASE_URL" }

[[services]]
id = "alloy"               # OCI image dependency (no native binary)
image = "google/alloydbomni:latest"
runtime = "micro_vm"       # explicit Apple Container route; never falls back to Docker
container_port = 5432
image_env = { POSTGRES_PASSWORD = "pw" }
export = { ALLOY_URL = "postgres://postgres:pw@{host}:{port}/postgres" }

[[services]]
id = "ci-pg"               # already started by GitLab CI services / Compose
external = { host = "postgres", port = 5432 }
export = { DATABASE_URL = "postgres://postgres@{host}:{port}/app" }

[[services]]
id = "k8s"                 # per-run namespace on the machine's K3s (`vat k8s up`)
cluster = "machine"        # the only backend; k8s_version / nodes are rejected
export = { TEST_NAMESPACE = "{namespace}" }  # KUBECONFIG is exported anyway

[[services]]
id = "fs"                  # gcloud Firestore emulator (exports FIRESTORE_EMULATOR_HOST)
preset = "gcloud-firestore" # gcloud-firestore | gcloud-datastore | gcloud-bigtable | gcloud-spanner

[[services]]
id = "web"                 # app under test; {port} is auto-allocated
cmd = ["pnpm", "run", "dev", "--", "--host", "127.0.0.1", "--port", "{port}"]
ready_http = "http://127.0.0.1:{port}/"
export = { APP_URL = "APP_URL" }

[[services]]
id = "http"
preset = "http-mock"       # required by hermetic scenarios

[[runners]]
id = "e2e"
requires = ["pg"]
cmd = ["pnpm", "run", "test:e2e"]
artifacts = ["test-results/**", "playwright-report/**"]

[[scenarios]]
id = "prod-like"
app = "web"
requires = ["pg", "http"]
runner = "e2e"
network = "hermetic"       # open | hermetic
```

## Services: native, Docker, or explicit Apple Container, plus external sidecars

- A `preset` service prefers the native Homebrew binary and falls back to the
  preset's official Docker image when the binary is missing. Force it with
  `runtime = "native"` or `runtime = "docker"`. Explicit
  `runtime = "micro_vm"` never falls back to Docker: for presets with a declared
  OCI route it checks Apple's image store, emits `image_pull` and performs a
  bounded pull when needed, re-verifies the image, then runs Apple `container`
  with loopback port readiness. Unsupported presets and MicroVM preset named
  volumes fail closed. Datastore/broker presets: postgres, redis, nats,
  rabbitmq, mysql, mongo.
- Emulator presets: `firestore`, `pubsub`, `datastore`, `bigtable`, `spanner`
  wrap the GCP `gcloud beta emulators` family (native needs gcloud + Java + the
  gcloud component; `runtime = auto` falls back to the cloud-cli Docker image —
  Spanner uses its own image — when the component is missing). Each exports the
  well-known host var (`FIRESTORE_EMULATOR_HOST`, `PUBSUB_EMULATOR_HOST`,
  `DATASTORE_EMULATOR_HOST`, `BIGTABLE_EMULATOR_HOST`, `SPANNER_EMULATOR_HOST`).
- `preset = "firebase"` is the Firebase Emulator Suite bundle: it requires a
  `firebase.json` in the workspace, runs `firebase emulators:start`, and exports
  each configured emulator's host var (`FIRESTORE_EMULATOR_HOST`,
  `FIREBASE_AUTH_EMULATOR_HOST`, `FIREBASE_DATABASE_EMULATOR_HOST`,
  `FIREBASE_STORAGE_EMULATOR_HOST`, `PUBSUB_EMULATOR_HOST`,
  `FIREBASE_EMULATOR_HUB`). It is native-only (firebase-tools + Java); there is
  no Docker fallback for firebase.
- An `image` service is an OCI dependency with no native binary (e.g. AlloyDB).
  `runtime = "docker"` uses Docker; explicit `runtime = "micro_vm"` uses Apple
  Container's bounded inspect/pull/verify preflight and never silently invokes
  Docker. It requires `container_port`; `image_env` is passed into the
  container; in `export`, `{host}`/`{port}` resolve to the mapped host endpoint,
  and `VAT_SERVICE_<ID>_{HOST,PORT}` are always exported.
- `runtime = "native"` on an `image` service runs a darwin/arm64 image from the
  native store (`vat image build|pull|import` first) as a seatbelt-confined
  native container for the run, then removes it. It shares the host network,
  so `port` (when set) must equal `container_port`; `{host}`/`{port}` in
  `ready_http` and `export` resolve to `127.0.0.1` and that port. It never
  invokes Docker or Apple Container.
- An `external` service is an already provisioned endpoint, such as a GitLab CI
  `services:` sidecar, GitHub Actions service container, local Docker Compose
  service, or host daemon. vat does not start or stop it; it waits for readiness,
  substitutes `{host}`/`{port}` in `ready_http`, `ready_cmd`, and `export`,
  injects `VAT_SERVICE_<ID>_{HOST,PORT}`, and records `owned_by_vat = false` in
  `vat state`.
- A `cmd` service is VAT-owned. For an IPv4 endpoint on literal `127.0.0.1`
  declared through `port`, `{host}`/`{port}`, or a fixed `127.0.0.1`
  `ready_http`, vat holds an exact run-scoped reservation through preparation
  and releases it only at the spawn boundary. `localhost`, `::1`, and other
  loopback spellings are rejected because they are not the same exact IPv4
  endpoint. An occupied endpoint fails closed with the exact service/endpoint;
  after spawn, the owned child must stay live and the endpoint must transition
  to ready before any runner starts. Use `external`, not `cmd`, to attach to an
  intentionally existing listener.
- Built-in emulators: `preset = "gcloud-pubsub"`, `"firebase-auth"`, `"gcloud-cloud-tasks"`,
  `"cloud-scheduler"`, `"cloud-workflows"`, and `"cloud-storage"` run vat's OWN
  in-process Rust emulator under `runtime = auto` — no gcloud, Java,
  firebase-tools, or Docker, and instant start. They export
  `PUBSUB_EMULATOR_HOST` / `FIREBASE_AUTH_EMULATOR_HOST` /
  `CLOUD_TASKS_EMULATOR_HOST` / `CLOUD_SCHEDULER_EMULATOR_HOST` /
  `CLOUD_WORKFLOWS_EMULATOR_HOST` / `STORAGE_EMULATOR_HOST` — point your client's
  base URL at `http://$HOST` for host:port vars; `STORAGE_EMULATOR_HOST` already
  includes `http://` because the GCS REST SDK expects a schemed endpoint.
  `cloud-tasks` (v2 REST) delivers each task's httpRequest to its
  target at scheduleTime (or `tasks/{t}:run`); `cloud-scheduler` (v1 REST) fires
  a job's httpTarget on its cron schedule or `jobs/{j}:run`; `cloud-workflows`
  (v1 REST) runs a subset Workflows interpreter whose `call: http.*` steps can
  orchestrate the other emulators; `cloud-storage` (GCS JSON API v1) is an
  in-memory object store (bucket CRUD, media/multipart upload, `alt=media`
  download, list, delete); `http-mock` is a transparent HTTP stub + record/replay
  proxy with HTTPS MITM — `preset = "http-mock"` exports `HTTP(S)_PROXY` + a
  CA-trust bundle so the runner's outbound third-party API calls (even hardcoded
  `https://`) are intercepted with no code change. Register stubs at
  `$VAT_HTTP_MOCK_HOST/__admin/stubs`; unstubbed calls record once then replay
  offline. `openapi` reads an OpenAPI document (`preset = "openapi"`,
  `spec = "api.yaml"`) and serves spec-derived responses (example, else a
  schema-synthesized body; path templating + `$ref`) — a working fake of a
  documented API with no stubs or recording. It runs standalone (the runner points
  its base URL at `$OPENAPI_MOCK_HOST`) and also backs the http-mock proxy:
  `POST $VAT_HTTP_MOCK_HOST/__admin/openapi` registers a spec for a host, so a
  proxied `https://` call is answered from the contract (resolution: stub >
  openapi > cassette > forward). `pubsub` still accepts `runtime = native`
  (gcloud) / `runtime = docker` (image) as a fidelity fallback; the others are
  built-in only (no official emulator exists).
- Pointing a client at `cloud-tasks` / `cloud-scheduler`: unlike `pubsub` /
  `firebase-auth` / `firestore` / GCS (whose SDKs auto-read their host var), the
  official Cloud Tasks / Cloud Scheduler SDKs do NOT read
  `CLOUD_TASKS_EMULATOR_HOST` / `CLOUD_SCHEDULER_EMULATOR_HOST` (Google ships no
  emulator) and default to gRPC, while vat serves REST — so an env/DNS host
  redirect fails. Build the client through one factory that, when the host var is
  set, forces the REST transport, an `http://$HOST` endpoint, and anonymous
  credentials. Python: `CloudTasksClient(transport="rest",
  credentials=AnonymousCredentials(), client_options={"api_endpoint":
  f"http://{host}"})`. Node: `new CloudTasksClient({fallback:'rest', apiEndpoint,
  port, protocol:'http'})`. Or skip the SDK and POST the v2 REST API directly
  (see `tests/vat_emulator_tasks.rs`).
- Removing mocks: declare the emulator presets your code touches (the runner hits
  real local services), add `http-mock` for arbitrary third-party HTTP, and
  `openapi` to fake a documented API from its spec — tests then need no
  hand-rolled service or HTTP-client mocks.
- Production-like scenarios: declare `[[scenarios]]` when you want vat to start
  the app-under-test plus dependencies and then run a test runner against it.
  `network = "hermetic"` requires a participating `preset = "http-mock"` service,
  sets localhost-only egress, defaults the run to seatbelt isolation, wraps
  direct-start app/dependency services, and records `test_run.scenario` topology
  in `vat state`.
- A `cluster = "machine"` service gives a run its own namespace on the
  machine's persistent K3s. Before the runner starts, vat brings the cluster up
  through the same path as `vat k8s up` (a cold machine boot may take minutes),
  creates namespace `<run-id>-<service-id>` labelled `vat.dev/run`, writes a
  per-run kubeconfig in the run's state dir whose `vat` context selects that
  namespace, and exports `KUBECONFIG`, `VAT_K8S_NAMESPACE`, and
  `VAT_SERVICE_<ID>_KUBECONFIG` (export templates may use `{kubeconfig}` and
  `{namespace}`). Readiness waits for the namespace's default ServiceAccount.
  At teardown vat deletes the namespace (`--wait=false`) when the `keep`
  policy removes the run; a kept run keeps it for `kubectl` diagnosis. The
  cluster itself is shared and persistent and is never deleted by a run.
  `k8s_version` and `nodes` are rejected: the machine cluster is one pinned
  K3s release on a single node. Failures emit structured `cluster_up_failed`
  or `cluster_namespace_failed` errors (no panic).
- Docker-backed services need a reachable Docker daemon; vat emits a structured
  `docker_unavailable` error (no panic) when it is missing. The runner itself is
  never containerized.
- Env export contract:

  | Service backing | Default exports | `export` map semantics | Raw service vars |
  |---|---|---|---|
  | `preset` datastore/broker | postgres/mysql -> `DATABASE_URL`; redis -> `REDIS_URL`; nats -> `NATS_URL`; rabbitmq -> `AMQP_URL`; mongo -> `MONGODB_URI`; opensearch -> `OPENSEARCH_URL` | If the value contains `{host}`/`{port}`, the key is the env var name; otherwise the value is a legacy alias receiving the default URL. | `VAT_SERVICE_<ID>_HOST`, `VAT_SERVICE_<ID>_PORT` |
  | `preset` built-in emulator | `PUBSUB_EMULATOR_HOST`, `FIREBASE_AUTH_EMULATOR_HOST`, `CLOUD_TASKS_EMULATOR_HOST`, `CLOUD_SCHEDULER_EMULATOR_HOST`, `CLOUD_WORKFLOWS_EMULATOR_HOST`, `STORAGE_EMULATOR_HOST`, `VAT_HTTP_MOCK_HOST`, or `OPENAPI_MOCK_HOST` | Same template/alias rule. `STORAGE_EMULATOR_HOST` includes `http://`; most other host vars are host:port. | `VAT_SERVICE_<ID>_HOST`, `VAT_SERVICE_<ID>_PORT` |
  | `image` | none | Key is always the env var name; value may use `{host}`/`{port}`. | `VAT_SERVICE_<ID>_HOST`, `VAT_SERVICE_<ID>_PORT` |
  | `external` | none | Key is always the env var name; value may use `{host}`/`{port}` from the attached endpoint. | `VAT_SERVICE_<ID>_HOST`, `VAT_SERVICE_<ID>_PORT`; state records `owned_by_vat = false` |
  | `cmd` | `VAT_SERVICE_<ID>_URL` when `ready_http` exists and no custom export is set | Template values use the key as env var name; otherwise the value aliases `ready_http`. | `VAT_SERVICE_<ID>_HOST`, `VAT_SERVICE_<ID>_PORT` only when a port is allocated |
  | `cluster = "machine"` | `KUBECONFIG`, `VAT_K8S_NAMESPACE` | Key is always the env var name; `{kubeconfig}` expands to the per-run kubeconfig path and `{namespace}` to the run namespace. | `VAT_SERVICE_<ID>_KUBECONFIG` |

- Runner scripts can detect configured vat runner/scenario mode with
  `VAT_WORKSPACE_BASE`; it points at the source workspace that vat cloned.
- `vat run --plan <path>` copies the opaque plan into the rootfs, injects
  `VAT_PLAN_PATH` and `VAT_PLAN_DIGEST`, and records the same evidence in
  `vat state`. The wrapped app/test tool owns the plan semantics.
- macOS native TCP presets can hit `kern.ipc.somaxconn` under connection churn
  and produce intermittent `ECONNREFUSED` even while the service is up. vat emits
  a structured `hint` when a service log reports that backlog cap. Prefer app
  connection pooling or raise the host limit, e.g.
  `sudo sysctl -w kern.ipc.somaxconn=1024`.

## Isolation and Egress

- `--isolation none|seatbelt` (also `vat.toml`-less default: `none`) picks the
  sandbox backend. `none` runs the command as a plain host process confined
  only by the copy-on-write rootfs — full native GPU/IO, zero syscall
  confinement. `seatbelt` wraps it in a macOS `sandbox-exec` profile that
  confines writes to the rootfs + temp and can enforce `[network].egress`;
  Metal still works because it's still a host process.
- `[network].egress` (or per-scenario `network = "hermetic"`, which implies
  `localhost-only`) is `open` (default, no restriction), `localhost-only`
  (deny outbound except loopback + unix sockets — vat's local
  emulators/http-mock proxy stay reachable), or `deny` (block all outbound,
  including localhost).
- Egress enforcement fails closed, not silently: picking a backend that
  cannot actually enforce a non-`open` egress policy is a hard error, not a
  warn-and-continue. `--isolation none` with `[network].egress` set to
  anything but `open` refuses to run. `--isolation seatbelt` with
  `sandbox-exec` unavailable on the host and a non-`open` policy also refuses
  to run, rather than silently falling back to the unconfined `none` backend;
  it only falls back when the policy is already `open`.
- This applies uniformly to both direct-command mode (`vat run -- <cmd>`) and
  runner-mode `vat.toml` commands — a declared runner cannot bypass the
  spec's isolation/egress policy.
- Native containers (`vat container run`, `runtime = "native"` services) are
  always seatbelt-confined: writes are limited to the container root, temp, and
  explicit `-v` mounts (`:ro` mounts are read-only); reads are not restricted.
  Seatbelt is a write/egress policy, not a hostile-code boundary, and there is
  no chroot. `vat container inspect` reports `uid_isolation`: it is
  `unavailable` unless vat runs as root with a root-created user pool
  (`vat native users`), so by default the workload runs as the invoking user.
  Do not claim per-container UID isolation from a non-root run.

## Command Patterns

- `vat run`: select the default runner, prepare or clone service images, start
  required services, wait for readiness, run the runner, capture evidence, stop
  services, and return the runner exit code.
- `vat run --scenario prod-like`: start the named scenario's app service,
  scenario deps, and runner deps, then run its selected runner.
- `vat run e2e`: explicitly run the `e2e` runner.
- `vat run --keep always e2e`: override `[workspace].keep` for one invocation so
  a passing probe run remains inspectable via `vat logs` / `vat state`.
- `vat capabilities --json`: full host backend/isolation/Docker/service
  discovery without requiring vat.toml; it keeps the normal Docker daemon
  probe, so `services.docker_services` is conclusively `available` or
  `unavailable`, and adds a bounded read-only Apple Container shared-builder advisory.
  `builder status` gives shared ownership (`shared_unknown`) and no automatic
  cleanup; supported configuration, optional live stats, and host-global disk
  observations remain distinct. A timeout, unsupported output, or probe error
  is nonfatal advisory evidence; VAT never mutates the builder/cache.
- `vat plan --json [e2e]`: print the selected configured topology without side
  effects.
- `vat doctor --json [e2e]`: check only the selected topology's host
  prerequisites without running app/tests. An explicit MicroVm/Apple-Container
  plan probes read-only `container system status` exactly once per invocation,
  never Docker even if it is on `PATH`, and returns
  `docker.daemon_probe.state=skipped` with a selected-plan reason and
  `services.docker_services=not_probed`—not unavailable. `docker.daemon=false`
  has no unavailable meaning there because no Docker command ran. An unselected Docker service is
  irrelevant; Docker runtime, Auto image, and eligible Auto preset fallback
  plans retain Docker probing. A `cluster = "machine"` service reports the
  machine K3s state instead of probing host Docker.
  Doctor does not autostart Apple Container or fall back to Docker: unsupported
  MicroVm presets without an OCI route and MicroVm preset named volumes fail
  closed. The separate shared-builder advisory may report timeout/unknown/error
  but never changes the runtime success result.
- `vat run --plan impact.json impacted`: expose an upstream plan to the runner
  through `VAT_PLAN_PATH` / `VAT_PLAN_DIGEST` and preserve plan evidence.
- `vat run -- cargo test -p app`: run one direct command without requiring
  vat.toml; the child exit code is forwarded.
- `vat logs <id> runner`: print retained runner stdout/stderr.
- `vat logs <id> <service-id>`: print retained service stdout/stderr.
- `vat state <id>`: read the agent-legible JSON state.
- `vat diff <id> --json`: read filesystem changes vs. the vat base.
- `vat gc --json`: dry-run retained workspace cleanup and report candidates
  without deleting anything.
- `vat gc --measure --json`: include `du -sk` disk sizes; omit it for fast
  metadata-only cleanup planning on huge stores.
- `vat gc --execute --keep-last 5`: prune old successful/created vats while
  preserving running, snapshot, failed, and newest retained vats.
- `vat k8s up [--api-port P] [--timeout S] [--json]`: enable the persistent K3s
  cluster in VAT's machine (booting the machine when stopped), wait for the
  API, write `~/.vat/kube/config` (context `vat`), and install the pinned
  kubectl. `--api-port` picks the host API port (default 6443; remembered).
- `vat k8s status [--json]`: report cluster state, API forwarding, kubeconfig,
  and kubectl.
- `vat k8s kubeconfig [--json]`: refresh and print the host kubeconfig path.
- `vat k8s kubectl -- <args>`: run the pinned kubectl against the cluster.
- `vat k8s down [--json]`: disable K3s and stop its pods; cluster state stays
  on the machine's disk for the next `vat k8s up`.
- `vat fork <id> [--name N]`: copy-on-write fork a retained vat's rootfs into a
  new runnable vat that records the source as its lineage; the fork is
  independent afterward (writes to one do not affect the other).
- `vat snapshot <id> [--name N]`: freeze a retained vat's rootfs into an
  immutable snapshot for later inspection or forking; a snapshot itself is not
  runnable.
- `vat gpu --json`: report the GPU(s) every vat on this host can reach,
  independent of any specific vat or run.
- `vat image build -t NAME:TAG DIR`: build a darwin/arm64 image from a
  Dockerfile subset (single-stage FROM scratch or a stored image, COPY without
  wildcards or flags, RUN, ENV, WORKDIR, CMD/ENTRYPOINT, EXPOSE, LABEL; no ADD,
  USER, ARG, or `.dockerignore`); Mach-O files are ad-hoc re-signed after
  relocation. `vat image pull|push REF` talks to an OCI
  registry using `~/.docker/config.json` credentials (Basic or Bearer);
  `vat image import|export` moves OCI layout tarballs.
- `vat container run [-d] [--rm] [--name N] [-e K=V] [-v HOST:CONT[:ro]]
  [--network host|none] IMAGE [CMD...]`: run in the foreground (exit code is
  the workload's) or detached; `vat container ps|logs|exec|stop|rm|inspect|diff`
  manage it. `stop [-t SECONDS]` sends SIGTERM to the process group, then
  SIGKILL after the grace period (default 10 s).
- `vat native users ls` reports the hidden `_vatN` UID pool and whether UID
  isolation would be active; `vat native users setup` creates the pool and
  requires root.

## Retention

Default `keep = "failed"` means successful configured runs clean up after
emitting JSON, while failed runs keep workspace state and logs for inspection.
Use `vat run --keep always ...` to retain one passing configured run without
editing `vat.toml`; use `--keep never` to force cleanup.
If retained vats accumulate, run `vat gc --json` first. GC is dry-run by
default, reads metadata only, and requires `--execute` before deleting. Add
`--measure` when `du -sk` disk sizes are needed. Add `--apparent` only when
file-length totals are needed; it walks every retained rootfs. Add
`--include-failed` only when failed debug workspaces are no longer needed.

## Boundaries

- vat is not a general-Compose replacement. Outside `vat machine`, it is not
  a daemon or a long-lived process manager. It is permanently headless:
  GUI/Desktop, dashboard, and tray/menu-bar surfaces are out of scope.
- Docker workloads belong on VAT's Docker Engine (`vat machine start`, above).
- Kubernetes is one persistent single-node K3s cluster in VAT's machine
  (`vat k8s up`), not a multi-node, multi-version, or Desktop-integrated
  cluster manager. `cluster = "machine"` services isolate runs by namespace,
  not by cluster.
- The runner is always a host process (never containerized) — the GPU story.
  Docker is only an option for run-scoped dependency *services*.
- Services in `vat.toml` are run-scoped dependencies of one runner invocation;
  containers are ephemeral (`docker run --rm`) and removed at teardown; external
  services are attached and probed but not lifecycle-managed by vat.
- vat does not schedule production work or manage restart policy.
"#;

const CORE: &str = r#"# VAT core workflow

Use `vat run` for an ad-hoc command or a configured runner. Use `vat plan --json`
before side effects, `vat doctor --json` for selected-topology readiness, and
`vat state`, `vat diff`, or `vat logs` to inspect retained evidence.
"#;

const SERVICES: &str = r#"# VAT services

Declare run-scoped dependencies in `vat.toml`, then inspect the resolved shape
with `vat plan --json`. Built-in emulators include `gcloud-pubsub` and
`gcloud-cloud-tasks`; native Lumen uses `preset = "lumen"` with an optional
`version = "lumen@<version>"`. Use `vat doctor --json` before a service-backed
run. Services are test dependencies, not durable production infrastructure.
"#;

const CONTAINER: &str = r#"# VAT container and compose workflow

Use `vat build` for a local Dockerfile build and `vat compose` for VAT's
documented bounded Compose subset. For a full Docker Engine, run
`vat machine start`; the stock `docker` CLI then works once you export
`DOCKER_HOST=unix://~/.vat/run/docker.sock` (VAT sets it for its own child
processes).

For Apple-native darwin/arm64 containers with no Linux VM, use `vat image
build|pull|push|import|export|ls|inspect|tag|rm` and `vat container
run|ps|logs|exec|stop|rm|inspect|diff`, or an `image` service with
`runtime = "native"` in `vat.toml`. Each container has a clonefile
copy-on-write root and seatbelt-confined writes; there is no chroot, and
`uid_isolation` is `unavailable` unless vat runs as root with a
`vat native users` pool.
"#;

const K8S: &str = r#"# VAT local Kubernetes workflow

Run `vat k8s up` to enable the persistent K3s cluster in VAT's machine, then
`vat k8s kubectl -- get nodes` (kubeconfig `~/.vat/kube/config`, context
`vat`). `vat k8s status`, `kubeconfig`, and `down` manage it. In vat.toml,
`cluster = "machine"` gives each run its own namespace and exports
`KUBECONFIG` and `VAT_K8S_NAMESPACE`. This is not a multi-node or
multi-version cluster manager.
"#;

const TOPICS: &[cli_std::llm::Topic] = &[
    cli_std::llm::Topic {
        id: "core",
        summary: "run, plan, doctor, and inspect one local VAT workflow",
        body: CORE,
    },
    cli_std::llm::Topic {
        id: "services",
        summary: "declare and preflight run-scoped vat.toml dependencies",
        body: SERVICES,
    },
    cli_std::llm::Topic {
        id: "container",
        summary: "use bounded build and compose, or the vat machine Docker Engine",
        body: CONTAINER,
    },
    cli_std::llm::Topic {
        id: "k8s",
        summary: "use the persistent machine K3s cluster and per-run namespaces",
        body: K8S,
    },
    cli_std::llm::Topic {
        id: "guide",
        summary: "complete backward-compatible VAT agent usage contract",
        body: GUIDE,
    },
];

// <HANDWRITE gap="missing-generator:logic" tracker="#1817" reason="DX command-inventory contract and offline guide text are hand-written pending codegen support">
pub fn exec(topic: &str, format: cli_std::llm::Format) -> Result<ExitCode> {
    let out = cli_std::llm::render("vat", crate::VERSION, TOPICS, topic, format)?;
    println!("{out}");
    Ok(ExitCode::SUCCESS)
}
// </HANDWRITE>
// CODEGEN-END
