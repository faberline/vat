//! Fixed-length roots and placeholder relocation.
//!
//! Every native root (a `vat image build` working root or a container root) is
//! a directory whose absolute path is exactly [`ROOT_LEN`] bytes. Content that
//! embeds the root path is committed into image layers with the root bytes
//! replaced by [`placeholder`] (same length), and rewritten back to the actual
//! root when a container is created. Same-length substitution keeps every
//! byte offset in binaries valid; patched Mach-O files are re-signed ad-hoc.

use std::os::unix::fs::{FileExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

/// Byte length of every native root path.
pub const ROOT_LEN: usize = 128;

/// The placeholder prefix; [`placeholder`] pads it with `@` to [`ROOT_LEN`].
pub const PLACEHOLDER_PREFIX: &str = "/@@VAT_ROOT@@";

/// The same-length placeholder written into image layers in place of the
/// build root: `/@@VAT_ROOT@@` padded with `@` to [`ROOT_LEN`] bytes.
pub fn placeholder() -> String {
    let mut s = String::from(PLACEHOLDER_PREFIX);
    while s.len() < ROOT_LEN {
        s.push('@');
    }
    s
}

/// Kind of root: build roots and container roots share one base directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RootKind {
    Build,
    Container,
}

impl RootKind {
    fn prefix(self) -> &'static str {
        match self {
            RootKind::Build => "b",
            RootKind::Container => "c",
        }
    }
}

/// Longest root base that still leaves room for `/<kind>-<16 hex>`.
pub const MAX_BASE_LEN: usize = ROOT_LEN - 1 - 2 - 16;

/// Compute (but do not create) a fixed-length root path under `base` for the
/// 16-hex-character `id`. `base` must be absolute and canonical.
pub fn root_path(base: &Path, kind: RootKind, id: &str) -> Result<PathBuf> {
    let base_str = base
        .to_str()
        .context("native root base must be valid UTF-8")?;
    if !base.is_absolute() {
        bail!("native root base {} must be absolute", base.display());
    }
    if base_str.len() > MAX_BASE_LEN {
        bail!(
            "native root base {} is {} bytes; it must be at most {} bytes so every root \
             fits the fixed {}-byte root length (set VAT_NATIVE_ROOT_BASE or VAT_NATIVE_HOME \
             to a shorter path)",
            base.display(),
            base_str.len(),
            MAX_BASE_LEN,
            ROOT_LEN
        );
    }
    if id.len() != 16 || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("native root id must be 16 hex characters, got {id:?}");
    }
    let mut path = format!(
        "{}/{}-{}",
        base_str.trim_end_matches('/'),
        kind.prefix(),
        id
    );
    while path.len() < ROOT_LEN {
        path.push('_');
    }
    debug_assert_eq!(path.len(), ROOT_LEN);
    Ok(PathBuf::from(path))
}

/// Allocate a fresh root path (not created) under `base`.
pub fn allocate(base: &Path, kind: RootKind) -> Result<(String, PathBuf)> {
    let id = super::random_hex(8);
    let path = root_path(base, kind, &id)?;
    Ok((id, path))
}

/// What kind of patch a relocation entry needs at container create.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum RelocKind {
    /// Regular non-Mach-O file: byte substitution.
    Text,
    /// Mach-O (thin or fat): byte substitution + ad-hoc re-sign.
    Macho,
    /// Symlink whose target embeds the root.
    Symlink,
}

/// One recorded relocation: root-relative path + kind.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, PartialOrd, Ord)]
pub struct Relocation {
    pub path: String,
    pub kind: RelocKind,
}

/// Counts reported after relocating a root.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelocStats {
    /// Files (and symlinks) whose bytes were rewritten.
    pub files: usize,
    /// Total occurrences replaced.
    pub occurrences: usize,
    /// Mach-O files re-signed ad-hoc.
    pub resigned: usize,
    /// Recorded entries that no longer exist or no longer contain the source
    /// bytes (deleted or replaced by a later layer).
    pub skipped: usize,
}

