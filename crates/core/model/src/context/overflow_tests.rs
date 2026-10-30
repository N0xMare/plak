use super::*;

// Frozen pre-conversion decoders check accepted values and exact serde errors.
#[derive(Debug, PartialEq)]
struct LegacyLimit(LimitOverflow);

impl<'de> Deserialize<'de> for LegacyLimit {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = LimitOverflowWire::deserialize(deserializer)?;
        if wire.required <= wire.available {
            return Err(serde::de::Error::custom(
                "context overflow must exceed available capacity",
            ));
        }
        Ok(Self(LimitOverflow {
            required: wire.required,
            available: wire.available,
        }))
    }
}

#[derive(Debug, PartialEq)]
struct LegacyContext(ContextOverflow);

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ContextOverflowWire {
    shared: Option<LegacyLimit>,
    input: Option<LegacyLimit>,
    output: Option<LegacyLimit>,
}

impl<'de> Deserialize<'de> for LegacyContext {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = ContextOverflowWire::deserialize(deserializer)?;
        if wire.shared.is_none() && wire.input.is_none() && wire.output.is_none() {
            return Err(serde::de::Error::custom(
                "context overflow must name at least one capacity axis",
            ));
        }
        Ok(Self(ContextOverflow {
            shared: wire.shared.map(|value| value.0),
            input: wire.input.map(|value| value.0),
            output: wire.output.map(|value| value.0),
        }))
    }
}

fn outcome<T>(result: Result<T, serde_json::Error>) -> Result<T, (String, usize, usize, String)> {
    result.map_err(|error| {
        (
            error.to_string(),
            error.line(),
            error.column(),
            format!("{:?}", error.classify()),
        )
    })
}

#[test]
fn limit_overflow_derived_decoder_matches_manual_errors_and_values() {
    for json in [
        r#"{"required":1,"available":0}"#,
        r#"{"available":0,"required":18446744073709551615}"#,
        r#"{"required":0,"available":0}"#,
        r#"{"required":1,"available":2}"#,
        r#"{"required":1}"#,
        r#"{"required":-1,"available":0}"#,
        r#"{"required":1.5,"available":0}"#,
        r#"{"required":1,"available":0,"extra":1}"#,
        r#"{"required":1,"required":2,"available":0}"#,
        "[1,0]",
        "[1,0,2]",
        "null",
        r#"{"required":"1","available":0}"#,
        "{\n\"required\": 2,\n\"available\": 2\n}",
        "{",
    ] {
        assert_eq!(
            outcome(serde_json::from_str::<LimitOverflow>(json)),
            outcome(serde_json::from_str::<LegacyLimit>(json).map(|value| value.0)),
            "{json}"
        );
    }
}

#[test]
fn context_overflow_derived_decoder_matches_manual_errors_and_values() {
    for json in [
        r#"{"input":{"required":2,"available":1}}"#,
        r#"{"shared":{"required":2,"available":1},"output":{"required":3,"available":0}}"#,
        "{}",
        r#"{"shared":null,"input":null,"output":null}"#,
        r#"{"input":{"required":1,"available":1}}"#,
        r#"{"input":{"required":2,"available":1,"extra":0}}"#,
        r#"{"input":{"required":2,"available":1},"extra":0}"#,
        r#"{"input":{"required":2,"available":1},"input":null}"#,
        r#"[{"required":2,"available":1},null,null]"#,
        "[null,null,null]",
        "null",
        r#"{"input":2}"#,
        "{\n\"input\": {\"required\": 2,\"available\": 2}\n}",
        "{",
    ] {
        assert_eq!(
            outcome(serde_json::from_str::<ContextOverflow>(json)),
            outcome(serde_json::from_str::<LegacyContext>(json).map(|value| value.0)),
            "{json}"
        );
    }
}
