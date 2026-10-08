//! Mutating admission webhook: every new pod outside the system namespaces
//! gets `PUBSUB_EMULATOR_HOST` / `STORAGE_EMULATOR_HOST`, so stock Google
//! client libraries talk to the machine's emulators with no code change.
//! Variables a container already sets are left alone, and the pod annotation
//! `vat.dev/gcp-emulators: "false"` opts out.

use std::sync::Arc;

use axum::extract::State;
use axum::routing::post;
use axum::{Json, Router};
use base64::Engine as _;
use serde_json::{json, Value};

use super::{GcpConfig, OPT_OUT};

pub fn router(cfg: GcpConfig) -> Router {
    Router::new()
        .route("/mutate", post(mutate))
        .with_state(Arc::new(cfg))
}

async fn mutate(State(cfg): State<Arc<GcpConfig>>, Json(review): Json<Value>) -> Json<Value> {
    let request = &review["request"];
    let mut response = json!({ "uid": request["uid"], "allowed": true });
    let patch = patch_for(&cfg, &request["object"]);
    if !patch.is_empty() {
        response["patchType"] = json!("JSONPatch");
        response["patch"] = json!(base64::engine::general_purpose::STANDARD
            .encode(serde_json::to_vec(&patch).unwrap_or_default()));
    }
    Json(json!({
        "apiVersion": review["apiVersion"].as_str().unwrap_or("admission.k8s.io/v1"),
        "kind": "AdmissionReview",
        "response": response,
    }))
}

/// JSON Patch operations adding the emulator env to every container.
pub fn patch_for(cfg: &GcpConfig, pod: &Value) -> Vec<Value> {
    if pod["metadata"]["annotations"][OPT_OUT].as_str() == Some("false") {
        return Vec::new();
    }
    let env = cfg.pod_env();
    let mut ops = Vec::new();
    for kind in ["initContainers", "containers"] {
        let Some(containers) = pod["spec"][kind].as_array() else {
            continue;
        };
        for (i, c) in containers.iter().enumerate() {
            let existing: Vec<&str> = c["env"]
                .as_array()
                .map(|e| e.iter().filter_map(|v| v["name"].as_str()).collect())
                .unwrap_or_default();
            let missing: Vec<Value> = env
                .iter()
                .filter(|(name, _)| !existing.contains(name))
                .map(|(name, value)| json!({ "name": name, "value": value }))
                .collect();
            if missing.is_empty() {
                continue;
            }
            if c["env"].is_array() {
                for item in missing {
                    ops.push(json!({ "op": "add", "path": format!("/spec/{kind}/{i}/env/-"), "value": item }));
                }
            } else {
                ops.push(
                    json!({ "op": "add", "path": format!("/spec/{kind}/{i}/env"), "value": missing }),
                );
            }
        }
    }
    ops
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adds_env_without_overriding() {
        let pod = json!({
            "metadata": {},
            "spec": {
                "initContainers": [{ "name": "init" }],
                "containers": [
                    { "name": "a" },
                    { "name": "b", "env": [{ "name": "PUBSUB_EMULATOR_HOST", "value": "mine:1" }] },
                ]
            }
        });
        let ops = patch_for(&GcpConfig::default(), &pod);
        assert_eq!(ops.len(), 3);
        assert_eq!(ops[0]["path"], "/spec/initContainers/0/env");
        assert_eq!(ops[1]["path"], "/spec/containers/0/env");
        assert_eq!(ops[1]["value"].as_array().unwrap().len(), 2);
        assert_eq!(ops[2]["path"], "/spec/containers/1/env/-");
        assert_eq!(ops[2]["value"]["name"], "STORAGE_EMULATOR_HOST");
    }

    #[test]
    fn annotation_opts_out() {
        let pod = json!({
            "metadata": { "annotations": { OPT_OUT: "false" } },
            "spec": { "containers": [{ "name": "a" }] }
        });
        assert!(patch_for(&GcpConfig::default(), &pod).is_empty());
    }
}
