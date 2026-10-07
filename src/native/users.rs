//! Dedicated per-container UIDs from a root-created hidden user pool.
//!
//! macOS has no user namespaces, so the only way to give a native container
//! its own UID is a real local account. `vat native users setup --count N`
//! (as root) creates hidden `_vat1.._vatN` accounts in a `_vat` group via
//! `dscl`. When vat itself runs with euid 0 and a free pool user exists, the
//! workload is started with `setgid`/`setuid` to that user and the container
//! root is chowned to it. In every other case the workload runs as the
//! invoking user and the container reports `uid_isolation: "unavailable"`
//! with the reason — vat never claims isolation that is not in effect.

use std::collections::BTreeSet;
use std::ffi::{CStr, CString};
use std::path::Path;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

/// Account-name prefix of pool users (`_vat1`, `_vat2`, …) and the group name.
pub const POOL_PREFIX: &str = "_vat";
/// Default first UID/GID for `setup` (outside the macOS 500+ interactive range
/// collisions in practice, and below the 0x7fff… system ranges).
pub const DEFAULT_FIRST_ID: u32 = 4200;
/// Hard cap on pool size probed by discovery.
const MAX_POOL: u32 = 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PoolUser {
    pub name: String,
    pub uid: u32,
    pub gid: u32,
}

/// Look up a local account by name.
pub fn lookup_user(name: &str) -> Option<PoolUser> {
    let cname = CString::new(name).ok()?;
    // SAFETY: getpwnam returns a pointer to static storage or NULL; we copy
    // the fields out immediately.
    let pw = unsafe { libc::getpwnam(cname.as_ptr()) };
    if pw.is_null() {
        return None;
    }
    let pw = unsafe { &*pw };
    let name = unsafe { CStr::from_ptr(pw.pw_name) }.to_string_lossy().to_string();
    Some(PoolUser { name, uid: pw.pw_uid, gid: pw.pw_gid })
}

fn uid_taken(uid: u32) -> Option<String> {
    let pw = unsafe { libc::getpwuid(uid) };
    if pw.is_null() {
        return None;
    }
    Some(unsafe { CStr::from_ptr((*pw).pw_name) }.to_string_lossy().to_string())
}

fn gid_taken(gid: u32) -> Option<String> {
    let gr = unsafe { libc::getgrgid(gid) };
    if gr.is_null() {
        return None;
    }
    Some(unsafe { CStr::from_ptr((*gr).gr_name) }.to_string_lossy().to_string())
}

/// The pool users that exist on this host (`_vat1`, `_vat2`, … until the
/// first gap).
pub fn discover_pool() -> Vec<PoolUser> {
    (1..=MAX_POOL)
        .map(|i| lookup_user(&format!("{POOL_PREFIX}{i}")))
        .take_while(Option::is_some)
        .flatten()
        .collect()
}

pub fn euid() -> u32 {
    unsafe { libc::geteuid() }
}

/// The outcome of the per-container UID decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Run the workload as this pool user.
    Active(PoolUser),
    /// Run as the invoking user; `uid_isolation` is reported unavailable.
    RunAsInvoker { reason: String },
    /// Do not start the container.
    Refuse { reason: String },
}

/// Decide how to run a container's workload.
///
/// - not root → run as the invoking user (isolation unavailable);
/// - root with a free pool user and a root base the pool can traverse → that
///   pool user;
/// - root otherwise → refuse, rather than run the workload as root.
pub fn decide(
    euid: u32,
    pool: &[PoolUser],
    in_use: &BTreeSet<u32>,
    base_traversable: std::result::Result<(), String>,
) -> Decision {
    if euid != 0 {
        let pool_note = if pool.is_empty() {
            "no _vat user pool exists".to_string()
        } else {
            format!("a pool of {} _vat users exists", pool.len())
        };
        return Decision::RunAsInvoker {
            reason: format!(
                "vat is not running as root (euid {euid}), so it cannot switch the workload to a \
                 dedicated user ({pool_note}); the workload runs as the invoking user"
            ),
        };
    }
    if pool.is_empty() {
        return Decision::Refuse {
            reason: "vat is running as root but no _vat user pool exists; refusing to run the \
                     workload as root (create the pool with `vat native users setup --count N`)"
                .into(),
        };
    }
    if let Err(why) = base_traversable {
        return Decision::Refuse {
            reason: format!(
                "pool users cannot reach the container root: {why} (set VAT_NATIVE_ROOT_BASE \
                 to a directory every pool user can traverse, e.g. /Users/Shared/vat-r)"
            ),
        };
    }
    match pool.iter().find(|user| !in_use.contains(&user.uid)) {
        Some(user) => Decision::Active(user.clone()),
        None => Decision::Refuse {
            reason: format!(
                "all {} _vat pool users are in use by running containers; grow the pool with \
                 `vat native users setup --count N`",
                pool.len()
            ),
        },
    }
}

