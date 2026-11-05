//! Operator presentation of a session-store storage-pressure refusal.

use plakous::sessions::{
    AgentSessionsError, AgentSessionsErrorKind, SessionStoreAdmission, SessionStorePressure,
};

pub(crate) fn sessions_remedy(
    error: &AgentSessionsError,
) -> Option<plakous_host_protocol::notice_remedy::NoticeRemedy> {
    use plakous_host_protocol::notice_remedy::NoticeRemedy as R;
    match error.kind() {
        AgentSessionsErrorKind::StorageFull => Some(R::FreeDiskSpace {}),
        AgentSessionsErrorKind::StoragePressure => {
            Some(match error.storage_pressure().map(|p| p.admission()) {
                Some(SessionStoreAdmission::Settlement | SessionStoreAdmission::Recovery) => {
                    R::ReclaimThenRecover {}
                }
                _ => R::ReclaimRetainedHistory {},
            })
        }
        _ => None,
    }
}

/// Frames `raw` by the class of the refused write: new work refused at the
/// ordinary threshold, or a running turn whose outcome could not be recorded
/// inside the reserve and therefore needs recovery.
pub(crate) fn storage_pressure_message(
    pressure: Option<SessionStorePressure>,
    raw: &str,
) -> String {
    let detail = pressure.map_or_else(|| "a retention bound".to_owned(), |p| p.to_string());
    if pressure.is_some_and(|pressure| pressure.subordinate()) {
        return format!(
            "session store is past the mark where child agents' new work stops ({detail}) - this agent's new work is refused so that the root keeps the rest for its own turn, compaction and cleanup; reclaim superseded history, then remove unneeded sessions\n{raw}"
        );
    }
    match pressure.map(|pressure| pressure.admission()) {
        Some(SessionStoreAdmission::Settlement | SessionStoreAdmission::Recovery) => format!(
            "session store reserve is exhausted ({detail}) - a running turn could not record its outcome and needs recovery; reclaim superseded history, remove unneeded sessions, then explicitly recover\n{raw}"
        ),
        _ => format!(
            "session store is full ({detail}) - new work is refused so the reserve can finish and recover running turns; reclaim superseded history, then remove unneeded sessions; a fresh conversation, compaction and recovery do not reclaim it\n{raw}"
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
        "session store disk is full - reclaim history, remove unused sessions, or free disk space; the logical reserve does not reserve disk blocks\n{raw}"
    )
}

/// Fill levels of the shared session store at which the operator is warned,
/// as percentages of where new work stops.
pub(crate) const FILL_WARNING_STEPS: [u8; 3] = [70, 85, 95];

/// How full the shared session store is: its most filled aggregate bound,
/// relative to where new work stops.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct StoreFill {
    bound: plakous::sessions::SessionStoreBound,
    used: u64,
    /// Where new work stops.
    ordinary: u64,
    /// Where child agents' new work stops.
    subordinate: u64,
    limit: u64,
}

impl StoreFill {
    /// The most filled of `bounds`.
    pub(crate) fn of(bounds: &[plakous::sessions::SessionStoreBoundUsage]) -> Option<Self> {
        Self::most_filled(bounds.iter().map(|bound| Self {
            bound: bound.bound(),
            used: bound.used(),
            ordinary: bound.ordinary_threshold(),
            subordinate: bound.subordinate_threshold(),
            limit: bound.limit(),
        }))
    }

    /// The fill whose used share of where its new work stops is largest.
    fn most_filled(fills: impl IntoIterator<Item = Self>) -> Option<Self> {
        fills
            .into_iter()
            .max_by_key(|fill| u128::from(fill.used) * 1_000_000 / u128::from(fill.ordinary.max(1)))
    }

    /// Used share of where new work stops, in whole percent.
    pub(crate) fn percent(&self) -> u8 {
        let percent = u128::from(self.used) * 100 / u128::from(self.ordinary.max(1));
        u8::try_from(percent).unwrap_or(u8::MAX)
    }

    /// The highest warning step this fill has reached.
    pub(crate) fn warning_step(&self) -> Option<u8> {
        let percent = self.percent();
        FILL_WARNING_STEPS
            .iter()
            .rev()
            .copied()
            .find(|step| percent >= *step)
    }

    /// The fill the session facts carry: from the first warning step only.
    pub(crate) fn shown_percent(&self) -> Option<u8> {
        self.warning_step().map(|_| self.percent())
    }

    /// Whether child agents' new work is refused at this fill.
    pub(crate) const fn children_stopped(&self) -> bool {
        self.used >= self.subordinate
    }

    /// Why a child agent's new work is refused at this fill, naming the
    /// children's mark and the root's stop.
    pub(crate) fn children_refusal(&self) -> String {
        format!(
            "the shared session store is {}% full: {} {} has reached {}, where child agents' new work stops so that the root keeps the rest up to {}",
            self.percent(),
            self.bound,
            self.used,
            self.subordinate,
            self.ordinary
        )
    }

