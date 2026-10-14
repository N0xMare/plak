//! Operator presentation of a session-store storage-pressure refusal.

use plakous::sessions::{
    AgentSessionsError, AgentSessionsErrorKind, SessionStoreAdmission, SessionStorePressure,
};

/// Frames `raw` by the class of the refused write: new work refused at the
/// ordinary threshold, or a running turn whose outcome could not be recorded
/// inside the reserve and therefore needs recovery.
pub(crate) fn storage_pressure_message(
    pressure: Option<SessionStorePressure>,
    raw: &str,
) -> String {
    let detail = pressure.map_or_else(|| "a retention bound".to_owned(), |p| p.to_string());
    match pressure.map(|pressure| pressure.admission()) {
        Some(SessionStoreAdmission::Settlement | SessionStoreAdmission::Recovery) => format!(
            "session store reserve is exhausted ({detail}) - a running turn could not record its outcome and needs recovery; free retained state with `plak session gc --all`, then `plak session delete <id>` on sessions you no longer need, then run /recover\n{raw}"
        ),
        _ => format!(
            "session store is full ({detail}) - new work is refused so the reserve can finish and recover running turns; reclaim superseded history with `plak session gc --all`, then free sessions you no longer need with `plak session delete <id>` (/clear, compaction and /recover do not reclaim it)\n{raw}"
        ),
    }
}

/// The storage-pressure framing of a session workflow failure, if it is one.
pub(crate) fn sessions_storage_pressure(error: &AgentSessionsError, raw: &str) -> Option<String> {
    if error.kind() == AgentSessionsErrorKind::StorageFull {
        return Some(storage_full_message(raw));
    }
    (error.kind() == AgentSessionsErrorKind::StoragePressure)
        .then(|| storage_pressure_message(error.storage_pressure(), raw))
}

pub(crate) fn storage_full_message(raw: &str) -> String {
    format!(
        "session store disk is full - reclaim history with `plak session gc --all`, delete unused sessions with `plak session delete <id>`, or free disk space; the logical reserve does not reserve disk blocks\n{raw}"
    )
}

#[cfg(test)]
mod tests {
    #[test]
    fn logical_storage_refusal_keeps_remedies_and_definite_non_admission_copy() {
        let message = super::storage_pressure_message(
            None,
            "prompt was not admitted: new work was refused under storage pressure",
        );
        assert!(
            message
                .lines()
                .next()
                .unwrap()
                .starts_with("session store is full (")
        );
        assert!(message.contains("plak session gc --all"));
        assert!(message.contains("plak session delete <id>"));
        assert!(message.contains("prompt was not admitted"));
        assert!(!message.contains("needs recovery"));
    }

    #[test]
    fn physical_storage_full_names_remedies_without_promising_reserved_blocks() {
        let message = super::storage_full_message("prompt was not admitted");
        assert!(message.contains("plak session gc --all"));
        assert!(message.contains("plak session delete <id>"));
        assert!(message.contains("free disk space"));
        assert!(message.contains("logical reserve does not reserve disk blocks"));
        assert!(message.contains("prompt was not admitted"));
    }
}