/// Can a non-owner, non-group user traverse every ancestor of `path`?
pub fn world_traversable(path: &Path) -> std::result::Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let mut current = Some(path);
    while let Some(dir) = current {
        if let Ok(meta) = std::fs::metadata(dir) {
            if meta.is_dir() && meta.permissions().mode() & 0o001 == 0 {
                return Err(format!("{} is not world-searchable (o+x)", dir.display()));
            }
        }
        current = dir.parent();
    }
    Ok(())
}

/// `dscl` invocations that create the `_vat` group and `count` hidden pool
/// users with UIDs `first_id+1 ..= first_id+count` and GID `first_id`.
pub fn setup_commands(count: u32, first_id: u32) -> Vec<Vec<String>> {
    let gid = first_id.to_string();
    let mut cmds: Vec<Vec<String>> = Vec::new();
    let dscl = |args: &[&str]| -> Vec<String> {
        let mut v = vec!["/usr/bin/dscl".to_string(), ".".to_string()];
        v.extend(args.iter().map(|s| s.to_string()));
        v
    };
    let group = format!("/Groups/{POOL_PREFIX}");
    cmds.push(dscl(&["-create", &group]));
    cmds.push(dscl(&["-create", &group, "PrimaryGroupID", &gid]));
    cmds.push(dscl(&["-create", &group, "RealName", "vat native container users"]));
    cmds.push(dscl(&["-create", &group, "Password", "*"]));
    for i in 1..=count {
        let user = format!("/Users/{POOL_PREFIX}{i}");
        let uid = (first_id + i).to_string();
        let real = format!("vat native container user {i}");
        cmds.push(dscl(&["-create", &user]));
        cmds.push(dscl(&["-create", &user, "UniqueID", &uid]));
        cmds.push(dscl(&["-create", &user, "PrimaryGroupID", &gid]));
        cmds.push(dscl(&["-create", &user, "UserShell", "/usr/bin/false"]));
        cmds.push(dscl(&["-create", &user, "NFSHomeDirectory", "/var/empty"]));
        cmds.push(dscl(&["-create", &user, "RealName", &real]));
        cmds.push(dscl(&["-create", &user, "IsHidden", "1"]));
        cmds.push(dscl(&["-create", &user, "Password", "*"]));
        cmds.push(dscl(&["-append", &group, "GroupMembership", &format!("{POOL_PREFIX}{i}")]));
    }
    cmds
}

/// Check that the planned IDs don't collide with unrelated accounts.
pub fn check_setup_ids(count: u32, first_id: u32) -> Result<()> {
    if let Some(name) = gid_taken(first_id) {
        if name != POOL_PREFIX {
            bail!("GID {first_id} already belongs to group {name:?}; pass --first-id");
        }
    }
    for i in 1..=count {
        let uid = first_id + i;
        let expected = format!("{POOL_PREFIX}{i}");
        if let Some(name) = uid_taken(uid) {
            if name != expected {
                bail!("UID {uid} already belongs to {name:?}; pass --first-id");
            }
        }
        if let Some(existing) = lookup_user(&expected) {
            if existing.uid != uid {
                bail!("{expected} already exists with UID {}; pass --first-id {}", existing.uid, existing.uid - i);
            }
        }
    }
    Ok(())
}

