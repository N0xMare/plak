use super::super::{
    MAX_COLLECTED_OUTPUT_BYTES, MAX_COLLECTED_OUTPUT_ITEMS, OutputItem, output_bytes,
};
use super::*;

#[test]
fn coalesced_output_is_bounded_without_discarding_the_terminal() {
    for item in [
        OutputItem::Text {
            text: "x".repeat(MAX_COLLECTED_OUTPUT_BYTES / 2),
        },
        OutputItem::Text { text: "x".into() },
    ] {
        let entry = ParkedCell::default();
        let count = if output_bytes(&item) > 1 {
            3
        } else {
            MAX_COLLECTED_OUTPUT_ITEMS + 1
        };
        for _ in 0..count {
            entry.publish(Ok(CellResponse::Yielded {
                cell: 1,
                items: vec![item.clone()],
            }));
        }
        entry.publish(Ok(CellResponse::Result {
            cell: 1,
            items: Vec::new(),
            error: None,
        }));
        let Some(Ok(CellResponse::Result {
            items, error: None, ..
        })) = entry.take(1)
        else {
            panic!("the terminal survives output saturation")
        };
        assert!(items.len() <= MAX_COLLECTED_OUTPUT_ITEMS);
        assert!(items.iter().map(output_bytes).sum::<usize>() <= MAX_COLLECTED_OUTPUT_BYTES);
        assert!(
            matches!(items.last(), Some(OutputItem::Text { text }) if text.contains("output omitted"))
        );
        assert!(entry.take(1).is_none());
    }
}

#[test]
fn an_abandoned_resume_is_not_issued_twice() {
    let entry = ParkedCell::default();
    assert!(!entry.begin_resume(), "the initial observation is running");
    entry.publish(Ok(CellResponse::Yielded {
        cell: 1,
        items: Vec::new(),
    }));
    assert!(!entry.begin_resume(), "observe the buffered yield first");
    assert!(matches!(
        entry.take(1),
        Some(Ok(CellResponse::Yielded { .. }))
    ));
    assert!(entry.begin_resume());
    assert!(
        !entry.begin_resume(),
        "a cancelled waiter still owns its resume"
    );
}

fn terminal() -> CellResponse {
    CellResponse::Result {
        cell: 1,
        items: vec![OutputItem::Text {
            text: "terminal".into(),
        }],
        error: None,
    }
}

#[test]
fn loss_after_terminal_keeps_the_result() {
    let entry = ParkedCell::default();
    entry.publish(Ok(terminal()));
    entry.publish(Err("late loss".into()));
    assert_eq!(entry.take(1), Some(Ok(terminal())));
}

#[test]
fn terminal_after_loss_keeps_the_loss() {
    let entry = ParkedCell::default();
    entry.publish(Err("first loss".into()));
    entry.publish(Ok(terminal()));
    assert_eq!(entry.take(1), Some(Err("first loss".into())));
}

#[test]
fn yield_after_terminal_keeps_the_terminal_output_exactly() {
    let entry = ParkedCell::default();
    entry.publish(Ok(terminal()));
    entry.publish(Ok(CellResponse::Yielded {
        cell: 1,
        items: vec![OutputItem::Text {
            text: "late yield".into(),
        }],
    }));
    assert_eq!(entry.take(1), Some(Ok(terminal())));
}
