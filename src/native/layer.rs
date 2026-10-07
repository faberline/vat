//! Filesystem scans, before/after diffs, and gzip tar layers with whiteouts.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use super::root::{is_macho, replace_all, sanitize_rel, RelocKind, Relocation};

/// Type of a scanned entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EntryKind {
    File,
    Dir,
    Symlink,
    /// Sockets, fifos, devices: never committed to layers.
    Other,
}

/// One scanned filesystem entry (metadata only).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub kind: EntryKind,
    pub size: u64,
    pub mode: u32,
    pub mtime_ns: i64,
    pub ctime_ns: i64,
    pub ino: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<PathBuf>,
}

/// A scanned tree: root-relative path → entry.
pub type Tree = BTreeMap<String, Entry>;

/// Scan options: root-relative subtrees whose *contents* are excluded from
/// layers (the root's scratch `tmp/`, the build HOME's caches).
#[derive(Debug, Clone, Default)]
pub struct ScanOptions {
    pub exclude_contents: Vec<String>,
}

impl ScanOptions {
    pub fn native_default() -> Self {
        ScanOptions { exclude_contents: vec!["tmp".into()] }
    }

    fn excluded(&self, rel: &str) -> bool {
        self.exclude_contents.iter().any(|prefix| {
            rel.len() > prefix.len()
                && rel.starts_with(prefix.as_str())
                && rel.as_bytes()[prefix.len()] == b'/'
        })
    }
}

/// Scan `root` (not following symlinks).
pub fn scan_tree(root: &Path, options: &ScanOptions) -> Result<Tree> {
    let mut tree = Tree::new();
    let mut walker = walkdir::WalkDir::new(root).follow_links(false).min_depth(1).into_iter();
    while let Some(entry) = walker.next() {
        let entry = entry.with_context(|| format!("scan {}", root.display()))?;
        let rel = entry
            .path()
            .strip_prefix(root)
            .expect("walkdir yields paths under root");
        let Some(rel) = rel.to_str() else {
            bail!("non-UTF-8 path {} is not supported in native images", entry.path().display());
        };
        let rel = rel.to_string();
        if options.excluded(&rel) {
            if entry.file_type().is_dir() {
                walker.skip_current_dir();
            }
            continue;
        }
        let meta = entry.metadata().with_context(|| format!("stat {}", entry.path().display()))?;
        let file_type = meta.file_type();
        let kind = if file_type.is_symlink() {
            EntryKind::Symlink
        } else if file_type.is_dir() {
            EntryKind::Dir
        } else if file_type.is_file() {
            EntryKind::File
        } else {
            EntryKind::Other
        };
        let target = if kind == EntryKind::Symlink {
            Some(std::fs::read_link(entry.path())?)
        } else {
            None
        };
        tree.insert(
            rel,
            Entry {
                kind,
                size: meta.len(),
                mode: meta.permissions().mode(),
                mtime_ns: meta.mtime() * 1_000_000_000 + meta.mtime_nsec(),
                ctime_ns: meta.ctime() * 1_000_000_000 + meta.ctime_nsec(),
                ino: meta.ino(),
                target,
            },
        );
    }
    Ok(tree)
}

/// Before/after difference of two scans.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TreeDiff {
    pub added: Vec<String>,
    pub modified: Vec<String>,
    /// Topmost deleted paths only (a deleted directory's children are implied).
    pub deleted: Vec<String>,
}

impl TreeDiff {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.modified.is_empty() && self.deleted.is_empty()
    }
}

fn changed(a: &Entry, b: &Entry) -> bool {
    if a.kind != b.kind || a.mode != b.mode {
        return true;
    }
    match a.kind {
        // A directory's own metadata (mode/mtime) is all a layer records;
        // children are diffed separately.
        EntryKind::Dir => a.mtime_ns != b.mtime_ns,
        EntryKind::Symlink => a.target != b.target,
        _ => {
            a.size != b.size || a.mtime_ns != b.mtime_ns || a.ino != b.ino || a.ctime_ns != b.ctime_ns
        }
    }
}

fn parent_of(path: &str) -> Option<&str> {
    path.rfind('/').map(|idx| &path[..idx])
}