/// Shell-quote one argument for `--print` output.
pub fn shell_quote(arg: &str) -> String {
    if !arg.is_empty() && arg.bytes().all(|b| b.is_ascii_alphanumeric() || b"/._-=:".contains(&b)) {
        arg.to_string()
    } else {
        format!("'{}'", arg.replace('\'', "'\\''"))
    }
}

/// Run the setup commands (requires euid 0).
pub fn run_setup(count: u32, first_id: u32) -> Result<()> {
    if euid() != 0 {
        bail!(
            "`vat native users setup` must run as root (euid is {}); re-run with sudo, or pass \
             --print to see the dscl commands",
            euid()
        );
    }
    check_setup_ids(count, first_id)?;
    for cmd in setup_commands(count, first_id) {
        let status = std::process::Command::new(&cmd[0])
            .args(&cmd[1..])
            .status()
            .with_context(|| format!("run {}", cmd.join(" ")))?;
        if !status.success() {
            bail!("`{}` failed with {status}", cmd.join(" "));
        }
    }
    Ok(())
}

/// Recursively chown a container root to the pool user (lchown: symlinks
/// themselves, never their targets).
pub fn chown_tree(root: &Path, uid: u32, gid: u32) -> Result<()> {
    for entry in walkdir::WalkDir::new(root).follow_links(false) {
        let entry = entry?;
        std::os::unix::fs::lchown(entry.path(), Some(uid), Some(gid))
            .with_context(|| format!("chown {}", entry.path().display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pool(n: u32) -> Vec<PoolUser> {
        (1..=n)
            .map(|i| PoolUser { name: format!("_vat{i}"), uid: 4200 + i, gid: 4200 })
            .collect()
    }

    #[test]
    fn non_root_runs_as_invoker_with_reason() {
        let d = decide(501, &pool(2), &BTreeSet::new(), Ok(()));
        match d {
            Decision::RunAsInvoker { reason } => {
                assert!(reason.contains("not running as root"));
                assert!(reason.contains("pool of 2"));
            }
            other => panic!("unexpected {other:?}"),
        }
        assert!(matches!(
            decide(501, &[], &BTreeSet::new(), Ok(())),
            Decision::RunAsInvoker { .. }
        ));
    }

    #[test]
    fn root_without_pool_refuses_instead_of_running_as_root() {
        assert!(matches!(decide(0, &[], &BTreeSet::new(), Ok(())), Decision::Refuse { .. }));
    }

    #[test]
    fn root_with_pool_picks_first_free_user() {
        let in_use: BTreeSet<u32> = [4201].into_iter().collect();
        assert_eq!(decide(0, &pool(3), &in_use, Ok(())), Decision::Active(pool(3)[1].clone()));
        let all: BTreeSet<u32> = [4201, 4202].into_iter().collect();
        assert!(matches!(decide(0, &pool(2), &all, Ok(())), Decision::Refuse { .. }));
    }

    #[test]
    fn root_with_untraversable_base_refuses() {
        let d = decide(0, &pool(1), &BTreeSet::new(), Err("/Users/me is not world-searchable".into()));
        match d {
            Decision::Refuse { reason } => assert!(reason.contains("VAT_NATIVE_ROOT_BASE")),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn setup_commands_create_hidden_users() {
        let cmds = setup_commands(2, 4200);
        let flat: Vec<String> = cmds.iter().map(|c| c.join(" ")).collect();
        assert!(flat.contains(&"/usr/bin/dscl . -create /Groups/_vat PrimaryGroupID 4200".to_string()));
        assert!(flat.contains(&"/usr/bin/dscl . -create /Users/_vat2 UniqueID 4202".to_string()));
        assert!(flat.contains(&"/usr/bin/dscl . -create /Users/_vat1 IsHidden 1".to_string()));
        assert!(flat.contains(&"/usr/bin/dscl . -create /Users/_vat1 UserShell /usr/bin/false".to_string()));
        assert_eq!(shell_quote("*"), "'*'");
        assert_eq!(shell_quote("vat native container user 1"), "'vat native container user 1'");
    }

    #[test]
    fn this_host_has_no_pool_or_reports_it() {
        // Discovery must not panic; the pool is normally absent in CI.
        let _ = discover_pool();
        assert!(lookup_user("root").is_some_and(|u| u.uid == 0));
    }
}
