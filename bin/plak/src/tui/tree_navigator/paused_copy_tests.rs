use super::{AgentScope, FooterView, RESTORED_PAUSED_NAVIGATOR_HINT, agents_footer_hint};

fn footer(paused: bool, hidden: usize) -> String {
    agents_footer_hint(FooterView {
        restored_paused: paused,
        scope: AgentScope::default(),
        hidden,
        history: false,
        failures_only: false,
        query: "",
        offer_recover: false,
    })
}

/// The canvas footer read
/// `restored_paused · r ResumeTree` — a wire state and a typed enum
/// variant where the operator needs prose and a keystroke.
#[test]
fn paused_canvas_copy_is_prose_with_a_remedy() {
    let paused = footer(true, 0);
    assert!(paused.contains("Tree paused"), "{paused}");
    assert!(paused.contains("r resumes"), "{paused}");
    for copy in [paused.as_str(), RESTORED_PAUSED_NAVIGATOR_HINT] {
        assert!(!copy.contains("ResumeTree"), "{copy}");
        assert!(!copy.contains("restored_paused"), "{copy}");
    }
    assert!(RESTORED_PAUSED_NAVIGATOR_HINT.contains("r resumes"));

    let live = footer(false, 24);
    assert!(
        live.starts_with("active + last 5m · 24 older hidden · f all"),
        "{live}"
    );
    assert!(live.contains("↑/↓ select"), "{live}");
    assert!(live.ends_with("Esc back"), "{live}");
    assert!(live.contains("x acknowledge"), "{live}");
    assert!(!live.contains("ResumeTree"), "{live}");
}
