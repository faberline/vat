//! A minimal OCI Distribution registry server with filesystem storage.
//!
//! Reusable by tests (serve on any `TcpListener`) and by anything that needs
//! a local registry. Implements the subset of the OCI Distribution spec the
//! push/pull workflows use:
//!
//! - `GET /v2/` (API version check), `GET /v2/_catalog`, `GET /v2/<name>/tags/list`
//! - blobs: `HEAD`/`GET`/`DELETE /v2/<name>/blobs/<digest>`
//! - uploads: `POST /v2/<name>/blobs/uploads/` (session, or monolithic with
//!   `?digest=`, or cross-repo `?mount=`), `PATCH` (chunk, `Content-Range`
//!   checked), `PUT ...?digest=` (finalize), `GET` (status), `DELETE` (cancel)
//! - manifests: `PUT`/`GET`/`HEAD`/`DELETE /v2/<name>/manifests/<tag|digest>`
//!   (PUT verifies every referenced blob/manifest exists)
//!
//! Optional auth: HTTP Basic, or a Bearer token flow (`GET /token` with Basic
//! credentials returns a token; `/v2/` challenges with
//! `WWW-Authenticate: Bearer realm=...`). Blobs are global (not per-repo).
//! Request bodies are streamed to disk; response bodies are read into memory.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Router;
use base64::Engine as _;
use http_body_util::BodyExt;
use sha2::{Digest as _, Sha256};
use tokio::io::AsyncWriteExt;

/// Registry authentication mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Auth {
    None,
    Basic { user: String, password: String },
    /// Bearer tokens issued by `/token` to clients presenting these Basic
    /// credentials (anonymous token requests are refused).
    Bearer { user: String, password: String },
}

/// Server configuration.
#[derive(Debug, Clone)]
pub struct Config {
    /// Storage directory (created if missing).
    pub root: PathBuf,
    pub auth: Auth,
}

struct Registry {
    root: PathBuf,
    auth: Auth,
    token: String,
}

/// Build the axum router for a registry rooted at `config.root`.
pub fn router(config: Config) -> std::io::Result<Router> {
    std::fs::create_dir_all(config.root.join("blobs").join("sha256"))?;
    std::fs::create_dir_all(config.root.join("uploads"))?;
    std::fs::create_dir_all(config.root.join("repositories"))?;
    let mut token = [0u8; 16];
    getrandom::fill(&mut token).map_err(|e| std::io::Error::other(e.to_string()))?;
    let state = Arc::new(Registry {
        root: config.root,
        auth: config.auth,
        token: token.iter().map(|b| format!("{b:02x}")).collect(),
    });
    Ok(Router::new().fallback(handle).with_state(state))
}

/// Serve until the future is dropped / the process exits.
pub async fn serve(listener: tokio::net::TcpListener, config: Config) -> std::io::Result<()> {
    let app = router(config)?;
    axum::serve(listener, app).await
}

fn error(status: StatusCode, code: &str, message: &str) -> Response {
    let body = serde_json::json!({
        "errors": [{ "code": code, "message": message, "detail": serde_json::Value::Null }]
    });
    (
        status,
        [(header::CONTENT_TYPE, "application/json")],
        body.to_string(),
    )
        .into_response()
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.split('/').all(|part| {
            !part.is_empty()
                && part != "."
                && part != ".."
                && !part.starts_with('_')
                && part
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b))
        })
}

fn digest_hex(digest: &str) -> Option<&str> {
    let hex = digest.strip_prefix("sha256:")?;
    (hex.len() == 64 && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))).then_some(hex)
}

fn valid_tag(tag: &str) -> bool {
    !tag.is_empty()
        && tag.len() <= 128
        && tag.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}

fn percent_decode(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Some(v) = raw.get(i + 1..i + 3).and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(if bytes[i] == b'+' { b' ' } else { bytes[i] });
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

fn query_param(query: Option<&str>, key: &str) -> Option<String> {
    query?.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        (percent_decode(k) == key).then(|| percent_decode(v))
    })
}

/// What a `/v2/...` path addresses.
#[derive(Debug, PartialEq, Eq)]
enum Route {
    Base,
    Catalog,
    Tags(String),
    Manifest(String, String),
    Blob(String, String),
    UploadStart(String),
    Upload(String, String),
}

