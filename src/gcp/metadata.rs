//! A GKE metadata server with Workload Identity.
//!
//! The caller is identified by its source IP. A pod IP maps to its Kubernetes
//! service account; when that KSA carries `iam.gke.io/gcp-service-account`,
//! the pod acts as that Google service account, otherwise as the workload
//! identity pool principal (as on GKE). Anything else (Docker containers, the
//! node itself) gets the node's Compute Engine default service account.
//!
//! Tokens are local fakes: emulators accept them, real Google APIs do not.

use std::collections::HashMap;
use std::future::Future;
use std::net::IpAddr;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::{ConnectInfo, Request, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Router;
use base64::Engine as _;
use serde_json::{json, Value};

use super::conn::Peer;
use super::GcpConfig;

const SCOPE: &str = "https://www.googleapis.com/auth/cloud-platform";
const TOKEN_TTL: i64 = 3599;
const CACHE_TTL: Duration = Duration::from_secs(15);
const ID_TOKEN_SECRET: &[u8] = b"vat-local-metadata";

/// The pod behind a source IP.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PodIdentity {
    pub namespace: String,
    pub pod: String,
    pub ksa: String,
    /// The KSA's `iam.gke.io/gcp-service-account` annotation.
    pub gsa: Option<String>,
}

pub type ResolveFuture = Pin<Box<dyn Future<Output = Option<PodIdentity>> + Send>>;
/// Looks up the pod that owns an IP.
pub type Resolver = Arc<dyn Fn(IpAddr) -> ResolveFuture + Send + Sync>;

/// Who is asking.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Caller {
    pub email: String,
    pub pod: Option<PodIdentity>,
}

pub struct Metadata {
    cfg: GcpConfig,
    resolver: Resolver,
    cache: Mutex<HashMap<IpAddr, (Instant, Option<PodIdentity>)>>,
}

/// K3s's default pod CIDR; other callers are never pods.
fn is_pod_ip(ip: IpAddr) -> bool {
    matches!(ip, IpAddr::V4(v4) if v4.octets()[0] == 10 && v4.octets()[1] == 42)
}

impl Metadata {
    pub fn new(cfg: GcpConfig, resolver: Resolver) -> Arc<Self> {
        Arc::new(Self {
            cfg,
            resolver,
            cache: Mutex::new(HashMap::new()),
        })
    }

    pub async fn caller(&self, ip: IpAddr) -> Caller {
        let pod = if is_pod_ip(ip) {
            let cached = self
                .cache
                .lock()
                .unwrap()
                .get(&ip)
                .filter(|(at, _)| at.elapsed() < CACHE_TTL)
                .map(|(_, pod)| pod.clone());
            match cached {
                Some(pod) => pod,
                None => {
                    let pod = (self.resolver)(ip).await;
                    self.cache
                        .lock()
                        .unwrap()
                        .insert(ip, (Instant::now(), pod.clone()));
                    pod
                }
            }
        } else {
            None
        };
        let email = match &pod {
            Some(p) => p
                .gsa
                .clone()
                .filter(|g| !g.is_empty())
                .unwrap_or_else(|| self.cfg.workload_pool()),
            None => self.cfg.node_service_account(),
        };
        Caller { email, pod }
    }

    /// Everything a caller may read, except tokens.
    fn tree(&self, caller: &Caller) -> Value {
        let cfg = &self.cfg;
        let number = cfg.project_number();
        let account = json!({
            "aliases": ["default"],
            "email": caller.email,
            "scopes": [SCOPE],
        });
        let mut accounts = serde_json::Map::new();
        accounts.insert("default".into(), account.clone());
        accounts.insert(caller.email.clone(), account);
        let mut attributes = json!({
            "cluster-name": "vat",
            "cluster-location": cfg.zone,
            "cluster-uid": format!("{:032x}", number),
        });
        if let Some(pod) = &caller.pod {
            attributes["k8s-namespace"] = json!(pod.namespace);
            attributes["k8s-pod"] = json!(pod.pod);
            attributes["k8s-service-account"] = json!(pod.ksa);
        }
        json!({
            "instance": {
                "attributes": attributes,
                "hostname": format!("vat.{}.c.{}.internal", cfg.zone, cfg.project),
                "id": number ^ 0x5a5a_5a5a,
                "machine-type": format!("projects/{number}/machineTypes/e2-standard-4"),
                "name": "vat",
                "service-accounts": accounts,
                "zone": format!("projects/{number}/zones/{}", cfg.zone),
            },
            "project": {
                "attributes": {},
                "numeric-project-id": number,
                "project-id": cfg.project,
            },
            "universe": { "universe-domain": "googleapis.com" },
        })
    }

