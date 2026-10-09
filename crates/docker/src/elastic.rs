//! Host resources: what the machine costs macOS, the data disk shrinking
//! back, and the guest clock following the host's across sleep.
//!
//! - Disk: the guest agent discards free blocks (FITRIM) and
//!   Virtualization.framework punches the matching holes in the sparse
//!   `data.img`, so deleted images and volumes give the space back.
//! - Memory: guest RAM lives in Virtualization.framework's own VM process,
//!   not the VMM, so its physical footprint is what macOS pays. It is the
//!   high-water mark of what the guest ever touched: Virtualization.framework
//!   never returns guest pages while the VM runs. Its virtio balloon does
//!   take pages from the guest (measured: 2816 MiB inflated) but the VM
//!   process's footprint stays byte-identical, so the VMM does not drive it.
//!   Memory goes back only when the VM stops, so the VMM stops it once idle
//!   ([`IdleWatch`]) and launchd boots it on the next Docker client
//!   ([`crate::launchd`]).
//! - Clock: the guest's clock stands still while the host sleeps; the VMM
//!   notices the wall/monotonic gap and steps the guest clock.

use std::time::{Duration, Instant, SystemTime};

use serde::{Deserialize, Serialize};

/// Notices host sleep: the wall clock runs on while the host sleeps but
/// `Instant` (and the guest's clock) does not.
#[derive(Debug, Clone, Copy)]
pub struct WakeWatch {
    wall: SystemTime,
    mono: Instant,
}

/// A wall/monotonic gap above this means the host slept.
pub const SLEEP_GAP: Duration = Duration::from_secs(2);

impl WakeWatch {
    pub fn new(wall: SystemTime, mono: Instant) -> Self {
        Self { wall, mono }
    }

    /// Returns how long the host slept since the last check, if it did.
    pub fn check(&mut self, wall: SystemTime, mono: Instant) -> Option<Duration> {
        let wall_d = wall.duration_since(self.wall).unwrap_or_default();
        let mono_d = mono - self.mono;
        *self = Self { wall, mono };
        let gap = wall_d.saturating_sub(mono_d);
        (gap > SLEEP_GAP).then_some(gap)
    }
}

/// `elastic.json` next to the machine config: the VMM's view, for status.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ElasticState {
    /// The Virtualization.framework process holding guest memory.
    pub vm_pid: Option<u32>,
    /// Last clock sync with the guest: unix seconds, and the guest's offset
    /// from the host before it (ms).
    pub clock_synced_at: Option<i64>,
    pub clock_skew_ms: Option<i64>,
    pub host_sleeps: u64,
    /// Why and when the VMM last stopped: `idle`, `signal`, or `guest`.
    pub stopped_by: Option<String>,
    pub stopped_at: Option<i64>,
}

/// Decides when an idle machine should stop. Idle means nothing uses it: no
/// client connection, no running container, no addon keeping it awake. The
/// clock is monotonic, so time the host spends asleep does not count.
#[derive(Debug, Clone)]
pub struct IdleWatch {
    after: Duration,
    idle_since: Option<Instant>,
}

impl IdleWatch {
    /// Stop after `after` of idleness; zero never stops.
    pub fn new(after: Duration) -> Self {
        Self {
            after,
            idle_since: None,
        }
    }

    pub fn set_after(&mut self, after: Duration) {
        self.after = after;
    }

    /// Record whether the machine is in use now; true once it has been idle
    /// for long enough.
    pub fn observe(&mut self, busy: bool, now: Instant) -> bool {
        if busy || self.after.is_zero() {
            self.idle_since = None;
            return false;
        }
        let since = *self.idle_since.get_or_insert(now);
        now.duration_since(since) >= self.after
    }
}

/// Memory and CPU use of a process.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct ProcUsage {
    /// Physical footprint, what Activity Monitor shows as Memory.
    pub footprint_kib: u64,
    pub cpu_ms: u64,
}

#[cfg(target_os = "macos")]
#[allow(deprecated)] // libc points at the mach2 crate for the timebase
pub fn proc_usage(pid: u32) -> Option<ProcUsage> {
    let mut info: libc::rusage_info_v4 = unsafe { std::mem::zeroed() };
    let rc = unsafe {
        libc::proc_pid_rusage(
            pid as libc::c_int,
            libc::RUSAGE_INFO_V4,
            &mut info as *mut _ as *mut libc::rusage_info_t,
        )
    };
    if rc != 0 {
        return None;
    }
    // CPU times are in Mach absolute time units.
    let mut tb = libc::mach_timebase_info { numer: 0, denom: 0 };
    unsafe { libc::mach_timebase_info(&mut tb) };
    let ticks = info.ri_user_time + info.ri_system_time;
    let ns = ticks as u128 * tb.numer.max(1) as u128 / tb.denom.max(1) as u128;
    Some(ProcUsage {
        footprint_kib: info.ri_phys_footprint / 1024,
        cpu_ms: (ns / 1_000_000) as u64,
    })
}

#[cfg(not(target_os = "macos"))]
pub fn proc_usage(_pid: u32) -> Option<ProcUsage> {
    None
}

/// Pids of the Virtualization.framework VM processes (one per running VM).
#[cfg(target_os = "macos")]
pub fn vm_process_pids() -> Vec<u32> {
    const NAME: &str = "com.apple.Virtualization.VirtualMachine";
    let n = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
    if n <= 0 {
        return Vec::new();
    }
    let mut pids = vec![0 as libc::pid_t; n as usize + 64];
    let bytes = (pids.len() * std::mem::size_of::<libc::pid_t>()) as libc::c_int;
    let n = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), bytes) };
    pids.truncate(n.max(0) as usize);
    let mut buf = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    pids.into_iter()
        .filter(|&pid| {
            let len = unsafe { libc::proc_pidpath(pid, buf.as_mut_ptr().cast(), buf.len() as u32) };
            len > 0 && buf[..len as usize].ends_with(NAME.as_bytes())
        })
        .map(|pid| pid as u32)
        .collect()
}

#[cfg(not(target_os = "macos"))]
pub fn vm_process_pids() -> Vec<u32> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wake_watch_sees_only_wall_clock_jumps() {
        let wall = SystemTime::now();
        let mono = Instant::now();
        let mut w = WakeWatch::new(wall, mono);
        let d = Duration::from_secs(1);
        assert_eq!(w.check(wall + d, mono + d), None);
        let slept = w.check(wall + d * 602, mono + d * 2);
        assert_eq!(slept, Some(d * 600));
        assert_eq!(w.check(wall + d * 603, mono + d * 3), None);
    }

    #[test]
    fn idle_watch_stops_only_after_a_quiet_stretch() {
        let t = Instant::now();
        let s = Duration::from_secs(1);
        let mut w = IdleWatch::new(s * 60);
        assert!(!w.observe(false, t));
        assert!(!w.observe(false, t + s * 59));
        // Any use restarts the count.
        assert!(!w.observe(true, t + s * 59));
        assert!(!w.observe(false, t + s * 60));
        assert!(!w.observe(false, t + s * 119));
        assert!(w.observe(false, t + s * 120));

        let mut never = IdleWatch::new(Duration::ZERO);
        assert!(!never.observe(false, t));
        assert!(!never.observe(false, t + s * 86_400));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn proc_usage_reads_this_process() {
        let u = proc_usage(std::process::id()).unwrap();
        assert!(u.footprint_kib > 0);
    }
}
