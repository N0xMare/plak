use crate::ContentItem;

const BYTES_PER_TOKEN: usize = 4;

/// Applies the exact Codex 0.153.3 Code Mode result truncation policy.
#[must_use]
pub fn truncate_content_items(items: Vec<ContentItem>, max_tokens: usize) -> Vec<ContentItem> {
    if items
        .iter()
        .all(|item| matches!(item, ContentItem::InputText { .. }))
    {
        let mut combined = String::new();
        for text in items.iter().filter_map(|item| match item {
            ContentItem::InputText { text } => Some(text),
            _ => None,
        }) {
            if !combined.is_empty() {
                combined.push('\n');
            }
            combined.push_str(text);
        }
        if combined.len() <= max_tokens.saturating_mul(BYTES_PER_TOKEN) {
            return items;
        }
        let original_token_count = approximate_tokens(&combined);
        let total_lines = combined.lines().count();
        let truncated = truncate_middle(&combined, max_tokens);
        return vec![ContentItem::InputText {
            text: format!(
                "Warning: truncated output (original token count: {original_token_count})\nTotal output lines: {total_lines}\n\n{truncated}"
            ),
        }];
    }

    let mut remaining = max_tokens;
    let mut output = Vec::with_capacity(items.len());
    let mut omitted_text = 0_usize;
    let mut omitted_audio = 0_usize;
    for item in items {
        match item {
            ContentItem::InputText { text } => {
                // Empty text contributes no model content but still consumes an API array slot.
                if text.is_empty() {
                    continue;
                }
                if remaining == 0 {
                    omitted_text = omitted_text.saturating_add(1);
                    continue;
                }
                let cost = approximate_tokens(&text);
                if cost <= remaining {
                    output.push(ContentItem::InputText { text });
                    remaining = remaining.saturating_sub(cost);
                } else {
                    let text = truncate_middle(&text, remaining);
                    if text.is_empty() {
                        omitted_text = omitted_text.saturating_add(1);
                    } else {
                        output.push(ContentItem::InputText { text });
                    }
                    remaining = 0;
                }
            }
            image @ ContentItem::InputImage { .. } => output.push(image),
            ContentItem::InputAudio { audio_url } => {
                let cost = estimate_audio_tokens(&audio_url);
                if cost <= remaining {
                    output.push(ContentItem::InputAudio { audio_url });
                    remaining = remaining.saturating_sub(cost);
                } else {
                    omitted_audio = omitted_audio.saturating_add(1);
                }
            }
        }
    }
    if omitted_text > 0 {
        output.push(ContentItem::InputText {
            text: format!("[omitted {omitted_text} text items ...]"),
        });
    }
    if omitted_audio > 0 {
        output.push(ContentItem::InputText {
            text: format!("[omitted {omitted_audio} audio items ...]"),
        });
    }
    output
}

fn approximate_tokens(text: &str) -> usize {
    text.len().saturating_add(BYTES_PER_TOKEN - 1) / BYTES_PER_TOKEN
}

fn truncate_middle(text: &str, max_tokens: usize) -> String {
    if text.is_empty() {
        return String::new();
    }
    let max_bytes = max_tokens.saturating_mul(BYTES_PER_TOKEN);
    if max_tokens > 0 && text.len() <= max_bytes {
        return text.to_owned();
    }
    if max_bytes == 0 {
        return format!("…{} tokens truncated…", approximate_tokens(text));
    }
    let (left_budget, right_budget) = (max_bytes / 2, max_bytes - max_bytes / 2);
    let mut left = 0;
    let mut right = text.len();
    let mut suffix_started = false;
    for (index, character) in text.char_indices() {
        let end = index + character.len_utf8();
        if end <= left_budget {
            left = end;
            continue;
        }
        if index >= text.len().saturating_sub(right_budget) && !suffix_started {
            right = index;
            suffix_started = true;
        }
    }
    if right < left {
        right = left;
    }
    let removed = text
        .len()
        .saturating_sub(max_bytes)
        .saturating_add(BYTES_PER_TOKEN - 1)
        / BYTES_PER_TOKEN;
    format!(
        "{}…{removed} tokens truncated…{}",
        &text[..left],
        &text[right..]
    )
}

/// Truncation cost of one audio item, in tokens.
///
/// Audio is charged by the size of its data URL, the same `BYTES_PER_TOKEN`
/// rule every other item uses. Codex 0.153.3 charged decodable audio by
/// duration instead (10 tokens per second) and fell back to this rule for
/// everything it could not decode; that needed a full audio-codec dependency
/// (`symphonia`, 22 crates) for one measurement, so the fallback is now the
/// only rule. It is a local truncation budget, not a billed count: the effect
/// is that a long audio item is more likely to be omitted with the
/// `[omitted N audio items ...]` marker than it was before.
fn estimate_audio_tokens(audio_url: &str) -> usize {
    approximate_tokens(audio_url)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn multibyte_marker_uses_release_byte_budget_accounting() {
        let result = truncate_content_items(
            vec![ContentItem::InputText {
                text: "ééééé".to_owned(),
            }],
            1,
        );
        let ContentItem::InputText { text } = &result[0] else {
            panic!("text")
        };
        assert!(text.contains("…2 tokens truncated…"));
    }

    #[test]
    fn audio_is_charged_by_data_url_size_and_omitted_when_it_does_not_fit() {
        let url = format!("data:audio/wav;base64,{}", "A".repeat(4_000));
        let cost = url.len().div_ceil(4);
        assert!(cost > 10, "the fixture is bigger than the budget below");

        // Inside the budget the item survives untouched.
        let kept = truncate_content_items(
            vec![ContentItem::InputAudio {
                audio_url: url.clone(),
            }],
            cost,
        );
        assert!(matches!(&kept[..], [ContentItem::InputAudio { .. }]));

        // Over the budget it is omitted with its marker, never truncated.
        let dropped = truncate_content_items(vec![ContentItem::InputAudio { audio_url: url }], 10);
        let [ContentItem::InputText { text }] = &dropped[..] else {
            panic!("expected only the omission marker, got {dropped:?}");
        };
        assert_eq!(text, "[omitted 1 audio items ...]");
    }

    #[test]
    fn mixed_path_skips_empty_text_items_without_omission() {
        let result = truncate_content_items(
            vec![
                ContentItem::InputText {
                    text: String::new(),
                },
                ContentItem::InputImage {
                    image_url: "data:image/png;base64,AA==".to_owned(),
                    detail: None,
                },
                ContentItem::InputText {
                    text: "kept".to_owned(),
                },
            ],
            10,
        );
        assert_eq!(result.len(), 2);
        assert!(matches!(&result[0], ContentItem::InputImage { .. }));
        let ContentItem::InputText { text } = &result[1] else {
            panic!("expected remaining text item");
        };
        assert_eq!(text, "kept");
    }
}
