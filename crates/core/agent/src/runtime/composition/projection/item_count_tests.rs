use super::{LocalCompositionContentItem as Item, fit_item_count, fit_response_parts};
use plakous_model::{TextBlock, ToolResultPart};
use plakous_tools::{MAX_TOOL_OUTCOME_BYTES, ToolOutcome};

fn text(value: &str) -> Item {
    Item::Text(value.to_owned())
}

fn image(index: usize) -> Item {
    Item::Image {
        url: format!("data:image/png;base64,{index}"),
        detail: None,
    }
}

#[test]
fn refusal_keeps_the_running_cell_id_and_presentation_failure_reason()
-> Result<(), Box<dyn std::error::Error>> {
    let response = super::LocalCompositionResponse::Yielded {
        cell_id: "C".to_owned(),
        items: Vec::new(),
    };
    let error = super::AgentError::new(super::AgentErrorKind::Tool, "presentation refused");
    let parts = super::local_composition_refusal_parts(
        &response,
        std::time::Duration::ZERO,
        super::LocalCompositionResultProjection::TokensScriptV1,
        &error,
    )?;
    let outcome = ToolOutcome::success(parts)?;
    let [ToolResultPart::Text(header), ToolResultPart::Text(notice)] = outcome.parts() else {
        panic!("header and notice")
    };
    assert!(header.text().starts_with("Script running with cell ID C"));
    assert_eq!(
        notice.text(),
        "Warning: cell output omitted: presentation refused"
    );
    Ok(())
}

#[test]
fn output_within_the_bound_keeps_one_item_per_text_call() {
    let items = vec![text("a"), text(""), text("b"), image(0), text("c")];
    assert_eq!(
        fit_item_count(items, 4),
        (vec![text("a"), text("b"), image(0), text("c")], 0)
    );
}

#[test]
fn output_over_the_bound_joins_adjacent_text_around_media() {
    let items = vec![
        text("a"),
        text("b"),
        image(0),
        text("c"),
        text(""),
        text("d"),
    ];
    assert_eq!(
        fit_item_count(items, 3),
        (vec![text("a\nb"), image(0), text("c\nd")], 0)
    );
}

#[test]
fn media_beyond_the_bound_give_way_to_one_counted_notice() {
    let items = (0..6).map(image).collect::<Vec<_>>();
    let (fitted, omitted) = fit_item_count(items, 4);
    assert_eq!(fitted, vec![image(0), image(1), image(2)]);
    assert_eq!(omitted, 3);
}

#[test]
fn media_omission_keeps_text_runs_and_the_final_script_error() {
    let items = vec![
        text("a"),
        text("b"),
        image(0),
        text("c"),
        image(1),
        image(2),
        image(3),
        image(4),
        text("Script error:\nfailed"),
    ];
    assert_eq!(
        fit_item_count(items, 4),
        (
            vec![text("a\nb"), image(0), text("c\nScript error:\nfailed")],
            4
        )
    );
}

#[test]
fn within_both_limits_the_canonical_parts_are_byte_identical()
-> Result<(), Box<dyn std::error::Error>> {
    let header = TextBlock::new("status", None)?;
    let items = vec![
        text("a\n\"b"),
        text(""),
        text("é"),
        Item::Image {
            url: "data:image/png;base64,AA==".to_owned(),
            detail: None,
        },
    ];
    let original = super::local_composition_items_to_parts(header.clone(), &items)?;
    let fitted = fit_response_parts(header, items)?;
    assert_eq!(serde_json::to_vec(&fitted)?, serde_json::to_vec(&original)?);
    Ok(())
}

#[test]
fn exact_byte_limit_keeps_the_original_text_and_one_byte_over_cuts_it()
-> Result<(), Box<dyn std::error::Error>> {
    let header = TextBlock::new("status", None)?;
    let overhead = super::part_bytes(&ToolResultPart::Text(header.clone()))?
        + super::part_bytes(&ToolResultPart::Text(TextBlock::new("a", None)?))?
        - 1;
    let text = "a".repeat(MAX_TOOL_OUTCOME_BYTES - overhead);
    let parts = fit_response_parts(header.clone(), vec![Item::Text(text.clone())])?;
    let outcome = ToolOutcome::success(parts)?;
    assert_eq!(outcome.retained_bytes(), MAX_TOOL_OUTCOME_BYTES);
    assert!(matches!(&outcome.parts()[1], ToolResultPart::Text(part) if part.text() == text));

    let parts = fit_response_parts(header, vec![Item::Text(format!("{text}a"))])?;
    let outcome = ToolOutcome::success(parts)?;
    assert!(outcome.retained_bytes() <= MAX_TOOL_OUTCOME_BYTES);
    assert!(
        matches!(&outcome.parts()[1], ToolResultPart::Text(part) if part.text().contains("bytes omitted"))
    );
    Ok(())
}

#[test]
fn escaped_multibyte_text_is_cut_in_the_middle_to_fit_encoded_bytes()
-> Result<(), Box<dyn std::error::Error>> {
    let text = format!(
        "first{}last",
        "\u{0}\"\\é".repeat(MAX_TOOL_OUTCOME_BYTES / 3)
    );
    let parts = fit_response_parts(TextBlock::new("status", None)?, vec![Item::Text(text)])?;
    let outcome = ToolOutcome::success(parts)?;
    assert!(outcome.retained_bytes() <= MAX_TOOL_OUTCOME_BYTES);
    let [_, ToolResultPart::Text(text)] = outcome.parts() else {
        panic!("header and text")
    };
    assert!(text.text().starts_with("first"));
    assert!(text.text().ends_with("last"));
    assert!(text.text().contains("bytes omitted"));
    Ok(())
}

#[test]
fn count_and_byte_fitting_keep_the_error_and_count_original_media()
-> Result<(), Box<dyn std::error::Error>> {
    let mut items = (0..130)
        .map(|_| Item::Image {
            url: format!("data:image/png;base64,{}", "AAAA".repeat(65_536)),
            detail: None,
        })
        .collect::<Vec<_>>();
    items.push(text("Script error:\nfailed"));
    let parts = fit_response_parts(TextBlock::new("status", None)?, items)?;
    let outcome = ToolOutcome::success(parts)?;
    assert_eq!(outcome.parts().len(), 6);
    assert!(
        matches!(&outcome.parts()[4], ToolResultPart::Text(text) if text.text() == "Script error:\nfailed")
    );
    assert!(
        matches!(&outcome.parts()[5], ToolResultPart::Text(text) if text.text() == "Warning: 127 images/audio omitted; one tool result holds at most 128 parts and 1048576 JSON-encoded bytes")
    );
    Ok(())
}
