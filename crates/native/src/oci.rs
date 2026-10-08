//! OCI image-spec types, media types, digests, and image references.

use std::collections::BTreeMap;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

pub const MT_OCI_MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";
pub const MT_OCI_INDEX: &str = "application/vnd.oci.image.index.v1+json";
pub const MT_OCI_CONFIG: &str = "application/vnd.oci.image.config.v1+json";
pub const MT_OCI_LAYER_GZIP: &str = "application/vnd.oci.image.layer.v1.tar+gzip";
pub const MT_OCI_LAYER_TAR: &str = "application/vnd.oci.image.layer.v1.tar";
pub const MT_DOCKER_MANIFEST: &str = "application/vnd.docker.distribution.manifest.v2+json";
pub const MT_DOCKER_LIST: &str = "application/vnd.docker.distribution.manifest.list.v2+json";
pub const MT_DOCKER_CONFIG: &str = "application/vnd.docker.container.image.v1+json";
pub const MT_DOCKER_LAYER_GZIP: &str = "application/vnd.docker.image.rootfs.diff.tar.gzip";

/// Layer annotation carrying the JSON list of [`super::root::Relocation`]s.
pub const ANN_RELOCATIONS: &str = "vat.relocations";
/// Manifest annotation recording the fixed root length the image targets.
pub const ANN_ROOT_LENGTH: &str = "vat.root-length";
pub const ANN_REF_NAME: &str = "org.opencontainers.image.ref.name";
pub const ANN_CONTAINERD_NAME: &str = "io.containerd.image.name";
pub const ANN_CREATED: &str = "org.opencontainers.image.created";

/// The only platform the native runtime runs.
pub const OS: &str = "darwin";
pub const ARCH: &str = "arm64";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Platform {
    pub architecture: String,
    pub os: String,
    #[serde(
        rename = "os.version",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub os_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,
}

impl Platform {
    pub fn darwin_arm64() -> Self {
        Platform {
            architecture: ARCH.into(),
            os: OS.into(),
            os_version: None,
            variant: None,
        }
    }

