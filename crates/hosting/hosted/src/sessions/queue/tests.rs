use super::*;
use crate::sessions::projector::{
    Delivery, ProductIntents, SessionViews, Submission, SubmissionKind,
};

fn png(extra: usize) -> DraftImage {
    let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
    bytes.extend_from_slice(&[0, 0, 0, 13]);
    bytes.extend_from_slice(b"IHDR");
    bytes.extend_from_slice(&4_u32.to_be_bytes());
    bytes.extend_from_slice(&3_u32.to_be_bytes());
    bytes.resize(bytes.len() + extra, 0);
    DraftImage {
        name: "shot.png".into(),
        media_type: "image/png".into(),
        bytes,
        source_dimensions: None,
    }
}

#[test]
fn enqueue_bounds_images_and_an_edit_keeps_them() {
    let mut pending = Vec::new();
    let item = enqueue(
        &mut pending,
        Draft::new("look".into(), vec![png(0), png(1)]),
    )
    .unwrap();
    assert_eq!(item.images.len(), 2);
    assert_eq!(
        item.user_input("look").parts().len(),
        3,
        "text then both images"
    );
    edit(&mut pending, &item.id, item.revision, "look again").unwrap();
    assert_eq!(pending[0].text, "look again");
    assert_eq!(pending[0].images, item.images);
    assert_eq!(
        pending[0].protocol(false).images,
        ["[image: shot.png 4x3 1 KB]", "[image: shot.png 4x3 1 KB]"]
    );

    let refused =
        |draft: Draft, pending: &mut Vec<SessionQueueItem>| enqueue(pending, draft).unwrap_err();
    let nine = (0..=plakous::agent::MAX_USER_IMAGES)
        .map(|_| png(0))
        .collect();
    assert_eq!(
        refused(Draft::new("x".into(), nine), &mut pending),
        Refusal::TooManyImages
    );
    assert_eq!(
        refused(
            Draft::new("x".into(), vec![png(plakous::agent::MAX_USER_IMAGE_BYTES)]),
            &mut pending
        ),
        Refusal::ImageTooLarge
    );
    let four = vec![png(plakous::agent::MAX_USER_IMAGE_BYTES - 64); 4];
    let refusal = refused(Draft::new("x".into(), four), &mut pending);
    assert!(
        matches!(refusal, Refusal::MessageTooLarge(error)
            if error.to_string().contains("16 MiB per-message budget")),
        "{refusal:?}"
    );
    // Text counts as runtime admission encodes it: quotes and newlines
    // escape to two bytes each.
    let three = || vec![png(plakous::agent::MAX_USER_IMAGE_BYTES - 64); 3];
    let escaped = "\"\n".repeat(300_000);
    let refusal = refused(Draft::new(escaped.clone(), three()), &mut pending);
    assert!(
        matches!(refusal, Refusal::MessageTooLarge(_)),
        "{refusal:?}"
    );
    let plain = enqueue(&mut pending, Draft::new("x".repeat(escaped.len()), three())).unwrap();
    assert_eq!((plain.text.len(), plain.images.len()), (escaped.len(), 3));
    pending.retain(|item| item.id != plain.id);
    let mut mislabeled = png(0);
    mislabeled.media_type = "image/jpeg".into();
    assert_eq!(
        refused(Draft::new("x".into(), vec![mislabeled]), &mut pending),
        Refusal::NotAnImage
    );
    assert_eq!(
        refused(Draft::new(" ".into(), Vec::new()), &mut pending),
        Refusal::Empty
    );

    // The session's queued images share one aggregate bound.
    let large = || png(plakous::agent::MAX_USER_IMAGE_BYTES - 64);
    let mut full = Vec::new();
    while enqueue(&mut full, Draft::new("x".into(), vec![large(); 3])).is_ok() {}
    assert_eq!(
        refused(Draft::new("x".into(), vec![large(); 3]), &mut full),
        Refusal::ImagesFull
    );
    assert!(
        full.iter()
            .map(|item| image_bytes(&item.images))
            .sum::<usize>()
            <= MAX_PENDING_QUEUE_IMAGE_BYTES
    );
    assert_eq!(pending.len(), 1);

    // Images alone are a message: no empty text part reaches the model,
    // and an edit may clear the text of an item that keeps images.
    let only = enqueue(&mut pending, Draft::new(" ".into(), vec![png(2)])).unwrap();
    assert_eq!(only.text, "");
    assert!(matches!(
        only.user_input("").parts(),
        [plakous::agent::UserPart::Image { .. }]
    ));
    let revision = pending[0].revision;
    edit(&mut pending, &item.id, revision, " ").unwrap();
    assert_eq!(pending[0].text, "");
}

#[test]
fn queue_decisions_join_submissions_and_command_facts_by_exact_revision() {
    let mut session = SessionRecord::new("/work".into());
    let first = QueueItem::new("first");
    let second = QueueItem::new("second");
    session.pending_queue = vec![first.clone(), second.clone()];
    let intents = ProductIntents {
        submissions: vec![Submission {
            command_id: "queue-first".into(),
            kind: SubmissionKind::RootStart,
            queue_item_id: first.id.clone(),
            queue_item_revision: first.revision,
            target_turn_id: None,
            canonical_command: first.text.clone(),
            submitted_at_ms: first.submitted_at_ms,
        }],
        ..ProductIntents::default()
    };
    let mut views = SessionViews::empty();
    session.queue_commands = QueueCommands::from_join(&intents, Some(&views));
    assert!(queue_item_is_delivery_locked(&session, &first.id));
    assert_eq!(next_drainable(&session), Some(first.clone()));
    session.queue_projection_debt = true;
    assert_eq!(next_drainable(&session), None);
    session.queue_projection_debt = false;
    assert!(!remove_mutable(&mut session, &first.id, first.revision));

    views.state.deliveries.insert(
        "queue-first".into(),
        Delivery::Rejected {
            error: "busy".into(),
        },
    );
    session.queue_commands = QueueCommands::from_join(&intents, Some(&views));
    assert!(!queue_item_is_delivery_locked(&session, &first.id));
    assert_eq!(next_drainable(&session), Some(second.clone()));
    edit_mutable(&mut session, &first.id, first.revision, "edited").unwrap();
    assert_eq!(
        next_drainable(&session),
        Some(session.pending_queue[0].clone())
    );

    session.pending_queue[0] = first.clone();
    views
        .state
        .deliveries
        .insert("queue-first".into(), Delivery::Accepted);
    session.queue_commands = QueueCommands::from_join(&intents, Some(&views));
    assert_eq!(next_drainable(&session), Some(second));
    assert!(session.queue_commands.is_consumed(&first));
    assert!(!session.queue_commands.is_consumed(&QueueItem {
        revision: first.revision + 1,
        ..first.clone()
    }));
    assert_eq!(
        session.queue_commands,
        QueueCommands::from_join(&intents, Some(&views))
    );
}

#[test]
fn edits_and_moves_keep_the_operator_submit_time() {
    let mut pending = Vec::new();
    let first = enqueue(&mut pending, "first").unwrap();
    let second = enqueue(&mut pending, "second").unwrap();
    assert!(first.submitted_at_ms > 0);
    pending[0].submitted_at_ms = 11;
    pending[1].submitted_at_ms = 22;
    edit(&mut pending, &first.id, first.revision, "first, edited").unwrap();
    assert!(move_to(&mut pending, &second.id, second.revision, 0));
    assert_eq!(
        pending
            .iter()
            .map(|item| (item.text.as_str(), item.submitted_at_ms))
            .collect::<Vec<_>>(),
        vec![("second", 22), ("first, edited", 11)]
    );
}
