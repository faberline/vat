# Changelog

Breaking changes and upgrade steps per release. [STATUS.md](STATUS.md) is the
support matrix at each tag; the release notes on GitHub list every merged
change.

## 0.5.0 (unreleased)

vat is reorganised around three pillars (#38): Apple-native containers, the
full Docker Engine in one shared Linux machine, and local GKE in that
machine. Upgrading from 0.4.x needs the steps below.

### Breaking changes

- **Apple Silicon macOS only.** Releases ship only `aarch64-apple-darwin`
  (#47). The `x86_64-unknown-linux-gnu` and `aarch64-unknown-linux-gnu`
  tarballs are gone. `install.sh` exits 2 on Linux and on Intel macOS. Run
  Linux workloads inside `vat machine` on a Mac instead.
- **Unknown `vat.toml` keys are errors** (#36). Every table now rejects keys
  it does not know. The error names the key and its table or service,
  suggests the closest accepted key, and lists the accepted ones:

  ```text
  unknown key `ready_htp` in [[services]] id=`api`; did you mean `ready_http`? ...
  ```

  To migrate, run `vat plan` in each project and rename or delete every key
  it reports. `ready_tcp` was never a key: drop it (a `cmd` service with a
  `{port}` already waits for the port to accept connections) or use
  `ready_cmd`.
- **A bare-alias `export` on a `cmd` service needs `ready_http`** (#37). An
  `export` value without `{host}`/`{port}` names the variable that receives
  the service's `ready_http` URL. Without `ready_http` it is now a
  validation error instead of being dropped. Use a template such as
  `export = { API_URL = "http://{host}:{port}" }`.
- **`vat cluster` and the kind/k3d/minikube backends are retired** (#38).
  There is one persistent single-node K3s cluster in vat's machine:
  `vat k8s up` starts it and writes `~/.vat/kube/config` (context `vat`).
  In `vat.toml`, `cluster = "auto"`, `"kind"`, `"k3d"`, and `"minikube"` now
  fail with `use cluster = "machine"`. Clusters that `vat cluster create`
  made stay in kind/k3d/minikube until you delete them with those tools.
  The one-boot Apple Container K3s guest is gone too.
- **The `docker` shim is retired** (#38). `vat docker install-shim` and
  `vat docker status` no longer exist, and vat translates no Docker argv.
  Delete the `docker -> vat` symlink you installed with `--dir`, install the
  stock `docker` CLI, and point it at vat's machine:

  ```bash
  vat machine start
  export DOCKER_HOST="unix://$HOME/.vat/run/docker.sock"
  ```

- **vat's own Docker users default to vat's machine** (#38). `vat build`,
  `runtime = "docker"` and `auto` image services, compose runners, and
  capability probes use `~/.vat/run/docker.sock`, booting the machine on
  demand, instead of the host's Docker daemon. An explicit `DOCKER_HOST` or
  `DOCKER_CONTEXT` still wins, and `VAT_ENGINE=external` keeps your own
  daemon. Images in another daemon (Docker Desktop, OrbStack, colima) are
  not visible in the machine; rebuild or pull them there.

### Fixed

- `export` `{host}`/`{port}` templates on a `cmd` service now reach the
  runner whatever readiness probe the service uses (#37). In 0.4.x they were
  silently dropped with `ready_cmd`, although `vat plan` listed them. A
  project that worked around this with `VAT_SERVICE_<ID>_HOST`/`_PORT` can
  switch back to `export`.

### Added

- The shared Linux machine (`vat machine`), its Docker Engine socket,
  persistent K3s (`vat k8s`), and local GKE (`vat gcp`) (#38). A launchd job
  keeps `~/.vat/run/docker.sock`, so a Docker client wakes a stopped
  machine. The machine stops itself after 5 idle minutes (`--idle-stop`),
  trims its disk, and follows the host clock.
- The machine follows a vat upgrade (#41). A stopped machine is re-staged
  silently. An idle machine is restarted onto the new build by
  `vat machine start`, `vat machine exec`, `vat k8s up`, or vat's own Docker
  users. A busy machine keeps running and prints a hint, and
  `vat machine status` reports `vmm_outdated`.
- `vat machine rm --yes` removes the machine, its data disk, its launchd
  job, `docker.sock`, `vat.sock`, an empty `~/.vat/run`, and, for the last
  machine, `~/.vat/machine/bin`. See [Uninstall](README.md#uninstall).
- Apple-native OCI `darwin/arm64` images and containers (`vat image`,
  `vat container`, `vat native`) (#38).

See [STATUS.md](STATUS.md) for the bounds and gates of each surface.
