// HANDWRITE-BEGIN gap="missing-generator:e2e-test:dc898059" tracker="#1479" reason="R7/AC3/AC4/AC5: a `docker_available()` skip helper plus `build_fails_missing_dockerfile` (no subprocess, always runs) and the Docker-gated `build_produces_tagged_image_in_docker_engine` test asserting both a successful `BuildReport` and that the engine's image store has the tag."

use std::collections::BTreeMap;
use std::fs::File;
use std::io::Write;
use std::path::PathBuf;
use std::process::Command;

use vat::commands::build;

/// Skip helper: `vat build` targets vat's Docker Engine (or whatever
/// DOCKER_HOST names); skip when no daemon answers.
fn docker_available() -> bool {
    Command::new("docker")
        .arg("info")
        .output()
        .is_ok_and(|o| o.status.success())
}

/// AC3: build_image fails cleanly with a clear error when the Dockerfile path
/// does not exist. No subprocess is spawned; this test always runs without
/// requiring a Docker daemon.
#[test]
fn build_fails_missing_dockerfile() {
    let context = PathBuf::from("/tmp/nonexistent-context");
    let dockerfile = PathBuf::from("/tmp/nonexistent-dockerfile");
    let tag = "test:latest";
    let build_args: Vec<(String, String)> = vec![];

    let result = build::build_image(&context, &dockerfile, tag, &build_args);

    assert!(result.is_err());
    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains("dockerfile not found"),
        "error should mention missing dockerfile: {}",
        err_msg
    );
}

/// AC4/AC5: a minimal Dockerfile builds through the Docker Engine, the
/// BuildReport is complete, and the engine's image store has the tag.
#[test]
fn build_produces_tagged_image_in_docker_engine() {
    if !docker_available() {
        eprintln!("no Docker daemon answers; skipping build smoke test");
        return;
    }

    let tempdir = tempfile::tempdir().expect("create tempdir");
    let dockerfile_path = tempdir.path().join("Dockerfile");
    let mut dockerfile = File::create(&dockerfile_path).expect("create Dockerfile");
    writeln!(dockerfile, "FROM busybox:latest").expect("write Dockerfile");
    writeln!(dockerfile, "RUN echo 'test build'").expect("write Dockerfile");

    let context = tempdir.path().to_path_buf();
    let tag = "vat-test-build:latest";
    let build_args: Vec<(String, String)> = vec![];

    let result = build::build_image(&context, &dockerfile_path, tag, &build_args);
    assert!(result.is_ok(), "build_image should succeed: {:?}", result);
    let report = result.unwrap();

    assert_eq!(report.tag, tag);
    assert!(
        report.dockerfile.contains("Dockerfile"),
        "dockerfile path should be in report"
    );
    assert_eq!(report.build_args, BTreeMap::new());
    assert!(report.duration_ms > 0);

    let inspect = Command::new("docker")
        .args(["image", "inspect", tag])
        .output()
        .expect("run docker image inspect");
    assert!(
        inspect.status.success(),
        "docker image inspect {tag}: {}",
        String::from_utf8_lossy(&inspect.stderr)
    );

    let _ = Command::new("docker").args(["image", "rm", tag]).output();
}
// HANDWRITE-END