fn parse_route(path: &str) -> Option<Route> {
    let rest = path.strip_prefix("/v2")?;
    let rest = rest.strip_prefix('/').unwrap_or(rest);
    if rest.is_empty() {
        return Some(Route::Base);
    }
    if rest == "_catalog" {
        return Some(Route::Catalog);
    }
    let trimmed = rest.trim_end_matches('/');
    let segs: Vec<&str> = trimmed.split('/').collect();
    let n = segs.len();
    let name = |upto: usize| segs[..upto].join("/");
    if n >= 3 && segs[n - 2] == "blobs" && segs[n - 1] == "uploads" {
        return Some(Route::UploadStart(name(n - 2)));
    }
    if n >= 4 && segs[n - 3] == "blobs" && segs[n - 2] == "uploads" {
        return Some(Route::Upload(name(n - 3), segs[n - 1].to_string()));
    }
    if n >= 3 && segs[n - 2] == "tags" && segs[n - 1] == "list" {
        return Some(Route::Tags(name(n - 2)));
    }
    if n >= 3 && segs[n - 2] == "manifests" {
        return Some(Route::Manifest(name(n - 2), segs[n - 1].to_string()));
    }
    if n >= 3 && segs[n - 2] == "blobs" {
        return Some(Route::Blob(name(n - 2), segs[n - 1].to_string()));
    }
    None
}

impl Registry {
    fn blob_path(&self, digest: &str) -> Option<PathBuf> {
        Some(self.root.join("blobs").join("sha256").join(digest_hex(digest)?))
    }

    fn repo_dir(&self, name: &str) -> PathBuf {
        self.root.join("repositories").join(name)
    }

    fn tag_path(&self, name: &str, tag: &str) -> PathBuf {
        self.repo_dir(name).join("_manifests").join("tags").join(tag)
    }

    fn revision_path(&self, name: &str, digest: &str) -> Option<PathBuf> {
        Some(self.repo_dir(name).join("_manifests").join("revisions").join(digest_hex(digest)?))
    }

    fn upload_path(&self, uuid: &str) -> Option<PathBuf> {
        (uuid.len() == 32 && uuid.bytes().all(|b| b.is_ascii_hexdigit()))
            .then(|| self.root.join("uploads").join(uuid))
    }

    /// Resolve a manifest reference (tag or digest) to its digest.
    fn resolve_manifest(&self, name: &str, reference: &str) -> Option<String> {
        if digest_hex(reference).is_some() {
            return self
                .revision_path(name, reference)
                .filter(|p| p.is_file())
                .map(|_| reference.to_string());
        }
        if !valid_tag(reference) {
            return None;
        }
        std::fs::read_to_string(self.tag_path(name, reference))
            .ok()
            .map(|s| s.trim().to_string())
    }

    fn authorized(&self, headers: &HeaderMap) -> bool {
        let presented = headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        match &self.auth {
            Auth::None => true,
            Auth::Basic { user, password } => basic_matches(presented, user, password),
            Auth::Bearer { .. } => presented
                .strip_prefix("Bearer ")
                .is_some_and(|t| t.trim() == self.token),
        }
    }

    fn challenge(&self, headers: &HeaderMap, scope: Option<String>) -> Response {
        let value = match &self.auth {
            Auth::Bearer { .. } => {
                let host = headers
                    .get(header::HOST)
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("127.0.0.1");
                let mut v = format!("Bearer realm=\"http://{host}/token\",service=\"vat-registry\"");
                if let Some(scope) = scope {
                    v.push_str(&format!(",scope=\"{scope}\""));
                }
                v
            }
            _ => "Basic realm=\"vat-registry\"".to_string(),
        };
        let mut resp = error(StatusCode::UNAUTHORIZED, "UNAUTHORIZED", "authentication required");
        if let Ok(v) = HeaderValue::from_str(&value) {
            resp.headers_mut().insert(header::WWW_AUTHENTICATE, v);
        }
        resp
    }
}

