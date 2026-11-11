//! Exact offline profile rows shared by process and in-process fixtures.

use plakous_host_protocol::views::{ModelEntry, ProviderSection, ReadyCatalog};

pub(crate) fn ready_catalog(models: &[&str]) -> ReadyCatalog {
    vec![ProviderSection {
        account: "codex".to_owned(),
        label: "codex".to_owned(),
        ready: true,
        entries: models
            .iter()
            .map(|model| ModelEntry {
                id: (*model).to_owned(),
                tag: "offline exact binding".to_owned(),
                reasoning: true,
                effort_options: vec!["low".to_owned(), "medium".to_owned()],
                extras: Vec::new(),
                unavailable_reason: None,
            })
            .collect(),
    }]
    .into()
}