const MH_MAGIC: u32 = 0xfeed_face;
const MH_CIGAM: u32 = 0xcefa_edfe;
const MH_MAGIC_64: u32 = 0xfeed_facf;
const MH_CIGAM_64: u32 = 0xcffa_edfe;
const FAT_MAGIC: u32 = 0xcafe_babe;
const FAT_MAGIC_64: u32 = 0xcafe_babf;

/// Is `header` (the first bytes of a file) a Mach-O image? Fat headers are
/// accepted only with a plausible architecture count so Java class files
/// (which share `0xCAFEBABE`) are not misclassified.
pub fn is_macho(header: &[u8]) -> bool {
    if header.len() < 8 {
        return false;
    }
    let le = u32::from_le_bytes(header[0..4].try_into().unwrap());
    if matches!(le, MH_MAGIC | MH_MAGIC_64 | MH_CIGAM | MH_CIGAM_64) {
        return true;
    }
    let be = u32::from_be_bytes(header[0..4].try_into().unwrap());
    if matches!(be, FAT_MAGIC | FAT_MAGIC_64) {
        let nfat = u32::from_be_bytes(header[4..8].try_into().unwrap());
        return nfat > 0 && nfat < 45;
    }
    false
}

/// Does a Mach-O with this header carry a code signature worth refreshing?
/// Object files (`MH_OBJECT`) are never signed; executables, dylibs, bundles,
/// and fat files are.
fn macho_wants_signature(header: &[u8]) -> bool {
    if header.len() < 16 {
        return false;
    }
    let le = u32::from_le_bytes(header[0..4].try_into().unwrap());
    let filetype = match le {
        MH_MAGIC | MH_MAGIC_64 => u32::from_le_bytes(header[12..16].try_into().unwrap()),
        MH_CIGAM | MH_CIGAM_64 => u32::from_be_bytes(header[12..16].try_into().unwrap()),
        _ => return is_macho(header), // fat
    };
    // MH_EXECUTE=2, MH_DYLIB=6, MH_BUNDLE=8, MH_DYLINKER=7
    matches!(filetype, 2 | 6 | 7 | 8)
}

/// Replace every occurrence of `from` with `to` (same length) in the recorded
/// files under `root`. Writes only the changed byte ranges in place so an
/// APFS clone keeps sharing every untouched block. Mach-O files are re-signed
/// with `codesign -s - -f`.
pub fn relocate(
    root: &Path,
    relocations: &[Relocation],
    from: &[u8],
    to: &[u8],
) -> Result<RelocStats> {
    if from.len() != to.len() {
        bail!(
            "relocation requires same-length roots ({} vs {} bytes)",
            from.len(),
            to.len()
        );
    }
    let mut stats = RelocStats::default();
    if from == to {
        return Ok(stats);
    }
    for reloc in relocations {
        let rel = sanitize_rel(&reloc.path)?;
        let path = root.join(&rel);
        let meta = match std::fs::symlink_metadata(&path) {
            Ok(meta) => meta,
            Err(_) => {
                stats.skipped += 1;
                continue;
            }
        };
        if meta.file_type().is_symlink() {
            if relocate_symlink(&path, from, to)? {
                stats.files += 1;
                stats.occurrences += 1;
            } else {
                stats.skipped += 1;
            }
            continue;
        }
        if !meta.is_file() {
            stats.skipped += 1;
            continue;
        }
        let count = relocate_file(&path, &meta, from, to)?;
        if count == 0 {
            stats.skipped += 1;
            continue;
        }
        stats.files += 1;
        stats.occurrences += count;
        let mut header = [0u8; 32];
        let n = read_prefix(&path, &mut header)?;
        if is_macho(&header[..n]) && macho_wants_signature(&header[..n]) {
            resign(&path, &meta)?;
            stats.resigned += 1;
        }
    }
    Ok(stats)
}

fn read_prefix(path: &Path, buf: &mut [u8]) -> Result<usize> {
    let file = std::fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut total = 0;
    while total < buf.len() {
        let n = file.read_at(&mut buf[total..], total as u64)?;
        if n == 0 {
            break;
        }
        total += n;
    }
    Ok(total)
}