fn basic_matches(presented: &str, user: &str, password: &str) -> bool {
    let Some(encoded) = presented.strip_prefix("Basic ") else { return false };
    let Ok(decoded) = base64::engine::general_purpose::STANDARD.decode(encoded.trim()) else {
        return false;
    };
    decoded == format!("{user}:{password}").into_bytes()
}

fn location(resp: &mut Response, value: &str) {
    if let Ok(v) = HeaderValue::from_str(value) {
        resp.headers_mut().insert(header::LOCATION, v);
    }
}

fn set_header(resp: &mut Response, name: &'static str, value: &str) {
    if let Ok(v) = HeaderValue::from_str(value) {
        resp.headers_mut().insert(name, v);
    }
}

/// Stream a request body onto the end of `path`; returns bytes written.
async fn append_body(path: &Path, body: Body) -> std::io::Result<u64> {
    let mut file = tokio::fs::OpenOptions::new().create(true).append(true).open(path).await?;
    let mut body = body;
    let mut written = 0u64;
    while let Some(frame) = body.frame().await {
        let frame = frame.map_err(|e| std::io::Error::other(e.to_string()))?;
        if let Ok(data) = frame.into_data() {
            file.write_all(&data).await?;
            written += data.len() as u64;
        }
    }
    file.flush().await?;
    file.sync_all().await?;
    Ok(written)
}

fn file_digest(path: &Path) -> std::io::Result<String> {
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!(
        "sha256:{}",
        hasher.finalize().iter().map(|b| format!("{b:02x}")).collect::<String>()
    ))
}

fn new_uuid() -> String {
    let mut buf = [0u8; 16];
    getrandom::fill(&mut buf).expect("OS randomness");
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

fn upload_response(status: StatusCode, name: &str, uuid: &str, size: u64) -> Response {
    let mut resp = status.into_response();
    location(&mut resp, &format!("/v2/{name}/blobs/uploads/{uuid}"));
    set_header(&mut resp, "Docker-Upload-UUID", uuid);
    let range = if size == 0 { "0-0".to_string() } else { format!("0-{}", size - 1) };
    set_header(&mut resp, "Range", &range);
    set_header(&mut resp, "Content-Length", "0");
    resp
}

fn blob_created(name: &str, digest: &str) -> Response {
    let mut resp = StatusCode::CREATED.into_response();
    location(&mut resp, &format!("/v2/{name}/blobs/{digest}"));
    set_header(&mut resp, "Docker-Content-Digest", digest);
    resp
}

/// Finalize an upload file into the blob store after verifying its digest.
#[allow(clippy::result_large_err)] // internal; the error is the HTTP response itself
fn commit_upload(reg: &Registry, upload: &Path, digest: &str) -> Result<(), Response> {
    let Some(target) = reg.blob_path(digest) else {
        let _ = std::fs::remove_file(upload);
        return Err(error(StatusCode::BAD_REQUEST, "DIGEST_INVALID", "unsupported or malformed digest"));
    };
    let actual = file_digest(upload).map_err(|e| {
        error(StatusCode::INTERNAL_SERVER_ERROR, "UNKNOWN", &e.to_string())
    })?;
    if actual != digest {
        let _ = std::fs::remove_file(upload);
        return Err(error(
            StatusCode::BAD_REQUEST,
            "DIGEST_INVALID",
            &format!("provided digest {digest} does not match content {actual}"),
        ));
    }
    if target.is_file() {
        let _ = std::fs::remove_file(upload);
    } else if let Err(e) = std::fs::rename(upload, &target) {
        return Err(error(StatusCode::INTERNAL_SERVER_ERROR, "UNKNOWN", &e.to_string()));
    }
    Ok(())
}

/// Referenced digests a manifest/index requires to already exist.
fn manifest_references(bytes: &[u8]) -> Result<(String, Vec<(String, bool)>), String> {
    let value: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|e| format!("manifest is not JSON: {e}"))?;
    let media_type = value
        .get("mediaType")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let mut refs = Vec::new();
    if let Some(config) = value.get("config") {
        let digest = config.get("digest").and_then(|d| d.as_str()).ok_or("config has no digest")?;
        refs.push((digest.to_string(), false));
        for layer in value.get("layers").and_then(|l| l.as_array()).cloned().unwrap_or_default() {
            let digest = layer.get("digest").and_then(|d| d.as_str()).ok_or("layer has no digest")?;
            refs.push((digest.to_string(), false));
        }
    } else if let Some(manifests) = value.get("manifests").and_then(|m| m.as_array()) {
        for m in manifests {
            let digest = m.get("digest").and_then(|d| d.as_str()).ok_or("manifest has no digest")?;
            refs.push((digest.to_string(), true));
        }
    } else {
        return Err("document is neither an image manifest nor an index".into());
    }
    Ok((media_type, refs))
}

