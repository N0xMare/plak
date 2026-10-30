use super::*;
use serde_json::json;

#[test]
fn documents_are_bounded_by_bytes_alone_under_the_same_strict_rules()
-> Result<(), Box<dyn std::error::Error>> {
    let long = format!(r#"{{"content":"{}"}}"#, "x".repeat(MAX_STRING_BYTES + 1));
    assert!(SemanticJsonV1::parse(&long).is_err());
    assert!(SemanticJsonV1::parse_document(&long, 1 << 20)?.is_object());
    let wide = format!("[{}0]", "0,".repeat(MAX_NODES));
    assert!(SemanticJsonV1::parse_document(&wide, 1 << 20)?.is_array());
    assert_eq!(
        SemanticJsonV1::parse_document(r#"{"a":1,"a":2}"#, 1 << 20),
        Err(SemanticJsonError::DuplicateKey)
    );
    assert_eq!(
        SemanticJsonV1::parse_document(&long, 1024),
        Err(SemanticJsonError::ResourceLimit)
    );
    let value = json!({"b": [1.0, 2, "\u{e000}"], "a": {"minItems": 1.0}});
    assert_eq!(
        SemanticJsonV1::canonical_document_bytes(value.clone(), usize::MAX)?,
        SemanticJsonV1::from_value(value)?
            .canonical_bytes()
            .to_vec()
    );
    let many = Value::Array(vec![Value::from(1); MAX_NODES + 1]);
    assert!(SemanticJsonV1::from_value(many.clone()).is_err());
    assert!(SemanticJsonV1::canonical_document_bytes(many, usize::MAX).is_ok());
    Ok(())
}

#[expect(
    clippy::needless_pass_by_value,
    reason = "json macro needs owned test values"
)]
fn root(properties: Map<String, Value>, required: Vec<Value>) -> Value {
    json!({
        "$schema": SCHEMA_URI,
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false
    })
}

