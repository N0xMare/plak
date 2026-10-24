use super::{codex_code_mode_wait_schema, product_javascript_wait_schema};
use plakous_model::PortableToolSchemaV2;
use serde_json::{Value, json};

fn accepts(schema: &Value, arguments: &Value) -> bool {
    PortableToolSchemaV2::new(schema.clone())
        .expect("wait schema is valid")
        .parse_instance(&arguments.to_string())
        .is_ok()
}

#[test]
fn cell_wait_contracts_share_a_600_second_ceiling() {
    for schema in [
        codex_code_mode_wait_schema(),
        product_javascript_wait_schema(),
    ] {
        assert_eq!(
            schema["properties"]["yield_time_ms"]["maximum"],
            json!(600_000)
        );
        assert!(accepts(
            &schema,
            &json!({"cell_id": "2", "yield_time_ms": 600_000})
        ));
        assert!(!accepts(
            &schema,
            &json!({"cell_id": "2", "yield_time_ms": 600_001})
        ));
    }
}

#[test]
fn cell_wait_hints_clamp_before_schema_validation() {
    for cell in [json!("2"), json!(2), json!("none")] {
        let arguments = json!({"cell_id": cell, "yield_time_ms": 900_000, "max_tokens": 500, "terminate": false});
        let normalized = super::normalized_wait_arguments(&arguments.to_string(), Some("2"))
            .expect("a large hint is normalized");
        assert_eq!(
            serde_json::from_str::<Value>(&normalized).expect("normalized JSON"),
            json!({"cell_id": "2", "yield_time_ms": 600_000, "max_tokens": 500, "terminate": false})
        );
    }
    for hint in [0, 10_000, 600_000] {
        let arguments = json!({"cell_id": "2", "yield_time_ms": hint}).to_string();
        assert!(super::normalized_wait_arguments(&arguments, None).is_none());
    }
    for hint in [json!(-1), json!(600_000.5), json!("900000"), Value::Null] {
        let arguments = json!({"cell_id": "2", "yield_time_ms": hint}).to_string();
        assert!(super::normalized_wait_arguments(&arguments, None).is_none());
    }
}

/// Both `wait` vocabularies require `cell_id` as a **string**, while the
/// cell identifiers the product hands the model are bare numerals: a cell
/// announces itself as "Script running with cell ID 2".
///
/// This pins the presented contract, which is identity-bearing and does
/// not change. `normalized_wait_arguments` is what lets a model echo the
/// numeral it was shown, by normalising before this contract validates.
#[test]
fn a_wait_cell_id_is_accepted_as_a_string_and_refused_as_the_number_it_looks_like() {
    for schema in [
        codex_code_mode_wait_schema(),
        product_javascript_wait_schema(),
    ] {
        assert!(accepts(&schema, &json!({"cell_id": "2"})));
        assert!(!accepts(&schema, &json!({"cell_id": 2})));
        // The other two shapes a rejected `wait` could have had.
        assert!(!accepts(&schema, &json!({})));
        assert!(!accepts(&schema, &json!({"cell_id": "2", "cell": "2"})));
    }
    // `terminate` and the two budgets are optional in both vocabularies.
    assert!(accepts(
        &codex_code_mode_wait_schema(),
        &json!({"cell_id": "2", "yield_time_ms": 1000, "max_tokens": 500, "terminate": false})
    ));
}

