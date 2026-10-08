// CODEGEN-BEGIN
//! macOS seatbelt backend.
//!
//! Wraps the command in `sandbox-exec` with a generated profile that allows
//! broad reads (so toolchains resolve) but confines **writes** to the vat's
//! rootfs and the system temp dirs. The GPU is untouched: a seatbelt'd process
//! is still a host process, so Metal/MPS/MLX keep working — the contrast with
//! Docker's Linux VM holds even under isolation.
//!
//! `sandbox-exec` is deprecated by Apple but remains functional and is the
//! pragmatic v1 mechanism. A future backend may move to the Endpoint Security
//! / App Sandbox entitlement route; this trait boundary makes that swap local.

use std::path::Path;

use crate::sandbox::Sandbox;
use crate::spec::EgressPolicy;

pub struct SeatbeltBackend {
    /// Outbound network egress policy baked into the generated profile.
    pub egress: EgressPolicy,
}

/// Is `sandbox-exec` present on this host?
pub fn available() -> bool {
    which("sandbox-exec").is_some()
}

impl Sandbox for SeatbeltBackend {
    fn name(&self) -> &'static str {
        "seatbelt"
    }

    fn resolve(&self, rootfs: &Path, program: &str, args: &[String]) -> (String, Vec<String>) {
        // Wrap the command in `sandbox-exec -p <profile> -- <program> <args>`.
        let profile = profile_for(rootfs, self.egress);
        let mut argv = vec!["-p".to_string(), profile, program.to_string()];
        argv.extend(args.iter().cloned());
        ("sandbox-exec".to_string(), argv)
    }
}

/// Build a seatbelt profile string confining writes to `rootfs` + temp, and —
/// per the egress policy — restricting outbound network. With
/// [`EgressPolicy::Open`] the profile is byte-identical to the write-only
/// confinement (no network lines), so existing seatbelt runs are unchanged.
fn profile_for(rootfs: &Path, egress: EgressPolicy) -> String {
    let root = rootfs.display();
    // (allow default) then deny writes, then re-allow writes only under the
    // rootfs subtree and temp. Reads stay open so interpreters/toolchains
    // resolve their libraries.
    let mut profile = format!(
        "(version 1)\n\
         (allow default)\n\
         (deny file-write*)\n\
         (allow file-write* (subpath \"{root}\"))\n\
         (allow file-write* (subpath \"/private/tmp\"))\n\
         (allow file-write* (subpath \"/private/var/folders\"))\n\
         (allow file-write* (subpath \"/tmp\"))\n"
    );
    profile.push_str(egress_rules(egress));
    profile
}

/// Network egress rules shared by every generated profile: only OUTBOUND
/// network is filtered; reads/file/GPU are untouched. localhost stays
/// reachable under `localhost-only` so vat's local emulators + http-mock proxy
/// still work (the routing in v1/v2 targets 127.0.0.1).
fn egress_rules(egress: EgressPolicy) -> &'static str {
    match egress {
        EgressPolicy::Open => "",
        EgressPolicy::LocalhostOnly => {
            "(deny network*)\n\
             (allow network* (remote ip \"localhost:*\"))\n\
             (allow network* (remote unix-socket))\n"
        }
        EgressPolicy::Deny => "(deny network*)\n",
    }
}

/// Quote a host path as a seatbelt string literal.
fn sb_literal(path: &Path) -> String {
    let raw = path.display().to_string();
    format!("\"{}\"", raw.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Profile for a native-runtime container or a `vat image build` RUN step.
///
/// Stricter than [`profile_for`] on writes: there is **no** blanket allowance
/// for `/tmp`, `/private/tmp`, or `/private/var/folders`. Writes are allowed
/// only under the container root (whose `tmp/` is the workload's `TMPDIR`),
/// each extra writable subpath (read-write `-v` mounts, the invoking user's
/// per-user cache dir for Metal shader caches), and the character devices
/// every process expects (`/dev/null`, ttys, `/dev/fd/*`). Reads stay broad
/// (`allow default`) so dyld, the shared cache, and system frameworks —
/// including Metal — resolve; a macOS process cannot run without them.
/// `egress` reuses the vat seatbelt backend's network rules (`--network none`
/// maps to [`EgressPolicy::Deny`]).
pub fn native_container_profile(
    root: &Path,
    writable: &[std::path::PathBuf],
    egress: EgressPolicy,
) -> String {
    let mut profile = String::from(
        "(version 1)\n\
         (allow default)\n\
         (deny file-write*)\n",
    );
    profile.push_str(&format!(
        "(allow file-write* (subpath {}))\n",
        sb_literal(root)
    ));
    for path in writable {
        profile.push_str(&format!(
            "(allow file-write* (subpath {}))\n",
            sb_literal(path)
        ));
    }
    profile.push_str(
        "(allow file-write* (literal \"/dev/null\") (literal \"/dev/zero\") \
         (literal \"/dev/dtracehelper\") (regex #\"^/dev/tty\") (regex #\"^/dev/fd/\"))\n",
    );
    profile.push_str(egress_rules(egress));
    profile
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn root() -> PathBuf {
        PathBuf::from("/vat/rootfs")
    }

    /// `open` must be byte-identical to the pre-egress write-confinement profile.
    #[test]
    fn open_profile_is_write_confinement_only() {
        let expected = "(version 1)\n\
             (allow default)\n\
             (deny file-write*)\n\
             (allow file-write* (subpath \"/vat/rootfs\"))\n\
             (allow file-write* (subpath \"/private/tmp\"))\n\
             (allow file-write* (subpath \"/private/var/folders\"))\n\
             (allow file-write* (subpath \"/tmp\"))\n";
        assert_eq!(profile_for(&root(), EgressPolicy::Open), expected);
        // No network directive at all under `open`.
        assert!(!profile_for(&root(), EgressPolicy::Open).contains("network"));
    }

    #[test]
    fn localhost_only_denies_then_allows_localhost() {
        let p = profile_for(&root(), EgressPolicy::LocalhostOnly);
        assert!(p.contains("(deny network*)"));
        assert!(p.contains("(allow network* (remote ip \"localhost:*\"))"));
        // Write-confinement still present.
        assert!(p.contains("(allow file-write* (subpath \"/vat/rootfs\"))"));
    }

    #[test]
    fn native_profile_confines_writes_without_temp_allowances() {
        let p = native_container_profile(
            Path::new("/r/c-1"),
            &[PathBuf::from("/data/rw")],
            EgressPolicy::Open,
        );
        assert!(p.contains("(deny file-write*)"));
        assert!(p.contains("(allow file-write* (subpath \"/r/c-1\"))"));
        assert!(p.contains("(allow file-write* (subpath \"/data/rw\"))"));
        assert!(!p.contains("/private/tmp"));
        assert!(!p.contains("/private/var/folders"));
        assert!(!p.contains("network"));
        let none = native_container_profile(Path::new("/r/c-1"), &[], EgressPolicy::Deny);
        assert!(none.contains("(deny network*)"));
    }

    #[test]
    fn native_profile_escapes_quotes() {
        let p = native_container_profile(Path::new("/r/a\"b"), &[], EgressPolicy::Open);
        assert!(p.contains("(subpath \"/r/a\\\"b\")"));
    }

    #[test]
    fn deny_blocks_all_outbound_without_localhost_allow() {
        let p = profile_for(&root(), EgressPolicy::Deny);
        assert!(p.contains("(deny network*)"));
        assert!(!p.contains("allow network"));
    }
}

/// Minimal PATH lookup (no extra deps).
fn which(bin: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(bin))
        .find(|candidate| candidate.is_file())
}
// CODEGEN-END
