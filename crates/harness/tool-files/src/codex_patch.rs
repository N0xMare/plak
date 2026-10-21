//! Codex `*** Begin Patch` envelope parser (pin `c4f2746c43a3ab87c2978657d7f2c274546e97fe`).
//!
//! This is the identity-exact mutate presentation for Codex OAuth. It lowers to
//! the same transactional workspace mutator as git-style `apply_patch`.

use std::path::PathBuf;

use plakous_model::{
    ToolConcurrency, ToolDefinition, ToolEffect, ToolInputContract, ToolName, ToolOutputContract,
    Utf8TextContractV1,
};

use crate::text::PLAKOUS_CODEX_APPLY_PATCH_FREEFORM_DESCRIPTION;
use plakous_tools::{ToolRuntimeError, ToolRuntimeErrorKind};

/// Canonical registry name for Codex OAuth freeform patch envelope input.
pub(crate) const CODEX_PATCH_ENVELOPE_CANONICAL_NAME: &str = "patch_envelope";
/// Registered-tool id for the Codex freeform patch envelope capability.
pub const CODEX_PATCH_ENVELOPE_REGISTERED_TOOL_ID: &str = "plakous.codex.patch-envelope.v1";
/// Maximum raw patch envelope bytes accepted on the Codex OAuth wire.
pub const CODEX_PATCH_ENVELOPE_MAX_BYTES: u32 = 1_048_576;

/// Codex CLI 0.153.x Lark grammar adapted to the accepted Update File forms.
///
/// Adapted from OpenAI Codex (Apache-2.0); see the README License section.
pub const CODEX_APPLY_PATCH_GRAMMAR_V1: &str = "start: begin_patch hunk+ end_patch\nbegin_patch: \"*** Begin Patch\" LF\nend_patch: \"*** End Patch\" LF?\n\nhunk: add_hunk | delete_hunk | update_hunk\nadd_hunk: \"*** Add File: \" filename LF add_line+\ndelete_hunk: \"*** Delete File: \" filename LF\nupdate_hunk: \"*** Update File: \" filename LF (change_move change? | change)\n\nfilename: /(.+)/\nadd_line: \"+\" /(.*)/ LF -> line\n\nchange_move: \"*** Move to: \" filename LF\nchange: (change_context | change_line)+ eof_line?\nchange_context: (\"@@\" | \"@@ \" /(.+)/) LF\nchange_line: (\"+\" | \"-\" | \" \") /(.*)/ LF\neof_line: \"*** End of File\" LF\n\n%import common.LF\n";

