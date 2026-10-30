use super::*;

#[test]
fn parses_add_update_and_delete() {
    let patch = "*** Begin Patch\n*** Add File: hello.txt\n+Hello\n*** Update File: src/app.py\n@@ def greet():\n-print(\"Hi\")\n+print(\"Hello\")\n*** Delete File: obsolete.txt\n*** End Patch\n";
    let ops = parse_codex_patch(patch).expect("parse");
    assert_eq!(ops.len(), 3);
    match &ops[0] {
        CodexPatchOp::Add { path, contents } => {
            assert_eq!(path, &PathBuf::from("hello.txt"));
            assert_eq!(contents, "Hello\n");
        }
        _ => panic!("add"),
    }
    match &ops[2] {
        CodexPatchOp::Delete { path } => assert_eq!(path, &PathBuf::from("obsolete.txt")),
        _ => panic!("delete"),
    }
}

#[test]
fn applies_context_hunk() {
    let original = "def greet():\nprint(\"Hi\")\n";
    let ops = parse_codex_patch(
        "*** Begin Patch\n*** Update File: src/app.py\n@@ def greet():\n-print(\"Hi\")\n+print(\"Hello\")\n*** End Patch\n",
    )
    .expect("parse");
    let CodexPatchOp::Update { chunks, .. } = &ops[0] else {
        panic!("update");
    };
    let next = apply_codex_update_chunks("src/app.py", original, chunks).expect("apply");
    assert_eq!(next, "def greet():\nprint(\"Hello\")\n");
}

#[test]
fn accepts_workspace_absolute_add_path_for_execute_lowering() {
    let ops =
        parse_codex_patch("*** Begin Patch\n*** Add File: /app/hello.txt\n+Hello\n*** End Patch\n")
            .expect("parse");
    match &ops[0] {
        CodexPatchOp::Add { path, contents } => {
            assert_eq!(path, &PathBuf::from("/app/hello.txt"));
            assert_eq!(contents, "Hello\n");
        }
        _ => panic!("add"),
    }
}

fn apply(original: &str, patch: &str) -> Result<String, ToolRuntimeError> {
    let ops = parse_codex_patch(patch)?;
    let CodexPatchOp::Update { chunks, .. } = &ops[0] else {
        panic!("update");
    };
    apply_codex_update_chunks("f.py", original, chunks)
}

#[test]
fn whitespace_and_punctuation_differences_still_locate_the_lines() {
    let original = "def f():\n    x = 1   \n    return x  # \u{2014} done\n";
    let next = apply(
        original,
        "*** Begin Patch\n*** Update File: f.py\n@@ def f():\n-    x = 1\n+    x = 2\n     return x  # - done\n*** End Patch\n",
    )
    .expect("apply");
    assert_eq!(next, "def f():\n    x = 2\n    return x  # - done\n");
}

#[test]
fn a_blank_line_is_context_and_the_first_hunk_may_omit_its_marker() {
    let original = "a\n\nb\n";
    let next = apply(
        original,
        "*** Begin Patch\n*** Update File: f.py\n a\n\n-b\n+c\n*** End Patch\n",
    )
    .expect("apply");
    assert_eq!(next, "a\n\nc\n");
}

#[test]
fn crlf_and_a_missing_final_newline_are_preserved() {
    let next = apply(
        "one\r\ntwo\r\n",
        "*** Begin Patch\n*** Update File: f.py\n@@\n-two\n+2\n*** End Patch\n",
    )
    .expect("apply");
    assert_eq!(next, "one\r\n2\r\n");
    let next = apply(
        "one\ntwo",
        "*** Begin Patch\n*** Update File: f.py\n@@\n-two\n+2\n*** End Patch\n",
    )
    .expect("apply");
    assert_eq!(next, "one\n2");
}

#[test]
fn an_anchorless_insert_appends_and_an_anchored_one_follows_its_anchor() {
    let next = apply(
        "a\nb\n",
        "*** Begin Patch\n*** Update File: f.py\n@@\n+z\n*** End Patch\n",
    )
    .expect("apply");
    assert_eq!(next, "a\nb\nz\n");
    let next = apply(
        "a\nb\n",
        "*** Begin Patch\n*** Update File: f.py\n@@ a\n+z\n*** End Patch\n",
    )
    .expect("apply");
    assert_eq!(next, "a\nz\nb\n");
}

#[test]
fn a_missing_hunk_names_the_file_and_the_expected_lines() {
    let error = apply(
        "a\n",
        "*** Begin Patch\n*** Update File: f.py\n@@\n-nope\n+yes\n*** End Patch\n",
    )
    .expect_err("missing");
    assert_eq!(
        error.message().as_str(),
        "failed to find the expected lines in f.py:\nnope"
    );
}

#[test]
fn parses_move_with_and_without_changes() {
    assert_eq!(
        apply_codex_update_chunks("mixed", "a\r\nb\n", &[]).expect("unchanged"),
        "a\r\nb\n"
    );
    for changes in ["", "@@\n-a\n+b\n"] {
        let ops = parse_codex_patch(&format!(
            "*** Begin Patch\n*** Update File: a.txt\n*** Move to: b.txt\n{changes}*** End Patch\n"
        ))
        .expect("move");
        let CodexPatchOp::Update {
            move_to, chunks, ..
        } = &ops[0]
        else {
            panic!("update")
        };
        assert_eq!(move_to.as_deref(), Some(std::path::Path::new("b.txt")));
        assert_eq!(
            apply_codex_update_chunks("a.txt", "a\n", chunks).expect("apply"),
            if changes.is_empty() { "a\n" } else { "b\n" }
        );
    }
}
