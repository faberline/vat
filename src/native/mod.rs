//! Native runtime: lightweight Apple-native containers (pillar 1).
//!
//! A native container is a **macOS process** — so Metal, MPS, MLX, and
//! `tensorflow-metal` reach the Apple GPU exactly as on the host — whose
//! filesystem is an APFS `clonefile` copy-on-write root materialized from an
//! OCI image with a `darwin/arm64` platform. There is no Linux and no VM.
//!
//! ## Why roots have a fixed length
//!
//! macOS has no unprivileged chroot and no mount namespaces, so a container's
//! root is an ordinary host directory and absolute paths baked into image
//! content (shebangs, `pyvenv.cfg`, `.pth` files, Mach-O load paths) must be
//! *relocatable*. vat uses Homebrew-style placeholder relocation:
//!
//! - Every root — build-time and run-time — lives at a path of exactly
//!   [`root::ROOT_LEN`] bytes (`<base>/<kind>-<hex>` padded with `_`).
//! - When a build step is committed into a layer, every new/changed regular
//!   file (and symlink target) containing the build root's bytes has them
//!   replaced by a same-length placeholder (`/@@VAT_ROOT@@@@…`), and the
//!   patched paths are recorded on the layer descriptor
//!   (`vat.relocations` annotation).
//! - At container create, the cached unpacked snapshot is `clonefile`d into the
//!   container root and only the recorded files are rewritten placeholder →
//!   actual root. The length is identical, so binary offsets stay valid;
//!   patched Mach-O files are re-signed ad-hoc (`codesign -s - -f`).
//!
//! ## Environment contract
//!
//! Inside a container `VAT_ROOT` is the actual root. Image `WorkingDir`,
//! absolute `Entrypoint`/`Cmd` executables that exist in the image, and
//! absolute `PATH` entries are interpreted relative to the root; the host
//! system dirs `/usr/bin:/bin:/usr/sbin:/sbin` are appended to `PATH`.
//! `$VAT_ROOT` / `${VAT_ROOT}` in image `Env` values is substituted at run
//! time, so image authors refer to in-image absolute paths as
//! `$VAT_ROOT/app/...` in `ENV` and `RUN`.
//!
//! ## Confinement
//!
//! Seatbelt (`sandbox-exec`) via
//! [`crate::sandbox::seatbelt::native_container_profile`]: writes only under
//! the root, read-write mounts, and the per-user cache dir; `--network none`
//! denies network. Optional per-container UIDs from a root-created user pool
//! ([`users`]). Not a hostile-code boundary: macOS has no namespaces/cgroups.

pub mod build;
pub mod container;
#[cfg(feature = "registry")]
pub mod distribution;
pub mod layer;
pub mod oci;
pub mod root;
pub mod store;
pub mod users;

use std::path::PathBuf;

use anyhow::{Context, Result};

/// The native-runtime home (`~/.vat/native` by default, see
/// [`crate::paths::native_home`]), created and canonicalized so every path
/// derived from it is a real path (seatbelt matches real paths, and the
/// relocation scan matches the exact bytes processes see).
pub fn home() -> Result<PathBuf> {
    let home = crate::paths::native_home()?;
    std::fs::create_dir_all(&home)
        .with_context(|| format!("create native home {}", home.display()))?;
    std::fs::canonicalize(&home).with_context(|| format!("canonicalize {}", home.display()))
}

/// Directory holding native container records (`<home>/containers`).
pub fn containers_dir() -> Result<PathBuf> {
    Ok(home()?.join("containers"))
}

/// Base directory for fixed-length roots: `$VAT_NATIVE_ROOT_BASE`, else
/// `<home>/r`. Created and canonicalized.
pub fn roots_base() -> Result<PathBuf> {
    let base = match std::env::var_os("VAT_NATIVE_ROOT_BASE") {
        Some(custom) => PathBuf::from(custom),
        None => home()?.join("r"),
    };
    std::fs::create_dir_all(&base)
        .with_context(|| format!("create native root base {}", base.display()))?;
    std::fs::canonicalize(&base).with_context(|| format!("canonicalize {}", base.display()))
}

