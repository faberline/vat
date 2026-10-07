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
  runner. Docker runtime, Auto image, eligible Auto preset fallback, and
  selected cluster plans retain the Docker probe (a cluster needs Docker).
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
- For Docker-free, one-command local Kubernetes work on Apple Container, first
  ensure an independently installed `kubectl` is first on `PATH` (VAT rejects
  an OrbStack-provided binary), run `vat k8s ephemeral image build`, then run
  `vat k8s ephemeral run -- kubectl get nodes` (or another host command). VAT
  injects a private `KUBECONFIG`, `VAT_K8S_CACHE_DIR`, and
  `VAT_K8S_API_SERVER` only into that command and deletes them with the exact
  machine after a confirmed create. If create terminal completion is uncertain,
  VAT retains a non-secret recovery marker and fails safely instead of claiming
  cleanup. Its final stdout line is a terminal
  `vat_k8s_ephemeral_result` JSON record, even when the child exit code is
  forwarded unchanged.
- For an agent sequence such as apply → inspect → test → delete, use
  `vat k8s session create --ttl 30m` after the same image build. Keep the
  returned id, then use text `vat k8s session exec --timeout 30 <id> -- kubectl
  ...` or agent JSON `vat k8s session exec --format json --timeout 30 <id> --
  kubectl ...` for each host command and finish with `vat k8s session delete
  <id>`. Omit `--timeout` only when the remaining lease TTL is the intended
  bound; an explicit timeout is 1..=14400 seconds and cannot exceed that TTL.
  Each exec re-inspects the exact backing id and API endpoint and re-verifies
  host API access before injecting the private kubeconfig, rechecks the lease
  at spawn, owns the child process group, and holds the private operation lock
  through cleanup. Normal exit, deadline, or SIGINT/SIGTERM reaps the group;
  VAT removes its private exec marker only after that group is absent. If VAT
  crashes, a starting or live marker blocks later exec, delete, and cleanup
  fail-closed rather than claiming an arbitrary recovered command was stopped.
  JSON emits one `vat.k8s.session.exec.v1` document with separate bounded
  stdout/stderr, the child exit code, no raw-stream replay, and a
  `status --verify-api` next step; its child intentionally has private
  credentials, so it is not an untrusted-child boundary. A session is one-boot
  and lease-bounded, not restart-safe or daemon-managed; `vat k8s session
  cleanup` reclaims expired leases and abandoned creates. The independent-
  kubectl leased real-host E2E passed 1/1 (36 filtered) in 29.97s, including
  text commands, strict JSON exec with `--timeout 30`, status verification, and
  exact delete. It does not establish crash-safe termination or persistence.
- `vat k8s session status <id>` is unchanged and reports non-secret lease and
  exact-machine state only. `vat k8s session status --verify-api <id>` is an
  opt-in bounded proof, not a lifecycle operation: it proceeds only for an
  active, unexpired session with no retained port-forward or exec marker. Under the
  private operation lock it rechecks expiry, proves the exact backing identity,
  endpoint, and private credential material, rechecks expiry immediately before
  one bounded API probe, and reports `api_checked=true`, `api_state=reachable`
  on success. Expired or recovery-marker state returns the non-probing
  `api_checked=false`, `api_state=not_checked`; busy, unavailable, or
  identity-mismatched state fails closed without changing lease or credentials.
  Focused fake coverage passed 4/4 and the precise status unit passed 1/1. The
  independent-kubectl leased E2E passed 1/1 (36 filtered) in 29.97s and includes
  `status --verify-api` after text and strict JSON exec; it is bounded active-
  lease evidence, not persistence or a general API-status guarantee.
