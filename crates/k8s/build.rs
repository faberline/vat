//! Build script: compile the vendored googleapis protos for the built-in gRPC
//! emulators. A no-op without the `emulator` feature or before the protos are
//! vendored, so the build never depends on a system protoc.

fn main() {
    // Cargo exposes enabled features as CARGO_FEATURE_<NAME>.
    if std::env::var_os("CARGO_FEATURE_EMULATOR").is_none() {
        return;
    }
    let pubsub = "proto/google/pubsub/v1/pubsub.proto";
    if !std::path::Path::new(pubsub).exists() {
        return;
    }
    let protoc = protoc_bin_vendored::protoc_bin_path().expect("vendored protoc binary");
    std::env::set_var("PROTOC", protoc);

    // Compile the built-in gRPC surfaces from the vendored proto tree:
    //   - google.pubsub.v1 (Pub/Sub emulator)
    //   - google.cloud.tasks.v2 (Cloud Tasks emulator gRPC front-end)
    //   - google.cloud.scheduler.v1 (Cloud Scheduler emulator gRPC front-end)
    // The vendored tree carries the shared google/api, google/iam/v1,
    // google/rpc, google/type deps so protoc resolves everything against
    // `proto/`.
    let mut protos = vec![pubsub];
    for optional in [
        "proto/google/cloud/tasks/v2/cloudtasks.proto",
        "proto/google/cloud/scheduler/v1/cloudscheduler.proto",
    ] {
        if std::path::Path::new(optional).exists() {
            protos.push(optional);
        }
    }
    tonic_build::configure()
        .build_client(true)
        .build_server(true)
        .compile_protos(&protos, &["proto"])
        .expect("compile vendored gRPC protos (pubsub + cloud tasks)");
    for proto in &protos {
        println!("cargo:rerun-if-changed={proto}");
    }
}
