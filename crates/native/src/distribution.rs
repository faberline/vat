//! OCI Distribution client for `vat image pull` / `vat image push`.
//!
//! - Auth: anonymous first; on `401` the `WWW-Authenticate` challenge is
//!   answered with a Bearer token (fetched from the challenge realm, using
//!   Basic credentials when configured) or with Basic credentials.
//!   Credentials come from `$DOCKER_CONFIG/config.json` or
//!   `~/.docker/config.json` `auths` entries (`auth` or `username`/`password`);
//!   credential helpers (`credsStore` / `credHelpers`) are not supported.
//! - Transport: HTTPS, except plain HTTP for `localhost`, `127.0.0.1`, `[::1]`
//!   (any port) and hosts listed in `VAT_INSECURE_REGISTRIES`
//!   (comma-separated `host[:port]`).
//! - Pull selects the `darwin/arm64` entry from an index, verifies every
//!   digest, checks the config platform before fetching layers, and streams
//!   layers to disk.
//! - Push uploads missing blobs (POST + monolithic PUT; blobs are read into
//!   memory) and then PUTs the manifest under the destination tag.

use std::io::Write as _;
use std::path::PathBuf;

use anyhow::{anyhow, bail, Context, Result};
use base64::Engine as _;
use reqwest::header::{
    HeaderMap, HeaderValue, ACCEPT, AUTHORIZATION, CONTENT_TYPE, LOCATION, WWW_AUTHENTICATE,
};
use reqwest::{Method, StatusCode};
use serde::Serialize;
use sha2::{Digest as _, Sha256};

use super::oci::{
    self, digest_hex, sha256_digest, ImageConfig, Index, Manifest, Reference, MT_DOCKER_LAYER_GZIP,
    MT_DOCKER_LIST, MT_DOCKER_MANIFEST, MT_OCI_INDEX, MT_OCI_LAYER_GZIP, MT_OCI_LAYER_TAR,
    MT_OCI_MANIFEST,
};
use super::store::ImageStore;

const MANIFEST_ACCEPT: &str = "application/vnd.oci.image.index.v1+json, application/vnd.oci.image.manifest.v1+json, application/vnd.docker.distribution.manifest.list.v2+json, application/vnd.docker.distribution.manifest.v2+json";

#[derive(Debug, Clone, Serialize)]
pub struct PullOutcome {
    pub reference: String,
    /// Local name the image was tagged as (absent for digest-only pulls).
    pub tagged: Option<String>,
    pub digest: String,
    pub layers: usize,
    pub fetched_blobs: usize,
    pub cached_blobs: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct PushOutcome {
    pub source: String,
    pub destination: String,
    pub digest: String,
    pub uploaded_blobs: usize,
    pub existing_blobs: usize,
}

/// Whether `host[:port]` is reached over plain HTTP.
pub fn insecure_host(host: &str) -> bool {
    let bare = if let Some(rest) = host.strip_prefix('[') {
        rest.split(']').next().unwrap_or(rest).to_string()
    } else {
        host.rsplit_once(':')
            .map(|(h, _)| h)
            .unwrap_or(host)
            .to_string()
    };
    if matches!(bare.as_str(), "localhost" | "127.0.0.1" | "::1") {
        return true;
    }
    std::env::var("VAT_INSECURE_REGISTRIES")
        .map(|list| {
            list.split(',')
                .map(str::trim)
                .any(|h| !h.is_empty() && (h == host || h == bare))
        })
        .unwrap_or(false)
}

fn docker_config_path() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("DOCKER_CONFIG") {
        return Some(PathBuf::from(dir).join("config.json"));
    }
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".docker").join("config.json"))
}

fn normalize_auth_key(key: &str) -> String {
    let key = key
        .trim_start_matches("https://")
        .trim_start_matches("http://");
    let host = key.split('/').next().unwrap_or(key);
    match host {
        "index.docker.io" | "docker.io" | "registry-1.docker.io" => "registry-1.docker.io".into(),
        other => other.to_string(),
    }
}

