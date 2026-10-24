use super::{AgentScope, RESTORED_PAUSED_NAVIGATOR_HINT, agents_footer_hint};

/// The canvas footer read
/// `restored_paused · r ResumeTree` — a wire state and a typed enum
/// variant where the operator needs prose and a keystroke.
#[test]
fn paused_canvas_copy_is_prose_with_a_remedy() {
    let footer = agents_footer_hint(true, AgentScope::default(), 0);
    assert!(footer.contains("Tree paused"), "{footer}");
    assert!(footer.contains("r resumes"), "{footer}");
    for copy in [footer.as_str(), RESTORED_PAUSED_NAVIGATOR_HINT] {
        assert!(!copy.contains("ResumeTree"), "{copy}");
        assert!(!copy.contains("restored_paused"), "{copy}");
    }
    assert!(RESTORED_PAUSED_NAVIGATOR_HINT.contains("r resumes"));

    let live = agents_footer_hint(false, AgentScope::default(), 24);
    assert!(
        live.starts_with("active + last 5m · 24 older hidden · f all"),
        "{live}"
    );
    assert!(live.contains("↑/↓ select"), "{live}");
    assert!(live.ends_with("Esc back"), "{live}");
    assert!(live.contains("x acknowledge"), "{live}");
    assert!(!live.contains("ResumeTree"), "{live}");
}