/// Patch one regular file in place; returns the occurrence count.
fn relocate_file(path: &Path, meta: &std::fs::Metadata, from: &[u8], to: &[u8]) -> Result<usize> {
    let bytes = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
    let offsets: Vec<usize> = memchr::memmem::find_iter(&bytes, from).collect();
    if offsets.is_empty() {
        return Ok(0);
    }
    let mode = meta.permissions().mode();
    let restore_mode = mode & 0o200 == 0;
    if restore_mode {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode | 0o200))
            .with_context(|| format!("make {} writable", path.display()))?;
    }
    let result = (|| -> Result<()> {
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(path)
            .with_context(|| format!("open {} for relocation", path.display()))?;
        for offset in &offsets {
            file.write_all_at(to, *offset as u64)
                .with_context(|| format!("patch {}", path.display()))?;
        }
        Ok(())
    })();
    if restore_mode {
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode));
    }
    result?;
    let atime = filetime::FileTime::from_last_access_time(meta);
    let mtime = filetime::FileTime::from_last_modification_time(meta);
    let _ = filetime::set_file_times(path, atime, mtime);
    Ok(offsets.len())
}

fn relocate_symlink(path: &Path, from: &[u8], to: &[u8]) -> Result<bool> {
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    let target =
        std::fs::read_link(path).with_context(|| format!("readlink {}", path.display()))?;
    let bytes = target.as_os_str().as_bytes();
    if memchr::memmem::find(bytes, from).is_none() {
        return Ok(false);
    }
    let patched = replace_all(bytes, from, to);
    let new_target = PathBuf::from(std::ffi::OsString::from_vec(patched));
    let meta = std::fs::symlink_metadata(path)?;
    std::fs::remove_file(path).with_context(|| format!("remove symlink {}", path.display()))?;
    std::os::unix::fs::symlink(&new_target, path)
        .with_context(|| format!("recreate symlink {}", path.display()))?;
    let mtime = filetime::FileTime::from_last_modification_time(&meta);
    let _ = filetime::set_symlink_file_times(path, mtime, mtime);
    Ok(true)
}

/// Replace every non-overlapping occurrence of `from` with `to`.
pub fn replace_all(haystack: &[u8], from: &[u8], to: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(haystack.len());
    let mut last = 0;
    for offset in memchr::memmem::find_iter(haystack, from) {
        out.extend_from_slice(&haystack[last..offset]);
        out.extend_from_slice(to);
        last = offset + from.len();
    }
    out.extend_from_slice(&haystack[last..]);
    out
}