- If either one-shot or leased K3s bootstrap fails, VAT renders the original
  error first, then emits only six fixed read-only diagnostics within a
  six-second total and one-second-per-probe budget:
  `guest_install_log`, `guest_k3s_system`, `backing_container_logs`,
  `machine_boot_log`, `machine_inspect`, and `container_system_status`.
  `guest_install_log` is staged non-sensitive installer evidence; private
  kubeconfig/cache and host credentials are excluded. This diagnostic path
  leaves the existing 300-second bootstrap behavior unchanged, does not retry
  or rerun `k3s --version`, introduces no wrapper/recovery command, and still
  runs exact cleanup. The deterministic fake regression passed. The independent-
  kubectl Service-port-forward E2E passed 1/1 (36 filtered) in 49.57s: it loaded
  the local alpine fixture, used an in-pod HTTP probe because BusyBox lacks
  `httpd`, verified the Service endpoint, text and strict one-document JSON
  loopback forwarding to a credential-free host child, confirmed cleanup and
  closed local ports, then deleted the exact active lease. This remains one
  Service-only session; it does not establish persistence or OS-sandbox behavior.
- To use a locally built or already-pulled image without Docker or a registry,
  run `vat k8s session image load <id> <local-image-ref>` before the Kubernetes
  workload. VAT requires exactly one locally inspected `linux/arm64` variant,
  saves it to a private bounded OCI archive, imports it into that lease's
  `k8s.io` namespace, verifies the canonical reference, and removes the host
  and guest archives before reporting success. It accepts no arbitrary tar;
  use `imagePullPolicy: Never` to prove a workload used the local image. The
  opt-in local-image real-host E2E passed 1/1 (36 filtered) in 49.73s: one
  already-local Apple `alpine:3.20` loaded into one active lease, a pod ran it
  with `imagePullPolicy=Never` and emitted its marker log, then exact session
  cleanup completed. This is not registry-pull generality, persistence, GUI,
  or Docker Engine/API evidence.
- To test one active K3s Service from a host assertion without injecting cluster
  credential variables into that assertion's child environment, run text
  `vat k8s session port-forward run <id> service/<name> <remote-port> --
  <host-command>` or the only JSON form `vat k8s session port-forward run
  --format json <id> service/<name> <remote-port> -- <host-command>`. VAT
  accepts only one literal Service selector and starts kubectl with `--address
  127.0.0.1`; `--local-port 0` lets kubectl choose the loopback port. It waits
  for readiness, then gives exactly one foreground host child only
  `VAT_K8S_PORT_FORWARD_HOST`, `_PORT`, `_ADDR`, `_RESOURCE`, and `_NAMESPACE`
  plus a private HOME. VAT strips `KUBECONFIG`, `VAT_K8S_CACHE_DIR`,
  `VAT_K8S_API_SERVER`, `VAT_K8S_EPHEMERAL`, and `VAT_HOME` from that child
  environment. This is credential hygiene, not a same-UID OS sandbox or
  adversarial-child security boundary.
  The host child joins the authenticated kubectl process group, so normal
  cleanup reaps the leader and waits for ordinary cooperative, non-daemonizing
  descendants to be gone; a child that daemonizes or escapes the group is out of
  contract. Each v2 marker carries a CSPRNG private recovery token, and its
  retained 0600 `operation.lock` is `CLOEXEC`, so kubectl/host work cannot keep
  the flock after a SIGKILLed VAT and the next mutating session operation can
  reconcile from the held lock rather than recorded owner-PID liveness. Recovery
  signals only a leader authenticated from that v2 identity
  and forward shape; a missing or changed leader leaves the marker in place and
  fails closed. Before unlinking storage VAT writes a durable `cleaning`
  tombstone, so torn cleanup is retried. Historical v1 markers are never
  signalled: they permit storage-only cleanup only after their recorded process
  group is already absent.
  Text forwards child output and starts its terminal record on a new line after
  that output. JSON waits until the shared group and private
  marker are confirmed cleaned, then emits exactly one
  `vat.k8s.session.port-forward.v1` `vat_json` document with the child exit,
  separate 64 KiB serialized-capped stdout/stderr, truncation/lossy flags, and
  a `status --verify-api` next step—never raw child-stream replay. VAT-owned
  setup, API, tunnel, and cleanup errors are masked; opaque credential-free
  child output in a successful result is not arbitrarily redacted. It silently
  rechecks the lease after API verification and immediately before both the
  exact kubectl and host-child spawns, so expiry starts no tunnel. A partial
  capture-reader setup reaps the direct child and completes outer-group cleanup
  before reader joining. This is not a public listener, ingress/LB, a background
  tunnel, or arbitrary resource port-forwarding. The independent-kubectl
  real-host gate passed 1/1 (36 filtered) in 49.57s after loading a local alpine
  fixture, using an in-pod HTTP probe because BusyBox lacks `httpd`, verifying
  the Service endpoint, text and strict one-document JSON loopback forwarding to
  a credential-free host child, confirmed cleanup and closed local ports, and
  exact lease deletion. It is not broader Kubernetes, ingress, public-listener,
  or detached-descendant evidence.
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
id = "k8s"                 # ephemeral local Kubernetes cluster
cluster = "auto"           # auto (kind→k3d→minikube) | kind | k3d | minikube
# k8s_version = "1.30"
# nodes = 1
export = { KUBECONFIG = "{kubeconfig}" }

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
- A `cluster` service spins up an ephemeral local Kubernetes cluster (kind, k3d,
  or minikube; `auto` picks the first installed). vat creates it before the
  runner, exports `KUBECONFIG` (the `{kubeconfig}` token) plus
  `VAT_SERVICE_<ID>_KUBECONFIG`, probes readiness with `kubectl get nodes`, and
  deletes it at teardown per the `keep` policy. With no backend it emits a
  structured `cluster_backend_unavailable` error (no panic). All backends need
  Docker on Apple Silicon.