    /// The operator notice for this fill. The text names the store file; the
    /// product's renderer adds the reclaim commands from the remedy.
    pub(crate) fn notice(&self, path: &std::path::Path) -> String {
        let children = if self.children_stopped() {
            "child agents' new work is refused".to_owned()
        } else {
            format!("child agents' new work stops at {}", self.subordinate)
        };
        format!(
            "session store {} is {}% full: {} {} of {} where new work stops (limit {}); {children}",
            path.display(),
            self.percent(),
            self.bound,
            self.used,
            self.ordinary,
            self.limit,
        )
    }
}

/// A fill of `used` against a store whose new work stops at 1000.
#[cfg(test)]
pub(crate) fn test_fill(used: u64) -> StoreFill {
    StoreFill {
        bound: plakous::sessions::SessionStoreBound::ObjectBytes,
        used,
        ordinary: 1000,
        subordinate: 857,
        limit: 1143,
    }
}

/// Warns once per fill step as the store fills; a step warns again only
/// after the fill fell below it.
#[derive(Debug, Default)]
pub(crate) struct FillWarnings {
    warned: Option<u8>,
}

impl FillWarnings {
    /// The step to warn about now, if this reading crossed a new one.
    pub(crate) fn observe(&mut self, fill: Option<&StoreFill>) -> Option<u8> {
        let step = fill?.warning_step();
        let warn = step.filter(|step| self.warned.is_none_or(|warned| *step > warned));
        self.warned = step;
        warn
    }
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
        assert!(message.contains("reclaim superseded history"));
        assert!(message.contains("remove unneeded sessions"));
        assert!(!message.contains("plak session"));
        assert!(message.contains("prompt was not admitted"));
        assert!(!message.contains("needs recovery"));
    }

    #[test]
    fn physical_storage_full_names_remedies_without_promising_reserved_blocks() {
        let message = super::storage_full_message("prompt was not admitted");
        assert!(message.contains("reclaim history"));
        assert!(message.contains("remove unused sessions"));
        assert!(!message.contains("plak session"));
        assert!(message.contains("free disk space"));
        assert!(message.contains("logical reserve does not reserve disk blocks"));
        assert!(message.contains("prompt was not admitted"));
    }

    use super::test_fill as fill;

    #[test]
    fn store_fill_warns_once_at_each_step_and_again_after_falling_below() {
        let mut warnings = super::FillWarnings::default();
        let steps: Vec<_> = [0, 699, 700, 750, 849, 850, 949, 950, 999, 1000, 1200]
            .into_iter()
            .map(|used| warnings.observe(Some(&fill(used))))
            .collect();
        assert_eq!(
            steps,
            [
                None,
                None,
                Some(70),
                None,
                None,
                Some(85),
                None,
                Some(95),
                None,
                None,
                None
            ]
        );
        // A reading that fails changes nothing; reclaiming below a step re-arms it.
        assert_eq!(warnings.observe(None), None);
        assert_eq!(warnings.observe(Some(&fill(860))), None);
        assert_eq!(warnings.observe(Some(&fill(960))), Some(95));
        assert_eq!(warnings.observe(Some(&fill(100))), None);
        assert_eq!(warnings.observe(Some(&fill(720))), Some(70));
    }

    #[test]
    fn store_fill_reads_the_most_filled_bound_relative_to_its_own_stop() {
        let checkpoints = super::StoreFill {
            bound: plakous::sessions::SessionStoreBound::Checkpoints,
            used: 400,
            ordinary: 1000,
            subordinate: 857,
            limit: 1143,
        };
        // Fewer units used, but a larger share of where its new work stops.
        let bytes = super::StoreFill {
            bound: plakous::sessions::SessionStoreBound::ObjectBytes,
            used: 90,
            ordinary: 100,
            subordinate: 85,
            limit: 114,
        };
        for order in [[checkpoints, bytes], [bytes, checkpoints]] {
            let fill = super::StoreFill::most_filled(order).unwrap();
            assert_eq!(
                fill.bound,
                plakous::sessions::SessionStoreBound::ObjectBytes
            );
            assert_eq!(fill.percent(), 90);
            assert!(fill.children_stopped());
        }
        assert_eq!(super::StoreFill::most_filled([]), None);
        let refusal = bytes.children_refusal();
        assert!(
            refusal.contains("object bytes 90 has reached 85, where child agents' new work stops")
                && refusal.ends_with("keeps the rest up to 100"),
            "{refusal}"
        );
    }

    #[test]
    fn store_fill_notice_names_the_file_the_stop_and_the_children_mark() {
        let path = std::path::Path::new("/plak/state/k/runtime.sqlite3");
        let below = fill(700).notice(path);
        assert!(
            below.starts_with("session store /plak/state/k/runtime.sqlite3 is 70% full"),
            "{below}"
        );
        assert!(
            below.contains("object bytes 700 of 1000 where new work stops (limit 1143)"),
            "{below}"
        );
        assert!(
            below.contains("child agents' new work stops at 857"),
            "{below}"
        );
        let past = fill(857);
        assert!(past.children_stopped());
        assert!(!fill(856).children_stopped());
        assert!(
            past.notice(path)
                .contains("child agents' new work is refused")
        );
        assert_eq!(fill(1200).percent(), 120);
    }
}
