//! JWKS fetch/cache/single-flight orchestrator for subscription OIDC.
//!
//! [`JwksCache`] holds the async mutex across the fetch (single-flight) and
//! bounds both the mutex wait and each fetch by the caller's deadline; dialects
//! supply fetch + crypto validation. Shared ladder: purge negatives → fresh hit
//! → negative miss → fetch rounds → install → hit or record negative.

use std::collections::BTreeMap;
use std::future::Future;
use std::time::{Duration, Instant};

/// In-process JWKS snapshot + negative cache.
#[derive(Clone, Debug)]
pub(crate) struct JwksCacheState<K> {
    /// Validated signing keys keyed by `kid`.
    pub keys: BTreeMap<String, K>,
    /// Positive snapshot expiry (`None` = empty).
    pub expires_at: Option<Instant>,
    /// Negative cache: `kid` → expiry.
    pub negative: BTreeMap<String, Instant>,
    /// Cap on negative entries (oldest expiry evicted first).
    pub max_negative: usize,
}

impl<K> Default for JwksCacheState<K> {
    fn default() -> Self {
        Self {
            keys: BTreeMap::new(),
            expires_at: None,
            negative: BTreeMap::new(),
            max_negative: 64,
        }
    }
}

impl<K> JwksCacheState<K> {
    /// Empty cache with a negative-entry cap.
    #[must_use]
    pub fn with_max_negative(max_negative: usize) -> Self {
        Self {
            max_negative,
            ..Self::default()
        }
    }

    /// Non-empty snapshot still within positive TTL.
    #[must_use]
    pub fn is_fresh(&self, now: Instant) -> bool {
        self.expires_at.is_some_and(|expires_at| expires_at > now) && !self.keys.is_empty()
    }

    /// Drop expired negatives.
    pub fn purge_negatives(&mut self, now: Instant) {
        self.negative.retain(|_, expires_at| *expires_at > now);
    }

    /// Install a newly fetched positive snapshot.
    pub fn install_snapshot(
        &mut self,
        keys: BTreeMap<String, K>,
        expires_at: Instant,
        clear_negatives: bool,
    ) {
        self.keys = keys;
        self.expires_at = Some(expires_at);
        if clear_negatives {
            self.negative.clear();
        }
    }

    /// Record a kid miss.
    pub fn record_negative(&mut self, kid: &str, expires_at: Instant) {
        if self.max_negative == 0 {
            return;
        }
        if self.negative.len() >= self.max_negative
            && !self.negative.contains_key(kid)
            && let Some(oldest) = self
                .negative
                .iter()
                .min_by_key(|(_, expiry)| **expiry)
                .map(|(key, _)| key.clone())
        {
            self.negative.remove(&oldest);
        }
        self.negative.insert(kid.to_owned(), expires_at);
    }

    /// `kid` is negatively cached and not expired.
    #[must_use]
    pub fn is_negative(&self, kid: &str, now: Instant) -> bool {
        self.negative
            .get(kid)
            .is_some_and(|expires_at| *expires_at > now)
    }
}

/// Validated JWKS snapshot from a dialect fetch.
#[derive(Clone, Debug)]
pub struct FetchedJwks<K> {
    /// Accepted keys keyed by `kid`.
    pub keys: BTreeMap<String, K>,
    /// Absolute positive-cache expiry.
    pub expires_at: Instant,
}

/// Dialect policy for the shared JWKS resolve ladder.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct JwksResolvePolicy {
    /// Fetch rounds when no fresh snapshot exists.
    pub rounds_when_stale: u32,
    /// Fetch rounds when a fresh snapshot lacks the kid.
    pub rounds_when_fresh_miss: u32,
    /// Clear negatives on every successful fetch (xAI generation semantics).
    pub clear_negatives_on_fetch: bool,
    /// When true, negatives apply only while a fresh snapshot exists.
    pub negative_requires_fresh_snapshot: bool,
    /// Negative-cache TTL after an unresolved kid.
    pub negative_ttl: Duration,
}

impl JwksResolvePolicy {
    /// Codex-style: time-based negatives that survive fetches; 2 stale rounds.
    #[must_use]
    pub const fn codex() -> Self {
        Self {
            rounds_when_stale: 2,
            rounds_when_fresh_miss: 1,
            clear_negatives_on_fetch: false,
            negative_requires_fresh_snapshot: false,
            negative_ttl: Duration::from_secs(30),
        }
    }

    /// SuperGrok-style: clear negatives on fetch; single fetch round.
    #[must_use]
    pub const fn xai() -> Self {
        Self {
            rounds_when_stale: 1,
            rounds_when_fresh_miss: 1,
            clear_negatives_on_fetch: true,
            negative_requires_fresh_snapshot: true,
            // Matches positive TTL so a generation miss dies with the snapshot.
            negative_ttl: Duration::from_secs(600),
        }
    }
}

