//! Local native image store: content-addressed blobs, a refs index, cached
//! unpacked snapshots per chain id, and OCI image-layout import/export.
//!
//! Layout under `<native home>`:
//!
//! ```text
//! images/oci-layout
//! images/blobs/sha256/<hex>     manifests, configs, layers
//! images/refs.json              name → manifest digest
//! snapshots/<chain hex>/        unpacked rootfs after layer N (placeholder content)
//! ```

use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use super::oci::{
    self, chain_ids, digest_hex, sha256_digest, Descriptor, ImageConfig, Index, Manifest,
    Reference,
};
use super::root::Relocation;

const OCI_LAYOUT: &str = "{\"imageLayoutVersion\":\"1.0.0\"}";

/// One entry of `refs.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefEntry {
    pub digest: String,
    pub updated_at: String,
}

/// A resolved local image.
#[derive(Debug, Clone)]
pub struct LocalImage {
    /// The local name it was resolved through (if any).
    pub reference: Option<String>,
    pub manifest_digest: String,
    pub manifest: Manifest,
    pub manifest_size: u64,
    pub config: ImageConfig,
}

impl LocalImage {
    /// Union of the relocations recorded on every layer.
    pub fn relocations(&self) -> Result<Vec<Relocation>> {
        let mut by_path: BTreeMap<String, Relocation> = BTreeMap::new();
        for layer in &self.manifest.layers {
            for reloc in layer.relocations()? {
                by_path.insert(reloc.path.clone(), reloc);
            }
        }
        Ok(by_path.into_values().collect())
    }

    /// Short summary used by `vat image ls`/inspect.
    pub fn summary(&self, names: Vec<String>) -> serde_json::Value {
        let size: u64 = self.manifest.layers.iter().map(|l| l.size).sum::<u64>()
            + self.manifest.config.size
            + self.manifest_size;
        let relocations: usize = self
            .manifest
            .layers
            .iter()
            .map(|l| l.relocations().map(|r| r.len()).unwrap_or(0))
            .sum();
        serde_json::json!({
            "names": names,
            "digest": self.manifest_digest,
            "config_digest": self.manifest.config.digest,
            "platform": format!("{}/{}", self.config.os, self.config.architecture),
            "created": self.config.created,
            "layers": self.manifest.layers.len(),
            "size": size,
            "relocations": relocations,
        })
    }
}

/// The local image store.
#[derive(Debug, Clone)]
pub struct ImageStore {
    /// `<native home>/images`
    root: PathBuf,
    /// `<native home>/snapshots`
    snapshots: PathBuf,
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

impl ImageStore {
    /// Open the store under the native home.
    pub fn open() -> Result<Self> {
        Self::at(&super::home()?)
    }