    fn access_token(&self, caller: &Caller) -> Value {
        let now = chrono::Utc::now().timestamp();
        let claims = json!({
            "email": caller.email,
            "ns": caller.pod.as_ref().map(|p| &p.namespace),
            "ksa": caller.pod.as_ref().map(|p| &p.ksa),
            "iat": now,
        });
        let opaque = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(claims.to_string());
        json!({
            "access_token": format!("ya29.vat.{opaque}"),
            "expires_in": TOKEN_TTL,
            "token_type": "Bearer",
        })
    }

    fn id_token(&self, caller: &Caller, audience: &str) -> String {
        let now = chrono::Utc::now().timestamp();
        let sub = u128::from_le_bytes(
            blake3::hash(caller.email.as_bytes()).as_bytes()[..16]
                .try_into()
                .unwrap(),
        ) % 10u128.pow(21);
        let claims = json!({
            "iss": "https://accounts.google.com",
            "aud": audience,
            "azp": sub.to_string(),
            "sub": sub.to_string(),
            "email": caller.email,
            "email_verified": true,
            "iat": now,
            "exp": now + TOKEN_TTL + 1,
        });
        jsonwebtoken::encode(
            &jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS256),
            &claims,
            &jsonwebtoken::EncodingKey::from_secret(ID_TOKEN_SECRET),
        )
        .unwrap_or_default()
    }
}

pub fn router(md: Arc<Metadata>) -> Router {
    Router::new().fallback(handle).with_state(md)
}

fn reply(status: StatusCode, content_type: &str, body: String) -> Response {
    let mut resp = (status, body).into_response();
    let h = resp.headers_mut();
    h.insert("Metadata-Flavor", HeaderValue::from_static("Google"));
    h.insert(
        header::SERVER,
        HeaderValue::from_static("GKE Metadata Server"),
    );
    if let Ok(v) = HeaderValue::from_str(content_type) {
        h.insert(header::CONTENT_TYPE, v);
    }
    resp
}

fn text(body: impl Into<String>) -> Response {
    reply(StatusCode::OK, "application/text", body.into())
}

fn not_found() -> Response {
    reply(StatusCode::NOT_FOUND, "text/html", "Not Found\n".into())
}

fn flavored(headers: &HeaderMap) -> bool {
    let is = |name: &str, want: &str| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.eq_ignore_ascii_case(want))
    };
    is("metadata-flavor", "Google") || is("x-google-metadata-request", "True")
}

fn query(req: &Request) -> HashMap<String, String> {
    req.uri()
        .query()
        .map(|q| {
            url::form_urlencoded::parse(q.as_bytes())
                .into_owned()
                .collect()
        })
        .unwrap_or_default()
}

fn leaf_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Array(items) => items
            .iter()
            .map(|i| {
                i.as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| i.to_string())
                    + "\n"
            })
            .collect(),
        other => other.to_string(),
    }
}

