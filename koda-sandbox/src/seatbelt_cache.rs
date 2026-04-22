//! Compiled-SBPL cache (Phase 4a of #934).
//!
//! ## Why this exists
//!
//! Every call to [`crate::seatbelt::build_command`] /
//! [`crate::seatbelt::build_command_with_proxy`] re-runs the *exact same*
//! sequence of canonicalize syscalls, env reads, format!s, path validations,
//! and string concatenations to produce a profile that depends only on
//! `(canonical_root, home, proxy_settings, policy)`. Inside a slot's
//! lifetime — and across slots that share a policy — these inputs are
//! stable, so the work is pure waste.
//!
//! The Phase 4 acceptance gate is `acquire_slot() < 15 ms p95 warm`, and
//! a quick instrumentation pass shows the cold-path profile build is
//! ~3-6 ms on a warm cache (fs canonicalize dominates), well over a
//! third of the budget. After this cache the warm path is a single
//! `HashMap::get` clone — sub-microsecond on every benchmark machine.
//!
//! ## Key shape
//!
//! [`ProfileKey`] owns every input that affects the emitted profile
//! string. We use the *full* key (not its hash) as the `HashMap` key so
//! collisions are physically impossible — a hash-only key would be
//! a security defect (two policies hashing to the same bucket would
//! share a profile, silently widening one of them).
//!
//! All key fields are owned (`String` / `PathBuf` / `SandboxPolicy`)
//! rather than borrowed: the cache outlives any single `build_command`
//! call, and `Arc<Mutex<…>>` of borrowed-key entries would force the
//! caller to hold the lock for the lifetime of every read.
//!
//! ## Concurrency
//!
//! `Mutex<HashMap>` rather than `RwLock`. Writes are rare (one per new
//! `(root, policy)` pair, which in practice is bounded by the number
//! of slots × distinct trust modes — usually <20 entries for the
//! lifetime of a koda process). Read contention is the only concern,
//! and `Mutex` outperforms `RwLock` for sub-microsecond critical
//! sections on every libstd platform we ship.
//!
//! No eviction. The set of distinct keys is bounded by the application
//! shape (you only have so many trust modes × project roots × proxy
//! configs in a single koda process). If we ever ship multi-tenant or
//! long-running daemon mode, adding an LRU is a separate concern.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use crate::policy::SandboxPolicy;

/// Cache key: every input that materially affects the emitted profile.
///
/// Field order matters for `Hash` derive determinism across builds (the
/// derive hashes fields in declaration order). Reorder only if you also
/// bump the cache via process restart — there's no on-disk state to
/// invalidate, so reordering is safe in practice, but stable order keeps
/// micro-benchmarks reproducible.
///
/// Why not `(String, String, Option<(u16, bool, bool)>, SandboxPolicy)`
/// as a tuple? Named fields give documentable intent at the use site
/// and let us add fields without rewriting every constructor. The cost
/// is two extra struct types — worth it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ProfileKey {
    /// Canonicalized project root (already passed through
    /// `canonicalize` and `validate_seatbelt_path` by the caller).
    /// Distinct projects produce different profiles; the same project
    /// under a different symlink path produces the *same* profile
    /// because canonicalize resolves it.
    pub canonical_root: PathBuf,

    /// `$HOME` snapshot. Read once per `build_command` call today, but
    /// the value is process-stable in every realistic koda flow (we
    /// don't `setenv` HOME mid-session). Including it in the key keeps
    /// the cache correct in the pathological case where someone *does*
    /// mutate HOME.
    pub home: String,

    /// Proxy parameters, when the proxied profile is in use. `None`
    /// selects the open-network profile. Tuple order matches
    /// [`crate::seatbelt::build_proxied_profile_string`]: port, then
    /// `allow_local_binding`, then `weaker_macos_isolation`. Any change
    /// to that signature must be reflected here or stale entries leak.
    pub proxy: Option<(u16, bool, bool)>,

    /// The full policy. `Hash` is derived so adding a policy field
    /// automatically invalidates relevant cache entries on the next
    /// process start (still no rebuild within a single process — but
    /// since policy values are immutable inside a slot, that's fine).
    pub policy: SandboxPolicy,
}