    /// Open (creating) a store rooted at `home`.
    pub fn at(home: &Path) -> Result<Self> {
        let store = ImageStore { root: home.join("images"), snapshots: home.join("snapshots") };
        std::fs::create_dir_all(store.root.join("blobs").join("sha256"))
            .with_context(|| format!("create {}", store.root.display()))?;
        std::fs::create_dir_all(&store.snapshots)?;
        let layout = store.root.join("oci-layout");
        if !layout.exists() {
            super::write_atomic(&layout, OCI_LAYOUT.as_bytes())?;
        }
        Ok(store)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn blob_path(&self, digest: &str) -> Result<PathBuf> {
        Ok(self.root.join("blobs").join("sha256").join(digest_hex(digest)?))
    }

    pub fn has_blob(&self, digest: &str) -> bool {
        self.blob_path(digest).map(|p| p.is_file()).unwrap_or(false)
    }

    pub fn read_blob(&self, digest: &str) -> Result<Vec<u8>> {
        let path = self.blob_path(digest)?;
        std::fs::read(&path).with_context(|| format!("blob {digest} is missing from the local store"))
    }

    pub fn open_blob(&self, digest: &str) -> Result<std::fs::File> {
        let path = self.blob_path(digest)?;
        std::fs::File::open(&path)
            .with_context(|| format!("blob {digest} is missing from the local store"))
    }

    /// Store bytes, returning their digest.
    pub fn put_blob(&self, bytes: &[u8]) -> Result<String> {
        let digest = sha256_digest(bytes);
        let path = self.blob_path(&digest)?;
        if !path.is_file() {
            super::write_atomic(&path, bytes)?;
        }
        Ok(digest)
    }

    /// Stream a blob in, verifying `expected` when given. Returns
    /// `(digest, size)`.
    pub fn ingest(&self, mut reader: impl Read, expected: Option<&str>) -> Result<(String, u64)> {
        if let Some(expected) = expected {
            digest_hex(expected)?;
        }
        let tmp_dir = self.root.join("tmp");
        std::fs::create_dir_all(&tmp_dir)?;
        let tmp = tmp_dir.join(format!("ingest-{}", super::random_hex(8)));
        let mut file = std::fs::File::create(&tmp)?;
        let mut hasher = Sha256::new();
        let mut size = 0u64;
        let mut buf = vec![0u8; 1 << 16];
        loop {
            let n = reader.read(&mut buf)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            file.write_all(&buf[..n])?;
            size += n as u64;
        }
        file.sync_all()?;
        drop(file);
        let digest = format!("sha256:{}", oci::hex(&hasher.finalize()));
        if let Some(expected) = expected {
            if expected != digest {
                let _ = std::fs::remove_file(&tmp);
                bail!("digest mismatch: expected {expected}, got {digest}");
            }
        }
        self.adopt(&tmp, &digest)?;
        Ok((digest, size))
    }

    /// Move an already-hashed file into the blob store.
    pub fn adopt(&self, file: &Path, digest: &str) -> Result<()> {
        let path = self.blob_path(digest)?;
        if path.is_file() {
            let _ = std::fs::remove_file(file);
            return Ok(());
        }
        std::fs::rename(file, &path)
            .with_context(|| format!("move blob into {}", path.display()))?;
        Ok(())
    }

    /// Scratch directory for blobs being written by the builder.
    pub fn tmp_dir(&self) -> Result<PathBuf> {
        let dir = self.root.join("tmp");
        std::fs::create_dir_all(&dir)?;
        Ok(dir)
    }

    fn refs_path(&self) -> PathBuf {
        self.root.join("refs.json")
    }

    pub fn refs(&self) -> Result<BTreeMap<String, RefEntry>> {
        let path = self.refs_path();
        match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .with_context(|| format!("parse {}", path.display())),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
            Err(err) => Err(err).with_context(|| format!("read {}", path.display())),
        }
    }

    fn write_refs(&self, refs: &BTreeMap<String, RefEntry>) -> Result<()> {
        super::write_atomic(&self.refs_path(), &serde_json::to_vec_pretty(refs)?)
    }

    /// Point local name `name` at `manifest_digest`.
    pub fn set_ref(&self, name: &str, manifest_digest: &str) -> Result<String> {
        let canonical = Reference::parse(name)?.local_name();
        let mut refs = self.refs()?;
        refs.insert(
            canonical.clone(),
            RefEntry { digest: manifest_digest.to_string(), updated_at: now() },
        );
        self.write_refs(&refs)?;
        Ok(canonical)
    }

    /// Load and validate an image manifest + config by manifest digest.
    pub fn load(&self, manifest_digest: &str) -> Result<LocalImage> {
        let bytes = self.read_blob(manifest_digest)?;
        let manifest: Manifest = serde_json::from_slice(&bytes)
            .with_context(|| format!("parse manifest {manifest_digest}"))?;
        let config_bytes = self.read_blob(&manifest.config.digest)?;
        let config: ImageConfig = serde_json::from_slice(&config_bytes)
            .with_context(|| format!("parse image config {}", manifest.config.digest))?;
        config.require_darwin_arm64()?;
        if config.rootfs.diff_ids.len() != manifest.layers.len() {
            bail!(
                "image {manifest_digest} is inconsistent: {} layers but {} diff_ids",
                manifest.layers.len(),
                config.rootfs.diff_ids.len()
            );
        }
        Ok(LocalImage {
            reference: None,
            manifest_digest: manifest_digest.to_string(),
            manifest,
            manifest_size: bytes.len() as u64,
            config,
        })
    }

