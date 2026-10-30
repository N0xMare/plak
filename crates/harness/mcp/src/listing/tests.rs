use serde_json::json;

use super::*;

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn tool(name: &str, schema: Value) -> Result<Tool, Box<dyn std::error::Error>> {
    let Value::Object(schema) = schema else {
        return Err("schema".into());
    };
    Ok(Tool::new(
        name.to_owned(),
        format!("{name} tool"),
        Arc::new(schema),
    ))
}

fn spec() -> Result<McpServerSpec, Box<dyn std::error::Error>> {
    Ok(McpServerSpec::stdio(
        McpServerName::new("fake")?,
        "fake",
        [],
    ))
}

fn object() -> Value {
    json!({"type": "object", "properties": {"q": {"type": "string"}}})
}

#[test]
fn wildcard_selection_omits_unlowerable_tools_with_a_notice() -> TestResult {
    let tools = [
        tool("echo", object())?,
        tool(
            "map",
            json!({"type": "object", "additionalProperties": {"type": "string"}}),
        )?,
    ];
    let frozen = freeze_listing(&spec()?, &tools, false, &McpToolSelection::All)?;
    assert_eq!(frozen.records().len(), 1);
    assert_eq!(
        frozen.records()[0].exposed_name().as_str(),
        "mcp__fake__echo"
    );
    assert_eq!(frozen.records()[0].description(), "echo tool");
    assert_eq!(
        frozen.notices(),
        [
            "mcp tool fake/map: input schema is not lowerable: additionalProperties with a schema at /; omitted"
        ]
    );
    let named = freeze_listing(
        &spec()?,
        &tools,
        false,
        &McpToolSelection::Named(vec!["map".into()]),
    );
    assert_eq!(
        named.err().map(|error| error.kind()),
        Some(McpErrorKind::Selection)
    );
    Ok(())
}

#[test]
fn named_selection_follows_its_order_and_respects_the_allowlist() -> TestResult {
    let tools = [
        tool("a", object())?,
        tool("b", object())?,
        tool("c", object())?,
    ];
    let spec = spec()?.with_allowlist(["a".to_owned(), "c".to_owned()]);
    let frozen = freeze_listing(
        &spec,
        &tools,
        false,
        &McpToolSelection::Named(vec!["c".into(), "a".into()]),
    )?;
    let names = frozen
        .records()
        .iter()
        .map(McpToolRecord::mcp_name)
        .collect::<Vec<_>>();
    assert_eq!(names, ["c", "a"]);
    let all = freeze_listing(&spec, &tools, false, &McpToolSelection::All)?;
    assert_eq!(all.records().len(), 2);
    for (name, message) in [
        ("b", "mcp tool fake/b is not in the server's allowlist"),
        ("z", "mcp tool fake/z is not in the server's allowlist"),
    ] {
        let error = freeze_listing(
            &spec,
            &tools,
            false,
            &McpToolSelection::Named(vec![name.into()]),
        )
        .err()
        .ok_or("selection bound")?;
        assert_eq!(error.message(), message);
    }
    let open = freeze_listing(
        &self::spec()?,
        &tools,
        false,
        &McpToolSelection::Named(vec!["z".into()]),
    )
    .err()
    .ok_or("selection bound")?;
    assert_eq!(open.message(), "mcp server fake has no tool z");
    Ok(())
}

#[test]
fn names_colliding_after_mapping_refuse_the_bind_naming_both() -> TestResult {
    let tools = [tool("a.b", object())?, tool("a/b", object())?];
    let error = freeze_listing(&spec()?, &tools, false, &McpToolSelection::All)
        .err()
        .ok_or("collision bound")?;
    assert_eq!(
        error.message(),
        "mcp tools fake/a.b and fake/a/b both expose as mcp__fake__a_b"
    );
    Ok(())
}

#[test]
fn enabled_resources_need_the_capability_and_their_own_name() -> TestResult {
    let tools = [tool("read_resource", object())?, tool("echo", object())?];
    let enabled = spec()?.with_resources(true);
    let missing = freeze_listing(&enabled, &tools[1..], false, &McpToolSelection::All)
        .err()
        .ok_or("capability bound")?;
    assert_eq!(
        missing.message(),
        "mcp server fake has resources enabled but does not advertise the resources capability"
    );
    let collision = freeze_listing(&enabled, &tools, true, &McpToolSelection::All)
        .err()
        .ok_or("collision bound")?;
    assert_eq!(
        collision.message(),
        "mcp tool fake/read_resource and the resources tool both expose as mcp__fake__read_resource"
    );
    let narrowed = freeze_listing(
        &enabled,
        &tools,
        true,
        &McpToolSelection::Named(vec!["echo".into()]),
    )?;
    assert_eq!(narrowed.records().len(), 1);
    let off = freeze_listing(&spec()?, &tools, false, &McpToolSelection::All)?;
    assert_eq!(off.records().len(), 2);
    Ok(())
}

#[test]
fn long_descriptions_are_cut_with_a_marker() -> TestResult {
    let mut long = tool("long", object())?;
    long.description = Some("\u{e9}".repeat(4000).into());
    let frozen = freeze_listing(&spec()?, &[long], false, &McpToolSelection::All)?;
    let description = frozen.records()[0].description();
    assert!(description.len() <= MAX_DESCRIPTION_BYTES);
    assert!(description.ends_with(DESCRIPTION_CUT_MARKER));
    Ok(())
}

#[test]
fn the_tool_digest_ignores_key_order_and_tracks_schema_edits() -> TestResult {
    let server = McpServerName::new("fake")?;
    let first = tool(
        "t",
        json!({"type": "object", "properties": {"a": {"type": "string"}, "b": {"type": "integer"}}}),
    )?;
    let reordered = tool(
        "t",
        json!({"properties": {"b": {"type": "integer"}, "a": {"type": "string"}}, "type": "object"}),
    )?;
    let edited = tool(
        "t",
        json!({"type": "object", "properties": {"a": {"type": "integer"}}}),
    )?;
    assert_eq!(
        listed_tool_digest(&server, &first)?,
        listed_tool_digest(&server, &reordered)?
    );
    assert_ne!(
        listed_tool_digest(&server, &first)?,
        listed_tool_digest(&server, &edited)?
    );
    Ok(())
}
