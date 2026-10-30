use super::*;
use serde_json::json;

#[test]
fn named_state_numbers_refuse_inexact_integers_and_normalize_floats() -> Result<(), StateError> {
    let state = NamedState::default();
    for number in [
        json!((1_u64 << 53) + 1),
        json!(-((1_i64 << 53) + 1)),
        json!(1_792_956_173_562_340_043_u64),
        json!(1e16),
        json!(-1e16),
        json!(18_014_398_509_481_984.0_f64),
    ] {
        assert!(matches!(
            state.put("number".to_owned(), &json!({"nested": [number]})),
            Err(StateError::InexactNumber)
        ));
    }
    let normalized = state
        .put("one".to_owned(), &json!(1.0))?
        .put("zero".to_owned(), &json!(-0.0))?;
    assert_eq!(normalized.get("one")?, Some(json!(1)));
    assert_eq!(normalized.get("zero")?, Some(json!(0)));
    assert!(
        state
            .put("max".to_owned(), &json!((1_u64 << 53) - 1))
            .is_ok()
    );
    Ok(())
}

#[test]
fn named_state_integral_float_lists_round_trip_or_refuse_before_storage() -> Result<(), StateError>
{
    let state = NamedState::default();
    assert!(matches!(
        state.put("near".into(), &json!(vec![1e16; 35_000])),
        Err(StateError::InexactNumber)
    ));
    assert_eq!(state.get("near")?, None);
    let state = state.put("near".into(), &json!(vec![1e15; 26_000]))?;
    assert_eq!(
        state.get("near")?,
        Some(json!(vec![1_000_000_000_000_000_u64; 26_000]))
    );
    assert!(matches!(
        validate_state_value(b"10000000000000000"),
        Err(StateError::InexactNumber)
    ));
    Ok(())
}

#[test]
fn named_state_restore_preserves_typed_error_sources() {
    use std::error::Error as _;

    for (key, bytes) in [
        ("bad/key", b"null".as_slice()),
        ("json", b"not-json".as_slice()),
    ] {
        let Err(error) =
            NamedState::from_values(BTreeMap::from([(key.to_owned(), Arc::from(bytes))]))
        else {
            panic!("invalid stored state was accepted");
        };
        assert_eq!(error, error.clone());
        assert!(error.source().is_some());
        let RuntimeManifestError::InvalidNamedState(source) = error else {
            panic!("typed state source missing");
        };
        if key == "bad/key" {
            assert!(matches!(*source, StateError::InvalidKey));
        } else {
            assert!(source.source().is_some());
            assert!(matches!(*source, StateError::Json(_)));
        }
    }
}

#[test]
fn named_state_canonical_values_and_key_contract() -> Result<(), RuntimeManifestError> {
    let state =
        NamedState::default().put("_A.1-z".to_owned(), &json!({"z": null, "a": [true, 3]}))?;
    assert_eq!(
        state.values()["_A.1-z"].as_ref(),
        br#"{"a":[true,3],"z":null}"#
    );
    assert_eq!(state.get("missing")?, None);
    assert_eq!(
        state.get("_A.1-z")?,
        Some(json!({"z": null, "a": [true, 3]}))
    );
    let null = state.put("null".to_owned(), &Value::Null)?;
    assert_eq!(null.get("null")?, Some(Value::Null));
    for key in ["", "1key", "a/b", "a b", "é", &"a".repeat(65)] {
        assert!(state.put(key.to_owned(), &json!(1)).is_err(), "{key}");
        assert!(state.get(key).is_err());
        assert!(state.drop_key(key).is_err());
    }
    assert!(state.put("a".repeat(64), &json!(1)).is_ok());
    assert!(validate_state_value(br#"{"z":null, "a":1}"#).is_err());
    assert!(validate_state_value(br#"{"a":1,"a":1}"#).is_err());
    assert!(validate_state_value(b"null trailing").is_err());
    Ok(())
}

#[test]
fn named_state_limits_are_atomic_and_replacement_releases_capacity()
-> Result<(), RuntimeManifestError> {
    let large = Value::String("x".repeat(MAX_STATE_VALUE_BYTES - 2));
    let mut state = NamedState::default();
    for key in ["a", "b", "c", "d"] {
        state = state.put(key.to_owned(), &large)?;
    }
    assert_eq!(
        state
            .values()
            .values()
            .map(|value| value.len())
            .sum::<usize>(),
        MAX_STATE_BYTES
    );
    assert!(
        matches!(state.put("fifth".to_owned(), &json!(0)), Err(StateError::StateTooLarge { bytes }) if bytes == MAX_STATE_BYTES + 1)
    );
    assert!(
        state
            .put(
                "a".to_owned(),
                &Value::String("x".repeat(MAX_STATE_VALUE_BYTES - 1))
            )
            .is_err()
    );
    assert_eq!(state.values().len(), 4);
    let smaller = state
        .put("a".to_owned(), &json!(0))?
        .put("fifth".to_owned(), &json!(0))?;
    assert_eq!(smaller.values().len(), 5);
    let mut keys = NamedState::default();
    for index in 0..MAX_STATE_KEYS {
        keys = keys.put(format!("key{index}"), &json!(index))?;
    }
    assert!(matches!(
        keys.put("overflow".to_owned(), &json!(0)),
        Err(StateError::TooManyKeys)
    ));
    assert_eq!(
        keys.put("key0".to_owned(), &json!(1))?.values().len(),
        MAX_STATE_KEYS
    );
    assert_eq!(
        keys.drop_key("key0")?
            .put("new".to_owned(), &json!(0))?
            .values()
            .len(),
        MAX_STATE_KEYS
    );
    Ok(())
}