    /// Resolve a local name (`name`, `name:tag`, full digest, or a unique
    /// digest prefix of at least 6 hex characters).
    pub fn resolve(&self, name: &str) -> Result<LocalImage> {
        let refs = self.refs()?;
        if let Ok(reference) = Reference::parse(name) {
            if let Some(digest) = &reference.digest {
                if self.has_blob(digest) {
                    let mut image = self.load(digest)?;
                    image.reference = Some(name.to_string());
                    return Ok(image);
                }
            }
            let canonical = reference.local_name();
            if let Some(entry) = refs.get(&canonical) {
                let mut image = self.load(&entry.digest)?;
                image.reference = Some(canonical);
                return Ok(image);
            }
        }
        let prefix = name.strip_prefix("sha256:").unwrap_or(name);
        if prefix.len() >= 6 && prefix.bytes().all(|b| b.is_ascii_hexdigit()) {
            let matches: BTreeSet<&String> = refs
                .values()
                .map(|e| &e.digest)
                .filter(|d| d.trim_start_matches("sha256:").starts_with(prefix))
                .collect();
            if matches.len() == 1 {
                return self.load(matches.into_iter().next().unwrap());
            }
            if matches.len() > 1 {
                bail!("image id prefix {name:?} is ambiguous");
            }
        }
        bail!("no local native image named {name:?} (see `vat image ls`)")
    }

    /// Names pointing at each manifest digest.
    pub fn names_by_digest(&self) -> Result<BTreeMap<String, Vec<String>>> {
        let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for (name, entry) in self.refs()? {
            out.entry(entry.digest).or_default().push(name);
        }
        Ok(out)
    }

    /// All tagged images.
    pub fn list(&self) -> Result<Vec<(Vec<String>, LocalImage)>> {
        let mut out = Vec::new();
        for (digest, names) in self.names_by_digest()? {
            match self.load(&digest) {
                Ok(image) => out.push((names, image)),
                Err(err) => eprintln!("vat: skipping unreadable image {digest}: {err:#}"),
            }
        }
        Ok(out)
    }

    /// Remove a name; returns the manifest digest it pointed at. Blobs and
    /// snapshots no longer referenced by any name are garbage-collected.
    pub fn remove(&self, name: &str) -> Result<(String, String)> {
        let mut refs = self.refs()?;
        let canonical = Reference::parse(name).map(|r| r.local_name()).ok();
        let key = match canonical.filter(|c| refs.contains_key(c)) {
            Some(key) => key,
            None => {
                let image = self.resolve(name)?;
                let names: Vec<String> = refs
                    .iter()
                    .filter(|(_, e)| e.digest == image.manifest_digest)
                    .map(|(n, _)| n.clone())
                    .collect();
                if names.len() != 1 {
                    bail!("{name:?} matches {} names; remove them by name", names.len());
                }
                names[0].clone()
            }
        };
        let entry = refs.remove(&key).expect("key checked above");
        self.write_refs(&refs)?;
        Ok((key, entry.digest))
    }

