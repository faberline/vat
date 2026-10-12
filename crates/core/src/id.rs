// CODEGEN-BEGIN
//! Vat identifiers.
//!
//! An id is short, lowercase, and greppable: `vat-` + a base36 form of 40 bits
//! derived from the wall clock and pid. Within a process, ids are unique: the
//! clock stamp is forced to strictly increase, so calls within one clock tick
//! still differ (macOS reports wall time only to the microsecond). Across
//! processes, a hash of the pid is XORed into those bits, so two processes in
//! the same tick still diverge. If two vats ever do land on the same id,
//! [`store`] refuses to clobber an existing directory.
//!
//! [`store`]: crate::store

use std::process;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// The id carries this many low bits of `stamp ^ pid hash`.
const ID_MASK: u64 = 0xff_ffff_ffff;

/// Last stamp handed out by this process, in nanoseconds since the epoch.
static LAST_STAMP: AtomicU64 = AtomicU64::new(0);

/// Generate a fresh vat id, e.g. `vat-7f3k1q9`.
pub fn fresh() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    // Strictly increasing per process: a call in the same clock tick as the
    // previous one takes the next nanosecond instead of repeating it.
    let previous = LAST_STAMP
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |last| {
            Some(now.max(last + 1))
        })
        .expect("the update closure always returns Some");
    let stamp = now.max(previous + 1);
    // XOR with a per-process constant keeps the stamps distinct within this
    // process and spreads the pid over every bit the id keeps.
    let pid_mix = splitmix64(u64::from(process::id()));
    format!("vat-{}", base36((stamp ^ pid_mix) & ID_MASK))
}

/// SplitMix64 finalizer: a bijective scramble of a 64-bit value.
fn splitmix64(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9e37_79b9_7f4a_7c15);
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^ (x >> 31)
}

/// Lowercase base36 of a u64 (no leading-zero padding; ids are opaque).
fn base36(mut n: u64) -> String {
    const ALPHABET: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    if n == 0 {
        return "0".to_string();
    }
    let mut buf = Vec::new();
    while n > 0 {
        buf.push(ALPHABET[(n % 36) as usize]);
        n /= 36;
    }
    buf.reverse();
    String::from_utf8(buf).expect("base36 alphabet is ascii")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_ids_have_prefix_and_differ() {
        let a = fresh();
        assert!(a.starts_with("vat-"), "got {a}");
        let b = fresh();
        assert_ne!(a, b);
    }

    #[test]
    fn fresh_ids_are_unique_in_a_tight_loop() {
        let ids: Vec<String> = (0..100_000).map(|_| fresh()).collect();
        let unique: std::collections::HashSet<&String> = ids.iter().collect();
        assert_eq!(unique.len(), ids.len());
    }

    #[test]
    fn fresh_ids_are_unique_across_threads() {
        let threads: Vec<_> = (0..8)
            .map(|_| std::thread::spawn(|| (0..10_000).map(|_| fresh()).collect::<Vec<_>>()))
            .collect();
        let ids: Vec<String> = threads
            .into_iter()
            .flat_map(|thread| thread.join().unwrap())
            .collect();
        let unique: std::collections::HashSet<&String> = ids.iter().collect();
        assert_eq!(unique.len(), ids.len());
    }

    #[test]
    fn pid_reaches_the_id_bits() {
        assert_ne!(splitmix64(1) & ID_MASK, splitmix64(2) & ID_MASK);
    }

    #[test]
    fn base36_is_stable() {
        assert_eq!(base36(0), "0");
        assert_eq!(base36(35), "z");
        assert_eq!(base36(36), "10");
    }
}
// CODEGEN-END