/// Diff two scans of the same root.
pub fn diff(before: &Tree, after: &Tree) -> TreeDiff {
    let mut out = TreeDiff::default();
    for (path, entry) in after {
        match before.get(path) {
            None => out.added.push(path.clone()),
            Some(old) if changed(old, entry) => out.modified.push(path.clone()),
            Some(_) => {}
        }
    }
    let deleted: BTreeSet<&String> = before.keys().filter(|p| !after.contains_key(*p)).collect();
    for path in &deleted {
        let mut implied = false;
        let mut cursor = parent_of(path);
        while let Some(parent) = cursor {
            if deleted.contains(&parent.to_string()) {
                implied = true;
                break;
            }
            if let Some(entry) = after.get(parent) {
                if entry.kind != EntryKind::Dir {
                    // Parent replaced by a non-directory: the replacement
                    // entry removes the old subtree on apply.
                    implied = true;
                    break;
                }
            }
            cursor = parent_of(parent);
        }
        if !implied {
            out.deleted.push((*path).clone());
        }
    }
    out
}

/// Writer adapter that hashes and counts every byte written through it.
pub struct HashWriter<W: Write> {
    inner: W,
    hasher: Sha256,
    count: u64,
}

impl<W: Write> HashWriter<W> {
    pub fn new(inner: W) -> Self {
        HashWriter { inner, hasher: Sha256::new(), count: 0 }
    }

    /// Return the inner writer, the `sha256:` digest, and the byte count.
    pub fn finish(self) -> (W, String, u64) {
        let digest = format!("sha256:{}", super::oci::hex(&self.hasher.finalize()));
        (self.inner, digest, self.count)
    }
}

impl<W: Write> Write for HashWriter<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.hasher.update(&buf[..n]);
        self.count += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// A written layer blob.
#[derive(Debug, Clone)]
pub struct LayerBlob {
    /// Digest of the compressed blob.
    pub digest: String,
    /// Compressed size.
    pub size: u64,
    /// Digest of the uncompressed tar (`rootfs.diff_ids`).
    pub diff_id: String,
    /// Files whose committed content had the build root replaced by the
    /// placeholder.
    pub relocations: Vec<Relocation>,
    /// Number of tar entries (including whiteouts).
    pub entries: usize,
}

/// Does `path` contain `needle`? Streams the file in chunks so large model
/// weights are not read into memory.
fn file_contains(path: &Path, needle: &[u8]) -> Result<bool> {
    let mut file = std::fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let finder = memchr::memmem::Finder::new(needle);
    let chunk = 1 << 20;
    let mut buf = vec![0u8; chunk + needle.len()];
    let mut carry = 0usize;
    loop {
        let n = file.read(&mut buf[carry..])?;
        if n == 0 {
            return Ok(false);
        }
        let filled = carry + n;
        if finder.find(&buf[..filled]).is_some() {
            return Ok(true);
        }
        let keep = needle.len().saturating_sub(1).min(filled);
        buf.copy_within(filled - keep..filled, 0);
        carry = keep;
    }
}

fn tar_header(entry_type: tar::EntryType, mode: u32, mtime_ns: i64, size: u64) -> tar::Header {
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(entry_type);
    header.set_mode(mode & 0o7777);
    header.set_uid(0);
    header.set_gid(0);
    header.set_mtime(mtime_ns.div_euclid(1_000_000_000).max(0) as u64);
    header.set_size(size);
    header
}

