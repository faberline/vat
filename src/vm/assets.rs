// CODEGEN-BEGIN
//! Machine boot assets: kernel, initramfs, data disk, and the guest scripts
//! copied into the state share.
//!
//! Everything is fetched or generated lazily on `vat machine start` and
//! cached under `~/.vat/machine/assets`. Downloads are pinned by SHA-256.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};

use super::{write_atomic, MachineConfig, MachinePaths};

const MINIROOTFS_PATH: &str = "v3.22/releases/aarch64/alpine-minirootfs-3.22.6-aarch64.tar.gz";
const MINIROOTFS_SHA256: &str = "821565fa8f3953eefd12497b166b4b50add2f7c57fb312e75862f5867e06fefe";

/// Alpine mirrors probed when a machine first needs packages; the fastest
/// one from this host wins. The CDN alone can crawl at tens of KB/s from some
/// networks, which turns a one-minute provision into a timeout.
const ALPINE_CDN: &str = "https://dl-cdn.alpinelinux.org/alpine";
const ALPINE_MIRRORS: &[&str] = &[
    ALPINE_CDN,
    "https://mirrors.edge.kernel.org/alpine",
    "https://mirror.leaseweb.com/alpine",
    "https://uk.alpinelinux.org/alpine",
    "https://mirror.xtom.com.hk/alpine",
    "https://ftp.udx.icscoe.jp/Linux/alpine",
    "https://mirrors.tuna.tsinghua.edu.cn/alpine",
    "https://mirror.twds.com.tw/alpine",
];
/// How long a probed mirror choice is reused before probing again.
const MIRROR_TTL: std::time::Duration = std::time::Duration::from_secs(7 * 24 * 3600);

/// Kata Containers publishes the arm64 guest kernel Apple's `container` uses;
/// it has everything dockerd and K3s need built in (no modules).
const KATA_RELEASE_URL: &str = "https://github.com/kata-containers/kata-containers/releases/download/3.28.0/kata-static-3.28.0-arm64.tar.zst";
const KATA_KERNEL_MEMBER: &str = "./opt/kata/share/kata-containers/vmlinux-6.18.15-186";
const KERNEL_NAME: &str = "vmlinux-6.18.15-186";

/// Guest files, embedded so a vat binary always carries matching guest logic.
const GUEST_FILES: &[(&str, &str, bool)] = &[
    ("bootstrap.sh", include_str!("guest/bootstrap.sh"), true),
    ("provision.sh", include_str!("guest/provision.sh"), true),
    ("configure.sh", include_str!("guest/configure.sh"), true),
    ("udhcpc.script", include_str!("guest/udhcpc.script"), true),
    ("inittab", include_str!("guest/inittab"), false),
    ("vat-net.initd", include_str!("guest/vat-net.initd"), true),
    (
        "vat-agent.initd",
        include_str!("guest/vat-agent.initd"),
        true,
    ),
    ("k3s.initd", include_str!("guest/k3s.initd"), true),
    ("daemon.json", include_str!("guest/daemon.json"), false),
    ("versions.env", include_str!("guest/versions.env"), false),
];
/// The static in-VM agent (vsock dialer, uplinks, status heartbeat), built
/// from `guest-agent/` by `scripts/build-guest-agent.sh`.
const GUEST_AGENT: &[u8] = include_bytes!("../../guest-agent/dist/vat-guest-aarch64");
const INIT: &str = include_str!("guest/init");
const UDHCPC: &str = include_str!("guest/udhcpc.script");

/// Resolved boot inputs for the VMM.
#[derive(Debug, Clone)]
pub struct BootAssets {
    pub kernel: PathBuf,
    pub initramfs: PathBuf,
}

/// Prepare kernel + initramfs (cached) and return their paths.
pub fn ensure_boot_assets(paths: &MachinePaths) -> Result<BootAssets> {
    std::fs::create_dir_all(&paths.assets)?;
    Ok(BootAssets {
        kernel: ensure_kernel(&paths.assets)?,
        initramfs: ensure_initramfs(&paths.assets)?,
    })
}