/// Ad-hoc re-sign a patched Mach-O (`codesign -s - -f`). arm64 macOS refuses
/// to execute code whose signature no longer matches its pages.
fn resign(path: &Path, meta: &std::fs::Metadata) -> Result<()> {
    let mode = meta.permissions().mode();
    let restore_mode = mode & 0o200 == 0;
    if restore_mode {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode | 0o200))?;
    }
    let output = std::process::Command::new("/usr/bin/codesign")
        .args(["-s", "-", "-f"])
        .arg(path)
        .output();
    if restore_mode {
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode));
    }
    let output = output.context("run /usr/bin/codesign")?;
    if !output.status.success() {
        bail!(
            "ad-hoc re-sign of relocated Mach-O {} failed: {}",
            path.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

/// Validate a root-relative path from image metadata (no absolute paths, no
/// `..`), returning it as a relative `PathBuf`.
pub fn sanitize_rel(raw: &str) -> Result<PathBuf> {
    use std::path::Component;
    let mut out = PathBuf::new();
    for component in Path::new(raw).components() {
        match component {
            Component::Normal(part) => out.push(part),
            Component::CurDir | Component::RootDir => {}
            Component::ParentDir | Component::Prefix(_) => {
                bail!("refusing path {raw:?} that escapes the root")
            }
        }
    }
    if out.as_os_str().is_empty() {
        bail!("empty root-relative path");
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholder_has_fixed_length_and_prefix() {
        let p = placeholder();
        assert_eq!(p.len(), ROOT_LEN);
        assert!(p.starts_with("/@@VAT_ROOT@@"));
        assert!(p.ends_with('@'));
    }

    #[test]
    fn root_paths_are_padded_to_fixed_length() {
        let root = root_path(Path::new("/tmp/x"), RootKind::Container, "0123456789abcdef").unwrap();
        let s = root.to_str().unwrap();
        assert_eq!(s.len(), ROOT_LEN);
        assert!(s.starts_with("/tmp/x/c-0123456789abcdef_"));
        let build = root_path(Path::new("/tmp/x/"), RootKind::Build, "0123456789abcdef").unwrap();
        assert!(build
            .to_str()
            .unwrap()
            .starts_with("/tmp/x/b-0123456789abcdef_"));
    }

    #[test]
    fn overlong_base_is_rejected() {
        let base = format!("/{}", "a".repeat(MAX_BASE_LEN));
        let err = root_path(Path::new(&base), RootKind::Container, "0123456789abcdef").unwrap_err();
        assert!(err.to_string().contains("at most"));
        let ok = format!("/{}", "a".repeat(MAX_BASE_LEN - 1));
        assert!(root_path(Path::new(&ok), RootKind::Container, "0123456789abcdef").is_ok());
    }

    #[test]
    fn macho_detection() {
        assert!(is_macho(&[
            0xcf, 0xfa, 0xed, 0xfe, 0x0c, 0, 0, 1, 0, 0, 0, 0
        ]));
        assert!(is_macho(&[0xca, 0xfe, 0xba, 0xbe, 0, 0, 0, 2]));
        // Java class file: CAFEBABE + minor/major version (e.g. 0x0000_0034).
        assert!(!is_macho(&[0xca, 0xfe, 0xba, 0xbe, 0, 0, 0, 0x34]));
        assert!(!is_macho(b"#!/bin/sh\n"));
    }

    #[test]
    fn relocate_rewrites_text_and_symlinks_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let from = placeholder();
        let to = format!("/{}", "z".repeat(ROOT_LEN - 1));
        std::fs::write(
            root.join("script.sh"),
            format!("#!/bin/sh\nexec {from}/bin/tool {from}/x\n"),
        )
        .unwrap();
        std::fs::set_permissions(
            root.join("script.sh"),
            std::fs::Permissions::from_mode(0o555),
        )
        .unwrap();
        std::os::unix::fs::symlink(format!("{from}/target"), root.join("link")).unwrap();
        std::fs::write(root.join("plain.txt"), "nothing here").unwrap();
        let relocs = vec![
            Relocation {
                path: "script.sh".into(),
                kind: RelocKind::Text,
            },
            Relocation {
                path: "link".into(),
                kind: RelocKind::Symlink,
            },
            Relocation {
                path: "plain.txt".into(),
                kind: RelocKind::Text,
            },
            Relocation {
                path: "gone".into(),
                kind: RelocKind::Text,
            },
        ];
        let stats = relocate(root, &relocs, from.as_bytes(), to.as_bytes()).unwrap();
        assert_eq!(stats.files, 2);
        assert_eq!(stats.occurrences, 3);
        assert_eq!(stats.skipped, 2);
        let script = std::fs::read_to_string(root.join("script.sh")).unwrap();
        assert_eq!(script, format!("#!/bin/sh\nexec {to}/bin/tool {to}/x\n"));
        let mode = std::fs::metadata(root.join("script.sh"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o555);
        assert_eq!(
            std::fs::read_link(root.join("link")).unwrap(),
            PathBuf::from(format!("{to}/target"))
        );
    }

    #[test]
    fn sanitize_rejects_escapes() {
        assert!(sanitize_rel("../etc/passwd").is_err());
        assert!(sanitize_rel("a/../../b").is_err());
        assert_eq!(sanitize_rel("/app/x").unwrap(), PathBuf::from("app/x"));
    }
}
