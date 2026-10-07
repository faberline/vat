// CODEGEN-BEGIN
// AW-EC-BEGIN
// @ec vat-llm-agent-usage-guide
// @capability agent-native-gpu-native-dev-containers
// @claim agent-legible-state-and-diff-surface
// @contract agent-legible-state-and-diff-surface
// @category behavior
// @required_for_production true
// @command cargo test -p vat --test vat_toml_runner -- --nocapture
// AW-EC-END

// Contract: `vat llm --topic guide` exits successfully.
// Contract: The guide mentions vat.toml runner mode and direct command mode.
// Contract: The guide mentions state, diff, and logs evidence commands.
// Contract: The guide describes unchanged no-flag session status and the bounded `vat k8s session status --verify-api <id>` proof: only an active unexpired lease without a port-forward or exec marker may enter the private operation lock; it rechecks expiry after lock and immediately before the bounded private-credential API probe, verifies exact backing identity/endpoint, reports api_checked=true/api_state=reachable on success, returns non-probing api_checked=false/api_state=not_checked for expired/recovery state, and fails closed without lease/credential mutation for busy, unavailable, or identity-mismatched state. It remains one-boot/nonpersistent with no GUI or Docker Engine/API; focused fake coverage passed 4/4, a precise unit passed 1/1, and the independent-kubectl leased E2E passed 1/1 (36 filtered) in 29.97s with status --verify-api after text and strict JSON exec. That is bounded active-lease evidence, not a general API-status guarantee.
// Contract: The guide describes bounded text `vat k8s session exec [--timeout SECONDS] <id> -- COMMAND` and JSON `vat k8s session exec --format json [--timeout SECONDS] <id> -- COMMAND`: omission uses remaining TTL; explicit 1..=14400 seconds cannot exceed it. Both forms validate the active lease, exact backing/API identity, private credentials, and owned host API under the private lock, recheck expiry before spawn, own/reap the process group on normal exit, deadline, or interruption, and retain the lock through cleanup. A starting/live crash marker blocks later exec/delete/cleanup fail-closed rather than claiming termination. JSON returns one `vat.k8s.session.exec.v1`/`vat_json` document with child_exit_code, separate stdout/stderr with 64 KiB serialized-value caps and truncated/utf8-lossy indicators, api_verified=true, runtime_invoked=true, session_record_mutated=false, no raw replay, and no private paths. The leased real-host E2E passed 1/1 (36 filtered) in 29.97s, including JSON exec with `--timeout 30`, status verification, and exact delete.
// Contract: The guide describes unchanged text Service-forward and only `vat k8s session port-forward run --format json <id> service/<name> <port> -- COMMAND` for JSON: its loopback Service tunnel gives a credential-free child endpoint metadata, keeps the lock through shared-PGID cleanup, silently rechecks TTL after API proof and before exact kubectl/child spawn, then emits one post-cleanup `vat.k8s.session.port-forward.v1` document with child exit, separate 64 KiB serialized streams, no raw replay, masked VAT-owned failures, opaque-child preservation, and status-verify next. Partial reader setup cleans direct/outer children before reader joining. The independent-kubectl real-host E2E passed 1/1 (36 filtered) in 49.57s, covering one loopback Service text and strict JSON tunnel with confirmed cleanup and closed local ports; it is not a general tunnel guarantee.
// Contract: The guide preserves permanent headless, non-Docker, non-daemon, full capabilities versus selected-plan doctor semantics: a full Docker probe maps `services.docker_services` to available or unavailable; `vat capabilities` exposes only a bounded read-only shared-builder advisory (shared_unknown ownership, automatic_cleanup=false, separate configuration/observed stats/global disk, nonfatal timeout/unknown/probe errors, no lifecycle mutation). An explicit Apple-Container-only plan uses one read-only container status probe and executes no Docker command; it maps `services.docker_services` to not_probed, retains `docker.daemon_probe.state=skipped` as provenance, and makes daemon=false non-unavailable evidence. Unsupported no-OCI-route presets and named volumes fail closed; builder advisory never decides doctor runtime success; Docker/Auto/cluster retains probe. K3s remains one-boot/local-lease only and requires an independently installed kubectl on PATH before K3s use. Independent-kubectl one-shot, leased, local-image, and Service-forward E2Es passed 1/1 (36 filtered) in 28.38s, 29.97s, 49.73s, and 49.57s. The local-image result is one already-local Apple alpine:3.20 pod with imagePullPolicy=Never, a marker log, and exact session cleanup; it does not establish registry-pull generality, persistence, GUI, or Docker Engine/API. The leased result covers strict JSON exec with `--timeout 30` and bounded `status --verify-api`; the Service-forward result covers one Service-only loopback strict JSON tunnel with a credential-free child, confirmed cleanup, and closed local ports. It adds no durable status backend: `status --verify-api` remains a private-lock, identity/endpoint, expiry-rechecked bounded active-lease proof, while `session exec --format json` is a credentialed agent-result form with no raw replay or session mutation. Those results do not establish persistence, a general cluster/tunnel, OS isolation, or crash-safe termination. It also documents `vat machine start` as the Docker Engine path: the stock docker CLI then works with DOCKER_HOST=unix://~/.vat/run/docker.sock.
#[test]
#[ignore = "AW EC gate: run via `aw health --verify-ec` or `cargo test -- --ignored`"]
fn vat_llm_agent_usage_guide() {
    let command = "cargo test -p vat --test vat_toml_runner -- --nocapture";
    let id = "vat-llm-agent-usage-guide";
    let mut root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    while !root.join(".aw").is_dir() {
        assert!(
            root.pop(),
            "AW EC {id}: no .aw/ project root above {}",
            env!("CARGO_MANIFEST_DIR")
        );
    }
    let output = std::process::Command::new("sh")
        .arg("-c")
        .arg(command)
        .current_dir(&root)
        .output()
        .unwrap_or_else(|e| panic!("AW EC {id}: failed to spawn `{command}`: {e}"));
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    if output.status.success()
        && aw_ec_cargo_test_executed_count(command, &stdout, &stderr) == Some(0)
    {
        panic!("AW EC {id} FAILED: cargo test command passed but executed 0 tests: {command}\nstdout:\n{stdout}\nstderr:\n{stderr}");
    }
    assert!(
        output.status.success(),
        "AW EC {id} FAILED (exit {:?}): {command}\nstdout:\n{stdout}\nstderr:\n{stderr}",
        output.status.code()
    );
}

fn aw_ec_cargo_test_executed_count(command: &str, stdout: &str, stderr: &str) -> Option<usize> {
    if !command.contains("cargo test") {
        return None;
    }
    let mut total = 0usize;
    let mut saw_count = false;
    for line in stdout.lines().chain(stderr.lines()) {
        let Some(count) = aw_ec_parse_cargo_running_test_count(line) else {
            continue;
        };
        total = total.saturating_add(count);
        saw_count = true;
    }
    saw_count.then_some(total)
}

fn aw_ec_parse_cargo_running_test_count(line: &str) -> Option<usize> {
    let rest = line.trim().strip_prefix("running ")?;
    let number = rest
        .strip_suffix(" tests")
        .or_else(|| rest.strip_suffix(" test"))?;
    number.trim().parse().ok()
}
// CODEGEN-END
