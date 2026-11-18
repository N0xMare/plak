use super::{
    Anchored, Applier, Delivery, Derived, FeedCommandKind, Lane, NOT_RETAINED_ROW, NoticeScope,
    OpenTurn, PauseCause, ROUND_PROSE_ORDINAL, ReadActor, ReadItem, ReadPart, RowKey,
    TranscriptRow, control_error_label, host_owns_subject, item_rows, session_wide, tool_row,
};
use crate::sessions::projector::LastRootTurn;
use crate::sessions::projector::{RootContinuationCause, RootContinuationDecision};

impl Applier<'_> {
    pub(super) fn command_rejected(
        &mut self,
        command_id: &str,
        kind: FeedCommandKind,
        error: plakous::agent::ControlErrorKind,
    ) {
        let error = control_error_label(error);
        self.views.state.deliveries.insert(
            command_id.to_owned(),
            Delivery::Rejected {
                error: error.clone(),
            },
        );
        if matches!(kind, FeedCommandKind::Root { .. } | FeedCommandKind::Steer) {
            self.push(TranscriptRow::warning(
                NoticeScope::Queue,
                format!(
                    "{}: {}",
                    crate::runtime::PROMPT_NOT_ADMITTED,
                    error.replace('_', " ")
                ),
            ));
            if session_wide(&error) {
                self.views.state.pause = Some(Anchored {
                    anchor: self.seq(),
                    value: Some(PauseCause::SessionRefusal {
                        command_id: command_id.to_owned(),
                        error,
                    }),
                });
            }
        }
    }

    pub(super) fn command_accepted(
        &mut self,
        command_id: &str,
        kind: FeedCommandKind,
        turn_id: Option<plakous::model::TurnId>,
    ) {
        self.views
            .state
            .deliveries
            .insert(command_id.to_owned(), Delivery::Accepted);
        match kind {
            FeedCommandKind::Root { origin } => {
                let host_wake = origin == plakous::model::TurnOrigin::HostWake;
                self.views.state.counters.root_turns += 1;
                if let Some(turn_id) = turn_id {
                    // A host wake has no operator prompt to resubmit.
                    self.views.state.last_root_turn = Some(LastRootTurn {
                        turn_id: turn_id.to_string(),
                        command_id: command_id.to_owned(),
                        outcome: None,
                        prompt: None,
                        committed_after_prompt: false,
                        history_seen: host_wake,
                    });
                    self.views.state.open_turns.insert(
                        turn_id.to_string(),
                        OpenTurn {
                            command_id: command_id.to_owned(),
                            host_wake,
                            round_anchor: None,
                            cancel_requested: false,
                        },
                    );
                }
                // A loss continuation resumes the interrupted work, including
                // its permission to wake for children. Ordinary host wakes and
                // transient retries must not clear an operator's suppression.
                // Later intents still join after this fact; only a pause
                // between the decision and this acceptance vetoes the rearm.
                let resumes_loss = self.intents.root_continuations.iter().any(|intent| {
                    intent.anchor < self.seq()
                        && matches!(
                            intent.value.cause,
                            RootContinuationCause::Handoff | RootContinuationCause::OwnerLost
                        )
                        && matches!(
                            &intent.value.decision,
                            RootContinuationDecision::NewTurn {
                                command_id: resumed, ..
                            } if resumed == command_id
                        )
                        && !self.intents.wake_suppressions.iter().any(|suppression| {
                            suppression.anchor < self.seq()
                                && (suppression.anchor, suppression.ordinal)
                                    > (intent.anchor, intent.ordinal)
                        })
                });
                if !host_wake || resumes_loss {
                    let anchor = self.seq();
                    self.views.state.wake = Some(Anchored {
                        anchor,
                        value: false,
                    });
                }
                if !host_wake {
                    let anchor = self.seq();
                    self.views.state.pause = Some(Anchored {
                        anchor,
                        value: None,
                    });
                }
            }
            FeedCommandKind::Cancel => {
                let open =
                    turn_id.and_then(|turn| self.views.state.open_turns.get_mut(&turn.to_string()));
                let host_wake = open.is_some_and(|turn| {
                    turn.cancel_requested = true;
                    turn.host_wake
                });
                if host_wake {
                    self.views.state.wake = Some(Anchored {
                        anchor: self.seq(),
                        value: true,
                    });
                }
            }
            _ => {}
        }
    }

    /// Records where history lands relative to the newest root turn's
    /// prompt: its first segment is the prompt, anything after is a round.
    pub(super) fn root_history(
        &mut self,
        turn_id: Option<plakous::model::TurnId>,
        segment: plakous::model::HistorySegmentKind,
        items: Option<&[ReadItem]>,
    ) {
        let Some(last) = self.views.state.last_root_turn.as_mut() else {
            return;
        };
        let owned = turn_id.is_some_and(|turn| turn.to_string() == last.turn_id);
        if owned && !last.history_seen {
            last.history_seen = true;
            if segment == plakous::model::HistorySegmentKind::RecentIntent {
                last.prompt = items.and_then(operator_text);
            }
            return;
        }
        last.committed_after_prompt = true;
    }

    #[allow(
        clippy::needless_pass_by_value,
        reason = "projection boundaries consume owned event facts rather than retaining borrowed inputs across settlement"
    )]
    pub(super) fn items(
        &mut self,
        turn_id: Option<String>,
        first_ordinal: u64,
        items: Option<Vec<ReadItem>>,
        segment: plakous::model::HistorySegmentKind,
    ) {
        let Some(items) = items else {
            self.push(TranscriptRow::info(NoticeScope::Session, NOT_RETAINED_ROW));
            return;
        };
        let sent_at_ms = turn_id
            .as_ref()
            .and_then(|turn| self.views.state.open_turns.get(turn))
            .and_then(|turn| self.intents.submission(&turn.command_id))
            .map(|submission| submission.submitted_at_ms);
        let round_anchor = if items.iter().any(|item| item.actor == ReadActor::Assistant) {
            turn_id
                .as_ref()
                .and_then(|turn| self.views.state.open_turns.get_mut(turn))
                .and_then(|turn| turn.round_anchor.take())
        } else {
            None
        };
        let mut prose = ROUND_PROSE_ORDINAL;
        for (index, item) in items.iter().enumerate() {
            let ordinal = first_ordinal.saturating_add(index as u64);
            for derived in item_rows(item, ordinal, sent_at_ms) {
                match derived {
                    Derived::Row(row) => match (*row, round_anchor) {
                        (TranscriptRow::User { text, .. }, _) if segment == plakous::model::HistorySegmentKind::Steering => {
                            // Canonical history proves application, not which
                            // identically worded submission supplied a timestamp.
                            self.push(TranscriptRow::Steer { text, sent_at_ms: None });
                        }
                        (row, Some(anchor)) if item.actor == ReadActor::Assistant => {
                            let key = RowKey::new(anchor, Lane::Runtime, prose);
                            prose = prose.saturating_add(1);
                            self.replace(key, row);
                        }
                        (row, _) => self.push(row),
                    },
                    Derived::Call(ReadPart::Call {
                        call_id,
                        canonical,
                        exposed,
                        arguments,
                        parent_call_id,
                        cell_id,
                        subject,
                    }) if !self.views.state.calls.contains_key(call_id) => {
                        let mut tool = tool_row(call_id, canonical, exposed, arguments.as_deref());
                        tool.parent_call_id.clone_from(parent_call_id);
                        tool.cell_id.clone_from(cell_id);
                        if subject.is_some()
                            && !(tool.subject.is_some() && host_owns_subject(canonical, exposed))
                        {
                            tool.subject.clone_from(subject);
                        }
                        self.call_key(call_id, turn_id.clone(), || tool);
                    }
                    // A result committed in the same item as its call is a
                    // provider-hosted call's: it has no operation, so the
                    // segment's commit settles it.
                    Derived::Call(ReadPart::Result { call_id, result })
                        if item.parts.iter().any(|part| {
                            matches!(part, ReadPart::Call { call_id: called, .. } if called == call_id)
                        }) =>
                    {
                        self.hosted_result(call_id, result);
                    }
                    Derived::Call(_) => {}
                }
            }
        }
    }
}

/// The text of a prompt segment that is one operator message of text alone.
fn operator_text(items: &[ReadItem]) -> Option<String> {
    let [item] = items else {
        return None;
    };
    if item.actor != ReadActor::User || item.harness_authored || item.parts.is_empty() {
        return None;
    }
    let mut text = String::new();
    for part in &item.parts {
        let ReadPart::Text(part) = part else {
            return None;
        };
        text.push_str(part);
    }
    Some(text)
}