- `vat k8s ephemeral` is not that persistent-cluster surface. It uses only an
  auto-boot Apple systemd machine plus its exact inspected backing container;
  it bootstraps one K3s node for one foreground host command and removes the
  private 0600 kubeconfig, kubectl cache, recovery marker, and exact machine
  afterwards. It does not use Docker, `container machine run`, a background
  daemon, a durable kubeconfig, or `vat cluster` state. Use
  `vat k8s ephemeral cleanup` only to reconcile a marker whose recorded VAT
  process is no longer alive.
- `vat k8s session` extends that Docker-free substrate only as a bounded active
  lease: `create --ttl 30m`, separate text `exec [--timeout SECONDS] <id> --
  kubectl ...` or one-document `exec --format json [--timeout SECONDS] <id> --
  kubectl ...` calls, `status`, then explicit `delete`. Omitted timeout means
  the remaining lease TTL; an explicit 1..=14400-second timeout cannot exceed
  it. Every exec owns a process group and holds the operation lock through
  cleanup; normal exit, deadline, or interrupt reaps the group before marker
  removal. A crash leaves a starting/live marker that blocks later lifecycle
  operations fail-closed rather than implying crash-safe termination. JSON exec
  captures separate stdout/stderr concurrently, retains only a serialized-
  JSON-bounded 64 KiB suffix per stream, and never replays raw child output; it
  does not turn the credential-bearing child into a sandbox. VAT keeps the
  private credential/cache directory at mode 0700/0600, does not print its
  path, rejects an expired lease or changed backing id/API endpoint, and removes
  credentials only after exact machine absence is confirmed. It has no
  background reaper; `session cleanup` reclaims expired leases and abandoned
  creates when an agent invokes it.
- `vat k8s session port-forward run` is a narrower leased-session operation:
  one `service/<lowercase-dns-label>` port is exposed only at `127.0.0.1` while
  one host child runs. Text behavior is unchanged; `--format json` is the only
  machine form. VAT holds the kubeconfig only for kubectl and gives that child
  endpoint metadata after stripping `KUBECONFIG`, K3s cache/API variables, and
  `VAT_HOME` from the child environment. That is not a same-UID OS sandbox or an
  adversarial-child security boundary. The child joins kubectl's authenticated
  process group and must remain cooperative and non-daemonizing. VAT owns that
  group, private cache, a v2 CSPRNG recovery marker, and a retained `CLOEXEC`
  operation lock through cleanup. Normal JSON completion emits one
  `vat.k8s.session.port-forward.v1` document only after group cleanup and marker
  removal are confirmed; it contains the child exit plus separately bounded
  64 KiB serialized stdout/stderr and never replays raw streams. VAT masks its
  own setup/API/tunnel/cleanup failures but does not arbitrarily redact opaque
  credential-free child output. It silently rechecks the lease after API proof
  and immediately before kubectl and host-child spawns; if it expires, no tunnel
  starts. A partial reader setup reaps the direct child and completes outer-group
  cleanup before readers join. An interrupted marker is reconciled under a later
  held lock rather than owner-PID liveness; if it cannot be authenticated, it is
  retained fail-closed before any further mutation. The independent-kubectl
  Service-forward E2E passed 1/1 (36 filtered) in 49.57s and covers one
  loopback Service text and strict JSON tunnel with a credential-free host child,
  confirmed cleanup, and closed local ports; it is not a general Kubernetes
  tunnel guarantee.
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
  | `cluster` | `KUBECONFIG` | `{kubeconfig}` expands to the isolated kubeconfig path. | `VAT_SERVICE_<ID>_KUBECONFIG` |

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
  irrelevant; Docker runtime, Auto image, eligible Auto preset fallback, and
  selected cluster plans retain Docker probing, and cluster requires Docker.
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
- `vat cluster create [--backend auto|kind|k3d|minikube] [--name N]`: create a
  standalone local Kubernetes cluster (outlives a run); `vat cluster ls --json`,
  `vat cluster kubeconfig <name>`, and `vat cluster delete <name>` manage it.
