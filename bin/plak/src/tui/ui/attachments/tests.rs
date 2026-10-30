use super::*;

fn image(name: &str) -> DraftImage {
    DraftImage {
        name: name.to_owned(),
        media_type: "image/png".to_owned(),
        bytes: image_attachments::png(4, 3, 8),
        source_dimensions: None,
    }
}

fn ready(attachments: &mut Attachments, number: u32, name: &str) {
    attachments.entries.push(Entry {
        id: u64::from(number),
        number,
        image: Some(image(name)),
    });
}

#[test]
fn tokens_are_found_by_char_range_and_malformed_ones_are_text() {
    let text = "é [image #1] and [image #12][image #] [image #x] [image #3";
    let spans = token_spans(text);
    assert_eq!(spans, vec![(2..12, 1), (17..28, 12)]);
}

#[test]
fn a_draft_sends_its_images_in_token_order_renumbered_from_one() {
    let mut attachments = Attachments::default();
    ready(&mut attachments, 2, "b.png");
    ready(&mut attachments, 3, "c.png");
    ready(&mut attachments, 5, "unreferenced.png");
    attachments.entries.push(Entry {
        id: 9,
        number: 4,
        image: None,
    });
    assert_eq!(attachments.pending_in("[image #4]"), Some(4));
    let draft =
        attachments.take_draft("compare [image #3] with [image #2], again [image #3] [image #7]");
    assert_eq!(
        draft.text,
        "compare [image #1] with [image #2], again [image #1] [image #7]"
    );
    let names: Vec<_> = draft
        .images
        .iter()
        .map(|image| image.name.as_str())
        .collect();
    assert_eq!(names, ["c.png", "b.png"]);
    // Taken images leave; the rest stay until pruned.
    assert!(attachments.get(2).is_none() && attachments.get(5).is_some());

    ready(&mut attachments, 6, "only.png");
    let draft = attachments.take_draft(" [image #6] ");
    assert_eq!(draft.text, "");
    assert_eq!(draft.images.len(), 1);
}

#[test]
fn backspace_and_delete_remove_a_token_as_one_unit_and_undo_restores_it() {
    use super::super::test_support::{key, launch_basic};
    use crossterm::event::KeyCode;

    let mut app = App::new(launch_basic(), true);
    app.insert_chars("look ");
    app.begin_attachment(|| Ok(image("a.png")));
    app.settle_attachments();
    assert_eq!(app.input, "look [image #1] ");
    assert_eq!(app.attachment_count(), 1);

    // The trailing space goes first, then the whole token.
    app.handle_key(key(KeyCode::Backspace));
    assert_eq!(app.input, "look [image #1]");
    app.handle_key(key(KeyCode::Backspace));
    assert_eq!(app.input, "look ");
    assert_eq!(app.attachment_count(), 0);
    app.composer_undo_step();
    assert_eq!(app.input, "look [image #1]");
    assert_eq!(app.attachment_count(), 1);

    // Delete at the token's start removes it whole.
    app.input_cursor = 5;
    app.composer_selection.set_caret(&app.input, 5);
    app.handle_key(key(KeyCode::Delete));
    assert_eq!(app.input, "look ");

    // A failed read removes its token and says why.
    app.begin_attachment(|| Err("image x.png: cannot be read".to_owned()));
    assert!(app.input.contains("[image #"));
    app.settle_attachments();
    assert_eq!(app.input, "look ");
    assert_eq!(app.attachment_count(), 0);
}

#[test]
fn an_image_past_the_per_message_budget_is_not_attached_and_names_it() {
    use super::super::test_support::launch_basic;

    let maximum = || DraftImage {
        bytes: image_attachments::png(
            4,
            3,
            image_attachments::MAX_IMAGE_BYTES - image_attachments::png(4, 3, 0).len(),
        ),
        ..image("big.png")
    };
    let mut app = App::new(launch_basic(), true);
    for _ in 0..3 {
        app.begin_attachment(move || Ok(maximum()));
        app.settle_attachments();
    }
    assert_eq!(app.attachment_count(), 3);
    app.begin_attachment(move || Ok(maximum()));
    app.settle_attachments();
    assert_eq!(app.attachment_count(), 3);
    assert_eq!(app.input, "[image #1] [image #2] [image #3] ");
    assert!(
        app.pane.blocks.iter().any(|block| matches!(block,
            ScrollBlock::System(text) if text.starts_with("not attached: ")
                && text.contains("over the 16 MiB per-message budget"))),
        "the refusal names the per-message budget"
    );
}

#[test]
fn a_returned_draft_takes_the_first_numbers_ahead_of_the_current_draft() {
    use super::super::test_support::launch_basic;

    let mut app = App::new(launch_basic(), true);
    app.begin_attachment(|| Ok(image("new.png")));
    app.settle_attachments();
    assert_eq!(app.input, "[image #1] ");
    let text = app.adopt_draft_images(Draft::new(
        "see [image #1]".to_owned(),
        vec![image("old.png"), image("other.png")],
    ));
    assert_eq!(text, "see [image #1] [image #2]");
    assert_eq!(app.input, "[image #3] ");
    app.set_input(format!("{text} {}", app.input));
    let draft = app.take_draft(&app.input.clone());
    let names: Vec<_> = draft
        .images
        .iter()
        .map(|image| image.name.as_str())
        .collect();
    assert_eq!(names, ["old.png", "other.png", "new.png"]);
}
