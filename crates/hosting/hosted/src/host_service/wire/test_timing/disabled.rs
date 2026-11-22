//! A production build does not record connection probe timings.

#[inline]
pub fn phase(_name: &'static str) -> tracing::Span {
    tracing::Span::none()
}

#[inline]
pub fn pong(_name: &'static str, _id: u64) {}

#[inline]
pub fn writer(_name: &'static str, _line: &[u8]) {}