/// Builds the Utf8 canonical definition bound to Codex OAuth custom `apply_patch`.
///
/// # Errors
///
/// Returns a definition error when name or contract bounds are invalid.
pub fn codex_patch_envelope_definition() -> Result<ToolDefinition, ToolRuntimeError> {
    ToolDefinition::new(
        ToolName::new(CODEX_PATCH_ENVELOPE_CANONICAL_NAME.to_owned()).map_err(|error| {
            ToolRuntimeError::from_sensitive_source(
                ToolRuntimeErrorKind::InvalidDefinition,
                "Codex patch envelope name is invalid",
                &error,
            )
        })?,
        "codex-patch-envelope-v1",
        PLAKOUS_CODEX_APPLY_PATCH_FREEFORM_DESCRIPTION,
        ToolInputContract::Utf8(
            Utf8TextContractV1::new(CODEX_PATCH_ENVELOPE_MAX_BYTES).map_err(|error| {
                ToolRuntimeError::from_sensitive_source(
                    ToolRuntimeErrorKind::InvalidDefinition,
                    "Codex patch envelope UTF-8 contract is invalid",
                    &error,
                )
            })?,
        ),
        ToolOutputContract::portable_model_content(),
        ToolConcurrency::Serial,
        ToolEffect::Mutating,
    )
    .map_err(|error| {
        ToolRuntimeError::from_sensitive_source(
            ToolRuntimeErrorKind::InvalidDefinition,
            "Codex patch envelope definition is invalid",
            &error,
        )
    })
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum CodexPatchOp {
    Add {
        path: PathBuf,
        contents: String,
    },
    Delete {
        path: PathBuf,
    },
    Update {
        path: PathBuf,
        move_to: Option<PathBuf>,
        chunks: Vec<CodexUpdateChunk>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CodexUpdateChunk {
    change_context: Option<String>,
    old_lines: Vec<String>,
    new_lines: Vec<String>,
    end_of_file: bool,
}

pub(crate) fn looks_like_codex_patch(text: &str) -> bool {
    text.trim_start().starts_with("*** Begin Patch")
}

pub(crate) fn parse_codex_patch(text: &str) -> Result<Vec<CodexPatchOp>, ToolRuntimeError> {
    let normalized = text.replace('\r', "");
    let mut lines = normalized.lines().peekable();
    let begin = lines.next().ok_or_else(|| patch_error("patch is empty"))?;
    if begin.trim() != "*** Begin Patch" {
        return Err(patch_error("patch must start with *** Begin Patch"));
    }
    let mut ops = Vec::new();
    while let Some(line) = lines.peek().copied() {
        let trimmed = line.trim_end();
        if trimmed == "*** End Patch" {
            let _ = lines.next();
            if lines.next().is_some() {
                return Err(patch_error("unexpected content after *** End Patch"));
            }
            if ops.is_empty() {
                return Err(patch_error("patch contains no file changes"));
            }
            return Ok(ops);
        }
        if let Some(path) = trimmed.strip_prefix("*** Add File: ") {
            let _ = lines.next();
            ops.push(parse_add_file(path, &mut lines)?);
        } else if let Some(path) = trimmed.strip_prefix("*** Delete File: ") {
            let _ = lines.next();
            ops.push(CodexPatchOp::Delete {
                path: patch_path(path)?,
            });
        } else if let Some(path) = trimmed.strip_prefix("*** Update File: ") {
            let _ = lines.next();
            ops.push(parse_update_file(path, &mut lines)?);
        } else {
            return Err(patch_error(
                "patch file operation must be *** Add File, *** Update File, or *** Delete File",
            ));
        }
    }
    Err(patch_error("patch is missing *** End Patch"))
}

/// Applies update chunks the way upstream `apply-patch` does: CRLF files keep
/// CRLF, a file without a final newline stays without one, and lines are
/// located with upstream's four increasingly lenient comparisons.
pub(crate) fn apply_codex_update_chunks(
    path: &str,
    original: &str,
    chunks: &[CodexUpdateChunk],
) -> Result<String, ToolRuntimeError> {
    if chunks.is_empty() {
        return Ok(original.to_owned());
    }
    let crlf = original.contains("\r\n");
    let final_newline = original.is_empty() || original.ends_with('\n');
    let mut lines: Vec<String> = original
        .split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line).to_owned())
        .collect();
    if lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }
    let replacements = compute_replacements(path, &lines, chunks)?;
    let mut next = lines;
    for (start, old_len, new_segment) in replacements.into_iter().rev() {
        if start + old_len > next.len() {
            return Err(patch_error_at(path, "patch hunk is out of range", &[]));
        }
        next.splice(start..start + old_len, new_segment);
    }
    let separator = if crlf { "\r\n" } else { "\n" };
    let mut text = next.join(separator);
    if final_newline && !next.is_empty() {
        text.push_str(separator);
    }
    Ok(text)
}

fn parse_add_file(
    path: &str,
    lines: &mut std::iter::Peekable<std::str::Lines<'_>>,
) -> Result<CodexPatchOp, ToolRuntimeError> {
    let mut contents = String::new();
    while let Some(line) = lines.peek().copied() {
        let trimmed = line.trim_end();
        if trimmed.starts_with("*** ") {
            break;
        }
        let _ = lines.next();
        let Some(rest) = line.strip_prefix('+') else {
            return Err(patch_error("add-file lines must start with +"));
        };
        contents.push_str(rest);
        contents.push('\n');
    }
    Ok(CodexPatchOp::Add {
        path: patch_path(path)?,
        contents,
    })
}