    pub fn is_darwin_arm64(&self) -> bool {
        self.os == OS && self.architecture == ARCH
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Descriptor {
    pub media_type: String,
    pub digest: String,
    pub size: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<Platform>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub annotations: BTreeMap<String, String>,
}

impl Descriptor {
    pub fn new(media_type: &str, digest: String, size: u64) -> Self {
        Descriptor {
            media_type: media_type.into(),
            digest,
            size,
            platform: None,
            annotations: BTreeMap::new(),
        }
    }

    /// Relocations recorded on a layer descriptor (empty when absent).
    pub fn relocations(&self) -> Result<Vec<super::root::Relocation>> {
        match self.annotations.get(ANN_RELOCATIONS) {
            None => Ok(Vec::new()),
            Some(raw) => serde_json::from_str(raw)
                .with_context(|| format!("parse {ANN_RELOCATIONS} on layer {}", self.digest)),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Manifest {
    pub schema_version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media_type: Option<String>,
    pub config: Descriptor,
    #[serde(default)]
    pub layers: Vec<Descriptor>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub annotations: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Index {
    pub schema_version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media_type: Option<String>,
    #[serde(default)]
    pub manifests: Vec<Descriptor>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub annotations: BTreeMap<String, String>,
}

/// The `config` member of an image config (Docker-compatible PascalCase).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContainerConfig {
    #[serde(rename = "Env", default, skip_serializing_if = "Vec::is_empty")]
    pub env: Vec<String>,
    #[serde(
        rename = "Entrypoint",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub entrypoint: Option<Vec<String>>,
    #[serde(rename = "Cmd", default, skip_serializing_if = "Option::is_none")]
    pub cmd: Option<Vec<String>>,
    #[serde(
        rename = "WorkingDir",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub working_dir: Option<String>,
    #[serde(rename = "Labels", default, skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,
    #[serde(
        rename = "ExposedPorts",
        default,
        skip_serializing_if = "BTreeMap::is_empty"
    )]
    pub exposed_ports: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RootFs {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub diff_ids: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct History {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub empty_layer: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created: Option<String>,
    pub architecture: String,
    pub os: String,
    #[serde(default)]
    pub config: ContainerConfig,
    pub rootfs: RootFs,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub history: Vec<History>,
}

impl ImageConfig {
    pub fn require_darwin_arm64(&self) -> Result<()> {
        if self.os != OS || self.architecture != ARCH {
            bail!(
                "image platform is {}/{}; the native runtime only runs {OS}/{ARCH} images",
                self.os,
                self.architecture
            );
        }
        Ok(())
    }
}

/// `sha256:<hex>` of `bytes`.
pub fn sha256_digest(bytes: &[u8]) -> String {
    format!("sha256:{}", hex(&Sha256::digest(bytes)))
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Validate a `sha256:<64 lowercase hex>` digest, returning the hex part.
pub fn digest_hex(digest: &str) -> Result<&str> {
    let Some(hex) = digest.strip_prefix("sha256:") else {
        bail!("unsupported digest {digest:?} (only sha256 is supported)");
    };
    if hex.len() != 64 || !hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
        bail!("malformed sha256 digest {digest:?}");
    }
    Ok(hex)
}

/// OCI chain ids for a list of layer diff ids:
/// `chain[0] = diff[0]`, `chain[n] = sha256(chain[n-1] + " " + diff[n])`.
pub fn chain_ids(diff_ids: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(diff_ids.len());
    for diff in diff_ids {
        let next = match out.last() {
            None => diff.clone(),
            Some(prev) => sha256_digest(format!("{prev} {diff}").as_bytes()),
        };
        out.push(next);
    }
    out
}

/// A parsed image reference: `[registry/]repository[:tag][@digest]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reference {
    /// Registry host[:port] when the first path component names one.
    pub registry: Option<String>,
    pub repository: String,
    pub tag: Option<String>,
    pub digest: Option<String>,
}

impl Reference {
    pub fn parse(raw: &str) -> Result<Self> {
        let raw = raw.trim();
        if raw.is_empty() {
            bail!("empty image reference");
        }
        let (name_tag, digest) = match raw.split_once('@') {
            Some((left, digest)) => {
                digest_hex(digest)?;
                (left, Some(digest.to_string()))
            }
            None => (raw, None),
        };
        let last_slash = name_tag.rfind('/');
        let (name, tag) = match name_tag.rfind(':') {
            Some(colon) if last_slash.is_none_or(|slash| colon > slash) => {
                (&name_tag[..colon], Some(name_tag[colon + 1..].to_string()))
            }
            _ => (name_tag, None),
        };
        let (registry, repository) = match name.split_once('/') {
            Some((first, rest))
                if first.contains('.') || first.contains(':') || first == "localhost" =>
            {
                (Some(first.to_string()), rest.to_string())
            }
            _ => (None, name.to_string()),
        };
        if repository.is_empty() {
            bail!("image reference {raw:?} has no repository");
        }
        let valid_repo = repository.split('/').all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b))
        });
        if !valid_repo {
            bail!("invalid repository {repository:?} in image reference {raw:?} (lowercase [a-z0-9._-] path components)");
        }
        if let Some(tag) = &tag {
            let valid_tag = !tag.is_empty()
                && tag.len() <= 128
                && tag
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b));
            if !valid_tag {
                bail!("invalid tag {tag:?} in image reference {raw:?}");
            }
        }
        Ok(Reference {
            registry,
            repository,
            tag,
            digest,
        })
    }

    /// Tag, defaulting to `latest`.
    pub fn tag_or_latest(&self) -> &str {
        self.tag.as_deref().unwrap_or("latest")
    }

    /// Canonical local name: `[registry/]repository:tag` (`latest` default).
    pub fn local_name(&self) -> String {
        let mut out = String::new();
        if let Some(registry) = &self.registry {
            out.push_str(registry);
            out.push('/');
        }
        out.push_str(&self.repository);
        out.push(':');
        out.push_str(self.tag_or_latest());
        out
    }

    /// Registry host for network operations (`docker.io` → `registry-1.docker.io`).
    pub fn registry_host(&self) -> String {
        match self.registry.as_deref() {
            None | Some("docker.io") | Some("index.docker.io") => "registry-1.docker.io".into(),
            Some(other) => other.to_string(),
        }
    }

    /// Repository path for network operations (Docker Hub official images
    /// live under `library/`).
    pub fn remote_repository(&self) -> String {
        let hub = matches!(
            self.registry.as_deref(),
            None | Some("docker.io") | Some("index.docker.io")
        );
        if hub && !self.repository.contains('/') {
            format!("library/{}", self.repository)
        } else {
            self.repository.clone()
        }
    }

    /// Manifest reference to request: digest if pinned, else tag.
    pub fn remote_reference(&self) -> String {
        self.digest
            .clone()
            .unwrap_or_else(|| self.tag_or_latest().to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_references() {
        let r = Reference::parse("demo").unwrap();
        assert_eq!(r.registry, None);
        assert_eq!(r.local_name(), "demo:latest");
        assert_eq!(r.registry_host(), "registry-1.docker.io");
        assert_eq!(r.remote_repository(), "library/demo");

        let r = Reference::parse("localhost:5000/team/app:v1").unwrap();
        assert_eq!(r.registry.as_deref(), Some("localhost:5000"));
        assert_eq!(r.repository, "team/app");
        assert_eq!(r.tag.as_deref(), Some("v1"));
        assert_eq!(r.local_name(), "localhost:5000/team/app:v1");
        assert_eq!(r.remote_repository(), "team/app");

        let r = Reference::parse("ghcr.io/o/i@sha256:".to_string().as_str());
        assert!(r.is_err());
        let digest = format!("sha256:{}", "a".repeat(64));
        let r = Reference::parse(&format!("ghcr.io/o/i@{digest}")).unwrap();
        assert_eq!(r.remote_reference(), digest);

        let r = Reference::parse("localhost/app").unwrap();
        assert_eq!(r.registry.as_deref(), Some("localhost"));
        assert!(Reference::parse("Bad/Name").is_err());
    }

    #[test]
    fn chain_ids_follow_the_oci_definition() {
        let a = sha256_digest(b"a");
        let b = sha256_digest(b"b");
        let chain = chain_ids(&[a.clone(), b.clone()]);
        assert_eq!(chain[0], a);
        assert_eq!(chain[1], sha256_digest(format!("{a} {b}").as_bytes()));
    }

    #[test]
    fn digest_validation() {
        assert!(digest_hex(&sha256_digest(b"x")).is_ok());
        assert!(digest_hex("sha256:ABC").is_err());
        assert!(digest_hex("md5:00").is_err());
    }

    #[test]
    fn config_round_trips_docker_field_names() {
        let config = ImageConfig {
            created: None,
            architecture: ARCH.into(),
            os: OS.into(),
            config: ContainerConfig {
                env: vec!["A=1".into()],
                cmd: Some(vec!["run".into()]),
                working_dir: Some("/app".into()),
                ..Default::default()
            },
            rootfs: RootFs {
                kind: "layers".into(),
                diff_ids: vec![],
            },
            history: vec![],
        };
        let json = serde_json::to_value(&config).unwrap();
        assert_eq!(json["config"]["Env"][0], "A=1");
        assert_eq!(json["config"]["WorkingDir"], "/app");
        assert_eq!(json["rootfs"]["type"], "layers");
        assert!(config.require_darwin_arm64().is_ok());
    }
}
