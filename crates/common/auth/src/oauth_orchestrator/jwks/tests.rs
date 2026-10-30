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
