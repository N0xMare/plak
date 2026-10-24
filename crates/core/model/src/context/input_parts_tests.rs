use super::ContextInputParts;

#[test]
fn responses_body_splits_instructions_tools_input_and_reasoning_items() {
    let body = br#"{"model":"m","instructions":"be brief","tools":[{"type":"function","name":"a"},{"type":"function","name":"b"}],"input":[{"role":"user","content":[{"type":"input_text","text":"hi"}]},{"type":"reasoning","encrypted_content":"xxxxxxxx"},{"type":"compaction","encrypted_content":"yyyy"}],"include":["reasoning.encrypted_content"]}"#;
    let parts = ContextInputParts::measure_wire(body).expect("object body");
    assert_eq!(parts.total_bytes(), body.len() as u64);
    assert_eq!(parts.system_bytes(), r#""be brief""#.len() as u64);
    assert_eq!(parts.tool_definition_count(), 2);
    assert_eq!(
        parts.tool_definition_bytes(),
        r#"[{"type":"function","name":"a"},{"type":"function","name":"b"}]"#.len() as u64
    );
    assert_eq!(
        parts.message_bytes(),
        r#"{"role":"user","content":[{"type":"input_text","text":"hi"}]}"#.len() as u64
    );
    assert_eq!(
        parts.reasoning_bytes(),
        (r#"{"type":"reasoning","encrypted_content":"xxxxxxxx"}"#.len()
            + r#"{"type":"compaction","encrypted_content":"yyyy"}"#.len()) as u64
    );
    let attributed = parts.system_bytes()
        + parts.tool_definition_bytes()
        + parts.message_bytes()
        + parts.reasoning_bytes();
    assert!(
        attributed < parts.total_bytes(),
        "envelope keys stay unattributed"
    );
    assert_eq!(
        parts.occupancy_tokens(),
        parts.system_bytes() / 4
            + parts.tool_definition_bytes() / 4
            + parts.message_bytes() / 4
            + parts.reasoning_bytes() / 4
            + (parts.total_bytes() - attributed) / 4
    );
}

#[test]
fn messages_body_moves_thinking_blocks_out_of_the_message_share() {
    let body = br#"{"model":"m","system":[{"type":"text","text":"sys"}],"tools":[{"name":"t"}],"messages":[{"role":"user","content":"q"},{"role":"assistant","content":[{"type":"thinking","thinking":"hmm","signature":"s"},{"type":"text","text":"a"}]}],"thinking":{"type":"adaptive"}}"#;
    let parts = ContextInputParts::measure_wire(body).expect("object body");
    assert_eq!(
        parts.system_bytes(),
        r#"[{"type":"text","text":"sys"}]"#.len() as u64
    );
    assert_eq!(parts.tool_definition_count(), 1);
    let thinking = r#"{"type":"thinking","thinking":"hmm","signature":"s"}"#.len() as u64;
    assert_eq!(parts.reasoning_bytes(), thinking);
    let assistant = r#"{"role":"assistant","content":[{"type":"thinking","thinking":"hmm","signature":"s"},{"type":"text","text":"a"}]}"#.len() as u64;
    let user = r#"{"role":"user","content":"q"}"#.len() as u64;
    assert_eq!(parts.message_bytes(), user + assistant - thinking);
}

#[test]
fn chat_body_counts_system_role_messages_as_system_and_omits_nothing_twice() {
    let body = br#"{"model":"m","messages":[{"role":"system","content":"sys"},{"role":"user","content":"q"},{"role":"assistant","content":"a","reasoning":"why"}],"tools":[]}"#;
    let parts = ContextInputParts::measure_wire(body).expect("object body");
    assert_eq!(
        parts.system_bytes(),
        r#"{"role":"system","content":"sys"}"#.len() as u64
    );
    assert_eq!(parts.tool_definition_count(), 0);
    assert_eq!(parts.tool_definition_bytes(), 2);
    assert_eq!(parts.reasoning_bytes(), r#""why""#.len() as u64);
    let assistant = r#"{"role":"assistant","content":"a","reasoning":"why"}"#.len() as u64;
    let user = r#"{"role":"user","content":"q"}"#.len() as u64;
    assert_eq!(
        parts.message_bytes(),
        user + assistant - r#""why""#.len() as u64
    );
}

#[test]
fn a_non_object_body_is_unmeasured_rather_than_a_zero_split() {
    assert!(ContextInputParts::measure_wire(b"[1,2]").is_none());
    assert!(ContextInputParts::measure_wire(b"not json").is_none());
}