fn ensure_kernel(assets: &Path) -> Result<PathBuf> {
    if let Some(custom) = std::env::var_os("VAT_MACHINE_KERNEL") {
        let custom = PathBuf::from(custom);
        if !custom.is_file() {
            bail!("VAT_MACHINE_KERNEL={} is not a file", custom.display());
        }
        return Ok(custom);
    }
    let dest = assets.join(KERNEL_NAME);
    if dest.is_file() {
        return Ok(dest);
    }
    // Reuse the identical kernel when Apple's `container` already fetched it.
    if let Some(home) = dirs::home_dir() {
        let apple = home
            .join("Library/Application Support/com.apple.container/kernels")
            .join(KERNEL_NAME);
        if apple.is_file() {
            std::fs::copy(&apple, &dest)
                .with_context(|| format!("copy kernel from {}", apple.display()))?;
            return Ok(dest);
        }
    }
    eprintln!("vat machine: fetching the guest kernel (one-time, ~600 MB Kata release archive)");
    let staging = tempfile::tempdir_in(assets)?;
    let status = Command::new("/bin/sh")
        .arg("-c")
        .arg(r#"curl -fsSL "$1" | tar -x -C "$2" -f - "$3""#)
        .arg("sh")
        .arg(KATA_RELEASE_URL)
        .arg(staging.path())
        .arg(KATA_KERNEL_MEMBER)
        .status()
        .context("run curl | tar for the kernel")?;
    let extracted = staging.path().join(KATA_KERNEL_MEMBER);
    if !status.success() || !extracted.is_file() {
        bail!("could not extract {KATA_KERNEL_MEMBER} from {KATA_RELEASE_URL}");
    }
    std::fs::rename(&extracted, &dest)?;
    Ok(dest)
}

fn ensure_minirootfs(assets: &Path) -> Result<PathBuf> {
    let dest = assets.join("alpine-minirootfs.tar.gz");
    if dest.is_file() && sha256_file(&dest)? == MINIROOTFS_SHA256 {
        return Ok(dest);
    }
    let url = format!("{}/{MINIROOTFS_PATH}", alpine_mirror(assets));
    download(&url, &dest, MINIROOTFS_SHA256)?;
    Ok(dest)
}

/// The initramfs is the Alpine minirootfs plus vat's tiny `/init`. Its name
/// carries a content hash so changing `init` produces a fresh image.
fn ensure_initramfs(assets: &Path) -> Result<PathBuf> {
    let rootfs = ensure_minirootfs(assets)?;
    let mut h = Sha256::new();
    h.update(MINIROOTFS_SHA256.as_bytes());
    h.update(INIT.as_bytes());
    h.update(UDHCPC.as_bytes());
    let tag = &hex(&h.finalize())[..12];
    let dest = assets.join(format!("initramfs-{tag}.cpio.gz"));
    if dest.is_file() {
        return Ok(dest);
    }
    let work = tempfile::tempdir_in(assets)?;
    let root = work.path();
    std::fs::create_dir_all(root.join("usr/share/udhcpc"))?;
    write_exec(&root.join("init"), INIT.as_bytes())?;
    write_exec(
        &root.join("usr/share/udhcpc/default.script"),
        UDHCPC.as_bytes(),
    )?;
    let tmp = dest.with_extension("part");
    // bsdtar can splice the minirootfs archive (`@archive`) into a newc cpio.
    let status = Command::new("/bin/sh")
        .arg("-c")
        .arg(r#"cd "$1" && tar -cf - --format newc --uid 0 --gid 0 "@$2" init usr/share/udhcpc/default.script | gzip -6 > "$3""#)
        .arg("sh")
        .arg(root)
        .arg(&rootfs)
        .arg(&tmp)
        .status()
        .context("build the initramfs with tar")?;
    if !status.success() {
        bail!("building the initramfs failed");
    }
    std::fs::rename(&tmp, &dest)?;
    Ok(dest)
}

/// Create the sparse data disk, growing (never shrinking) it to the
/// configured size. The guest formats it on first boot.
pub fn ensure_data_disk(paths: &MachinePaths, cfg: &MachineConfig) -> Result<()> {
    std::fs::create_dir_all(&paths.dir)?;
    let want = cfg.disk_gib * 1024 * 1024 * 1024;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&paths.data_img)
        .with_context(|| format!("open {}", paths.data_img.display()))?;
    if file.metadata()?.len() < want {
        file.set_len(want)?;
    }
    Ok(())
}

/// Write the guest scripts and per-start configuration into the state share.
pub fn write_guest_files(paths: &MachinePaths, cfg: &MachineConfig) -> Result<()> {
    std::fs::create_dir_all(&paths.guest)?;
    let mut version = Sha256::new();
    for (name, body, exec) in GUEST_FILES {
        let path = paths.guest.join(name);
        if *exec {
            write_exec(&path, body.as_bytes())?;
        } else {
            write_atomic(&path, body.as_bytes())?;
        }
        if matches!(*name, "provision.sh" | "versions.env") {
            version.update(body.as_bytes());
        }
    }
    write_exec(&paths.guest.join("vat-guest"), GUEST_AGENT)?;
    // Not part of the provision version: switching mirrors must not re-provision.
    write_atomic(
        &paths.guest.join("alpine.mirror"),
        format!("{}\n", alpine_mirror(&paths.assets)).as_bytes(),
    )?;
    write_atomic(
        &paths.guest.join("version"),
        hex(&version.finalize())[..16].as_bytes(),
    )?;
    let mut mounts = String::new();
    for m in &cfg.host_mounts {
        for t in &m.targets {
            mounts.push_str(&format!("{} {}\n", m.name, t));
        }
    }
    write_atomic(&paths.guest.join("host-mounts"), mounts.as_bytes())?;
    let mut uplinks = String::new();
    for u in &cfg.uplinks {
        uplinks.push_str(&format!("{} {} {}\n", u.bind, u.port, u.service));
    }
    write_atomic(&paths.guest.join("uplinks"), uplinks.as_bytes())?;
    let hosts: String = cfg.extra_hosts.iter().map(|l| format!("{l}\n")).collect();
    write_atomic(&paths.guest.join("hosts.extra"), hosts.as_bytes())?;
    let k3s_flag = paths.guest.join("k3s.enabled");
    if cfg.k8s {
        write_atomic(&k3s_flag, b"1")?;
    } else if k3s_flag.exists() {
        std::fs::remove_file(&k3s_flag)?;
    }
    // Guest-written files from the previous boot are stale now.
    for stale in [paths.guest_status(), paths.guest_boot()] {
        let _ = std::fs::remove_file(stale);
    }
    Ok(())
}

/// The Alpine mirror for boot assets and guest packages: `VAT_ALPINE_MIRROR`,
/// else the fastest probed mirror (cached under `assets`), else the CDN.
pub fn alpine_mirror(assets: &Path) -> String {
    if let Ok(m) = std::env::var("VAT_ALPINE_MIRROR") {
        if !m.trim().is_empty() {
            return m.trim().trim_end_matches('/').to_string();
        }
    }
    let cache = assets.join("alpine-mirror");
    let fresh = std::fs::metadata(&cache)
        .and_then(|m| m.modified())
        .is_ok_and(|t| t.elapsed().is_ok_and(|age| age < MIRROR_TTL));
    if fresh {
        if let Ok(m) = std::fs::read_to_string(&cache) {
            if ALPINE_MIRRORS.contains(&m.trim()) {
                return m.trim().to_string();
            }
        }
    }
    let best = probe_mirrors().unwrap_or_else(|| ALPINE_CDN.to_string());
    let _ = std::fs::create_dir_all(assets);
    let _ = write_atomic(&cache, best.as_bytes());
    best
}

/// Time a short download of each mirror's package index in parallel and
/// return the fastest that answers.
fn probe_mirrors() -> Option<String> {
    let probes: Vec<_> = ALPINE_MIRRORS
        .iter()
        .filter_map(|m| {
            Command::new("curl")
                .args([
                    "-s",
                    "-o",
                    "/dev/null",
                    "-m",
                    "4",
                    "-w",
                    "%{http_code} %{speed_download}",
                ])
                .arg(format!("{m}/v3.22/main/aarch64/APKINDEX.tar.gz"))
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::null())
                .spawn()
                .ok()
                .map(|child| (*m, child))
        })
        .collect();
    let mut best: Option<(f64, &str)> = None;
    for (mirror, child) in probes {
        // A timed-out transfer (exit 28) still reports a useful speed.
        let Ok(out) = child.wait_with_output() else {
            continue;
        };
        let text = String::from_utf8_lossy(&out.stdout);
        let mut parts = text.split_whitespace();
        if parts.next() != Some("200") {
            continue;
        }
        let speed: f64 = parts.next().and_then(|v| v.parse().ok()).unwrap_or(0.0);
        if best.is_none_or(|(s, _)| speed > s) {
            best = Some((speed, mirror));
        }
    }
    best.map(|(_, m)| m.to_string())
}