/// Normalisation turns the numeral a model was shown into the string the
/// contract declares, and leaves everything else exactly as it arrived so
/// the contract still decides.
#[test]
fn normalisation_accepts_a_numeric_cell_id_and_touches_nothing_else() {
    use super::normalized_wait_arguments;

    assert_eq!(
        normalized_wait_arguments(r#"{"cell_id":2,"terminate":true}"#, None).as_deref(),
        Some(r#"{"cell_id":"2","terminate":true}"#)
    );
    assert_eq!(
        normalized_wait_arguments(r#"{"cell_id":0}"#, None).as_deref(),
        Some(r#"{"cell_id":"0"}"#)
    );
    // Already a string, or a shape the contract must refuse: nothing
    // to normalise, so the original bytes are validated.
    for untouched in [
        r#"{"cell_id":"2"}"#,
        r#"{"cell_id":true}"#,
        r#"{"cell_id":-1}"#,
        r#"{"cell_id":1.5}"#,
        r#"{"cell_id":["2"]}"#,
        // Absent, null and empty are schema violations, not spellings.
        r"{}",
        r#"{"cell_id":null}"#,
        r#"{"cell_id":""}"#,
        "not json",
    ] {
        assert_eq!(
            normalized_wait_arguments(untouched, Some("7")),
            None,
            "{untouched}"
        );
    }
}

/// A call sent
/// `{"cell_id": "none", "yield_time_ms": 1}` and the call was rejected
/// `InvalidArguments`, twice, which converted the whole case to a
/// `product_failure`. A model that has lost track of whether a cell is
/// running writes one of these rather than inventing an identifier.
///
/// With at most one cell active per agent, "no particular cell" names the
/// running one and nothing else.
///
/// Every word here is a valid non-empty string, so this superset never
/// widens the presented contract: it reinterprets values the contract
/// already admits. An absent, null or empty `cell_id` is a schema violation
/// and stays one.
#[test]
fn every_spelling_of_no_particular_cell_names_the_running_one() {
    use super::normalized_wait_arguments;

    for spelling in [
        r#"{"cell_id":"none"}"#,
        r#"{"cell_id":"NONE"}"#,
        r#"{"cell_id":" none "}"#,
        r#"{"cell_id":"null"}"#,
        r#"{"cell_id":"all"}"#,
    ] {
        assert_eq!(
            normalized_wait_arguments(spelling, Some("3")).as_deref(),
            Some(r#"{"cell_id":"3"}"#),
            "{spelling}"
        );
    }
    // The rest of the call survives normalisation untouched.
    assert_eq!(
        normalized_wait_arguments(r#"{"cell_id":"none","yield_time_ms":1}"#, Some("3")).as_deref(),
        Some(r#"{"cell_id":"3","yield_time_ms":1}"#)
    );
    // With nothing running there is nothing to name, so the arguments
    // are left alone. Each word is still a valid string, so the contract
    // admits the call and the driver answers it before dispatch.
    for spelling in [
        r#"{"cell_id":"none"}"#,
        r#"{"cell_id":"null"}"#,
        r#"{"cell_id":"all"}"#,
    ] {
        assert_eq!(
            normalized_wait_arguments(spelling, None),
            None,
            "{spelling}"
        );
    }
    // Absent, null and empty are refused by the contract for the field
    // they are missing, whether or not a cell is running.
    for violation in [r"{}", r#"{"cell_id":null}"#, r#"{"cell_id":""}"#] {
        for active in [Some("3"), None] {
            assert_eq!(
                normalized_wait_arguments(violation, active),
                None,
                "{violation}"
            );
        }
    }
    // A named cell is never reinterpreted as "no particular cell".
    assert_eq!(
        normalized_wait_arguments(r#"{"cell_id":"nonesuch"}"#, Some("3")),
        None
    );
}

/// Normalising a call twice is the same as normalising it once. The tool
/// exposure applies this before validation with no active cell in hand, and
/// the driver applies it with one, so both run over the same call.
#[test]
fn normalisation_is_idempotent_across_both_application_points() {
    use super::normalized_wait_arguments;

    let once = normalized_wait_arguments(r#"{"cell_id":"none"}"#, Some("5")).expect("first pass");
    assert_eq!(once, r#"{"cell_id":"5"}"#);
    assert_eq!(normalized_wait_arguments(&once, Some("5")), None);
    assert_eq!(normalized_wait_arguments(&once, None), None);
}
