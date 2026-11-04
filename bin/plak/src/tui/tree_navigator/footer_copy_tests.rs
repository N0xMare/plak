use super::{AgentScope, FooterView, agents_footer_hint};

fn footer(hidden: usize) -> String {
    agents_footer_hint(FooterView {
        scope: AgentScope::default(),
        hidden,
        history: false,
        failures_only: false,
        query: "",
        offer_recover: false,
    })
}

/// The canvas footer is prose and keystrokes, never a wire state or a
/// typed enum variant.
#[test]
fn canvas_footer_copy_is_prose_with_keys() {
    let live = footer(24);
    assert!(
        live.starts_with("active + last 5m · 24 older hidden · f all"),
        "{live}"
    );
    assert!(live.contains("↑/↓ select"), "{live}");
    assert!(live.ends_with("Esc back"), "{live}");
    assert!(live.contains("x acknowledge"), "{live}");
    assert!(!live.contains("ResumeTree"), "{live}");
    assert!(!live.contains("paused"), "{live}");
}