#[test]
fn strict_parser_rejects_duplicates_trailing_and_numeric_range() {
    assert!(matches!(
        SemanticJsonV1::parse(r#"{"a":1,"a":2}"#),
        Err(SemanticJsonError::DuplicateKey)
    ));
    assert!(matches!(
        SemanticJsonV1::parse(r#"{"a":1,"\u0061":2}"#),
        Err(SemanticJsonError::DuplicateKey)
    ));
    for bad in ["0 1", "1e400", "1e-4000", "NaN", "Infinity", "01"] {
        assert!(SemanticJsonV1::parse(bad).is_err(), "{bad}");
    }
}

#[test]
fn rfc8785_appendix_b_complete_binary64_vectors() -> Result<(), SemanticJsonError> {
    let vectors = [
        (0x0000_0000_0000_0000, "0"),
        (0x8000_0000_0000_0000, "0"),
        (0x0000_0000_0000_0001, "5e-324"),
        (0x8000_0000_0000_0001, "-5e-324"),
        (0x7fef_ffff_ffff_ffff, "1.7976931348623157e+308"),
        (0xffef_ffff_ffff_ffff, "-1.7976931348623157e+308"),
        (0x4340_0000_0000_0000, "9007199254740992"),
        (0xc340_0000_0000_0000, "-9007199254740992"),
        (0x4430_0000_0000_0000, "295147905179352830000"),
        (0x44b5_2d02_c7e1_4af5, "9.999999999999997e+22"),
        (0x44b5_2d02_c7e1_4af6, "1e+23"),
        (0x44b5_2d02_c7e1_4af7, "1.0000000000000001e+23"),
        (0x444b_1ae4_d6e2_ef4e, "999999999999999700000"),
        (0x444b_1ae4_d6e2_ef4f, "999999999999999900000"),
        (0x444b_1ae4_d6e2_ef50, "1e+21"),
        (0x3eb0_c6f7_a0b5_ed8c, "9.999999999999997e-7"),
        (0x3eb0_c6f7_a0b5_ed8d, "0.000001"),
        (0x41b3_de43_5555_5553, "333333333.3333332"),
        (0x41b3_de43_5555_5554, "333333333.33333325"),
        (0x41b3_de43_5555_5555, "333333333.3333333"),
        (0x41b3_de43_5555_5556, "333333333.3333334"),
        (0x41b3_de43_5555_5557, "333333333.33333343"),
        (0xbecb_f647_612f_3696, "-0.0000033333333333333333"),
        (0x4314_3ff3_c1cb_0959, "1424953923781206.2"),
    ];
    for (bits, expected) in vectors {
        let value = SemanticJsonV1::from_value(Value::from(f64::from_bits(bits)))?;
        assert_eq!(value.canonical_bytes().as_ref(), expected.as_bytes());
    }
    Ok(())
}

#[test]
fn int53_whole_numbers_serialize_as_json_integers() -> Result<(), SemanticJsonError> {
    let from_int = SemanticJsonV1::from_value(json!({"minItems": 1}))?;
    let from_float = SemanticJsonV1::from_value(json!({"minItems": 1.0}))?;
    assert_eq!(from_int.digest(), from_float.digest());
    let serialized =
        serde_json::to_string(from_int.value().as_ref()).map_err(|_| SemanticJsonError::Invalid)?;
    assert_eq!(serialized, r#"{"minItems":1}"#);
    let serialized_float = serde_json::to_string(from_float.value().as_ref())
        .map_err(|_| SemanticJsonError::Invalid)?;
    assert_eq!(serialized_float, r#"{"minItems":1}"#);
    assert_eq!(from_int.canonical_bytes().as_ref(), b"{\"minItems\":1}");
    Ok(())
}

#[test]
fn rfc8785_property_order_and_reference_corpus() -> Result<(), SemanticJsonError> {
    let vectors = [
        (
            r#"{"\u20ac":"Euro Sign","\r":"Carriage Return","\ufb33":"Hebrew Letter Dalet With Dagesh","1":"One","\ud83d\ude00":"Emoji: Grinning Face","\u0080":"Control","\u00f6":"Latin Small Letter O With Diaeresis"}"#,
            "{\"\\r\":\"Carriage Return\",\"1\":\"One\",\"\u{80}\":\"Control\",\"ö\":\"Latin Small Letter O With Diaeresis\",\"€\":\"Euro Sign\",\"😀\":\"Emoji: Grinning Face\",\"\u{fb33}\":\"Hebrew Letter Dalet With Dagesh\"}",
        ),
        (
            r#"[56,{"d":true,"10":null,"1":[]}]"#,
            r#"[56,{"1":[],"10":null,"d":true}]"#,
        ),
        (
            r#"{"peach":"This sorting order","péché":"is wrong according to French","pêche":"but canonicalization MUST","sin":"ignore locale"}"#,
            r#"{"peach":"This sorting order","péché":"is wrong according to French","pêche":"but canonicalization MUST","sin":"ignore locale"}"#,
        ),
        (
            r#"{"1":{"f":{"f":"hi","F":5},"\n":56.0},"10":{},"":"empty","a":{},"111":[{"e":"yes","E":"no"}],"A":{}}"#,
            "{\"\":\"empty\",\"1\":{\"\\n\":56,\"f\":{\"F\":5,\"f\":\"hi\"}},\"10\":{},\"111\":[{\"E\":\"no\",\"e\":\"yes\"}],\"A\":{},\"a\":{}}",
        ),
        (
            r#"{"Unnormalized Unicode":"A\u030a"}"#,
            "{\"Unnormalized Unicode\":\"A\u{030a}\"}",
        ),
        (
            r#"{"numbers":[333333333.33333329,1E30,4.50,2e-3,0.000000000000000000000000001],"string":"\u20ac$\u000F\u000aA'\u0042\u0022\u005c\\\"\/","literals":[null,true,false]}"#,
            "{\"literals\":[null,true,false],\"numbers\":[333333333.3333333,1e+30,4.5,0.002,1e-27],\"string\":\"€$\\u000f\\nA'B\\\"\\\\\\\\\\\"/\"}",
        ),
        (
            r#"{"\u20ac":"Euro Sign","\r":"Carriage Return","\u000a":"Newline","1":"One","\u0080":"Control\u007f","\ud83d\ude02":"Smiley","\u00f6":"Latin Small Letter O With Diaeresis","\ufb33":"Hebrew Letter Dalet With Dagesh","</script>":"Browser Challenge"}"#,
            "{\"\\n\":\"Newline\",\"\\r\":\"Carriage Return\",\"1\":\"One\",\"</script>\":\"Browser Challenge\",\"\u{80}\":\"Control\u{7f}\",\"ö\":\"Latin Small Letter O With Diaeresis\",\"€\":\"Euro Sign\",\"😂\":\"Smiley\",\"\u{fb33}\":\"Hebrew Letter Dalet With Dagesh\"}",
        ),
    ];
    for (raw, expected) in vectors {
        assert_eq!(
            SemanticJsonV1::parse(raw)?.canonical_bytes().as_ref(),
            expected.as_bytes()
        );
    }
    Ok(())
}

#[test]
fn jcs_uses_raw_utf16_property_order_and_preserves_arrays() -> Result<(), SemanticJsonError> {
    let v = SemanticJsonV1::parse(r#"{"\uE000":1,"\uD834\uDD1E":2,"a":[2,1]}"#)?;
    assert_eq!(
        v.canonical_bytes().as_ref(),
        "{\"a\":[2,1],\"𝄞\":2,\"\":1}".as_bytes()
    );
    Ok(())
}

#[test]
fn parser_enforces_depth_node_string_and_byte_bounds() {
    let too_deep = format!(
        "{}0{}",
        "[".repeat(MAX_DEPTH + 1),
        "]".repeat(MAX_DEPTH + 1)
    );
    assert!(matches!(
        SemanticJsonV1::parse(&too_deep),
        Err(SemanticJsonError::ResourceLimit)
    ));
    let too_many_nodes = format!("[{}]", "0,".repeat(MAX_NODES));
    assert!(matches!(
        SemanticJsonV1::parse(&too_many_nodes),
        Err(SemanticJsonError::ResourceLimit)
    ));
    let too_long = format!("\"{}\"", "x".repeat(MAX_STRING_BYTES + 1));
    assert!(matches!(
        SemanticJsonV1::parse(&too_long),
        Err(SemanticJsonError::ResourceLimit)
    ));
    let too_many_bytes = format!(" \"{}\"", "x".repeat(MAX_BYTES));
    assert!(matches!(
        SemanticJsonV1::parse(&too_many_bytes),
        Err(SemanticJsonError::ResourceLimit)
    ));
}

#[test]
fn parser_accepts_exact_instance_bounds_and_rejects_plus_one() {
    let at_depth = format!("{}0{}", "[".repeat(MAX_DEPTH), "]".repeat(MAX_DEPTH));
    assert!(SemanticJsonV1::parse(&at_depth).is_ok());
    let at_nodes = format!(
        "[{}]",
        std::iter::repeat_n("0", MAX_NODES - 1)
            .collect::<Vec<_>>()
            .join(",")
    );
    assert!(SemanticJsonV1::parse(&at_nodes).is_ok());
    let at_string = format!("\"{}\"", "x".repeat(MAX_STRING_BYTES));
    assert!(SemanticJsonV1::parse(&at_string).is_ok());
    let at_bytes = format!("0{}", " ".repeat(MAX_BYTES - 1));
    assert!(SemanticJsonV1::parse(&at_bytes).is_ok());
    assert!(matches!(
        SemanticJsonV1::parse(&(at_bytes + " ")),
        Err(SemanticJsonError::ResourceLimit)
    ));
}

#[test]
fn parser_rejects_adversarial_number_grammar_and_surrogates() {
    for bad in [
        "+1",
        ".1",
        "1.",
        "1e",
        "1e+",
        "--1",
        "00",
        "-01",
        "[1,]",
        r#""\ud800""#,
    ] {
        assert!(SemanticJsonV1::parse(bad).is_err(), "{bad}");
    }
}

#[test]
fn schema_requires_exact_closed_required_object_shape() {
    let good = root(
        Map::from_iter([("x".into(), json!({"type":"string"}))]),
        vec![json!("x")],
    );
    assert!(PortableToolSchemaV1::new(good).is_ok());
    for bad in [
        json!({"type":"object","properties":{},"required":[],"additionalProperties":false}),
        json!({"$schema":"http://json-schema.org/draft/2020-12/schema","type":"object","properties":{},"required":[],"additionalProperties":false}),
        json!({"$schema":SCHEMA_URI,"type":"string"}),
        root(
            Map::from_iter([("x".into(), json!({"type":"string"}))]),
            vec![],
        ),
        root(
            Map::from_iter([("x".into(), json!({"type":"string"}))]),
            vec![json!("x"), json!("x")],
        ),
        root(
            Map::from_iter([("x".into(), json!({"type":"string"}))]),
            vec![json!("y")],
        ),
        root(
            Map::from_iter([(
                "x".into(),
                json!({"type":"object","properties":{},"required":[],"additionalProperties":true}),
            )]),
            vec![json!("x")],
        ),
    ] {
        assert!(PortableToolSchemaV1::new(bad).is_err());
    }
}

#[test]
fn schema_accepts_only_v1_types_keywords_and_scalar_enums() {
    for kind in ["string", "number", "integer", "boolean"] {
        let schema = root(
            Map::from_iter([("x".into(), json!({"type":kind}))]),
            vec![json!("x")],
        );
        assert!(PortableToolSchemaV1::new(schema).is_ok(), "{kind}");
    }
    let array = root(
        Map::from_iter([(
            "x".into(),
            json!({"type":"array","items":{"type":"boolean"}}),
        )]),
        vec![json!("x")],
    );
    assert!(PortableToolSchemaV1::new(array).is_ok());
    for child in [
        json!({"type":"null"}),
        json!({"type":["string","null"]}),
        json!({"type":"string","minLength":1}),
        json!({"type":"number","minimum":0}),
        json!({"type":"array","items":true}),
        json!({"type":"array"}),
        json!({"type":"string","properties":{}}),
        json!({"type":"string","enum":[]}),
        json!({"type":"string","enum":["a","a"]}),
        json!({"type":"integer","enum":[1.5]}),
        json!({"type":"object","properties":{},"required":[],"additionalProperties":false,"enum":[true]}),
    ] {
        let schema = root(Map::from_iter([("x".into(), child)]), vec![json!("x")]);
        assert!(PortableToolSchemaV1::new(schema).is_err());
    }
}

#[test]
fn schema_enforces_property_enum_depth_and_string_bounds() {
    let properties = (0..=MAX_PROPERTIES_PER_OBJECT)
        .map(|index| (format!("p{index}"), json!({"type":"boolean"})))
        .collect::<Map<_, _>>();
    let required = properties.keys().cloned().map(Value::String).collect();
    assert!(PortableToolSchemaV1::new(root(properties, required)).is_err());

    let enums = (0..=MAX_ENUM_ENTRIES)
        .map(|index| json!(index))
        .collect::<Vec<_>>();
    let schema = root(
        Map::from_iter([("x".into(), json!({"type":"number","enum":enums}))]),
        vec![json!("x")],
    );
    assert!(PortableToolSchemaV1::new(schema).is_err());

    let schema = root(
        Map::from_iter([(
            "x".into(),
            json!({"type":"string","description":"x".repeat(MAX_SCHEMA_STRING_BYTES + 1)}),
        )]),
        vec![json!("x")],
    );
    assert!(PortableToolSchemaV1::new(schema).is_err());

    let mut child = json!({"type":"boolean"});
    for _ in 0..=MAX_SCHEMA_DEPTH {
        child = json!({"type":"array","items":child});
    }
    let schema = root(Map::from_iter([("x".into(), child)]), vec![json!("x")]);
    assert!(PortableToolSchemaV1::new(schema).is_err());
}

#[test]
fn schema_accepts_independent_exact_v1_boundaries() -> Result<(), SemanticJsonError> {
    let properties = (0..MAX_PROPERTIES_PER_OBJECT)
        .map(|index| (format!("p{index}"), json!({"type":"boolean"})))
        .collect::<Map<_, _>>();
    let required = properties.keys().cloned().map(Value::String).collect();
    PortableToolSchemaV1::new(root(properties, required))?;

    let values = (0..MAX_ENUM_ENTRIES).map(Value::from).collect::<Vec<_>>();
    PortableToolSchemaV1::new(root(
        Map::from_iter([("x".into(), json!({"type":"integer","enum":values}))]),
        vec![json!("x")],
    ))?;

    let mut child = json!({"type":"boolean"});
    for _ in 0..MAX_SCHEMA_DEPTH - 1 {
        child = json!({"type":"array","items":child});
    }
    PortableToolSchemaV1::new(root(
        Map::from_iter([("x".into(), child)]),
        vec![json!("x")],
    ))?;

    PortableToolSchemaV1::new(root(
        Map::from_iter([(
            "x".into(),
            json!({"type":"string","description":"x".repeat(MAX_SCHEMA_STRING_BYTES)}),
        )]),
        vec![json!("x")],
    ))?;

    let mut visible = root(
        (0..8)
            .map(|index| {
                (
                    format!("p{index}"),
                    json!({"type":"string","description":""}),
                )
            })
            .collect(),
        (0..8).map(|index| json!(format!("p{index}"))).collect(),
    );
    let mut baseline = 0;
    validate_schema_strings(&visible, &mut baseline)?;
    let mut remaining = MAX_MODEL_VISIBLE_STRING_BYTES - baseline;
    let properties = visible["properties"]
        .as_object_mut()
        .ok_or(SemanticJsonError::InvalidSchema("test properties"))?;
    for index in 0..8 {
        let take = remaining.min(MAX_SCHEMA_STRING_BYTES);
        properties[&format!("p{index}")]["description"] = json!("x".repeat(take));
        remaining -= take;
    }
    assert_eq!(remaining, 0);
    let mut total = 0;
    validate_schema_strings(&visible, &mut total)?;
    assert_eq!(total, MAX_MODEL_VISIBLE_STRING_BYTES);
    PortableToolSchemaV1::new(visible)?;
    Ok(())
}

#[test]
fn maximum_legal_schema_and_adversarial_plus_one_validate() -> Result<(), SemanticJsonError> {
    let mut properties = Map::new();
    let mut required = Vec::new();
    for group in 0..4 {
        let mut nested = Map::new();
        let mut nested_required = Vec::new();
        for leaf in 0..63 {
            let name = format!("p{group}_{leaf}");
            nested.insert(name.clone(), json!({"type":"integer"}));
            nested_required.push(Value::String(name));
        }
        let name = format!("g{group}");
        properties.insert(
            name.clone(),
            json!({
                "type":"object", "properties":nested,
                "required":nested_required, "additionalProperties":false
            }),
        );
        required.push(Value::String(name));
    }
    let maximum = PortableToolSchemaV1::new(root(properties, required))?;
    let mut instance = Map::new();
    for group in 0..4 {
        let nested = (0..63)
            .map(|leaf| (format!("p{group}_{leaf}"), Value::from(leaf)))
            .collect();
        instance.insert(format!("g{group}"), Value::Object(nested));
    }
    maximum.validate(&SemanticJsonV1::from_value(Value::Object(instance))?)?;

    let mut too_many = Map::new();
    let mut too_many_required = Vec::new();
    for index in 0..=MAX_PROPERTIES_PER_OBJECT {
        let name = format!("p{index}");
        too_many.insert(name.clone(), json!({"type":"boolean"}));
        too_many_required.push(Value::String(name));
    }
    assert!(PortableToolSchemaV1::new(root(too_many, too_many_required)).is_err());
    Ok(())
}

#[test]
fn instance_validation_enforces_required_types_enum_and_safe_integer()
-> Result<(), SemanticJsonError> {
    let schema = PortableToolSchemaV1::new(root(
        Map::from_iter([
            ("choice".into(), json!({"type":"string","enum":["a","b"]})),
            ("count".into(), json!({"type":"integer"})),
            (
                "flags".into(),
                json!({"type":"array","items":{"type":"boolean"}}),
            ),
        ]),
        vec![json!("choice"), json!("count"), json!("flags")],
    ))?;
    assert!(
        schema
            .parse_instance(r#"{"choice":"a","count":9007199254740991,"flags":[true]}"#)
            .is_ok()
    );
    for bad in [
        r#"{"choice":"a","count":1}"#,
        r#"{"choice":"c","count":1,"flags":[]}"#,
        r#"{"choice":"a","count":1.5,"flags":[]}"#,
        r#"{"choice":"a","count":9007199254740992,"flags":[]}"#,
        r#"{"choice":"a","count":1,"flags":[0]}"#,
        r#"{"choice":"a","count":1,"flags":[],"extra":true}"#,
    ] {
        assert!(matches!(
            schema.parse_instance(bad),
            Err(SemanticJsonError::SchemaMismatch)
        ));
    }
    Ok(())
}

#[test]
fn serde_reconstruction_verifies_revision_canonical_bytes_and_digest()
-> Result<(), Box<dyn std::error::Error>> {
    let value = SemanticJsonV1::parse(r#"{"a":1}"#)?;
    let encoded = serde_json::to_string(&value)?;
    assert_eq!(serde_json::from_str::<SemanticJsonV1>(&encoded)?, value);
    let mut wire = serde_json::to_value(&value)?;
    wire["revision"] = json!(SEMANTIC_JSON_REVISION + 1);
    assert!(serde_json::from_value::<SemanticJsonV1>(wire).is_err());

    for field in ["canonical", "digest"] {
        let mut wire = serde_json::to_value(&value)?;
        wire[field] = json!([0]);
        assert!(
            serde_json::from_value::<SemanticJsonV1>(wire).is_err(),
            "{field}"
        );
    }
    let mut wire = serde_json::to_value(&value)?;
    wire["unknown"] = json!(true);
    assert!(serde_json::from_value::<SemanticJsonV1>(wire).is_err());
    Ok(())
}