- Every `vat k8s` command requires an independently installed `kubectl` first
  on `PATH`; VAT rejects an OrbStack-provided binary before K3s use. This is
  host-tool provenance, not a GUI or Docker Engine requirement. On this host
  Homebrew `kubernetes-cli` now supplies `/opt/homebrew/bin/kubectl`. The
  independent-kubectl one-shot, leased, local-image, and Service-forward E2Es
  passed 1/1 (36 filtered) in 28.38s, 29.97s, 49.73s, and 49.57s respectively.
  The local-image proof is one already-local Apple `alpine:3.20` pod with
  `imagePullPolicy=Never`, a marker log, and exact session cleanup—not
  registry-pull generality, persistence, GUI, or Docker Engine/API evidence.
- `vat k8s ephemeral image build`: explicitly build VAT's embedded systemd
  image into the Apple Container store. Its local tag identifies the embedded
  build asset revision, not a verified supply-chain image digest. It never
  starts a cluster.
- `vat k8s ephemeral run -- kubectl get nodes`: boot one disposable Apple K3s
  node, prove host API access, run that command with a private kubeconfig, then
  clean credentials and the exact owned machine. Its isolated HOME keeps
  kubectl's normal cache private; only a child shell can expand
  `$VAT_K8S_CACHE_DIR`, because arbitrary direct argv is never shell-expanded
  by VAT.
- Failed one-shot or leased K3s bootstrap keeps the root error first, then
  reports staged non-sensitive installer evidence through exactly
  `guest_install_log`, `guest_k3s_system`, `backing_container_logs`,
  `machine_boot_log`, `machine_inspect`, and `container_system_status` under a
  six-second total / one-second-per-probe read-only budget. It excludes private
  kubeconfig/cache and host credentials, preserves the existing 300-second
  bootstrap behavior, does not retry or rerun `k3s --version`, adds no
  wrapper/recovery command, and still performs exact cleanup. The deterministic
  fake regression passed. The independent-kubectl one-shot, leased, local-image,
  and Service-forward E2Es passed 1/1 (36 filtered) in 28.38s, 29.97s, 49.73s,
  and 49.57s. The local-image result loads one already-local Apple `alpine:3.20`
  into one lease, runs a pod with `imagePullPolicy=Never`, observes its marker
  log, then completes exact session cleanup; it is not registry-pull generality.
  The leased result covers strict JSON exec with `--timeout 30`; the
  Service-forward result covers one Service-only loopback strict JSON tunnel,
  credential-free child, confirmed cleanup, and closed local ports. These are
  bounded one-guest results, not persistence or a general cluster claim.

