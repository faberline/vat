// CODEGEN-BEGIN
// AW-EC-BEGIN
// @ec vat-build-container-gated-smoke
// @capability agent-native-gpu-native-dev-containers
// @claim vat-build-dockerfile-build-via-container-cli
// @contract local-agent-test-runner-protocol
// @category behavior
// @required_for_production true
// @command cargo test -p vat --test vat_build -- --nocapture
// AW-EC-END

// Contract: AC4: build_produces_tagged_image_in_docker_engine (gated on the docker_available() skip helper) writes a minimal, valid Dockerfile to a tempdir, runs vat build against it through the Docker Engine, and asserts both a successful BuildReport and that `docker image inspect` finds the tag in the engine's image store.
// Contract: AC5: the fixture Dockerfile used by this test is a plain, unmodified Dockerfile — vat build never edits, lints, or rewrites the Dockerfile it is given; the same command also succeeds manually against a real, already-existing repo Dockerfile without requiring any edit to it.
// Contract: Registered in the generated aw.toml EC inventory alongside the docker_available() skip helper so `aw ec gen --verify` / `aw health --verify-tests` pick this up as a configured EC-gated test command for the agent-native-gpu-native-dev-containers capability.
#[test]
#[ignore = "AW EC gate: run via `aw health --verify-ec` or `cargo test -- --ignored`"]
fn vat_build_container_gated_smoke() {
    let command = "cargo test -p vat --test vat_build -- --nocapture";
    let id = "vat-build-container-gated-smoke";
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