/// Basic credentials for `registry_host` from a Docker config document.
pub fn credentials_from_config(
    config: &serde_json::Value,
    registry_host: &str,
) -> Option<(String, String)> {
    let auths = config.get("auths")?.as_object()?;
    let wanted = normalize_auth_key(registry_host);
    for (key, entry) in auths {
        if normalize_auth_key(key) != wanted {
            continue;
        }
        if let Some(encoded) = entry
            .get("auth")
            .and_then(|a| a.as_str())
            .filter(|s| !s.is_empty())
        {
            let decoded = base64::engine::general_purpose::STANDARD
                .decode(encoded.trim())
                .ok()?;
            let decoded = String::from_utf8(decoded).ok()?;
            let (user, pass) = decoded.split_once(':')?;
            return Some((user.to_string(), pass.to_string()));
        }
        let user = entry.get("username").and_then(|u| u.as_str());
        let pass = entry.get("password").and_then(|p| p.as_str());
        if let (Some(user), Some(pass)) = (user, pass) {
            return Some((user.to_string(), pass.to_string()));
        }
    }
    None
}

fn load_credentials(registry_host: &str) -> Option<(String, String)> {
    let path = docker_config_path()?;
    let bytes = std::fs::read(path).ok()?;
    let config: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    credentials_from_config(&config, registry_host)
}

/// Parse a `WWW-Authenticate` challenge into `(scheme, params)`.
pub fn parse_challenge(raw: &str) -> (String, Vec<(String, String)>) {
    let raw = raw.trim();
    let (scheme, rest) = raw.split_once(' ').unwrap_or((raw, ""));
    let mut params = Vec::new();
    let mut chars = rest.chars().peekable();
    loop {
        while matches!(chars.peek(), Some(c) if *c == ',' || c.is_whitespace()) {
            chars.next();
        }
        let mut key = String::new();
        while let Some(&c) = chars.peek() {
            if c == '=' {
                break;
            }
            key.push(c);
            chars.next();
        }
        if key.is_empty() || chars.next().is_none() {
            break;
        }
        let mut value = String::new();
        if chars.peek() == Some(&'"') {
            chars.next();
            while let Some(c) = chars.next() {
                match c {
                    '\\' => {
                        if let Some(n) = chars.next() {
                            value.push(n);
                        }
                    }
                    '"' => break,
                    other => value.push(other),
                }
            }
        } else {
            while let Some(&c) = chars.peek() {
                if c == ',' {
                    break;
                }
                value.push(c);
                chars.next();
            }
        }
        params.push((key.trim().to_ascii_lowercase(), value.trim().to_string()));
    }
    (scheme.to_ascii_lowercase(), params)
}

struct Client {
    http: reqwest::Client,
    base: String,
    host: String,
    repository: String,
    actions: &'static str,
    credentials: Option<(String, String)>,
    authorization: Option<String>,
}

impl Client {
    fn new(reference: &Reference, actions: &'static str) -> Result<Self> {
        let host = reference.registry_host();
        let scheme = if insecure_host(&host) {
            "http"
        } else {
            "https"
        };
        let http = reqwest::Client::builder()
            .user_agent(format!("vat/{}", vat_core::VERSION))
            .connect_timeout(std::time::Duration::from_secs(30))
            .build()
            .context("build HTTP client")?;
        Ok(Client {
            http,
            base: format!("{scheme}://{host}"),
            credentials: load_credentials(&host),
            host,
            repository: reference.remote_repository(),
            actions,
            authorization: None,
        })
    }

    fn url(&self, path: &str) -> String {
        format!("{}/v2/{}/{}", self.base, self.repository, path)
    }

    fn absolute(&self, location: &str) -> String {
        if location.starts_with("http://") || location.starts_with("https://") {
            location.to_string()
        } else if location.starts_with('/') {
            format!("{}{location}", self.base)
        } else {
            format!("{}/{location}", self.base)
        }
    }

