//! Content-free timing for the offline connection latency probe.

#[inline]
pub fn phase(name: &'static str) -> tracing::Span {
    tracing::info_span!(target: "plak::test_wire", "connection work", phase = name)
}

pub fn pong(name: &'static str, id: u64) {
    let _timing =
        tracing::info_span!(target: "plak::test_wire", "pong timing", phase = name, id).entered();
}

pub fn writer(name: &'static str, line: &[u8]) {
    let Some(rest) = line.strip_prefix(br#"{"frame":"response","id":"#) else {
        return;
    };
    let Some(comma) = rest.iter().take(21).position(|byte| *byte == b',') else {
        return;
    };
    let (id, outcome) = rest.split_at(comma);
    if outcome.starts_with(br#","outcome":{"ok":{"answer":"pong""#)
        && let Some(id) = std::str::from_utf8(id).ok().and_then(|id| id.parse().ok())
    {
        pong(name, id);
    }
}