/// Resolve one signing key by `kid` against a locked cache state.
///
/// `fetch` runs while [`JwksCache`] still holds the cache mutex (single-flight).
///
/// # Errors
///
/// Propagates `fetch` errors and maps miss/negative through the provided closures.
pub(crate) async fn resolve_signing_key<K, E, F, Fut>(
    state: &mut JwksCacheState<K>,
    kid: &str,
    policy: &JwksResolvePolicy,
    negative_error: impl FnOnce() -> E,
    miss_error: impl FnOnce() -> E,
    mut fetch: F,
) -> Result<K, E>
where
    K: Clone,
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<FetchedJwks<K>, E>>,
{
    let now = Instant::now();
    state.purge_negatives(now);
    let fresh = state.is_fresh(now);
    if fresh && let Some(key) = state.keys.get(kid).cloned() {
        return Ok(key);
    }
    let honor_negative =
        state.is_negative(kid, now) && (fresh || !policy.negative_requires_fresh_snapshot);
    if honor_negative {
        return Err(negative_error());
    }

    let rounds = if fresh {
        policy.rounds_when_fresh_miss
    } else {
        policy.rounds_when_stale
    }
    .max(1);
    for _ in 0..rounds {
        let fetched = fetch().await?;
        state.install_snapshot(
            fetched.keys,
            fetched.expires_at,
            policy.clear_negatives_on_fetch,
        );
        if let Some(key) = state.keys.get(kid).cloned() {
            state.negative.remove(kid);
            return Ok(key);
        }
    }

    state.record_negative(kid, Instant::now() + policy.negative_ttl);
    Err(miss_error())
}

/// Single-flight JWKS cache shared by every subscription OIDC dialect.
///
/// The cancellation-safe async mutex is held across the fetch, so concurrent
/// resolvers of a missing key wait for one fetch instead of issuing their own.
/// Cancellation drops the guard, so no stuck in-flight state can strand later
/// callers.
#[derive(Debug)]
pub struct JwksCache<K> {
    state: tokio::sync::Mutex<JwksCacheState<K>>,
    policy: JwksResolvePolicy,
}

impl<K: Clone> JwksCache<K> {
    /// Empty cache that keeps at most `max_negative` unresolved key ids.
    #[must_use]
    pub fn new(policy: JwksResolvePolicy, max_negative: usize) -> Self {
        Self {
            state: tokio::sync::Mutex::new(JwksCacheState::with_max_negative(max_negative)),
            policy,
        }
    }

    /// Resolves `kid`, fetching under the single flight when needed.
    ///
    /// With a `deadline`, the wait for an in-flight fetch and every fetch
    /// round start only while time remains; the wait itself is bounded by the
    /// remaining time. `expired` names the failure once the deadline passed.
    /// A negative-cached or still-unresolved `kid` fails with `unresolved`.
    ///
    /// # Errors
    ///
    /// `expired`, `unresolved`, or the error `fetch` returns.
    pub async fn resolve<E, F, Fut>(
        &self,
        kid: &str,
        deadline: Option<Instant>,
        expired: impl Fn() -> E,
        unresolved: impl Fn() -> E,
        mut fetch: F,
    ) -> Result<K, E>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = Result<FetchedJwks<K>, E>>,
    {
        let remaining = || match deadline {
            None => Ok(None),
            Some(deadline) => crate::oauth_workflow::remaining_until(deadline, Instant::now())
                .map(Some)
                .ok_or_else(&expired),
        };
        let mut state = match remaining()? {
            None => self.state.lock().await,
            Some(wait) => tokio::time::timeout(wait, self.state.lock())
                .await
                .map_err(|_| expired())?,
        };
        remaining()?;
        resolve_signing_key(
            &mut state,
            kid,
            &self.policy,
            &unresolved,
            &unresolved,
            || {
                let started = remaining();
                let fetched = fetch();
                async move {
                    started?;
                    fetched.await
                }
            },
        )
        .await
    }
}