async fn handle(
    State(md): State<Arc<Metadata>>,
    ConnectInfo(Peer(ip)): ConnectInfo<Peer>,
    req: Request,
) -> Response {
    let path = req.uri().path().to_string();
    if path == "/" {
        return text("computeMetadata/\n");
    }
    let Some(rest) = path.strip_prefix("/computeMetadata") else {
        return not_found();
    };
    if req.headers().contains_key("x-forwarded-for") {
        return reply(
            StatusCode::FORBIDDEN,
            "text/html",
            "Request denied: X-Forwarded-For header present\n".into(),
        );
    }
    if !flavored(req.headers()) {
        return reply(
            StatusCode::FORBIDDEN,
            "text/html",
            "Missing required header \"Metadata-Flavor\": \"Google\"\n".into(),
        );
    }
    if rest.is_empty() || rest == "/" {
        return text("v1/\n");
    }
    let Some(rest) = rest.strip_prefix("/v1") else {
        return not_found();
    };
    let q = query(&req);
    let caller = md.caller(ip).await;
    let segs: Vec<&str> = rest.split('/').filter(|s| !s.is_empty()).collect();

    // Dynamic leaves: tokens are never part of the tree.
    if let ["instance", "service-accounts", acct, kind] = segs.as_slice() {
        if *kind == "token" || *kind == "identity" {
            if *acct != "default" && *acct != caller.email {
                return not_found();
            }
            if *kind == "token" {
                return reply(
                    StatusCode::OK,
                    "application/json",
                    md.access_token(&caller).to_string(),
                );
            }
            let Some(audience) = q.get("audience").filter(|a| !a.is_empty()) else {
                return reply(
                    StatusCode::BAD_REQUEST,
                    "text/html",
                    "non-empty audience parameter required\n".into(),
                );
            };
            return text(md.id_token(&caller, audience));
        }
    }

    let tree = md.tree(&caller);
    let mut node = &tree;
    for seg in &segs {
        match node.get(*seg) {
            Some(next) => node = next,
            None => return not_found(),
        }
    }
    let json_out = q.get("alt").map(String::as_str) == Some("json");
    let recursive = q.get("recursive").is_some_and(|v| v == "true");
    match node {
        Value::Object(map) if !recursive && !json_out => {
            let mut keys: Vec<String> = map
                .iter()
                .map(|(k, v)| {
                    if v.is_object() {
                        format!("{k}/\n")
                    } else {
                        format!("{k}\n")
                    }
                })
                .collect();
            keys.sort();
            text(keys.concat())
        }
        Value::Object(_) => reply(StatusCode::OK, "application/json", node.to_string()),
        leaf if json_out => reply(StatusCode::OK, "application/json", leaf.to_string()),
        leaf => text(leaf_text(leaf)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use http_body_util::BodyExt;
    use std::net::Ipv4Addr;
    use tower::ServiceExt;

    fn md() -> Arc<Metadata> {
        let resolver: Resolver = Arc::new(|ip| {
            Box::pin(async move {
                match ip {
                    IpAddr::V4(v) if v == Ipv4Addr::new(10, 42, 0, 7) => Some(PodIdentity {
                        namespace: "app".into(),
                        pod: "web-1".into(),
                        ksa: "web".into(),
                        gsa: Some("web@vat-local.iam.gserviceaccount.com".into()),
                    }),
                    IpAddr::V4(v) if v == Ipv4Addr::new(10, 42, 0, 8) => Some(PodIdentity {
                        namespace: "app".into(),
                        pod: "plain".into(),
                        ksa: "default".into(),
                        gsa: None,
                    }),
                    _ => None,
                }
            })
        });
        Metadata::new(GcpConfig::default(), resolver)
    }

    async fn get(ip: [u8; 4], uri: &str, flavor: bool) -> (StatusCode, HeaderMap, String) {
        let mut req = Request::builder().uri(uri);
        if flavor {
            req = req.header("Metadata-Flavor", "Google");
        }
        let mut req = req.body(Body::empty()).unwrap();
        req.extensions_mut()
            .insert(ConnectInfo(Peer(IpAddr::from(ip))));
        let resp = router(md()).oneshot(req).await.unwrap();
        let status = resp.status();
        let headers = resp.headers().clone();
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        (status, headers, String::from_utf8_lossy(&body).into())
    }

    #[tokio::test]
    async fn requires_the_flavor_header_and_returns_it() {
        let (s, h, _) = get(
            [10, 42, 0, 7],
            "/computeMetadata/v1/project/project-id",
            false,
        )
        .await;
        assert_eq!(s, StatusCode::FORBIDDEN);
        assert_eq!(h["metadata-flavor"], "Google");
        let (s, _, body) = get([10, 42, 0, 7], "/", false).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(body, "computeMetadata/\n");
    }

    #[tokio::test]
    async fn project_and_zone() {
        let (_, _, id) = get([1, 2, 3, 4], "/computeMetadata/v1/project/project-id", true).await;
        assert_eq!(id, "vat-local");
        let (_, _, zone) = get([1, 2, 3, 4], "/computeMetadata/v1/instance/zone", true).await;
        assert!(zone.starts_with("projects/") && zone.ends_with("/zones/us-central1-a"));
        let (_, _, n) = get(
            [1, 2, 3, 4],
            "/computeMetadata/v1/project/numeric-project-id",
            true,
        )
        .await;
        assert_eq!(n, GcpConfig::default().project_number().to_string());
    }

    #[tokio::test]
    async fn workload_identity_maps_pods_to_their_gsa() {
        let email = "/computeMetadata/v1/instance/service-accounts/default/email";
        let (_, _, wi) = get([10, 42, 0, 7], email, true).await;
        assert_eq!(wi, "web@vat-local.iam.gserviceaccount.com");
        let (_, _, pool) = get([10, 42, 0, 8], email, true).await;
        assert_eq!(pool, "vat-local.svc.id.goog");
        let (_, _, node) = get([172, 17, 0, 2], email, true).await;
        assert!(node.ends_with("-compute@developer.gserviceaccount.com"));
    }

    #[tokio::test]
    async fn tokens_and_identity() {
        let base = "/computeMetadata/v1/instance/service-accounts";
        let (s, h, body) = get([10, 42, 0, 7], &format!("{base}/default/token"), true).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(h[header::CONTENT_TYPE], "application/json");
        let tok: Value = serde_json::from_str(&body).unwrap();
        assert!(tok["access_token"].as_str().unwrap().starts_with("ya29."));
        assert_eq!(tok["token_type"], "Bearer");
        let (s, _, _) = get(
            [10, 42, 0, 7],
            &format!("{base}/web@vat-local.iam.gserviceaccount.com/token"),
            true,
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        let (s, _, _) = get(
            [10, 42, 0, 7],
            &format!("{base}/other@x.iam.gserviceaccount.com/token"),
            true,
        )
        .await;
        assert_eq!(s, StatusCode::NOT_FOUND);
        let (s, _, _) = get([10, 42, 0, 7], &format!("{base}/default/identity"), true).await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        let (_, _, jwt) = get(
            [10, 42, 0, 7],
            &format!("{base}/default/identity?audience=https://svc"),
            true,
        )
        .await;
        assert_eq!(jwt.split('.').count(), 3);
    }

    #[tokio::test]
    async fn directories_list_and_recurse() {
        let (_, _, list) = get(
            [10, 42, 0, 7],
            "/computeMetadata/v1/instance/service-accounts/",
            true,
        )
        .await;
        assert_eq!(list, "default/\nweb@vat-local.iam.gserviceaccount.com/\n");
        let (_, h, json) = get(
            [10, 42, 0, 7],
            "/computeMetadata/v1/instance/service-accounts/default/?recursive=true",
            true,
        )
        .await;
        assert_eq!(h[header::CONTENT_TYPE], "application/json");
        let v: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["email"], "web@vat-local.iam.gserviceaccount.com");
        assert_eq!(v["scopes"][0], SCOPE);
        let (_, _, scopes) = get(
            [10, 42, 0, 7],
            "/computeMetadata/v1/instance/service-accounts/default/scopes",
            true,
        )
        .await;
        assert_eq!(scopes, format!("{SCOPE}\n"));
    }
}
