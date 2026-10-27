use super::*;

#[derive(Clone, Default)]
struct Warnings(Arc<std::sync::Mutex<Vec<String>>>);

impl tracing::Subscriber for Warnings {
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
    fn event(&self, event: &tracing::Event<'_>) {
        struct Fields(String);
        impl tracing::field::Visit for Fields {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                use std::fmt::Write as _;
                let _ = write!(self.0, "{}={value:?} ", field.name());
            }
        }
        if event.metadata().target() == "plak.topology"
            && *event.metadata().level() == tracing::Level::WARN
        {
            let mut fields = Fields(String::new());
            event.record(&mut fields);
            self.0.lock().unwrap().push(fields.0);
        }
    }
}

#[test]
fn recovery_warning_identifies_the_turn_without_content() {
    let warnings = Warnings::default();
    let attempt = crate::agent_tree_protocol::AttemptId::new("child-test", 1);
    tracing::subscriber::with_default(warnings.clone(), || {
        warn_recovery_required(
            "child-test",
            &attempt,
            plakous::agent::AgentErrorKind::OutcomeUnknown,
        );
    });
    let events = warnings.0.lock().unwrap();
    assert_eq!(events.len(), 1);
    assert!(events[0].contains("agent_id=\"child-test\""));
    assert!(events[0].contains(&format!("attempt_id={attempt}")));
    assert!(events[0].contains("error_kind=OutcomeUnknown"));
    assert!(!events[0].contains("error="));
    assert_eq!(
        events[0].split('=').count(),
        5,
        "only message and three identity fields"
    );
}