/// Write a gzip tar layer of `diff` (read from `root`) to `out`.
///
/// When `relocate_from` is set (the build root), committed file contents and
/// symlink targets have those bytes replaced by `relocate_to` (the
/// placeholder) and each patched path is recorded as a relocation.
pub fn write_layer(
    root: &Path,
    tree_diff: &TreeDiff,
    after: &Tree,
    relocate: Option<(&[u8], &[u8])>,
    out: &Path,
) -> Result<LayerBlob> {
    let file = std::fs::File::create(out).with_context(|| format!("create {}", out.display()))?;
    let blob_writer = HashWriter::new(std::io::BufWriter::new(file));
    let gz = flate2::write::GzEncoder::new(blob_writer, flate2::Compression::default());
    let tar_writer = HashWriter::new(gz);
    let mut builder = tar::Builder::new(tar_writer);
    builder.follow_symlinks(false);
    let mut relocations = Vec::new();
    let mut entries = 0usize;

    for path in &tree_diff.deleted {
        let (dir, name) = match path.rfind('/') {
            Some(idx) => (&path[..idx + 1], &path[idx + 1..]),
            None => ("", path.as_str()),
        };
        let wh = format!("{dir}.wh.{name}");
        let mut header = tar_header(tar::EntryType::Regular, 0o644, 0, 0);
        builder.append_data(&mut header, &wh, std::io::empty())?;
        entries += 1;
    }

    let mut paths: Vec<&String> = tree_diff.added.iter().chain(tree_diff.modified.iter()).collect();
    paths.sort();
    for rel in paths {
        let entry = &after[rel];
        let full = root.join(rel);
        match entry.kind {
            EntryKind::Other => continue,
            EntryKind::Dir => {
                let mut header = tar_header(tar::EntryType::Directory, entry.mode, entry.mtime_ns, 0);
                builder.append_data(&mut header, format!("{rel}/"), std::io::empty())?;
            }
            EntryKind::Symlink => {
                let target = entry.target.clone().unwrap_or_default();
                let mut target_bytes = target.as_os_str().as_bytes().to_vec();
                if let Some((from, to)) = relocate {
                    if memchr::memmem::find(&target_bytes, from).is_some() {
                        target_bytes = replace_all(&target_bytes, from, to);
                        relocations.push(Relocation { path: rel.clone(), kind: RelocKind::Symlink });
                    }
                }
                let target = std::ffi::OsStr::from_bytes(&target_bytes);
                let mut header = tar_header(tar::EntryType::Symlink, entry.mode, entry.mtime_ns, 0);
                builder.append_link(&mut header, rel, target)?;
            }
            EntryKind::File => {
                let needs_patch = match relocate {
                    Some((from, _)) => file_contains(&full, from)?,
                    None => false,
                };
                if needs_patch {
                    let (from, to) = relocate.unwrap();
                    let bytes = std::fs::read(&full)?;
                    let kind = if is_macho(&bytes[..bytes.len().min(32)]) {
                        RelocKind::Macho
                    } else {
                        RelocKind::Text
                    };
                    let patched = replace_all(&bytes, from, to);
                    let mut header =
                        tar_header(tar::EntryType::Regular, entry.mode, entry.mtime_ns, patched.len() as u64);
                    builder.append_data(&mut header, rel, patched.as_slice())?;
                    relocations.push(Relocation { path: rel.clone(), kind });
                } else {
                    let file = std::fs::File::open(&full)
                        .with_context(|| format!("open {}", full.display()))?;
                    let size = file.metadata()?.len();
                    let mut header = tar_header(tar::EntryType::Regular, entry.mode, entry.mtime_ns, size);
                    builder.append_data(&mut header, rel, file.take(size))?;
                }
            }
        }
        entries += 1;
    }

    let tar_writer = builder.into_inner()?;
    let (gz, diff_id, _) = tar_writer.finish();
    let blob_writer = gz.finish()?;
    let (buf, digest, size) = blob_writer.finish();
    buf.into_inner()
        .map_err(|err| err.into_error())?
        .sync_all()?;
    relocations.sort();
    Ok(LayerBlob { digest, size, diff_id, relocations, entries })
}

/// Make every existing ancestor directory of `rel` (under `dst`) writable by
/// the owner, remembering original modes so they can be restored.
fn ensure_writable_ancestors(
    dst: &Path,
    rel: &Path,
    restore: &mut BTreeMap<PathBuf, u32>,
) {
    let mut current = dst.to_path_buf();
    let mut components: Vec<_> = rel.components().collect();
    components.pop();
    for component in std::iter::once(None).chain(components.into_iter().map(Some)) {
        if let Some(component) = component {
            current.push(component);
        }
        if restore.contains_key(&current) {
            continue;
        }
        if let Ok(meta) = std::fs::symlink_metadata(&current) {
            if meta.is_dir() {
                let mode = meta.permissions().mode();
                if mode & 0o300 != 0o300 {
                    let _ = std::fs::set_permissions(
                        &current,
                        std::fs::Permissions::from_mode(mode | 0o300),
                    );
                    restore.insert(current.clone(), mode);
                }
            }
        }
    }
}