    async fn authenticate(&mut self, challenge: &str) -> Result<()> {
        let (scheme, params) = parse_challenge(challenge);
        let param = |name: &str| {
            params
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.clone())
        };
        match scheme.as_str() {
            "bearer" => {
                let realm = param("realm")
                    .ok_or_else(|| anyhow!("Bearer challenge from {} has no realm", self.host))?;
                let scope = param("scope")
                    .unwrap_or_else(|| format!("repository:{}:{}", self.repository, self.actions));
                let mut query: Vec<(&str, String)> = vec![("scope", scope)];
                if let Some(service) = param("service") {
                    query.push(("service", service));
                }
                let mut request = self.http.get(&realm).query(&query);
                if let Some((user, pass)) = &self.credentials {
                    request = request.basic_auth(user, Some(pass));
                }
                let response = request
                    .send()
                    .await
                    .with_context(|| format!("request token from {realm}"))?;
                if !response.status().is_success() {
                    bail!(
                        "token request to {realm} failed with HTTP {}{}",
                        response.status(),
                        if self.credentials.is_none() {
                            " (no credentials configured in the Docker config)"
                        } else {
                            ""
                        }
                    );
                }
                let body: serde_json::Value =
                    response.json().await.context("parse token response")?;
                let token = body
                    .get("token")
                    .or_else(|| body.get("access_token"))
                    .and_then(|t| t.as_str())
                    .ok_or_else(|| anyhow!("token response from {realm} has no token"))?;
                self.authorization = Some(format!("Bearer {token}"));
            }
            "basic" => {
                let (user, pass) = self.credentials.as_ref().ok_or_else(|| {
                    anyhow!(
                        "{} requires Basic credentials; add them to the Docker config (`auths`)",
                        self.host
                    )
                })?;
                let encoded =
                    base64::engine::general_purpose::STANDARD.encode(format!("{user}:{pass}"));
                self.authorization = Some(format!("Basic {encoded}"));
            }
            other => bail!("unsupported auth scheme {other:?} from {}", self.host),
        }
        Ok(())
    }

    /// Send a request, answering at most one auth challenge.
    async fn send(
        &mut self,
        method: Method,
        url: &str,
        headers: HeaderMap,
        body: Option<&[u8]>,
    ) -> Result<reqwest::Response> {
        let mut attempt = 0;
        loop {
            let mut request = self
                .http
                .request(method.clone(), url)
                .headers(headers.clone());
            if let Some(auth) = &self.authorization {
                request = request.header(AUTHORIZATION, auth);
            }
            if let Some(body) = body {
                request = request.body(body.to_vec());
            }
            let response = request
                .send()
                .await
                .with_context(|| format!("{method} {url}"))?;
            if response.status() == StatusCode::UNAUTHORIZED && attempt == 0 {
                let challenge = response
                    .headers()
                    .get(WWW_AUTHENTICATE)
                    .and_then(|v| v.to_str().ok())
                    .ok_or_else(|| anyhow!("{method} {url}: HTTP 401 without an auth challenge"))?
                    .to_string();
                self.authenticate(&challenge).await?;
                attempt += 1;
                continue;
            }
            return Ok(response);
        }
    }
}

async fn failure(context: String, response: reqwest::Response) -> anyhow::Error {
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    let detail = serde_json::from_str::<serde_json::Value>(&body)
        .ok()
        .and_then(|v| {
            v.get("errors")?.as_array()?.first().map(|e| {
                format!(
                    "{}: {}",
                    e.get("code").and_then(|c| c.as_str()).unwrap_or("ERROR"),
                    e.get("message").and_then(|m| m.as_str()).unwrap_or("")
                )
            })
        })
        .unwrap_or_else(|| body.chars().take(200).collect());
    anyhow!("{context}: HTTP {status} {detail}")
}

fn runtime() -> Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("start async runtime")
}