/// Process-wide cache. `OnceLock` defers allocation to first use so
/// koda processes that never spawn a sandboxed command pay nothing.
fn cache() -> &'static Mutex<HashMap<ProfileKey, String>> {
    static CACHE: OnceLock<Mutex<HashMap<ProfileKey, String>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Look up a previously compiled profile, or compute + insert via `build`.
///
/// `build` is invoked at most once per distinct `key` in the lifetime
/// of the process. The closure runs **inside** the mutex, which means
/// concurrent `get_or_compute` calls for the same key serialize on the
/// build (preventing duplicate work) but calls for *different* keys
/// also serialize — acceptable because building is short (~ms) and we
/// optimize for the *steady-state* warm path where build never runs.
///
/// Returns an owned `String` rather than a `&str` borrowed from the
/// map: `sandbox-exec -p` copies the arg into its own buffer anyway,
/// and avoiding the borrow lets us release the mutex before the caller
/// hands the profile off to the `Command` builder.
pub fn get_or_compute<F>(key: ProfileKey, build: F) -> String
where
    F: FnOnce(&ProfileKey) -> String,
{
    let mut map = cache()
        .lock()
        .expect("seatbelt profile cache mutex poisoned");
    if let Some(profile) = map.get(&key) {
        return profile.clone();
    }
    let profile = build(&key);
    map.insert(key, profile.clone());
    profile
}

/// Drop every cached entry. Test-only — production callers never need
/// this because key invalidation is handled implicitly (a different
/// policy hashes to a different key). Useful in test setUp/tearDown
/// to keep one test's pollution from biasing another's miss/hit ratio.
#[cfg(test)]
pub fn clear() {
    cache()
        .lock()
        .expect("seatbelt profile cache mutex poisoned")
        .clear();
}

