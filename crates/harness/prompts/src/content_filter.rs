//! One developer fragment for a content-filtered sampling response.

use plakous_tools::PromptPin;

/// Bundled reference-client guidance, unchanged from its model messages.
pub const CONTENT_FILTER_GUIDANCE_TEXT: &str = "Your previous response was blocked by a content filter. Do not treat this as a transient failure or try to reproduce or work around the blocked content through repeated attempts, altered formatting, splitting, encoding, tools, subagents, or later wakes. Briefly explain the limitation and offer a permitted alternative. Continue unrelated authorized work.";

/// Resolves a bounded catalog override and encloses the developer fragment.
#[must_use]
pub fn render(override_text: Option<&str>) -> String {
    let text = override_text
        .filter(|text| !text.trim().is_empty() && text.len() <= 512)
        .unwrap_or(CONTENT_FILTER_GUIDANCE_TEXT);
    format!("<content_filter_guidance>\n{text}\n</content_filter_guidance>")
}

/// Pin for the complete default developer fragment.
pub static CONTENT_FILTER_PROMPT_PINS: &[PromptPin] = &[PromptPin::new(
    "instruction.content_filter_guidance",
    concat!(
        "<content_filter_guidance>\n",
        "Your previous response was blocked by a content filter. Do not treat this as a transient failure or try to reproduce or work around the blocked content through repeated attempts, altered formatting, splitting, encoding, tools, subagents, or later wakes. Briefly explain the limitation and offer a permitted alternative. Continue unrelated authorized work.",
        "\n</content_filter_guidance>"
    ),
    "01178807bab53fd4fed76ff1e0c4829f9c18f029482d6242e2ebedf5b5ba2059",
)];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn override_is_nonblank_and_bounded_in_utf8_bytes() {
        let default = render(None);
        assert_eq!(default, CONTENT_FILTER_PROMPT_PINS[0].text());
        for text in [" ".to_owned(), "x".repeat(513), "\u{e9}".repeat(257)] {
            assert_eq!(render(Some(&text)), default);
        }
        assert!(render(Some(&"x".repeat(512))).contains(&"x".repeat(512)));
        assert_eq!(
            render(Some(" permitted alternative ")),
            "<content_filter_guidance>\n permitted alternative \n</content_filter_guidance>"
        );
    }
}