- `vat k8s session create --ttl 30m`: create one bounded active Apple K3s lease
  and print its opaque id plus a runnable next command. `vat k8s session exec
  --timeout 30 <id> -- kubectl get nodes` is one bounded text invocation; omit
  the timeout only to use the remaining lease TTL. JSON uses
  `vat k8s session exec --format json --timeout 30 <id> -- kubectl get nodes -o
  json`. An explicit timeout is 1..=14400 seconds and cannot exceed remaining
  TTL. Both forms retain the private lock through owned-process-group cleanup;
  normal exit, timeout, or SIGINT/SIGTERM reaps the group and removes the exec
  marker only once absent. A crash marker blocks later exec, delete, and cleanup
  fail-closed rather than claiming recovery termination. After the same active-
  lease, exact-backing/API, private-credential, and owned API proof plus a final
  TTL recheck, JSON emits exactly one `vat.k8s.session.exec.v1` document with
  separate 64 KiB serialized-bounded streams, the child exit code, no raw replay,
  and no lease-record mutation. It masks private credential/cache paths on
  failure, but its child intentionally receives credentials. The independent-
  kubectl leased E2E passed 1/1 (36 filtered) in 29.97s, covering text commands,
  JSON exec with `--timeout 30`, status verification, and exact delete. No-flag
  `status <id>` remains lease/machine-state only. Opt-in `status --verify-api <id>`
  proceeds only for an active unexpired session with no retained port-forward or
  exec marker; it acquires the private operation lock,
  rechecks expiry after lock and immediately before its bounded private-
  credential API probe, and verifies the exact backing identity and endpoint.
  Success adds `api_checked=true`, `api_state=reachable`; expired/recovery
  states are non-probing `api_checked=false`, `api_state=not_checked`; busy,
  unavailable, and identity-mismatched state fails closed without lease or
  credential mutation. Focused fake coverage passed 4/4 and the precise status
  unit passed 1/1. The independent-kubectl leased E2E passed 1/1 (36 filtered)
  in 29.97s and includes `status --verify-api` after text and strict JSON exec;
  it is bounded active-lease evidence, not persistence or a general API-status
  guarantee. `delete <id>` confirms exact cleanup before removing credentials, and `cleanup` reclaims
  expired leases. This is explicitly one-boot, not a persistent/restartable
  local Kubernetes backend.
- `vat k8s session image load <id> <local-image-ref>`: deliver one already
  local Apple Container `linux/arm64` image to that active K3s lease without a
  Docker daemon or registry pull. VAT verifies one inspected variant and its
  OCI descriptor, keeps the transient OCI archive private and bounded, imports
  into `k8s.io`, verifies the canonical reference, then removes both archive
  copies. It does not accept arbitrary tar files or promise cross-platform
  delivery.
- `vat k8s session port-forward run [--format json] <id> service/<name>
  <remote-port> [--namespace <ns>] [--local-port <port>] -- <command...>`:
  start one foreground, loopback-only Service tunnel for one host child. Text
  forwards child output and starts its terminal record on a new line afterward;
  `--format json` is the only JSON spelling.
  The child gets `VAT_K8S_PORT_FORWARD_{HOST,PORT,ADDR,RESOURCE,NAMESPACE}` and
  a private HOME; VAT strips `KUBECONFIG`, K3s cache/API variables, and
  `VAT_HOME` from its environment. This is child-environment credential hygiene,
  not a same-UID OS sandbox or adversarial-child security boundary. The child
  shares the authenticated kubectl process group, so it and ordinary descendants
  must stay cooperative and non-daemonizing. VAT stops that group and removes
  forward state before terminal JSON reports `cleanup=confirmed`; recovery uses
  only a v2 CSPRNG-authenticated leader, persists a `cleaning` tombstone across
  torn cleanup, and fails closed rather than signal an unauthenticated group.
  A legacy v1 marker is never signalled and can only clear already-absent-group
  storage.
  `--local-port 0` selects an ephemeral loopback port. JSON holds the private
  operation lock through tunnel/group cleanup and emits exactly one
  `vat.k8s.session.port-forward.v1` result only after cleanup is confirmed; it
  preserves child exit, separately caps serialized stdout/stderr at 64 KiB, and
  supplies `status --verify-api` as next without raw-stream replay. VAT-owned
  setup/API/tunnel/cleanup errors are masked, while opaque credential-free child
  output is not arbitrarily redacted. Silent lease checks follow API verification
  and precede exact kubectl and host-child spawns; a crossed TTL creates no
  tunnel. Partial reader setup reaps the direct child and completes outer-group
  cleanup before reader join. Only Services are accepted; there is no
  pod/arbitrary-resource forwarding, ingress/LB, public bind, or background
  tunnel. The independent-kubectl Service-forward E2E passed 1/1 (36 filtered)
  in 49.57s, covering one loopback Service text and strict JSON tunnel with a
  credential-free host child, confirmed cleanup, and closed local ports. It is
  not a persistent Kubernetes, ingress/LB, public-listener, or general tunnel
  guarantee.
