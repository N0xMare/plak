//! Neutral export descriptions; only the optional bridge reads their fields.
use plakous::model::{AttemptId, CallId, TurnId};
use std::time::Duration;

pub(crate) enum AttributeValue {
    Integer(i64),
    Text(String),
    StaticText(&'static str),
}

pub(crate) struct Attribute {
    pub key: &'static str,
    pub value: AttributeValue,
}

impl Attribute {
    pub(super) fn new(key: &'static str, value: AttributeValue) -> Self {
        Self { key, value }
    }
}

pub(crate) enum SpanIdentity {
    Turn(TurnId),
    Attempt(AttemptId),
    Call(CallId),
    Synthetic(SyntheticKind),
}

#[derive(Clone, Copy)]
pub(crate) enum ParentSpec {
    Root,
    Turn,
    TurnOrRoot,
}

#[derive(Clone, Copy)]
pub(crate) enum SpanKind {
    Internal,
    Client,
}

pub(crate) enum Status {
    Ok,
    Error(&'static str),
}

impl Status {
    pub(super) fn error(reason: &'static str) -> Self {
        Self::Error(reason)
    }
}

pub(crate) struct SpanSpec {
    pub name: &'static str,
    pub span_id: SpanIdentity,
    pub parent: ParentSpec,
    pub kind: SpanKind,
    pub status: Status,
}

pub(crate) enum SpanTiming {
    Elapsed(Duration),
    Instant,
}

pub(crate) enum Span {
    Observed {
        spec: SpanSpec,
        timing: SpanTiming,
        attributes: Vec<Attribute>,
        take_ttft: bool,
    },
    Credential {
        provider: String,
        generation: u64,
        elapsed: Option<Duration>,
        status: Status,
        attributes: Vec<Attribute>,
    },
}

pub(super) fn push_str(attributes: &mut Vec<Attribute>, key: &'static str, value: &'static str) {
    attributes.push(Attribute {
        key,
        value: AttributeValue::StaticText(value),
    });
}

pub(super) fn push_string(attributes: &mut Vec<Attribute>, key: &'static str, value: String) {
    attributes.push(Attribute::new(key, AttributeValue::Text(value)));
}

pub(super) fn push_u64(attributes: &mut Vec<Attribute>, key: &'static str, value: u64) {
    attributes.push(Attribute::new(
        key,
        AttributeValue::Integer(i64::try_from(value).unwrap_or(i64::MAX)),
    ));
}

pub(super) fn push_u128(attributes: &mut Vec<Attribute>, key: &'static str, value: u128) {
    attributes.push(Attribute::new(key, AttributeValue::Text(value.to_string())));
}

pub(super) fn duration_ns(value: Duration) -> u128 {
    value.as_nanos()
}

#[derive(Clone, Copy)]
pub(crate) enum SyntheticKind {
    Compaction,
    Durability,
    DurabilityAttempt,
    Retention,
    ChildProcessExited,
}

impl SyntheticKind {
    pub(crate) const fn tag(self) -> &'static str {
        match self {
            Self::Compaction => "compaction",
            Self::Durability => "durability",
            Self::DurabilityAttempt => "durability_attempt",
            Self::Retention => "retention",
            Self::ChildProcessExited => "child_process_exited",
        }
    }
}