fn parse_update_file(
    path: &str,
    lines: &mut std::iter::Peekable<std::str::Lines<'_>>,
) -> Result<CodexPatchOp, ToolRuntimeError> {
    let move_to = if let Some(path) = lines
        .peek()
        .and_then(|line| line.strip_prefix("*** Move to: "))
    {
        let path = patch_path(path)?;
        let _ = lines.next();
        Some(path)
    } else {
        None
    };
    let mut chunks = Vec::new();
    while let Some(line) = lines.peek().copied() {
        let trimmed = line.trim_end();
        if trimmed.starts_with("*** ") && !trimmed.starts_with("*** End of File") {
            break;
        }
        if trimmed.starts_with("@@") {
            let _ = lines.next();
            let change_context = trimmed
                .strip_prefix("@@")
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_owned);
            chunks.push(parse_update_chunk(change_context, lines)?);
            continue;
        }
        // Upstream lets the first hunk of a file omit its `@@` marker.
        if chunks.is_empty() {
            chunks.push(parse_update_chunk(None, lines)?);
            continue;
        }
        return Err(patch_error("update hunks must start with @@"));
    }
    if chunks.is_empty() && move_to.is_none() {
        return Err(patch_error(
            "update file is missing hunks or a move destination",
        ));
    }
    Ok(CodexPatchOp::Update {
        path: patch_path(path)?,
        move_to,
        chunks,
    })
}

fn parse_update_chunk(
    change_context: Option<String>,
    lines: &mut std::iter::Peekable<std::str::Lines<'_>>,
) -> Result<CodexUpdateChunk, ToolRuntimeError> {
    let mut old_lines = Vec::new();
    let mut new_lines = Vec::new();
    let mut end_of_file = false;
    while let Some(line) = lines.peek().copied() {
        let trimmed = line.trim_end();
        if trimmed == "*** End of File" {
            let _ = lines.next();
            end_of_file = true;
            break;
        }
        if trimmed.starts_with("@@") || trimmed.starts_with("*** ") {
            break;
        }
        let _ = lines.next();
        if let Some(rest) = line.strip_prefix('+') {
            new_lines.push(rest.to_owned());
        } else if let Some(rest) = line.strip_prefix('-') {
            old_lines.push(rest.to_owned());
        } else if let Some(rest) = line.strip_prefix(' ') {
            old_lines.push(rest.to_owned());
            new_lines.push(rest.to_owned());
        } else if line.is_empty() {
            // A bare empty line is an empty context line, as upstream reads it.
            old_lines.push(String::new());
            new_lines.push(String::new());
        } else {
            return Err(patch_error("hunk lines must start with space, -, or +"));
        }
    }
    Ok(CodexUpdateChunk {
        change_context,
        old_lines,
        new_lines,
        end_of_file,
    })
}

fn compute_replacements(
    path: &str,
    original: &[String],
    chunks: &[CodexUpdateChunk],
) -> Result<Vec<(usize, usize, Vec<String>)>, ToolRuntimeError> {
    let mut replacements = Vec::new();
    let mut line_index = 0;
    for chunk in chunks {
        if let Some(context) = chunk.change_context.as_deref() {
            let found = seek_sequence(original, &[context.to_owned()], line_index, false)
                .ok_or_else(|| {
                    patch_error_at(path, "failed to find @@ context", &[context.to_owned()])
                })?;
            line_index = found + 1;
        }
        if chunk.old_lines.is_empty() {
            // A pure insert after an `@@` anchor lands after the anchor; with
            // no anchor it appends, as upstream does.
            let insertion = if chunk.change_context.is_some() && !chunk.end_of_file {
                line_index.min(original.len())
            } else {
                original.len()
            };
            replacements.push((insertion, 0, chunk.new_lines.clone()));
            continue;
        }
        let mut pattern: &[String] = &chunk.old_lines;
        let mut new_lines: &[String] = &chunk.new_lines;
        let mut found = seek_sequence(original, pattern, line_index, chunk.end_of_file);
        // A trailing empty old line stands for the file's final newline, which
        // the split lines do not carry; retry without it.
        if found.is_none() && pattern.last().is_some_and(String::is_empty) {
            pattern = &pattern[..pattern.len() - 1];
            if new_lines.last().is_some_and(String::is_empty) {
                new_lines = &new_lines[..new_lines.len() - 1];
            }
            found = seek_sequence(original, pattern, line_index, chunk.end_of_file);
        }
        let found = found.ok_or_else(|| {
            patch_error_at(path, "failed to find the expected lines", &chunk.old_lines)
        })?;
        replacements.push((found, pattern.len(), new_lines.to_vec()));
        line_index = found + pattern.len();
    }
    replacements.sort_by_key(|(index, _, _)| *index);
    Ok(replacements)
}