/// Test-only metrics so the unit tests can prove cache behavior
/// (rather than just inferring it from emitted strings).
#[cfg(test)]
pub fn len() -> usize {
    cache()
        .lock()
        .expect("seatbelt profile cache mutex poisoned")
        .len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn key_with_root(root: &str) -> ProfileKey {
        ProfileKey {
            canonical_root: PathBuf::from(root),
            home: "/Users/test".into(),
            proxy: None,
            policy: SandboxPolicy::default(),
        }
    }

    /// Tests share a single static cache, so they MUST clear it on
    /// entry — otherwise test ordering changes the hit/miss counts.
    /// Cargo runs tests in parallel by default; the mutex is the only
    /// synchronization point. We accept that two tests interleaving
    /// could race their inserts, so each test uses a unique root path
    /// to keep its keys disjoint from siblings.
    #[test]
    fn build_runs_exactly_once_per_unique_key() {
        clear();
        let calls = AtomicUsize::new(0);
        let key = key_with_root("/test/once-per-key");

        let p1 = get_or_compute(key.clone(), |_| {
            calls.fetch_add(1, Ordering::SeqCst);
            "PROFILE_A".to_string()
        });
        let p2 = get_or_compute(key.clone(), |_| {
            calls.fetch_add(1, Ordering::SeqCst);
            "PROFILE_B".to_string()
        });

        assert_eq!(p1, "PROFILE_A");
        assert_eq!(
            p2, "PROFILE_A",
            "second call must return cached value, not re-run build"
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "build closure must run exactly once for the same key"
        );
    }

    #[test]
    fn distinct_keys_get_distinct_profiles() {
        // Different canonical_root → different cache entry → different
        // build closure runs. Catches the security regression where a
        // single hash bucket could let two policies share a profile.
        let key_a = key_with_root("/test/distinct-a");
        let key_b = key_with_root("/test/distinct-b");

        let pa = get_or_compute(key_a.clone(), |k| {
            format!("ROOT={}", k.canonical_root.display())
        });
        let pb = get_or_compute(key_b.clone(), |k| {
            format!("ROOT={}", k.canonical_root.display())
        });

        assert_eq!(pa, "ROOT=/test/distinct-a");
        assert_eq!(pb, "ROOT=/test/distinct-b");
        assert_ne!(pa, pb);
    }

    #[test]
    fn proxy_change_invalidates_cache() {
        // Same root + policy, different proxy params → different key.
        // The whole point: a proxied profile MUST NOT be served when
        // the caller asked for the open-network one (that would defeat
        // 3c kernel enforcement on the next sandbox spawn).
        let mut k_open = key_with_root("/test/proxy-invalidates");
        k_open.proxy = None;

        let mut k_proxied = key_with_root("/test/proxy-invalidates");
        k_proxied.proxy = Some((8080, false, false));

        let p_open = get_or_compute(k_open, |_| "OPEN".to_string());
        let p_proxied = get_or_compute(k_proxied, |_| "PROXIED".to_string());

        assert_eq!(p_open, "OPEN");
        assert_eq!(
            p_proxied, "PROXIED",
            "proxy=Some must NOT return the proxy=None cached value"
        );
    }

    #[test]
    fn policy_change_invalidates_cache() {
        // Adding a deny path → different policy → different key.
        // Same security argument as proxy_change_invalidates_cache:
        // mutating policy fields without busting the cache would let
        // a tightened policy run with the looser cached profile.
        use crate::policy::PathPattern;

        let mut k_loose = key_with_root("/test/policy-invalidates");
        k_loose.policy = SandboxPolicy::default();

        let mut k_tight = key_with_root("/test/policy-invalidates");
        k_tight.policy = SandboxPolicy {
            fs: crate::policy::FsPolicy {
                deny_read: vec![PathPattern("/etc/secret".into())],
                ..Default::default()
            },
            ..Default::default()
        };

        let p_loose = get_or_compute(k_loose, |_| "LOOSE".to_string());
        let p_tight = get_or_compute(k_tight, |_| "TIGHT".to_string());

        assert_ne!(
            p_loose, p_tight,
            "policy delta must produce a fresh profile"
        );
    }

    #[test]
    fn weaker_isolation_flag_change_invalidates_cache() {
        // Phase 3e regression: the trustd toggle is the third element
        // of the proxy tuple. A buggy refactor that hashed only the
        // first two would silently serve a non-trustd profile to a
        // session that explicitly opted into weaker isolation, and
        // the user's gcloud calls would mysteriously fail TLS.
        let mut k_strict = key_with_root("/test/weaker-iso");
        k_strict.proxy = Some((8080, false, false));

        let mut k_weak = key_with_root("/test/weaker-iso");
        k_weak.proxy = Some((8080, false, true));

        let p_strict = get_or_compute(k_strict, |_| "STRICT".to_string());
        let p_weak = get_or_compute(k_weak, |_| "WEAK".to_string());

        assert_ne!(
            p_strict, p_weak,
            "weaker_macos_isolation toggle must produce distinct cache entries"
        );
    }

    #[test]
    fn clear_resets_the_cache() {
        // Belt-and-suspenders for the test helper itself: if clear()
        // ever stops working, every other cache test in the suite
        // becomes order-dependent and flaky.
        let key = key_with_root("/test/clear-resets");

        // Seed an entry.
        let calls = AtomicUsize::new(0);
        get_or_compute(key.clone(), |_| {
            calls.fetch_add(1, Ordering::SeqCst);
            "FIRST".to_string()
        });
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        // Pre-clear: cached, no rebuild.
        get_or_compute(key.clone(), |_| {
            calls.fetch_add(1, Ordering::SeqCst);
            "SECOND".to_string()
        });
        assert_eq!(calls.load(Ordering::SeqCst), 1, "should still be cached");

        clear();

        // Post-clear: rebuild.
        get_or_compute(key, |_| {
            calls.fetch_add(1, Ordering::SeqCst);
            "THIRD".to_string()
        });
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "clear() must force the next get to rebuild"
        );
    }
}