/// Parse `Cache-Control: max-age=` (first matching directive).
#[must_use]
pub fn parse_cache_control_max_age(value: &str) -> Option<Duration> {
    value.split(',').find_map(|directive| {
        let (name, value) = directive.trim().split_once('=')?;
        name.eq_ignore_ascii_case("max-age")
            .then(|| value.trim_matches('"').parse::<u64>().ok())
            .flatten()
            .map(Duration::from_secs)
    })
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "unit tests may assert with expect/unwrap"
)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Clone, Debug, PartialEq, Eq)]
    struct TestKey(u8);

    #[tokio::test]
    async fn fresh_hit_avoids_fetch() {
        let mut state = JwksCacheState::default();
        state.install_snapshot(
            BTreeMap::from([("k1".into(), TestKey(1))]),
            Instant::now() + Duration::from_secs(60),
            true,
        );
        let fetches = AtomicUsize::new(0);
        let key = resolve_signing_key(
            &mut state,
            "k1",
            &JwksResolvePolicy::codex(),
            || "negative",
            || "miss",
            || async {
                fetches.fetch_add(1, Ordering::SeqCst);
                Err("fetch")
            },
        )
        .await
        .unwrap();
        assert_eq!(key, TestKey(1));
        assert_eq!(fetches.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn miss_records_negative_and_second_call_short_circuits() {
        let mut state = JwksCacheState::default();
        let fetches = AtomicUsize::new(0);
        let policy = JwksResolvePolicy::xai();
        assert_eq!(
            resolve_signing_key(
                &mut state,
                "missing",
                &policy,
                || "negative",
                || "miss",
                || async {
                    fetches.fetch_add(1, Ordering::SeqCst);
                    Ok(FetchedJwks {
                        keys: BTreeMap::from([("other".into(), TestKey(9))]),
                        expires_at: Instant::now() + Duration::from_secs(60),
                    })
                },
            )
            .await,
            Err("miss")
        );
        assert!(state.is_negative("missing", Instant::now()));
        assert_eq!(
            resolve_signing_key(
                &mut state,
                "missing",
                &policy,
                || "negative",
                || "miss",
                || async {
                    fetches.fetch_add(1, Ordering::SeqCst);
                    Err("fetch")
                },
            )
            .await,
            Err("negative")
        );
        assert_eq!(fetches.load(Ordering::SeqCst), 1);
    }

    fn snapshot(kid: &str, key: u8) -> FetchedJwks<TestKey> {
        FetchedJwks {
            keys: BTreeMap::from([(kid.into(), TestKey(key))]),
            expires_at: Instant::now() + Duration::from_secs(60),
        }
    }

    #[tokio::test]
    async fn deadline_bounds_the_wait_behind_an_in_flight_fetch() {
        let cache = std::sync::Arc::new(JwksCache::new(JwksResolvePolicy::xai(), 8));
        let (fetch_started, started) = tokio::sync::oneshot::channel();
        let (release_fetch, release) = tokio::sync::oneshot::channel::<()>();
        let holder = tokio::spawn({
            let cache = std::sync::Arc::clone(&cache);
            async move {
                let mut signal = Some((fetch_started, release));
                cache
                    .resolve(
                        "k1",
                        None,
                        || "expired",
                        || "unresolved",
                        || {
                            let (fetch_started, release) = signal.take().unwrap();
                            async move {
                                fetch_started.send(()).unwrap();
                                release.await.unwrap();
                                Ok(snapshot("k1", 1))
                            }
                        },
                    )
                    .await
            }
        });
        started.await.unwrap();
        let begun = Instant::now();
        let waited = cache
            .resolve(
                "k1",
                Some(Instant::now() + Duration::from_millis(50)),
                || "expired",
                || "unresolved",
                || async { Err("fetch") },
            )
            .await;
        assert_eq!(waited, Err("expired"));
        assert!(begun.elapsed() < Duration::from_secs(5));
        release_fetch.send(()).unwrap();
        assert_eq!(holder.await.unwrap(), Ok(TestKey(1)));
    }

    #[tokio::test]
    async fn expired_deadline_refuses_before_fetching() {
        let cache = JwksCache::new(JwksResolvePolicy::codex(), 8);
        let fetches = AtomicUsize::new(0);
        let result = cache
            .resolve(
                "k1",
                Some(Instant::now()),
                || "expired",
                || "unresolved",
                || async {
                    fetches.fetch_add(1, Ordering::SeqCst);
                    Ok(snapshot("k1", 1))
                },
            )
            .await;
        assert_eq!(result, Err("expired"));
        assert_eq!(fetches.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn concurrent_misses_share_one_fetch() {
        let cache = JwksCache::new(JwksResolvePolicy::codex(), 8);
        let fetches = AtomicUsize::new(0);
        let resolve = || {
            cache.resolve(
                "k1",
                Some(Instant::now() + Duration::from_secs(30)),
                || "expired",
                || "unresolved",
                || async {
                    fetches.fetch_add(1, Ordering::SeqCst);
                    tokio::task::yield_now().await;
                    Ok(snapshot("k1", 1))
                },
            )
        };
        let (first, second) = tokio::join!(resolve(), resolve());
        assert_eq!(first, Ok(TestKey(1)));
        assert_eq!(second, Ok(TestKey(1)));
        assert_eq!(fetches.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn parse_max_age_reads_first_directive() {
        assert_eq!(
            parse_cache_control_max_age("public, max-age=120, must-revalidate"),
            Some(Duration::from_secs(120))
        );
        assert_eq!(parse_cache_control_max_age("no-store"), None);
    }
}
