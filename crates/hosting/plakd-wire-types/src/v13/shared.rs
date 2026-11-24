//! Reuse only the unchanged cases of tagged v12 DTOs during the draft.

use plakous_host_protocol::wire;
use schemars::{JsonSchema, Schema, SchemaGenerator};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{PlakExtension, console};

// Use the same set of tags in the decoder and schema. No unchecked constructor
// is exposed: even a typed v12 value must pass the tag check to enter v13.
macro_rules! shared_cases {
    ($name:ident, $inner:ty, $tag:literal, [$($replaced:literal),+ $(,)?]) => {
        #[derive(Clone, Debug, Eq, PartialEq, Serialize)]
        #[serde(transparent)]
        pub struct $name($inner);

        impl $name {
            #[must_use]
            pub const fn as_inner(&self) -> &$inner {
                &self.0
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: serde::Deserializer<'de>>(input: D) -> Result<Self, D::Error> {
                let value = Value::deserialize(input)?;
                if matches!(value.get($tag).and_then(Value::as_str), $(Some($replaced))|+) {
                    return Err(serde::de::Error::custom("not an unchanged protocol-13 case"));
                }
                serde_json::from_value(value).map(Self).map_err(serde::de::Error::custom)
            }
        }

        impl JsonSchema for $name {
            fn schema_name() -> std::borrow::Cow<'static, str> {
                stringify!($name).into()
            }

            fn json_schema(generator: &mut SchemaGenerator) -> Schema {
                let mut schema = <$inner>::json_schema(generator);
                if let Some(variants) = schema
                    .get_mut("oneOf")
                    .and_then(Value::as_array_mut)
                {
                    variants.retain(|variant| {
                        let tag = variant.get("properties")
                            .and_then(|properties| properties.get($tag))
                            .and_then(|tag| tag.get("const"))
                            .and_then(Value::as_str);
                        !matches!(tag, $(Some($replaced))|+)
                    });
                }
                schema
            }
        }
    };
}

shared_cases!(
    SharedClientFrame,
    wire::ClientFrame<PlakExtension>,
    "frame",
    ["request"]
);
shared_cases!(
    SharedHostFrame,
    wire::HostFrame<PlakExtension>,
    "frame",
    [
        "response",
        "event",
        "snapshot",
        "directory",
        "options_progress",
        "host_lifecycle"
    ]
);
shared_cases!(SharedOp, wire::Op<PlakExtension>, "op", ["attach"]);
shared_cases!(
    SharedAnswer,
    wire::Answer<PlakExtension>,
    "answer",
    ["attached", "sessions", "options", "host"]
);
shared_cases!(
    SharedRefusal,
    wire::RequestRefusal,
    "refusal",
    ["placement_refused"]
);
shared_cases!(
    SharedPlacementReason,
    wire::PlacementReason,
    "reason",
    ["runner_unreachable"]
);
shared_cases!(
    SharedHostResponse,
    console::HostResponse,
    "response",
    ["accounts_usage", "refused"]
);
shared_cases!(
    SharedObservation,
    wire::FramedObservation<PlakExtension>,
    "kind",
    [
        "assistant_delta",
        "reasoning_delta",
        "transcript_stream",
        "presentation_snapshot",
        "usage_limits"
    ]
);