fn list_repositories(root: &Path) -> Vec<String> {
    let base = root.join("repositories");
    let mut out = Vec::new();
    for entry in walkdir::WalkDir::new(&base).min_depth(1).into_iter().flatten() {
        if entry.file_type().is_dir() && entry.file_name() == "_manifests" {
            if let Some(parent) = entry.path().parent() {
                if let Ok(rel) = parent.strip_prefix(&base) {
                    out.push(rel.to_string_lossy().to_string());
                }
            }
        }
    }
    out.sort();
    out
}

async fn handle(State(reg): State<Arc<Registry>>, req: Request) -> Response {
    let (parts, body) = req.into_parts();
    let path = parts.uri.path().to_string();
    let query = parts.uri.query().map(str::to_string);
    let method = parts.method.clone();
    let headers = parts.headers;

    if path == "/token" {
        return match &reg.auth {
            Auth::Bearer { user, password } => {
                let presented = headers
                    .get(header::AUTHORIZATION)
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("");
                if basic_matches(presented, user, password) {
                    let body = serde_json::json!({ "token": reg.token, "access_token": reg.token, "expires_in": 3600 });
                    ([(header::CONTENT_TYPE, "application/json")], body.to_string()).into_response()
                } else {
                    error(StatusCode::UNAUTHORIZED, "UNAUTHORIZED", "invalid credentials")
                }
            }
            _ => error(StatusCode::NOT_FOUND, "UNSUPPORTED", "token auth is not enabled"),
        };
    }

    let Some(route) = parse_route(&path) else {
        return error(StatusCode::NOT_FOUND, "NOT_FOUND", "unknown endpoint");
    };
    if !reg.authorized(&headers) {
        let scope = match &route {
            Route::Base | Route::Catalog => None,
            Route::Tags(n) | Route::Manifest(n, _) | Route::Blob(n, _) | Route::UploadStart(n) | Route::Upload(n, _) => {
                let actions = if matches!(method, Method::GET | Method::HEAD) { "pull" } else { "pull,push" };
                Some(format!("repository:{n}:{actions}"))
            }
        };
        return reg.challenge(&headers, scope);
    }
    let mut resp = route_request(&reg, route, &method, query.as_deref(), &headers, body).await;
    resp.headers_mut().insert(
        "Docker-Distribution-API-Version",
        HeaderValue::from_static("registry/2.0"),
    );
    resp
}

