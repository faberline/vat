# Contributing to vat

## Brief

How to change `vat`. What it promises and the work roots it owns live in
[README.md](README.md); repository-wide authoring and verification rules live
in the root [CONTRIBUTING.md](https://github.com/faberline/workspace/blob/main/CONTRIBUTING.md).

Use `product-deliver` for authorized work. QA owns the red e2e case and its
registration. Dev owns the red unit test and scoped implementation. A fresh
`vat-qa` runs the declared complete gate. The controller owns Git, tracker,
and acceptance. Legacy AW use is explicit-only.

## Verification

Every PR runs the same gate in CI (`.github/workflows/ci.yml`, `macos-14`,
toolchain pinned by `rust-toolchain.toml`). Run it locally before pushing:

| Gate | Command |
|---|---|
| format | `cargo fmt --all --check` |
| lint | `cargo clippy --workspace --all-targets --locked -- -D warnings` |
| unit + integration tests, all crates | `cargo test --workspace --locked --no-fail-fast` |

Use `--workspace`, not `-p vat`: only the workspace run reaches the other
crates' tests, including vat-docker's `guest_agent_binary_matches_sources`,
which fails when `guest-agent/` changed without `scripts/build-guest-agent.sh`.

The VM E2Es (`vat_machine_e2e`, `vat_k8s_e2e`, `vat_gcp_e2e`) are `#[ignore]`d
and stay opt-in and local. GitHub's Apple Silicon macOS runners are virtual
machines and cannot nest Virtualization.framework guests, so CI cannot boot the
shared machine. Run them on a Mac with the commands in [STATUS.md](STATUS.md)
when a change touches the machine, K3s, or local GCP, and say in the PR that
you did.

A toolchain bump is its own change: update `rust-toolchain.toml` and fix any
new lints in the same PR.
