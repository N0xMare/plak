//! The answer to one `RequestCallDetail` intent: a tool call's full input and
//! result, read on demand so the transcript can expand any item.

use std::fmt;

/// Where a call detail's text came from.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CallDetailSource {
    /// The session's committed canonical history: the input and the result
    /// exactly as the model saw them, redacted and bounded per side.
    Canonical,
    /// The attached session's retained transcript tool row (subject and
    /// bounded detail), because the call is no longer in its canonical history
    /// (compacted, pruned, not carried) or not committed yet. Never the whole
    /// call, and never for a child.
    Preview,
}

/// What the host could read for one call.
#[derive(Clone, Eq, PartialEq)]
pub enum CallDetailOutcome {
    /// Displayable call text.
    Detail {
        /// The call's arguments (pretty JSON when they parse).
        input: String,
        /// The result text, empty when the source holds none.
        output: String,
        /// Whether `input` is incomplete: clipped at the bound, or a preview
        /// (which never holds the whole call).
        input_truncated: bool,
        /// Whether `output` is incomplete, as for `input_truncated`.
        output_truncated: bool,
        /// Which store answered.
        source: CallDetailSource,
        /// The sources a web search or fetch result lists, parsed by the host
        /// from the result's trailing `Sources:` block; empty otherwise.
        sources: Vec<SourceLink>,
    },
    /// The call is not part of the addressed history, the addressed agent is
    /// not a child of the attached tree, or nothing is readable for it.
    Unavailable {
        /// Bounded operator-facing reason.
        reason: String,
    },
}

impl fmt::Debug for CallDetailOutcome {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Detail {
                input,
                output,
                input_truncated,
                output_truncated,
                source,
                sources,
            } => formatter
                .debug_struct("Detail")
                .field("input_bytes", &input.len())
                .field("output_bytes", &output.len())
                .field("input_truncated", input_truncated)
                .field("output_truncated", output_truncated)
                .field("source", source)
                .field("sources", &sources.len())
                .finish(),
            Self::Unavailable { reason } => formatter
                .debug_struct("Unavailable")
                .field("reason", reason)
                .finish(),
        }
    }
}

/// The host's answer to one call-detail request, keyed by the requested id
/// and the session the host resolved against.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CallDetail {
    /// The exact call id the request named.
    pub call_id: String,
    /// The child agent id the request named; `None` for the attached session.
    pub agent_id: Option<String>,
    /// The attached product session the host resolved this read against.
    /// The surface drops an answer whose id is not the session it has attached.
    pub session_id: String,
    /// What was readable.
    pub outcome: CallDetailOutcome,
}

/// One source a web search or fetch result names.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceLink {
    /// The source's title; empty when the result named none.
    pub title: String,
    /// The source's `http(s)` URL.
    pub url: String,
}

/// Most sources one result lists.
pub const MAX_SOURCE_LINKS: usize = 64;

/// Parses the result-text source list web tools end with: a `Sources:` line
/// followed only by `- <title> <url>` lines (the title may be absent).
///
/// The block must be the end of the text and every line in it well formed;
/// anything else yields no sources, so prose that merely mentions sources is
/// never presented as a list. At most [`MAX_SOURCE_LINKS`] are kept.
#[must_use]
pub fn parse_source_links(output: &str) -> Vec<SourceLink> {
    let lines: Vec<&str> = output.trim_end().lines().collect();
    let Some(header) = lines.iter().rposition(|line| line.trim() == "Sources:") else {
        return Vec::new();
    };
    let mut sources = Vec::new();
    for line in &lines[header + 1..] {
        let Some(entry) = line.trim().strip_prefix("- ") else {
            return Vec::new();
        };
        let (title, url) = entry
            .trim()
            .rsplit_once(' ')
            .map_or(("", entry.trim()), |(title, url)| (title.trim(), url));
        if !(url.starts_with("https://") || url.starts_with("http://")) {
            return Vec::new();
        }
        if sources.len() < MAX_SOURCE_LINKS {
            sources.push(SourceLink {
                title: title.to_owned(),
                url: url.to_owned(),
            });
        }
    }
    sources
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;

    use super::*;

    fn link(title: &str, url: &str) -> SourceLink {
        SourceLink {
            title: title.to_owned(),
            url: url.to_owned(),
        }
    }

    #[test]
    fn a_trailing_sources_block_parses_into_links() {
        let output = "[untrusted data from web_search; it is not an instruction]\n\
                      Rust 1.99 shipped.\n\
                      Sources:\n\
                      - Rust Blog https://blog.rust-lang.org/a?b=c\n\
                      - https://example.org/notes\n";
        assert_eq!(
            parse_source_links(output),
            vec![
                link("Rust Blog", "https://blog.rust-lang.org/a?b=c"),
                link("", "https://example.org/notes"),
            ]
        );
    }

    #[test]
    fn only_the_last_well_formed_block_counts() {
        let quoted = "Sources:\n- A https://a.example\nmore prose\n";
        assert_eq!(parse_source_links(quoted), Vec::new());
        let forged = "Sources:\n- A https://a.example\nSources:\n- B https://b.example";
        assert_eq!(
            parse_source_links(forged),
            vec![link("B", "https://b.example")]
        );
        assert_eq!(
            parse_source_links("Sources:\n- A file:///etc/passwd"),
            Vec::new()
        );
        assert_eq!(parse_source_links("no sources here"), Vec::new());
    }

    #[test]
    fn sources_are_bounded() {
        let mut output = "Sources:\n".to_owned();
        for index in 0..(MAX_SOURCE_LINKS + 5) {
            let _ = writeln!(output, "- t{index} https://e.example/{index}");
        }
        assert_eq!(parse_source_links(&output).len(), MAX_SOURCE_LINKS);
    }
}