    /// Delete blobs and snapshots unreachable from any ref (and from any
    /// manifest digest in `keep`, e.g. images used by existing containers).
    pub fn gc(&self, keep: &[String]) -> Result<GcStats> {
        let mut live_blobs: BTreeSet<String> = BTreeSet::new();
        let mut live_chains: BTreeSet<String> = BTreeSet::new();
        let mut roots: Vec<String> = self.refs()?.into_values().map(|e| e.digest).collect();
        roots.extend(keep.iter().cloned());
        for digest in roots {
            let Ok(image) = self.load(&digest) else { continue };
            live_blobs.insert(digest_hex(&digest)?.to_string());
            live_blobs.insert(digest_hex(&image.manifest.config.digest)?.to_string());
            for layer in &image.manifest.layers {
                live_blobs.insert(digest_hex(&layer.digest)?.to_string());
            }
            for chain in chain_ids(&image.config.rootfs.diff_ids) {
                live_chains.insert(digest_hex(&chain)?.to_string());
            }
        }
        let mut stats = GcStats::default();
        let blobs = self.root.join("blobs").join("sha256");
        for entry in std::fs::read_dir(&blobs)?.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if !live_blobs.contains(&name) {
                stats.bytes += entry.metadata().map(|m| m.len()).unwrap_or(0);
                std::fs::remove_file(entry.path())?;
                stats.blobs += 1;
            }
        }
        for entry in std::fs::read_dir(&self.snapshots)?.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name == "empty" || !live_chains.contains(&name) {
                super::remove_tree(&entry.path())?;
                stats.snapshots += 1;
            }
        }
        Ok(stats)
    }

    /// The cached unpacked snapshot of `image` (placeholder content),
    /// materializing missing chain levels by cloning the parent snapshot and
    /// applying each layer.
    pub fn snapshot(&self, image: &LocalImage) -> Result<PathBuf> {
        let chains = chain_ids(&image.config.rootfs.diff_ids);
        if chains.is_empty() {
            let empty = self.snapshots.join("empty");
            std::fs::create_dir_all(&empty)?;
            return Ok(empty);
        }
        let mut parent: Option<PathBuf> = None;
        for (index, chain) in chains.iter().enumerate() {
            let path = self.snapshots.join(digest_hex(chain)?);
            if path.is_dir() {
                parent = Some(path);
                continue;
            }
            let layer = &image.manifest.layers[index];
            let tmp = self.snapshots.join(format!(".tmp-{}", super::random_hex(8)));
            match &parent {
                Some(parent) => super::clone_tree(parent, &tmp)?,
                None => std::fs::create_dir_all(&tmp)?,
            }
            let blob = self.open_blob(&layer.digest)?;
            let result = super::layer::apply_layer(&tmp, blob);
            if let Err(err) = result {
                let _ = super::remove_tree(&tmp);
                return Err(err).with_context(|| format!("unpack layer {}", layer.digest));
            }
            match std::fs::rename(&tmp, &path) {
                Ok(()) => {}
                Err(_) if path.is_dir() => {
                    // A concurrent materialization won the race.
                    super::remove_tree(&tmp)?;
                }
                Err(err) => {
                    let _ = super::remove_tree(&tmp);
                    return Err(err).with_context(|| format!("commit snapshot {}", path.display()));
                }
            }
            parent = Some(path);
        }
        Ok(parent.expect("at least one chain level"))
    }

    /// Export images to an OCI image layout directory.
    pub fn export(&self, names: &[String], dir: &Path) -> Result<Vec<String>> {
        std::fs::create_dir_all(dir.join("blobs").join("sha256"))
            .with_context(|| format!("create {}", dir.display()))?;
        std::fs::write(dir.join("oci-layout"), OCI_LAYOUT)?;
        let index_path = dir.join("index.json");
        let mut index: Index = match std::fs::read(&index_path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .with_context(|| format!("parse {}", index_path.display()))?,
            Err(_) => Index {
                schema_version: 2,
                media_type: Some(oci::MT_OCI_INDEX.into()),
                manifests: Vec::new(),
                annotations: BTreeMap::new(),
            },
        };
        let mut exported = Vec::new();
        for name in names {
            let image = self.resolve(name)?;
            let full = image.reference.clone().unwrap_or_else(|| name.clone());
            let mut digests = vec![image.manifest_digest.clone(), image.manifest.config.digest.clone()];
            digests.extend(image.manifest.layers.iter().map(|l| l.digest.clone()));
            for digest in digests {
                let dst = dir.join("blobs").join("sha256").join(digest_hex(&digest)?);
                if !dst.exists() {
                    let src = self.blob_path(&digest)?;
                    super::clone_tree(&src, &dst)
                        .with_context(|| format!("copy blob {digest}"))?;
                }
            }
            let tag = Reference::parse(&full).map(|r| r.tag_or_latest().to_string()).unwrap_or_else(|_| "latest".into());
            index.manifests.retain(|d| d.annotations.get(oci::ANN_CONTAINERD_NAME) != Some(&full));
            let mut desc = Descriptor::new(
                image.manifest.media_type.as_deref().unwrap_or(oci::MT_OCI_MANIFEST),
                image.manifest_digest.clone(),
                image.manifest_size,
            );
            desc.platform = Some(oci::Platform::darwin_arm64());
            desc.annotations.insert(oci::ANN_REF_NAME.into(), tag);
            desc.annotations.insert(oci::ANN_CONTAINERD_NAME.into(), full.clone());
            index.manifests.push(desc);
            exported.push(full);
        }
        std::fs::write(&index_path, serde_json::to_vec_pretty(&index)?)?;
        Ok(exported)
    }

    /// Import every darwin/arm64 image from an OCI image layout directory.
    /// `tag` overrides the name (only valid when the layout holds one image).
    pub fn import(&self, dir: &Path, tag: Option<&str>) -> Result<Vec<(String, String)>> {
        let layout = std::fs::read_to_string(dir.join("oci-layout"))
            .with_context(|| format!("{} is not an OCI image layout (no oci-layout file)", dir.display()))?;
        if !layout.contains("imageLayoutVersion") {
            bail!("{} has an invalid oci-layout file", dir.display());
        }
        let index_bytes = std::fs::read(dir.join("index.json"))
            .with_context(|| format!("read {}/index.json", dir.display()))?;
        let index: Index = serde_json::from_slice(&index_bytes)?;
        let blob = |digest: &str| -> Result<PathBuf> {
            Ok(dir.join("blobs").join("sha256").join(digest_hex(digest)?))
        };
        let mut manifests: Vec<(Descriptor, Option<String>)> = Vec::new();
        for desc in &index.manifests {
            let name = desc
                .annotations
                .get(oci::ANN_CONTAINERD_NAME)
                .cloned()
                .or_else(|| desc.annotations.get(oci::ANN_REF_NAME).cloned());
            match desc.media_type.as_str() {
                oci::MT_OCI_MANIFEST | oci::MT_DOCKER_MANIFEST => {
                    manifests.push((desc.clone(), name))
                }
                oci::MT_OCI_INDEX | oci::MT_DOCKER_LIST => {
                    let nested: Index = serde_json::from_slice(&std::fs::read(blob(&desc.digest)?)?)?;
                    let chosen = nested
                        .manifests
                        .into_iter()
                        .find(|d| d.platform.as_ref().is_some_and(|p| p.is_darwin_arm64()));
                    match chosen {
                        Some(chosen) => manifests.push((chosen, name)),
                        None => eprintln!(
                            "vat: skipping {} (no darwin/arm64 manifest)",
                            name.as_deref().unwrap_or(&desc.digest)
                        ),
                    }
                }
                other => eprintln!("vat: skipping index entry with media type {other}"),
            }
        }
        if manifests.is_empty() {
            bail!("{} contains no darwin/arm64 image", dir.display());
        }
        if tag.is_some() && manifests.len() > 1 {
            bail!("--tag needs a layout with exactly one image ({} found)", manifests.len());
        }
        let mut out = Vec::new();
        for (desc, name) in manifests {
            let manifest_bytes = std::fs::read(blob(&desc.digest)?)?;
            if sha256_digest(&manifest_bytes) != desc.digest {
                bail!("manifest {} in {} fails digest verification", desc.digest, dir.display());
            }
            let manifest: Manifest = serde_json::from_slice(&manifest_bytes)?;
            let mut digests = vec![manifest.config.digest.clone()];
            digests.extend(manifest.layers.iter().map(|l| l.digest.clone()));
            for digest in digests {
                if self.has_blob(&digest) {
                    continue;
                }
                let file = std::fs::File::open(blob(&digest)?)
                    .with_context(|| format!("blob {digest} missing from {}", dir.display()))?;
                self.ingest(file, Some(&digest))?;
            }
            self.ingest(manifest_bytes.as_slice(), Some(&desc.digest))?;
            self.load(&desc.digest)?;
            let name = match (tag, name) {
                (Some(tag), _) => tag.to_string(),
                (None, Some(name)) if Reference::parse(&name).is_ok() => name,
                (None, _) => bail!(
                    "image {} in {} has no usable name annotation; pass --tag NAME[:TAG]",
                    desc.digest,
                    dir.display()
                ),
            };
            let canonical = self.set_ref(&name, &desc.digest)?;
            out.push((canonical, desc.digest.clone()));
        }
        Ok(out)
    }
}

