//! Reuse only the unchanged cases of tagged v12 DTOs during the draft.

use plakous_host_protocol::wire;
use plakous_host_protocol::{SessionObservation, views::transcript::TranscriptPatch};
use schemars::{JsonSchema, Schema, SchemaGenerator};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;

use crate::{PlakExtension, console};

// Use the same set of tags in the decoder and schema. No unchecked constructor
// is exposed: even a typed v12 value must pass the tag check to enter v13.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct Shared<T>(T);

impl<T> Shared<T> {
    #[must_use]
    pub const fn as_inner(&self) -> &T {
        &self.0
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
        if T::replaced(value.get(T::TAG).and_then(Value::as_str)) {
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
                let tag = variant
                    .get("properties")
                    .and_then(|properties| properties.get(T::TAG))
                    .and_then(|tag| tag.get("const"))
                    .and_then(Value::as_str);
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

pub type SharedOp = Shared<wire::Op<PlakExtension>>;
impl Cases for wire::Op<PlakExtension> {
    const NAME: &'static str = "SharedOp";
    const TAG: &'static str = "op";
    const REPLACED: &'static [&'static str] = &["attach"];
}

pub type SharedAnswer = Shared<wire::Answer<PlakExtension>>;
impl Cases for wire::Answer<PlakExtension> {
    const NAME: &'static str = "SharedAnswer";
    const TAG: &'static str = "answer";
    const REPLACED: &'static [&'static str] = &["attached", "sessions", "options", "host"];
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
        if matches!(refusal, wire::RequestRefusal::PlacementRefused { .. }) {
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

pub type SharedHostResponse = Shared<console::HostResponse>;
impl Cases for console::HostResponse {
    const NAME: &'static str = "SharedHostResponse";
    const TAG: &'static str = "response";
    const REPLACED: &'static [&'static str] = &["accounts_usage", "refused"];
}

pub type SharedObservation = Shared<wire::FramedObservation<PlakExtension>>;

impl From<TranscriptPatch> for super::Observation {
    fn from(patch: TranscriptPatch) -> Self {
        Self::Shared(Box::new(Shared(wire::FramedObservation(
            SessionObservation::TranscriptPatch(patch),
        ))))
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
    ];
}
