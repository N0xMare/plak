//! Grok Direct `read_file`: the file text, then one metadata line.
//!
//! Nested cells keep the neutral JSON string (`RawUtf8V1`) and do not use this.

use crate::{ToolRuntimeError, ToolRuntimeErrorKind};

/// The Direct result string: verbatim selected text, then one metadata line.
///
/// `offset=<n> lines=<n> eof=<bool> next_offset=<n|null>`, then
/// `clipped_lines=<n>`, `lossy=true`, `workspace_generation=<n>` and
/// `final_newline=false`, each only when set, in that order. When the text is
/// non-empty and does not end in a newline, one separator newline precedes the
/// metadata line and `final_newline=false` says so, so dropping the metadata
/// line (and that separator) recovers the selected text exactly. There are no
/// line numbers.
pub(super) fn grok_text_from_canonical_read(canonical: &str) -> Result<String, ToolRuntimeError> {
    let read: crate::WorkspaceFileReadResult =
        serde_json::from_str(canonical).map_err(|error| {
            ToolRuntimeError::from_sensitive_source(
                ToolRuntimeErrorKind::Execution,
                "workspace read observation is not a neutral read result",
                &error,
            )
        })?;
    let mut text = read.content;
    let unterminated = !text.is_empty() && !text.ends_with('\n');
    if unterminated {
        text.push('\n');
    }
    let next = match read.next_offset {
        Some(next) => next.to_string(),
        None => "null".to_owned(),
    };
    let clipped =
        (read.clipped_lines != 0).then(|| format!(" clipped_lines={}", read.clipped_lines));
    let generation = read
        .workspace_generation
        .map(|generation| format!(" workspace_generation={generation}"));
    let line = format!(
        "offset={} lines={} eof={} next_offset={next}{}{}{}{}",
        read.offset,
        read.lines,
        read.eof,
        clipped.as_deref().unwrap_or(""),
        if read.lossy { " lossy=true" } else { "" },
        generation.as_deref().unwrap_or(""),
        if unterminated {
            " final_newline=false"
        } else {
            ""
        },
    );
    text.push_str(&line);
    text.push('\n');
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_keeps_content_and_one_metadata_line() -> Result<(), ToolRuntimeError> {
        let canonical =
            r#"{"content":"hello\n","offset":1,"lines":1,"eof":true,"next_offset":null}"#;
        let text = grok_text_from_canonical_read(canonical)?;
        assert_eq!(text, "hello\noffset=1 lines=1 eof=true next_offset=null\n");
        assert!(!text.contains('\t'));
        Ok(())
    }

    #[test]
    fn markers_appear_only_when_set_and_a_bare_line_stays_verbatim() -> Result<(), ToolRuntimeError>
    {
        let canonical = r#"{"content":"ab","offset":4,"lines":1,"eof":false,"next_offset":5,"lossy":true,"clipped_lines":2,"workspace_generation":9}"#;
        let text = grok_text_from_canonical_read(canonical)?;
        assert_eq!(
            text,
            "ab\noffset=4 lines=1 eof=false next_offset=5 clipped_lines=2 lossy=true workspace_generation=9 final_newline=false\n"
        );
        Ok(())
    }

    /// Drops the metadata line and, when it says so, the separator before it.
    fn selected_text(raised: &str) -> Result<&str, ToolRuntimeError> {
        let invalid =
            || ToolRuntimeError::new(ToolRuntimeErrorKind::Execution, "not a raised read");
        let body = raised.strip_suffix('\n').ok_or_else(invalid)?;
        let start = body.rfind('\n').map_or(0, |index| index + 1);
        let (text, metadata) = body.split_at(start);
        if metadata
            .split(' ')
            .any(|field| field == "final_newline=false")
        {
            return text.strip_suffix('\n').ok_or_else(invalid);
        }
        Ok(text)
    }

    #[test]
    fn the_selected_text_and_its_final_newline_survive_raising() -> Result<(), ToolRuntimeError> {
        for content in ["", "a\nb", "a\nb\n", "a\r\nb\r\n", "a\r\nb", "\n", "a\r"] {
            let canonical = serde_json::json!({
                "content": content, "offset": 1, "lines": 2, "eof": true, "next_offset": null
            })
            .to_string();
            let raised = grok_text_from_canonical_read(&canonical)?;
            assert_eq!(selected_text(&raised)?, content, "{raised:?}");
            assert_eq!(
                raised.contains("final_newline=false"),
                !content.is_empty() && !content.ends_with('\n'),
                "{raised:?}"
            );
        }
        let nonl = grok_text_from_canonical_read(
            r#"{"content":"a\r\nb","offset":1,"lines":2,"eof":true,"next_offset":null}"#,
        )?;
        assert_eq!(
            nonl,
            "a\r\nb\noffset=1 lines=2 eof=true next_offset=null final_newline=false\n"
        );
        let crlf = grok_text_from_canonical_read(
            r#"{"content":"a\r\nb\r\n","offset":1,"lines":2,"eof":true,"next_offset":null}"#,
        )?;
        assert_eq!(
            crlf,
            "a\r\nb\r\noffset=1 lines=2 eof=true next_offset=null\n"
        );
        Ok(())
    }

    #[test]
    fn an_empty_file_is_only_the_metadata_line() -> Result<(), ToolRuntimeError> {
        let canonical = r#"{"content":"","offset":1,"lines":0,"eof":true,"next_offset":null}"#;
        let text = grok_text_from_canonical_read(canonical)?;
        assert_eq!(text, "offset=1 lines=0 eof=true next_offset=null\n");
        Ok(())
    }
}