/// Garbage-collection counts.
#[derive(Debug, Clone, Default, Serialize)]
pub struct GcStats {
    pub blobs: usize,
    pub snapshots: usize,
    pub bytes: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native::oci::{ContainerConfig, RootFs};

    fn tiny_image(store: &ImageStore, name: &str) -> String {
        // One layer with a single file.
        let src = tempfile::tempdir().unwrap();
        std::fs::write(src.path().join("hello.txt"), "hi").unwrap();
        let opts = crate::native::layer::ScanOptions::native_default();
        let tree = crate::native::layer::scan_tree(src.path(), &opts).unwrap();
        let d = crate::native::layer::diff(&Default::default(), &tree);
        let out = store.tmp_dir().unwrap().join("layer");
        let blob = crate::native::layer::write_layer(src.path(), &d, &tree, None, &out).unwrap();
        store.adopt(&out, &blob.digest).unwrap();
        let config = ImageConfig {
            created: None,
            architecture: "arm64".into(),
            os: "darwin".into(),
            config: ContainerConfig::default(),
            rootfs: RootFs { kind: "layers".into(), diff_ids: vec![blob.diff_id.clone()] },
            history: vec![],
        };
        let config_bytes = serde_json::to_vec(&config).unwrap();
        let config_digest = store.put_blob(&config_bytes).unwrap();
        let manifest = Manifest {
            schema_version: 2,
            media_type: Some(oci::MT_OCI_MANIFEST.into()),
            config: Descriptor::new(oci::MT_OCI_CONFIG, config_digest, config_bytes.len() as u64),
            layers: vec![Descriptor::new(oci::MT_OCI_LAYER_GZIP, blob.digest, blob.size)],
            annotations: BTreeMap::new(),
        };
        let bytes = serde_json::to_vec(&manifest).unwrap();
        let digest = store.put_blob(&bytes).unwrap();
        store.set_ref(name, &digest).unwrap();
        digest
    }