fn accept_manifests() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(ACCEPT, HeaderValue::from_static(MANIFEST_ACCEPT));
    headers
}

async fn fetch_manifest(
    client: &mut Client,
    reference: &str,
    expected: Option<&str>,
) -> Result<(String, Vec<u8>, String)> {
    let url = client.url(&format!("manifests/{reference}"));
    let response = client
        .send(Method::GET, &url, accept_manifests(), None)
        .await?;
    if !response.status().is_success() {
        return Err(failure(
            format!("fetch manifest {reference} from {}", client.host),
            response,
        )
        .await);
    }
    let content_type = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.split(';').next().unwrap_or(s).trim().to_string());
    let bytes = response
        .bytes()
        .await
        .context("read manifest body")?
        .to_vec();
    let digest = sha256_digest(&bytes);
    if let Some(expected) = expected {
        if expected != digest {
            bail!("manifest digest mismatch: expected {expected}, registry served {digest}");
        }
    }
    let media_type = content_type
        .filter(|t| !t.is_empty() && t != "application/json" && t != "application/octet-stream")
        .or_else(|| {
            serde_json::from_slice::<serde_json::Value>(&bytes)
                .ok()
                .and_then(|v| {
                    v.get("mediaType")
                        .and_then(|m| m.as_str())
                        .map(str::to_string)
                })
        })
        .unwrap_or_default();
    Ok((digest, bytes, media_type))
}

async fn fetch_small_blob(client: &mut Client, digest: &str) -> Result<Vec<u8>> {
    let url = client.url(&format!("blobs/{digest}"));
    let response = client
        .send(Method::GET, &url, HeaderMap::new(), None)
        .await?;
    if !response.status().is_success() {
        return Err(failure(format!("fetch blob {digest}"), response).await);
    }
    let bytes = response.bytes().await.context("read blob")?.to_vec();
    let actual = sha256_digest(&bytes);
    if actual != digest {
        bail!("blob digest mismatch: expected {digest}, got {actual}");
    }
    Ok(bytes)
}