/// Upstream `seek_sequence`: exact, then trailing-whitespace-insensitive, then
/// whitespace-trimmed, then with typographic punctuation and spaces folded to
/// ASCII. An end-of-file chunk is tried at the end first.
fn seek_sequence(lines: &[String], pattern: &[String], start: usize, eof: bool) -> Option<usize> {
    if pattern.is_empty() {
        return Some(start);
    }
    if pattern.len() > lines.len() {
        return None;
    }
    let search_start = if eof {
        lines.len() - pattern.len()
    } else {
        start
    };
    let last = lines.len() - pattern.len();
    let comparisons: [fn(&str, &str) -> bool; 4] = [
        |left, right| left == right,
        |left, right| left.trim_end() == right.trim_end(),
        |left, right| left.trim() == right.trim(),
        |left, right| normalise_punctuation(left) == normalise_punctuation(right),
    ];
    for equal in comparisons {
        for index in search_start..=last {
            if pattern
                .iter()
                .enumerate()
                .all(|(offset, expected)| equal(&lines[index + offset], expected))
            {
                return Some(index);
            }
        }
    }
    if eof && start < search_start {
        return seek_sequence(lines, pattern, start, false);
    }
    None
}

fn normalise_punctuation(text: &str) -> String {
    text.trim()
        .chars()
        .map(|character| match character {
            '\u{2010}' | '\u{2011}' | '\u{2012}' | '\u{2013}' | '\u{2014}' | '\u{2015}'
            | '\u{2212}' => '-',
            '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' => '\'',
            '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{201F}' => '"',
            '\u{00A0}' | '\u{2002}' | '\u{2003}' | '\u{2004}' | '\u{2005}' | '\u{2006}'
            | '\u{2007}' | '\u{2008}' | '\u{2009}' | '\u{200A}' | '\u{202F}' | '\u{205F}'
            | '\u{3000}' => ' ',
            other => other,
        })
        .collect()
}

fn patch_path(path: &str) -> Result<PathBuf, ToolRuntimeError> {
    let path = path.trim();
    if path.is_empty() || path.contains('\0') {
        return Err(patch_error("patch path must be a relative workspace path"));
    }
    Ok(PathBuf::from(path))
}

fn patch_error(message: &'static str) -> ToolRuntimeError {
    ToolRuntimeError::new(ToolRuntimeErrorKind::InvalidArguments, message)
}

/// A hunk failure that names the file and the lines it looked for, bounded so
/// a large hunk cannot flood the observation.
fn patch_error_at(path: &str, message: &str, expected: &[String]) -> ToolRuntimeError {
    const BOUND: usize = 2_048;
    let mut text = format!("{message} in {path}");
    if !expected.is_empty() {
        text.push_str(":\n");
        text.push_str(&expected.join("\n"));
    }
    if text.len() > BOUND {
        let mut end = BOUND;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
        text.push_str("\n[...]");
    }
    ToolRuntimeError::new(ToolRuntimeErrorKind::InvalidArguments, text)
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "unit tests may fail fast on invalid fixtures"
)]
mod tests {
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
        let ops = parse_codex_patch(
            "*** Begin Patch\n*** Add File: /app/hello.txt\n+Hello\n*** End Patch\n",
        )
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
            )).expect("move");
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
}