/// Random lowercase hex of `bytes` random bytes.
pub(crate) fn random_hex(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    getrandom::fill(&mut buf).expect("OS randomness");
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

/// Remove a tree even when it contains read-only directories (image content
/// commonly has 0555 dirs).
pub(crate) fn remove_tree(path: &std::path::Path) -> Result<()> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err).with_context(|| format!("stat {}", path.display())),
    };
    if !meta.is_dir() {
        return std::fs::remove_file(path).with_context(|| format!("remove {}", path.display()));
    }
    if std::fs::remove_dir_all(path).is_ok() {
        return Ok(());
    }
    use std::os::unix::fs::PermissionsExt;
    for entry in walkdir::WalkDir::new(path)
        .follow_links(false)
        .into_iter()
        .flatten()
    {
        if entry.file_type().is_dir() {
            if let Ok(meta) = entry.metadata() {
                let mut perms = meta.permissions();
                perms.set_mode(perms.mode() | 0o700);
                let _ = std::fs::set_permissions(entry.path(), perms);
            }
        }
    }
    std::fs::remove_dir_all(path).with_context(|| format!("remove {}", path.display()))
}

/// Clone `src` (a file or a whole tree) to `dst` (which must not exist):
/// one APFS `clonefile(2)` when possible, else a faithful recursive copy that
/// keeps modes, symlinks, and mtimes (and, unlike the vat workspace clone,
/// skips nothing).
pub(crate) fn clone_tree(src: &std::path::Path, dst: &std::path::Path) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        if crate::overlay::clonefile_raw(src, dst).is_ok() {
            return Ok(());
        }
    }
    copy_faithful(src, dst)
}

fn copy_faithful(src: &std::path::Path, dst: &std::path::Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let meta = std::fs::symlink_metadata(src).with_context(|| format!("stat {}", src.display()))?;
    if !meta.is_dir() {
        copy_entry(src, dst, &meta)?;
        return Ok(());
    }
    let mut dirs = Vec::new();
    for entry in walkdir::WalkDir::new(src).follow_links(false) {
        let entry = entry?;
        let rel = entry.path().strip_prefix(src).expect("under src");
        let target = dst.join(rel);
        let meta = entry.metadata()?;
        if meta.is_dir() {
            std::fs::create_dir_all(&target)
                .with_context(|| format!("create {}", target.display()))?;
            std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755))?;
            dirs.push((target, meta));
        } else {
            copy_entry(entry.path(), &target, &meta)?;
        }
    }
    for (dir, meta) in dirs.into_iter().rev() {
        let mtime = filetime::FileTime::from_last_modification_time(&meta);
        let _ = filetime::set_file_times(&dir, mtime, mtime);
        std::fs::set_permissions(&dir, meta.permissions())?;
    }
    Ok(())
}

fn copy_entry(
    src: &std::path::Path,
    dst: &std::path::Path,
    meta: &std::fs::Metadata,
) -> Result<()> {
    let mtime = filetime::FileTime::from_last_modification_time(meta);
    if meta.file_type().is_symlink() {
        let target = std::fs::read_link(src)?;
        std::os::unix::fs::symlink(&target, dst)
            .with_context(|| format!("symlink {}", dst.display()))?;
        let _ = filetime::set_symlink_file_times(dst, mtime, mtime);
    } else if meta.is_file() {
        std::fs::copy(src, dst).with_context(|| format!("copy {}", src.display()))?;
        let _ = filetime::set_file_times(dst, mtime, mtime);
    }
    Ok(())
}

/// Write `bytes` to `path` atomically (temp file + rename in the same dir).
pub(crate) fn write_atomic(path: &std::path::Path, bytes: &[u8]) -> Result<()> {
    let dir = path
        .parent()
        .context("atomic write target has no parent directory")?;
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let tmp = dir.join(format!(
        ".{}.tmp-{}",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("file"),
        random_hex(6)
    ));
    std::fs::write(&tmp, bytes).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("rename into {}", path.display()))?;
    Ok(())
}