/// Apply a (gzip or plain) tar layer onto `dst`, honoring OCI whiteouts
/// (`.wh.<name>` deletes, `.wh..wh..opq` clears a directory).
pub fn apply_layer(dst: &Path, reader: impl Read) -> Result<()> {
    let mut buffered = std::io::BufReader::new(reader);
    let gz = {
        use std::io::BufRead;
        let head = buffered.fill_buf()?;
        head.len() >= 2 && head[0] == 0x1f && head[1] == 0x8b
    };
    let reader: Box<dyn Read> = if gz {
        Box::new(flate2::read::GzDecoder::new(buffered))
    } else {
        Box::new(buffered)
    };
    let mut archive = tar::Archive::new(reader);
    archive.set_preserve_permissions(true);
    archive.set_preserve_mtime(true);
    archive.set_unpack_xattrs(false);
    archive.set_overwrite(true);

    let mut restore_modes: BTreeMap<PathBuf, u32> = BTreeMap::new();
    // Directory entries: (path, mode, mtime) applied after all children.
    let mut deferred_dirs: Vec<(PathBuf, u32, u64)> = Vec::new();

    for entry in archive.entries()? {
        let mut entry = entry?;
        let raw_path = entry.path()?.into_owned();
        let raw_str = raw_path.to_string_lossy().to_string();
        let rel = match sanitize_rel(&raw_str) {
            Ok(rel) => rel,
            Err(_) if raw_str.trim_matches('/').trim_matches('.').is_empty() => continue,
            Err(err) => return Err(err).context("unsafe path in layer"),
        };
        let name = rel
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        let parent_rel = rel.parent().map(Path::to_path_buf).unwrap_or_default();

        if name == ".wh..wh..opq" {
            let dir = dst.join(&parent_rel);
            ensure_writable_ancestors(dst, &rel, &mut restore_modes);
            if let Ok(children) = std::fs::read_dir(&dir) {
                for child in children.flatten() {
                    super::remove_tree(&child.path())?;
                }
            }
            continue;
        }
        if let Some(target_name) = name.strip_prefix(".wh.") {
            let victim_rel = parent_rel.join(target_name);
            ensure_writable_ancestors(dst, &victim_rel, &mut restore_modes);
            super::remove_tree(&dst.join(&victim_rel))?;
            continue;
        }

        ensure_writable_ancestors(dst, &rel, &mut restore_modes);
        let target = dst.join(&rel);
        let entry_type = entry.header().entry_type();
        let existing = std::fs::symlink_metadata(&target).ok();
        if entry_type.is_dir() {
            if let Some(meta) = &existing {
                if !meta.is_dir() {
                    super::remove_tree(&target)?;
                }
            }
            std::fs::create_dir_all(&target)
                .with_context(|| format!("create {}", target.display()))?;
            let mode = entry.header().mode().unwrap_or(0o755);
            let mtime = entry.header().mtime().unwrap_or(0);
            if let Ok(meta) = std::fs::metadata(&target) {
                let current = meta.permissions().mode();
                if current & 0o300 != 0o300 {
                    let _ = std::fs::set_permissions(
                        &target,
                        std::fs::Permissions::from_mode(current | 0o300),
                    );
                }
            }
            deferred_dirs.push((target, mode, mtime));
            continue;
        }
        if let Some(meta) = &existing {
            // Replace any existing entry (dir, symlink, read-only file).
            if meta.is_dir() || meta.file_type().is_symlink() || meta.permissions().mode() & 0o200 == 0 {
                super::remove_tree(&target)?;
            }
        }
        if !entry
            .unpack_in(dst)
            .with_context(|| format!("unpack {}", rel.display()))?
        {
            bail!("layer entry {} escapes the root", rel.display());
        }
    }

    // Restore directories we opened up, then apply layer directory metadata
    // deepest-first so child mtimes don't disturb parents.
    for (dir, mode) in restore_modes.iter().rev() {
        if !deferred_dirs.iter().any(|(d, _, _)| d == dir) {
            let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(*mode));
        }
    }
    deferred_dirs.sort_by(|a, b| b.0.cmp(&a.0));
    for (dir, mode, mtime) in deferred_dirs {
        let time = filetime::FileTime::from_unix_time(mtime as i64, 0);
        let _ = filetime::set_file_times(&dir, time, time);
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(mode & 0o7777));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, body: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }

    #[test]
    fn diff_reports_added_modified_and_topmost_deletions() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(&root.join("keep.txt"), "a");
        write(&root.join("change.txt"), "a");
        write(&root.join("gone/x.txt"), "a");
        write(&root.join("gone/y/z.txt"), "a");
        write(&root.join("tmp/scratch"), "a");
        let opts = ScanOptions::native_default();
        let before = scan_tree(root, &opts).unwrap();
        assert!(before.contains_key("tmp"));
        assert!(!before.contains_key("tmp/scratch"));
        std::fs::write(root.join("change.txt"), "bb").unwrap();
        std::fs::remove_dir_all(root.join("gone")).unwrap();
        write(&root.join("new/file.txt"), "n");
        let after = scan_tree(root, &opts).unwrap();
        let d = diff(&before, &after);
        assert_eq!(d.added, vec!["new".to_string(), "new/file.txt".to_string()]);
        assert!(d.modified.contains(&"change.txt".to_string()));
        assert_eq!(d.deleted, vec!["gone".to_string()]);
    }

    #[test]
    fn layer_round_trip_with_whiteouts_and_relocation() {
        let src = tempfile::tempdir().unwrap();
        let root = src.path();
        let build_root = root.to_str().unwrap().to_string();
        let placeholder = "P".repeat(build_root.len());
        write(&root.join("base.txt"), "base");
        write(&root.join("old.txt"), "old");
        let opts = ScanOptions::native_default();
        let empty = Tree::new();
        let t0 = scan_tree(root, &opts).unwrap();
        let out = tempfile::tempdir().unwrap();
        write_layer(root, &diff(&empty, &t0), &t0, None, &out.path().join("l0")).unwrap();

        std::fs::remove_file(root.join("old.txt")).unwrap();
        write(&root.join("bin/run.sh"), &format!("#!/bin/sh\necho {build_root}/data\n"));
        std::fs::set_permissions(root.join("bin/run.sh"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
        std::os::unix::fs::symlink(format!("{build_root}/base.txt"), root.join("link")).unwrap();
        let t1 = scan_tree(root, &opts).unwrap();
        let d1 = diff(&t0, &t1);
        let l1 = write_layer(
            root,
            &d1,
            &t1,
            Some((build_root.as_bytes(), placeholder.as_bytes())),
            &out.path().join("l1"),
        )
        .unwrap();
        assert_eq!(
            l1.relocations,
            vec![
                Relocation { path: "bin/run.sh".into(), kind: RelocKind::Text },
                Relocation { path: "link".into(), kind: RelocKind::Symlink },
            ]
        );
        assert_ne!(l1.digest, l1.diff_id);

        let dst = tempfile::tempdir().unwrap();
        apply_layer(dst.path(), std::fs::File::open(out.path().join("l0")).unwrap()).unwrap();
        assert!(dst.path().join("old.txt").exists());
        apply_layer(dst.path(), std::fs::File::open(out.path().join("l1")).unwrap()).unwrap();
        assert!(!dst.path().join("old.txt").exists());
        assert_eq!(std::fs::read_to_string(dst.path().join("base.txt")).unwrap(), "base");
        let script = std::fs::read_to_string(dst.path().join("bin/run.sh")).unwrap();
        assert_eq!(script, format!("#!/bin/sh\necho {placeholder}/data\n"));
        let mode = std::fs::metadata(dst.path().join("bin/run.sh")).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o755);
        assert_eq!(
            std::fs::read_link(dst.path().join("link")).unwrap(),
            PathBuf::from(format!("{placeholder}/base.txt"))
        );
    }

    #[test]
    fn opaque_whiteout_clears_directory() {
        let dst = tempfile::tempdir().unwrap();
        write(&dst.path().join("d/a"), "a");
        write(&dst.path().join("d/b"), "b");
        let mut builder = tar::Builder::new(Vec::new());
        let mut header = tar_header(tar::EntryType::Regular, 0o644, 0, 0);
        builder.append_data(&mut header, "d/.wh..wh..opq", std::io::empty()).unwrap();
        let mut header = tar_header(tar::EntryType::Regular, 0o644, 0, 1);
        builder.append_data(&mut header, "d/c", &b"c"[..]).unwrap();
        let bytes = builder.into_inner().unwrap();
        apply_layer(dst.path(), bytes.as_slice()).unwrap();
        assert!(!dst.path().join("d/a").exists());
        assert!(dst.path().join("d/c").exists());
    }

    #[test]
    fn read_only_directories_accept_children() {
        let dst = tempfile::tempdir().unwrap();
        let mut builder = tar::Builder::new(Vec::new());
        let mut header = tar_header(tar::EntryType::Directory, 0o555, 0, 0);
        builder.append_data(&mut header, "ro/", std::io::empty()).unwrap();
        let mut header = tar_header(tar::EntryType::Regular, 0o444, 0, 1);
        builder.append_data(&mut header, "ro/f", &b"x"[..]).unwrap();
        let bytes = builder.into_inner().unwrap();
        apply_layer(dst.path(), bytes.as_slice()).unwrap();
        let mode = std::fs::metadata(dst.path().join("ro")).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o555);
        assert_eq!(std::fs::read_to_string(dst.path().join("ro/f")).unwrap(), "x");
        super::super::remove_tree(&dst.path().join("ro")).unwrap();
    }
}