async fn route_request(
    reg: &Registry,
    route: Route,
    method: &Method,
    query: Option<&str>,
    headers: &HeaderMap,
    body: Body,
) -> Response {
    let name_of = |route: &Route| -> Option<String> {
        match route {
            Route::Tags(n) | Route::Manifest(n, _) | Route::Blob(n, _) | Route::UploadStart(n) | Route::Upload(n, _) => Some(n.clone()),
            _ => None,
        }
    };
    if let Some(name) = name_of(&route) {
        if !valid_name(&name) {
            return error(StatusCode::BAD_REQUEST, "NAME_INVALID", "invalid repository name");
        }
    }
    match (route, method.clone()) {
        (Route::Base, Method::GET | Method::HEAD) => {
            ([(header::CONTENT_TYPE, "application/json")], "{}").into_response()
        }
        (Route::Catalog, Method::GET) => {
            let repos = list_repositories(&reg.root);
            ([(header::CONTENT_TYPE, "application/json")], serde_json::json!({ "repositories": repos }).to_string())
                .into_response()
        }
        (Route::Tags(name), Method::GET) => {
            let dir = reg.repo_dir(&name).join("_manifests").join("tags");
            let Ok(entries) = std::fs::read_dir(&dir) else {
                return error(StatusCode::NOT_FOUND, "NAME_UNKNOWN", "repository name not known to registry");
            };
            let mut tags: Vec<String> = entries
                .flatten()
                .map(|e| e.file_name().to_string_lossy().to_string())
                .collect();
            tags.sort();
            ([(header::CONTENT_TYPE, "application/json")], serde_json::json!({ "name": name, "tags": tags }).to_string())
                .into_response()
        }
        (Route::Blob(_, digest), m @ (Method::GET | Method::HEAD)) => {
            let Some(path) = reg.blob_path(&digest) else {
                return error(StatusCode::BAD_REQUEST, "DIGEST_INVALID", "malformed digest");
            };
            let Ok(meta) = std::fs::metadata(&path) else {
                return error(StatusCode::NOT_FOUND, "BLOB_UNKNOWN", "blob unknown to registry");
            };
            let mut resp = if m == Method::HEAD {
                StatusCode::OK.into_response()
            } else {
                match tokio::fs::read(&path).await {
                    Ok(bytes) => Body::from(bytes).into_response(),
                    Err(e) => return error(StatusCode::INTERNAL_SERVER_ERROR, "UNKNOWN", &e.to_string()),
                }
            };
            set_header(&mut resp, "Content-Length", &meta.len().to_string());
            set_header(&mut resp, "Content-Type", "application/octet-stream");
            set_header(&mut resp, "Docker-Content-Digest", &digest);
            resp
        }
        (Route::Blob(_, digest), Method::DELETE) => match reg.blob_path(&digest) {
            Some(path) if path.is_file() => {
                let _ = std::fs::remove_file(path);
                StatusCode::ACCEPTED.into_response()
            }
            _ => error(StatusCode::NOT_FOUND, "BLOB_UNKNOWN", "blob unknown to registry"),
        },
        (Route::UploadStart(name), Method::POST) => {
            if let Some(mount) = query_param(query, "mount") {
                if reg.blob_path(&mount).is_some_and(|p| p.is_file()) {
                    return blob_created(&name, &mount);
                }
            }
            let uuid = new_uuid();
            let upload = reg.root.join("uploads").join(&uuid);
            if let Some(digest) = query_param(query, "digest") {
                if let Err(e) = append_body(&upload, body).await {
                    return error(StatusCode::INTERNAL_SERVER_ERROR, "UNKNOWN", &e.to_string());
                }
                return match commit_upload(reg, &upload, &digest) {
                    Ok(()) => blob_created(&name, &digest),
                    Err(resp) => resp,
                };
            }
            if let Err(e) = std::fs::write(&upload, b"") {
                return error(StatusCode::INTERNAL_SERVER_ERROR, "UNKNOWN", &e.to_string());
            }
            upload_response(StatusCode::ACCEPTED, &name, &uuid, 0)
        }
        (Route::Upload(name, uuid), method) => {
            let Some(upload) = reg.upload_path(&uuid).filter(|p| p.is_file()) else {
                return error(StatusCode::NOT_FOUND, "BLOB_UPLOAD_UNKNOWN", "blob upload unknown to registry");
            };
            let size = std::fs::metadata(&upload).map(|m| m.len()).unwrap_or(0);
            match method {
                Method::GET => upload_response(StatusCode::NO_CONTENT, &name, &uuid, size),
                Method::DELETE => {
                    let _ = std::fs::remove_file(&upload);
                    StatusCode::NO_CONTENT.into_response()
                }
                Method::PATCH => {
                    if let Some(range) = headers.get(header::CONTENT_RANGE).and_then(|v| v.to_str().ok()) {
                        let start = range
                            .trim_start_matches("bytes ")
                            .split('-')
                            .next()
                            .and_then(|s| s.trim().parse::<u64>().ok());
                        if start != Some(size) {
                            let mut resp = error(
                                StatusCode::RANGE_NOT_SATISFIABLE,
                                "BLOB_UPLOAD_INVALID",
                                &format!("chunk must start at offset {size}"),
                            );
                            set_header(&mut resp, "Range", &format!("0-{}", size.saturating_sub(1)));
                            location(&mut resp, &format!("/v2/{name}/blobs/uploads/{uuid}"));
                            return resp;
                        }
                    }
                    match append_body(&upload, body).await {
                        Ok(n) => upload_response(StatusCode::ACCEPTED, &name, &uuid, size + n),
                        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, "UNKNOWN", &e.to_string()),
                    }
                }
                Method::PUT => {
                    let Some(digest) = query_param(query, "digest") else {
                        return error(StatusCode::BAD_REQUEST, "DIGEST_INVALID", "digest query parameter required");
                    };
                    if let Err(e) = append_body(&upload, body).await {
                        return error(StatusCode::INTERNAL_SERVER_ERROR, "UNKNOWN", &e.to_string());
                    }
                    match commit_upload(reg, &upload, &digest) {
                        Ok(()) => blob_created(&name, &digest),
                        Err(resp) => resp,
                    }
                }
                _ => error(StatusCode::METHOD_NOT_ALLOWED, "UNSUPPORTED", "method not allowed"),
            }
        }
        (Route::Manifest(name, reference), Method::PUT) => {
            let tmp = reg.root.join("uploads").join(new_uuid());
            if let Err(e) = append_body(&tmp, body).await {
                return error(StatusCode::INTERNAL_SERVER_ERROR, "UNKNOWN", &e.to_string());
            }
            let bytes = match std::fs::read(&tmp) {
                Ok(b) => b,
                Err(e) => return error(StatusCode::INTERNAL_SERVER_ERROR, "UNKNOWN", &e.to_string()),
            };
            let digest = match file_digest(&tmp) {
                Ok(d) => d,
                Err(e) => return error(StatusCode::INTERNAL_SERVER_ERROR, "UNKNOWN", &e.to_string()),
            };
            let (body_type, refs) = match manifest_references(&bytes) {
                Ok(v) => v,
                Err(msg) => {
                    let _ = std::fs::remove_file(&tmp);
                    return error(StatusCode::BAD_REQUEST, "MANIFEST_INVALID", &msg);
                }
            };
            for (dep, is_manifest) in &refs {
                let present = if *is_manifest {
                    reg.revision_path(&name, dep).is_some_and(|p| p.is_file())
                } else {
                    reg.blob_path(dep).is_some_and(|p| p.is_file())
                };
                if !present {
                    let _ = std::fs::remove_file(&tmp);
                    return error(
                        StatusCode::BAD_REQUEST,
                        if *is_manifest { "MANIFEST_UNKNOWN" } else { "MANIFEST_BLOB_UNKNOWN" },
                        &format!("referenced {dep} is unknown to the registry"),
                    );
                }
            }
            let is_digest_ref = digest_hex(&reference).is_some();
            if is_digest_ref && reference != digest {
                let _ = std::fs::remove_file(&tmp);
                return error(StatusCode::BAD_REQUEST, "DIGEST_INVALID", "manifest digest does not match reference");
            }
            if !is_digest_ref && !valid_tag(&reference) {
                let _ = std::fs::remove_file(&tmp);
                return error(StatusCode::BAD_REQUEST, "TAG_INVALID", "invalid tag");
            }
            let media_type = headers
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string)
                .filter(|s| !s.is_empty())
                .or_else(|| (!body_type.is_empty()).then_some(body_type))
                .unwrap_or_else(|| "application/vnd.oci.image.manifest.v1+json".into());
            if let Err(resp) = commit_upload(reg, &tmp, &digest) {
                return resp;
            }
            let revision = reg.revision_path(&name, &digest).expect("valid digest");
            let result = (|| -> std::io::Result<()> {
                std::fs::create_dir_all(revision.parent().expect("has parent"))?;
                std::fs::write(&revision, &media_type)?;
                if !is_digest_ref {
                    let tag = reg.tag_path(&name, &reference);
                    std::fs::create_dir_all(tag.parent().expect("has parent"))?;
                    std::fs::write(&tag, &digest)?;
                }
                Ok(())
            })();
            if let Err(e) = result {
                return error(StatusCode::INTERNAL_SERVER_ERROR, "UNKNOWN", &e.to_string());
            }
            let mut resp = StatusCode::CREATED.into_response();
            location(&mut resp, &format!("/v2/{name}/manifests/{digest}"));
            set_header(&mut resp, "Docker-Content-Digest", &digest);
            resp
        }
        (Route::Manifest(name, reference), m @ (Method::GET | Method::HEAD)) => {
            let Some(digest) = reg.resolve_manifest(&name, &reference) else {
                return error(StatusCode::NOT_FOUND, "MANIFEST_UNKNOWN", "manifest unknown to registry");
            };
            let media_type = reg
                .revision_path(&name, &digest)
                .and_then(|p| std::fs::read_to_string(p).ok())
                .unwrap_or_else(|| "application/vnd.oci.image.manifest.v1+json".into());
            let Some(path) = reg.blob_path(&digest) else {
                return error(StatusCode::NOT_FOUND, "MANIFEST_UNKNOWN", "manifest unknown to registry");
            };
            let bytes = match std::fs::read(&path) {
                Ok(b) => b,
                Err(_) => return error(StatusCode::NOT_FOUND, "MANIFEST_UNKNOWN", "manifest blob missing"),
            };
            let len = bytes.len();
            let mut resp = if m == Method::HEAD {
                StatusCode::OK.into_response()
            } else {
                Body::from(bytes).into_response()
            };
            set_header(&mut resp, "Content-Type", &media_type);
            set_header(&mut resp, "Content-Length", &len.to_string());
            set_header(&mut resp, "Docker-Content-Digest", &digest);
            resp
        }
        (Route::Manifest(name, reference), Method::DELETE) => {
            if digest_hex(&reference).is_none() {
                return error(StatusCode::BAD_REQUEST, "UNSUPPORTED", "delete manifests by digest");
            }
            let Some(revision) = reg.revision_path(&name, &reference).filter(|p| p.is_file()) else {
                return error(StatusCode::NOT_FOUND, "MANIFEST_UNKNOWN", "manifest unknown to registry");
            };
            let _ = std::fs::remove_file(revision);
            let tags_dir = reg.repo_dir(&name).join("_manifests").join("tags");
            if let Ok(entries) = std::fs::read_dir(&tags_dir) {
                for entry in entries.flatten() {
                    if std::fs::read_to_string(entry.path()).is_ok_and(|d| d.trim() == reference) {
                        let _ = std::fs::remove_file(entry.path());
                    }
                }
            }
            StatusCode::ACCEPTED.into_response()
        }
        _ => error(StatusCode::METHOD_NOT_ALLOWED, "UNSUPPORTED", "method not allowed"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_parse_from_the_right() {
        assert_eq!(parse_route("/v2/"), Some(Route::Base));
        assert_eq!(parse_route("/v2/_catalog"), Some(Route::Catalog));
        assert_eq!(parse_route("/v2/a/b/tags/list"), Some(Route::Tags("a/b".into())));
        assert_eq!(
            parse_route("/v2/team/app/manifests/v1"),
            Some(Route::Manifest("team/app".into(), "v1".into()))
        );
        assert_eq!(
            parse_route("/v2/app/blobs/uploads/"),
            Some(Route::UploadStart("app".into()))
        );
        assert_eq!(
            parse_route("/v2/x/blobs/uploads/abc"),
            Some(Route::Upload("x".into(), "abc".into()))
        );
        // A repository may itself be named "blobs".
        assert_eq!(
            parse_route("/v2/blobs/blobs/sha256:00"),
            Some(Route::Blob("blobs".into(), "sha256:00".into()))
        );
        assert_eq!(parse_route("/other"), None);
    }

    #[test]
    fn query_and_names() {
        assert_eq!(
            query_param(Some("a=1&digest=sha256%3Aab"), "digest").as_deref(),
            Some("sha256:ab")
        );
        assert!(valid_name("team/app"));
        assert!(!valid_name("../x"));
        assert!(!valid_name("Team"));
        assert!(!valid_name("a/_manifests"));
    }

    #[test]
    fn manifest_reference_extraction() {
        let m = br#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","config":{"digest":"sha256:a"},"layers":[{"digest":"sha256:b"}]}"#;
        let (mt, refs) = manifest_references(m).unwrap();
        assert_eq!(mt, "application/vnd.oci.image.manifest.v1+json");
        assert_eq!(refs, vec![("sha256:a".into(), false), ("sha256:b".into(), false)]);
        assert!(manifest_references(b"{}").is_err());
    }
}