async fn fetch_blob_to_store(client: &mut Client, store: &ImageStore, digest: &str) -> Result<u64> {
    let url = client.url(&format!("blobs/{digest}"));
    let mut response = client
        .send(Method::GET, &url, HeaderMap::new(), None)
        .await?;
    if !response.status().is_success() {
        return Err(failure(format!("fetch blob {digest}"), response).await);
    }
    let tmp = store
        .tmp_dir()?
        .join(format!("pull-{}", super::random_hex(8)));
    let result = async {
        let mut file = std::fs::File::create(&tmp)?;
        let mut hasher = Sha256::new();
        let mut size = 0u64;
        while let Some(chunk) = response.chunk().await.context("read blob body")? {
            hasher.update(&chunk);
            file.write_all(&chunk)?;
            size += chunk.len() as u64;
        }
        file.sync_all()?;
        let actual = format!("sha256:{}", oci::hex(&hasher.finalize()));
        if actual != digest {
            bail!("blob digest mismatch: expected {digest}, got {actual}");
        }
        store.adopt(&tmp, digest)?;
        Ok(size)
    }
    .await;
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// Pull `raw` (`[registry/]repo[:tag][@digest]`) into the local store.
pub fn pull(store: &ImageStore, raw: &str) -> Result<PullOutcome> {
    let reference = Reference::parse(raw)?;
    runtime()?.block_on(pull_async(store, &reference, raw))
}

async fn pull_async(store: &ImageStore, reference: &Reference, raw: &str) -> Result<PullOutcome> {
    let mut client = Client::new(reference, "pull")?;
    let (mut digest, mut bytes, mut media_type) = fetch_manifest(
        &mut client,
        &reference.remote_reference(),
        reference.digest.as_deref(),
    )
    .await?;
    if media_type == MT_OCI_INDEX || media_type == MT_DOCKER_LIST {
        let index: Index = serde_json::from_slice(&bytes).context("parse image index")?;
        let platforms: Vec<String> = index
            .manifests
            .iter()
            .filter_map(|m| {
                m.platform
                    .as_ref()
                    .map(|p| format!("{}/{}", p.os, p.architecture))
            })
            .collect();
        let chosen = index
            .manifests
            .iter()
            .find(|m| m.platform.as_ref().is_some_and(|p| p.is_darwin_arm64()))
            .ok_or_else(|| {
                anyhow!(
                    "{raw} has no {}/{} manifest (available: {})",
                    oci::OS,
                    oci::ARCH,
                    if platforms.is_empty() {
                        "none".to_string()
                    } else {
                        platforms.join(", ")
                    }
                )
            })?
            .digest
            .clone();
        (digest, bytes, media_type) = fetch_manifest(&mut client, &chosen, Some(&chosen)).await?;
    }
    if media_type != MT_OCI_MANIFEST && media_type != MT_DOCKER_MANIFEST {
        bail!("unsupported manifest media type {media_type:?} for {raw}");
    }
    let manifest: Manifest = serde_json::from_slice(&bytes).context("parse image manifest")?;
    let config_bytes = fetch_small_blob(&mut client, &manifest.config.digest).await?;
    let config: ImageConfig =
        serde_json::from_slice(&config_bytes).context("parse image config")?;
    config
        .require_darwin_arm64()
        .with_context(|| format!("refusing to pull {raw}"))?;
    for layer in &manifest.layers {
        if ![MT_OCI_LAYER_GZIP, MT_OCI_LAYER_TAR, MT_DOCKER_LAYER_GZIP]
            .contains(&layer.media_type.as_str())
        {
            bail!(
                "unsupported layer media type {:?} in {raw}",
                layer.media_type
            );
        }
        digest_hex(&layer.digest)?;
    }
    let mut fetched = 0;
    let mut cached = 0;
    for layer in &manifest.layers {
        if store.has_blob(&layer.digest) {
            cached += 1;
            continue;
        }
        eprintln!("pull: fetching {} ({} bytes)", layer.digest, layer.size);
        fetch_blob_to_store(&mut client, store, &layer.digest).await?;
        fetched += 1;
    }
    store.put_blob(&config_bytes)?;
    let stored = store.put_blob(&bytes)?;
    debug_assert_eq!(stored, digest);
    store.load(&digest)?;
    let tagged = if reference.tag.is_some() || reference.digest.is_none() {
        Some(store.set_ref(&reference.local_name(), &digest)?)
    } else {
        None
    };
    Ok(PullOutcome {
        reference: raw.to_string(),
        tagged,
        digest,
        layers: manifest.layers.len(),
        fetched_blobs: fetched,
        cached_blobs: cached,
    })
}

/// Push local image `source` to `destination` (default: `source`, which must
/// then name a registry or Docker Hub repository).
pub fn push(store: &ImageStore, source: &str, destination: Option<&str>) -> Result<PushOutcome> {
    let image = store.resolve(source)?;
    let dest_raw = destination.unwrap_or(source);
    let dest = Reference::parse(dest_raw)?;
    if dest.digest.is_some() {
        bail!("push destination {dest_raw:?} must be a tag, not a digest");
    }
    runtime()?.block_on(async {
        let mut client = Client::new(&dest, "pull,push")?;
        let mut blobs: Vec<String> = image
            .manifest
            .layers
            .iter()
            .map(|l| l.digest.clone())
            .collect();
        blobs.push(image.manifest.config.digest.clone());
        let mut uploaded = 0;
        let mut existing = 0;
        for digest in &blobs {
            let head = client
                .send(
                    Method::HEAD,
                    &client.url(&format!("blobs/{digest}")),
                    HeaderMap::new(),
                    None,
                )
                .await?;
            if head.status().is_success() {
                existing += 1;
                continue;
            }
            let data = store.read_blob(digest)?;
            let start = client
                .send(
                    Method::POST,
                    &client.url("blobs/uploads/"),
                    HeaderMap::new(),
                    None,
                )
                .await?;
            if start.status() != StatusCode::ACCEPTED {
                return Err(failure(
                    format!("start upload of {digest} to {}", client.host),
                    start,
                )
                .await);
            }
            let location = start
                .headers()
                .get(LOCATION)
                .and_then(|v| v.to_str().ok())
                .ok_or_else(|| anyhow!("upload start for {digest} returned no Location"))?
                .to_string();
            let mut target = client.absolute(&location);
            target.push(if target.contains('?') { '&' } else { '?' });
            target.push_str("digest=");
            target.push_str(&digest.replace(':', "%3A"));
            let mut headers = HeaderMap::new();
            headers.insert(
                CONTENT_TYPE,
                HeaderValue::from_static("application/octet-stream"),
            );
            eprintln!("push: uploading {digest} ({} bytes)", data.len());
            let put = client
                .send(Method::PUT, &target, headers, Some(&data))
                .await?;
            if put.status() != StatusCode::CREATED {
                return Err(failure(format!("upload {digest}"), put).await);
            }
            uploaded += 1;
        }
        let manifest_bytes = store.read_blob(&image.manifest_digest)?;
        let media_type = image
            .manifest
            .media_type
            .clone()
            .unwrap_or_else(|| MT_OCI_MANIFEST.to_string());
        let mut headers = HeaderMap::new();
        headers.insert(
            CONTENT_TYPE,
            HeaderValue::from_str(&media_type).context("manifest media type")?,
        );
        let url = client.url(&format!("manifests/{}", dest.tag_or_latest()));
        let put = client
            .send(Method::PUT, &url, headers, Some(&manifest_bytes))
            .await?;
        if put.status() != StatusCode::CREATED {
            return Err(failure(format!("put manifest {}", dest.tag_or_latest()), put).await);
        }
        Ok(PushOutcome {
            source: source.to_string(),
            destination: format!(
                "{}/{}:{}",
                dest.registry_host(),
                dest.remote_repository(),
                dest.tag_or_latest()
            ),
            digest: image.manifest_digest.clone(),
            uploaded_blobs: uploaded,
            existing_blobs: existing,
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn challenges_parse() {
        let (scheme, params) = parse_challenge(
            r#"Bearer realm="https://auth.example/token",service="registry.example",scope="repository:a/b:pull,push""#,
        );
        assert_eq!(scheme, "bearer");
        assert_eq!(
            params[0],
            ("realm".into(), "https://auth.example/token".into())
        );
        assert_eq!(params[1], ("service".into(), "registry.example".into()));
        assert_eq!(
            params[2],
            ("scope".into(), "repository:a/b:pull,push".into())
        );
        let (scheme, params) = parse_challenge(r#"Basic realm="x""#);
        assert_eq!(scheme, "basic");
        assert_eq!(params.len(), 1);
    }

    #[test]
    fn credentials_match_hosts() {
        let config = serde_json::json!({
            "auths": {
                "https://index.docker.io/v1/": { "auth": base64::engine::general_purpose::STANDARD.encode("hub:secret") },
                "localhost:5000": { "username": "u", "password": "p" }
            }
        });
        assert_eq!(
            credentials_from_config(&config, "registry-1.docker.io"),
            Some(("hub".into(), "secret".into()))
        );
        assert_eq!(
            credentials_from_config(&config, "localhost:5000"),
            Some(("u".into(), "p".into()))
        );
        assert_eq!(credentials_from_config(&config, "ghcr.io"), None);
    }

    #[test]
    fn plain_http_only_for_local_hosts() {
        assert!(insecure_host("localhost:5000"));
        assert!(insecure_host("127.0.0.1:1234"));
        assert!(insecure_host("[::1]:5000"));
        assert!(!insecure_host("ghcr.io"));
        assert!(!insecure_host("registry-1.docker.io"));
    }
}
