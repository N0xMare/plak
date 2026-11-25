//! Reuse the unchanged cases of the common dispatcher DTOs.

use plakous_host_protocol::wire;
use plakous_host_protocol::{SessionObservation, views::transcript::TranscriptPatch};
use schemars::{JsonSchema, Schema, SchemaGenerator};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;

use crate::{PlakExtension, SurfaceObservation, console};

// Use the same set of tags in the decoder and schema. No unchecked constructor
// is exposed: each constructor admits only the cases shared by protocol 13.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct Shared<T>(T);

impl<T> Shared<T> {
    #[must_use]
    pub const fn as_inner(&self) -> &T {
        &self.0
    }

    #[must_use]
    pub fn into_inner(self) -> T {
        self.0
    }
}

pub trait Cases {
    const NAME: &'static str;
    const TAG: &'static str;
    const REPLACED: &'static [&'static str];

    fn replaced(tag: Option<&str>) -> bool {
        tag.is_some_and(|tag| Self::REPLACED.contains(&tag))
    }
}

impl<'de, T: Cases + DeserializeOwned> Deserialize<'de> for Shared<T> {
    fn deserialize<D: serde::Deserializer<'de>>(input: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(input)?;
        let tag = if T::TAG.is_empty() {
            // For externally tagged variants, the object key is the tag.
            value
                .as_object()
                .and_then(|fields| fields.keys().next().map(String::as_str))
        } else {
            value.get(T::TAG).and_then(Value::as_str)
        };
        if T::replaced(tag) {
            return Err(serde::de::Error::custom(
                "not an unchanged protocol-13 case",
            ));
        }
        serde_json::from_value(value)
            .map(Self)
            .map_err(serde::de::Error::custom)
    }
}

impl<T: Cases + JsonSchema> JsonSchema for Shared<T> {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        T::NAME.into()
    }

    fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        let mut schema = T::json_schema(generator);
        if let Some(variants) = schema.get_mut("oneOf").and_then(Value::as_array_mut) {
            variants.retain(|variant| {
                let tag = if T::TAG.is_empty() {
                    variant
                        .get("required")
                        .and_then(|required| required.get(0))
                        .and_then(Value::as_str)
                } else {
                    variant
                        .get("properties")
                        .and_then(|properties| properties.get(T::TAG))
                        .and_then(|tag| tag.get("const"))
                        .and_then(Value::as_str)
                };
                !T::replaced(tag)
            });
        }
        schema
    }
}

pub type SharedClientFrame = Shared<wire::ClientFrame<PlakExtension>>;
impl Cases for wire::ClientFrame<PlakExtension> {
    const NAME: &'static str = "SharedClientFrame";
    const TAG: &'static str = "frame";
    const REPLACED: &'static [&'static str] = &["request"];
}

pub type SharedHostFrame = Shared<wire::HostFrame<PlakExtension>>;
impl Cases for wire::HostFrame<PlakExtension> {
    const NAME: &'static str = "SharedHostFrame";
    const TAG: &'static str = "frame";
    const REPLACED: &'static [&'static str] = &[
        "response",
        "event",
        "snapshot",
        "directory",
        "options_progress",
        "host_lifecycle",
    ];
}

impl super::HostFrame {
    /// Reuse the handshake, delivery and connection controls unchanged.
    pub fn unchanged(frame: wire::HostFrame<PlakExtension>) -> Option<Self> {
        use wire::HostFrame as F;
        match frame {
            F::Welcome(_)
            | F::Refused(_)
            | F::TerminalStream { .. }
            | F::Blob(_)
            | F::ResyncRequired { .. }
            | F::Moved { .. }
            | F::Detached { .. }
            | F::Bye => Some(Self::Shared(Box::new(Shared(frame)))),
            F::Response { .. }
            | F::Event { .. }
            | F::Snapshot(_)
            | F::Directory { .. }
            | F::OptionsProgress { .. }
            | F::HostLifecycle { .. } => None,
        }
    }
}

pub type SharedOp = Shared<wire::Op<PlakExtension>>;
impl Cases for wire::Op<PlakExtension> {
    const NAME: &'static str = "SharedOp";
    const TAG: &'static str = "op";
    const REPLACED: &'static [&'static str] = &["attach", "host"];
}

pub type SharedAnswer = Shared<wire::Answer<PlakExtension>>;
impl Cases for wire::Answer<PlakExtension> {
    const NAME: &'static str = "SharedAnswer";
    const TAG: &'static str = "answer";
    const REPLACED: &'static [&'static str] = &["attached", "sessions", "options", "host"];
}

impl super::Answer {
    pub fn unchanged(answer: wire::Answer<PlakExtension>) -> Option<Self> {
        use wire::Answer as A;
        match answer {
            A::Attached { .. } | A::Sessions { .. } | A::Options { .. } | A::Host { .. } => None,
            _ => Some(Self::Shared(Box::new(Shared(answer)))),
        }
    }
}

