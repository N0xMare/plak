//! Content-free, host-wide counts of transcript gap-repair asks.

use std::sync::atomic::{AtomicU64, Ordering};

static ROOT: AtomicU64 = AtomicU64::new(0);
static AGENT: AtomicU64 = AtomicU64::new(0);

pub(crate) fn record(scope: &plakous_host_protocol::views::transcript::TranscriptScope) {
    use plakous_host_protocol::views::transcript::TranscriptScope;
    let counter = match scope {
        TranscriptScope::Root => &ROOT,
        TranscriptScope::Agent(_) => &AGENT,
    };
    counter.fetch_add(1, Ordering::Relaxed);
}

pub(crate) fn snapshot() -> [u64; 2] {
    [ROOT.load(Ordering::Relaxed), AGENT.load(Ordering::Relaxed)]
}