    #[test]
    fn resolve_snapshot_export_import_and_gc() {
        let home = tempfile::tempdir().unwrap();
        let store = ImageStore::at(home.path()).unwrap();
        let digest = tiny_image(&store, "demo");
        let image = store.resolve("demo").unwrap();
        assert_eq!(image.manifest_digest, digest);
        assert_eq!(image.reference.as_deref(), Some("demo:latest"));
        assert!(store.resolve(&digest[7..19]).is_ok());

        let snap = store.snapshot(&image).unwrap();
        assert_eq!(std::fs::read_to_string(snap.join("hello.txt")).unwrap(), "hi");

        let layout = tempfile::tempdir().unwrap();
        store.export(&["demo".to_string()], layout.path()).unwrap();
        let other_home = tempfile::tempdir().unwrap();
        let other = ImageStore::at(other_home.path()).unwrap();
        let imported = other.import(layout.path(), None).unwrap();
        assert_eq!(imported, vec![("demo:latest".to_string(), digest.clone())]);
        let renamed = other.import(layout.path(), Some("again:v2")).unwrap();
        assert_eq!(renamed[0].0, "again:v2");

        store.remove("demo").unwrap();
        let stats = store.gc(&[]).unwrap();
        assert_eq!(stats.blobs, 3);
        assert_eq!(stats.snapshots, 1);
        assert!(store.resolve("demo").is_err());
    }
}