pub type SharedRefusal = Shared<wire::RequestRefusal>;
impl Cases for wire::RequestRefusal {
    const NAME: &'static str = "SharedRefusal";
    const TAG: &'static str = "refusal";
    const REPLACED: &'static [&'static str] = &["placement_refused"];
}

impl TryFrom<wire::RequestRefusal> for super::RequestRefusal {
    type Error = wire::RequestRefusal;

    fn try_from(refusal: wire::RequestRefusal) -> Result<Self, Self::Error> {
        if matches!(
            refusal,
            wire::RequestRefusal::PlacementRefused { .. }
                | wire::RequestRefusal::TranscriptScopeExpired
        ) {
            return Err(refusal);
        }
        Ok(Self::Shared(Shared(refusal)))
    }
}

pub type SharedPlacementReason = Shared<wire::PlacementReason>;
impl Cases for wire::PlacementReason {
    const NAME: &'static str = "SharedPlacementReason";
    const TAG: &'static str = "reason";
    const REPLACED: &'static [&'static str] = &["runner_unreachable"];
}

impl TryFrom<wire::PlacementReason> for super::PlacementReason {
    type Error = wire::PlacementReason;

    fn try_from(reason: wire::PlacementReason) -> Result<Self, Self::Error> {
        if matches!(reason, wire::PlacementReason::RunnerUnreachable { .. }) {
            return Err(reason);
        }
        Ok(Self::Shared(Shared(reason)))
    }
}

impl super::RequestRefusal {
    #[must_use]
    pub fn session_unavailable(detail: String) -> Self {
        Self::Shared(Shared(wire::RequestRefusal::SessionUnavailable { detail }))
    }
}

pub type SharedHostResponse = Shared<console::HostResponse>;
impl Cases for console::HostResponse {
    const NAME: &'static str = "SharedHostResponse";
    const TAG: &'static str = "response";
    const REPLACED: &'static [&'static str] = &[
        "accounts_usage",
        "refused",
        "profiles_list",
        "profiles_get",
        "templates_list",
        "templates_get",
    ];
}

impl super::HostResponse {
    pub fn unchanged(response: console::HostResponse) -> Option<Self> {
        use console::HostResponse as R;
        match response {
            R::AccountsUsage { .. }
            | R::Refused { .. }
            | R::ProfilesList { .. }
            | R::ProfilesGet { .. }
            | R::TemplatesList { .. }
            | R::TemplatesGet { .. } => None,
            _ => Some(Self::Shared(Box::new(Shared(response)))),
        }
    }
}

pub type SharedHostRequest = Shared<console::HostRequest>;
impl Cases for console::HostRequest {
    const NAME: &'static str = "SharedHostRequest";
    const TAG: &'static str = "request";
    const REPLACED: &'static [&'static str] = &["template_put"];
}

pub type SharedTemplateChange = Shared<console::TemplateChange>;
impl Cases for console::TemplateChange {
    const NAME: &'static str = "SharedTemplateChange";
    const TAG: &'static str = "field";
    const REPLACED: &'static [&'static str] = &["fast_mode"];
}

pub type SharedSurfaceObservation = Shared<SurfaceObservation>;
impl Cases for SurfaceObservation {
    const NAME: &'static str = "SharedSurfaceObservation";
    const TAG: &'static str = "";
    const REPLACED: &'static [&'static str] = &["profiles", "session_profile"];
}

impl super::SurfaceObservation {
    pub fn unchanged(observation: SurfaceObservation) -> Option<Self> {
        match observation {
            SurfaceObservation::Profiles(_) | SurfaceObservation::SessionProfile(_) => None,
            _ => Some(Self::Shared(Box::new(Shared(observation)))),
        }
    }
}

pub type SharedObservation = Shared<wire::FramedObservation<PlakExtension>>;

impl From<TranscriptPatch> for super::Observation {
    fn from(patch: TranscriptPatch) -> Self {
        Self::Shared(Box::new(Shared(wire::FramedObservation(
            SessionObservation::TranscriptPatch(patch),
        ))))
    }
}

impl super::Observation {
    pub fn unchanged(observation: SessionObservation<PlakExtension>) -> Option<Self> {
        use SessionObservation as O;
        match observation {
            O::AssistantDelta(_)
            | O::ReasoningDelta { .. }
            | O::TranscriptStream { .. }
            | O::PresentationSnapshot(_)
            | O::UsageLimits(_)
            | O::Extension(_)
            | O::TemplateReadiness(_) => None,
            _ => Some(Self::Shared(Box::new(Shared(wire::FramedObservation(
                observation,
            ))))),
        }
    }
}

impl Cases for wire::FramedObservation<PlakExtension> {
    const NAME: &'static str = "SharedObservation";
    const TAG: &'static str = "kind";
    const REPLACED: &'static [&'static str] = &[
        "assistant_delta",
        "reasoning_delta",
        "transcript_stream",
        "presentation_snapshot",
        "usage_limits",
        "extension",
        "template_readiness",
    ];
}