fn write_exec(path: &Path, body: &[u8]) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    write_atomic(path, body)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))?;
    Ok(())
}

fn download(url: &str, dest: &Path, sha256: &str) -> Result<()> {
    let tmp = dest.with_extension("part");
    let status = Command::new("curl")
        .args(["-fsSL", "-o"])
        .arg(&tmp)
        .arg(url)
        .status()
        .context("run curl")?;
    if !status.success() {
        bail!("download failed: {url}");
    }
    let got = sha256_file(&tmp)?;
    if got != sha256 {
        let _ = std::fs::remove_file(&tmp);
        bail!("checksum mismatch for {url}: got {got}, want {sha256}");
    }
    std::fs::rename(&tmp, dest)?;
    Ok(())
}

pub(crate) fn sha256_file(path: &Path) -> Result<String> {
    let mut f = std::fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(hex(&h.finalize()))
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The committed agent binary must be rebuilt whenever its sources change.
    #[test]
    fn guest_agent_binary_matches_sources() {
        let crate_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("guest-agent");
        let mut h = Sha256::new();
        for f in ["Cargo.toml", "Cargo.lock", "src/main.rs"] {
            h.update(std::fs::read(crate_dir.join(f)).unwrap());
        }
        let recorded = std::fs::read_to_string(crate_dir.join("dist/SOURCE_SHA256")).unwrap();
        assert_eq!(
            recorded.trim(),
            hex(&h.finalize()),
            "guest-agent sources changed; run scripts/build-guest-agent.sh"
        );
        assert_eq!(&GUEST_AGENT[..4], b"\x7fELF");
    }
}
// CODEGEN-END