- `vat fork <id> [--name N]`: copy-on-write fork a retained vat's rootfs into a
  new runnable vat that records the source as its lineage; the fork is
  independent afterward (writes to one do not affect the other).
- `vat snapshot <id> [--name N]`: freeze a retained vat's rootfs into an
  immutable snapshot for later inspection or forking; a snapshot itself is not
  runnable.
- `vat gpu --json`: report the GPU(s) every vat on this host can reach,
  independent of any specific vat or run.

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
- Apple Container K3s has a separate bounded headless path, not a durable
  replacement for a Kubernetes Desktop integration: `vat k8s ephemeral` is one
  command, while `vat k8s session` is a bounded active lease across explicit
  commands. Every `vat k8s` command requires an independently installed
  `kubectl` first on `PATH`; VAT rejects an OrbStack-provided binary before K3s
  use. This is host-tool provenance, not a GUI or Docker Engine dependency. On
  this host Homebrew `kubernetes-cli` now supplies `/opt/homebrew/bin/kubectl`.
  Independent-kubectl one-shot, leased, local-image, and Service-forward E2Es
  each passed 1/1 (36 filtered) in 28.38s, 29.97s, 49.73s, and 49.57s
  respectively. The local-image E2E loaded one already-local Apple `alpine:3.20`
  into one lease, ran a pod with `imagePullPolicy=Never`, observed its marker
  log, then completed exact session cleanup; it is not registry-pull generality.
  A lease can load one verified local `linux/arm64` image and run one
  foreground, Service-only `127.0.0.1` port-forward whose child receives endpoint
  metadata while VAT strips K3s credential variables and `VAT_HOME` from the
  child environment. That filtering is not a same-UID OS sandbox or
  adversarial-child security boundary; the child must not daemonize or escape its
  authenticated kubectl process group. Neither path has restart safety,
  reboot-safe retention, PVC/storage, ingress/LB, public listener, background
  tunnel, or multi-node promise because Apple's machine restart path is still a
  hard blocker. A bootstrap failure remains diagnostic-only: the root error is
  primary, then exactly the fixed six-label read-only evidence is emitted under
  its six-second total / one-second-per-probe budget before exact cleanup. It
  excludes private kubeconfig/cache and host credentials, changes neither the
  existing 300-second bootstrap behavior nor persistence, and does not retry or
  rerun `k3s --version` or add a wrapper/recovery command. The deterministic
  fake regression passed. The leased real-host result includes strict JSON exec
  with `--timeout 30`; the Service-forward result includes one Service-only
  loopback strict JSON tunnel with a credential-free child, confirmed cleanup,
  and closed local ports. Neither result establishes persistence, crash-safe
  termination, a general cluster, or OS-sandbox behavior.
- The runner is always a host process (never containerized) — the GPU story.
  Docker is only an option for run-scoped dependency *services*.
- Services in `vat.toml` are run-scoped dependencies of one runner invocation;
  containers are ephemeral (`docker run --rm`) and removed at teardown; external
  services are attached and probed but not lifecycle-managed by vat.
- vat does not schedule production work or manage restart policy.
- Standalone `vat cluster` clusters outlive a run as a convenience, but vat does
  not supervise them (no daemon, no restart, no health monitoring) — it only
  creates/lists/deletes/reports on explicit command, like kind/k3d themselves.
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
"#;

const K8S: &str = r#"# VAT local Kubernetes workflow

Use `vat k8s ephemeral image build` followed by `vat k8s ephemeral run -- ...`
for one command, or `vat k8s session create`, `exec`, and `delete` for a bounded
multi-command lease. An independently installed `kubectl` is required. This is
not persistent Kubernetes, a Desktop integration, or a general cluster manager.
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
        summary: "run one-shot or leased ephemeral Apple Container K3s work",
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
