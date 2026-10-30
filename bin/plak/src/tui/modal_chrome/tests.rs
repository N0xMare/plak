use super::{Hint, MODAL_MIN_HEIGHT, MODAL_MIN_WIDTH, ModalSizing, ScrollState, wrap_hints};
use ratatui::layout::Rect;

fn area(width: u16, height: u16) -> Rect {
    Rect {
        x: 0,
        y: 0,
        width,
        height,
    }
}

#[test]
fn wide_detail_breakpoint_and_readable_columns() {
    assert!(super::split_wide_detail(area(135, 30)).is_none());
    let (list, detail) = super::split_wide_detail(area(136, 30)).unwrap();
    assert_eq!(list.width, 81);
    assert_eq!(detail.width, 53);
    assert_eq!(detail.x, list.right() + 2);
    let (list, detail) = super::split_wide_detail(area(216, 30)).unwrap();
    assert_eq!(list.width, 120);
    assert_eq!(detail.width, 76);
    assert_eq!(detail.height, list.height);
}

#[test]
fn refuses_to_draw_below_the_hard_floor() {
    let sizing = ModalSizing::full_page();
    assert!(sizing.popup(area(MODAL_MIN_WIDTH - 1, 40)).is_none());
    assert!(sizing.popup(area(80, MODAL_MIN_HEIGHT - 1)).is_none());
    assert!(
        sizing
            .popup(area(MODAL_MIN_WIDTH, MODAL_MIN_HEIGHT))
            .is_some()
    );
}

#[test]
fn full_page_is_the_whole_terminal_above_the_floor() {
    let sizing = ModalSizing::full_page();
    assert_eq!(sizing.popup(area(80, 24)), Some(area(80, 24)));
    assert_eq!(sizing.popup(area(120, 40)), Some(area(120, 40)));
    assert_eq!(sizing.popup(area(200, 50)), Some(area(200, 50)));
    let shifted = Rect {
        x: 2,
        y: 3,
        width: 120,
        height: 40,
    };
    assert_eq!(sizing.popup(shifted), Some(shifted));
    assert!(sizing.full_screen);
    assert_eq!((sizing.h_pad, sizing.v_pad, sizing.v_margin), (1, 0, 0));
}

#[test]
fn compact_and_narrow_rules_drop_margin_and_padding() {
    let sizing = ModalSizing::resume_picker();
    let compact = sizing.for_area(area(120, 24));
    assert_eq!((compact.v_margin, compact.v_pad, compact.h_pad), (0, 0, 1));
    let narrow = sizing.for_area(area(50, 40));
    assert_eq!((narrow.width_num, narrow.width_den), (1, 1));
    assert_eq!(narrow.h_pad, 1);
    let roomy = sizing.for_area(area(200, 50));
    assert_eq!((roomy.v_margin, roomy.v_pad, roomy.h_pad), (4, 1, 2));
}

#[test]
fn popup_is_centered_and_clamped() {
    let sizing = ModalSizing::resume_picker();
    let popup = sizing.popup(area(200, 50)).expect("roomy terminal draws");
    assert_eq!(popup.width, 120, "max width clamps the fraction");
    assert_eq!(popup.x, (200 - 120) / 2);
    assert_eq!(popup.height, 50 - 8);
    assert_eq!(popup.y, 4);
    let popup = sizing.popup(area(80, 24)).expect("80x24 draws");
    assert_eq!(popup.height, 24, "compact drops the vertical margin");
    assert_eq!(popup.width, 52);
}

#[test]
fn hints_wrap_greedily_into_rows() {
    let hints = [
        Hint::new("Space", "disable"),
        Hint::new("Enter", "select"),
        Hint::new("Esc", "close"),
    ];
    assert_eq!(wrap_hints(&hints, 200).len(), 1);
    let rows = wrap_hints(&hints, 20);
    assert!(
        rows.len() > 1,
        "a narrow footer wraps rather than truncates"
    );
    assert_eq!(
        rows.iter().map(Vec::len).sum::<usize>(),
        hints.len(),
        "wrapping never drops a hint"
    );
}

#[test]
fn scroll_state_pages_clamps_and_reveals() {
    let mut scroll = ScrollState::default();
    scroll.measure(100, 10);
    assert!(scroll.page_down());
    assert_eq!(scroll.offset(), 10);
    assert!(scroll.end());
    assert_eq!(scroll.offset(), 90);
    assert!(!scroll.page_down(), "clamped at the bottom");
    assert!(scroll.home());
    assert_eq!(scroll.offset(), 0);
    scroll.reveal(42);
    assert_eq!(scroll.offset(), 33);
    scroll.reveal(0);
    assert_eq!(scroll.offset(), 0);
    scroll.measure(5, 10);
    assert_eq!(scroll.max_offset(), 0);
    assert!(!scroll.page_down(), "short content does not scroll");
}
