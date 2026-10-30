use super::{
    Anchored, Applier, Delivery, Derived, FeedCommandKind, Lane, NOT_RETAINED_ROW, NoticeScope,
    OpenTurn, ROUND_PROSE_ORDINAL, ReadActor, ReadItem, ReadPart, RowKey, TranscriptRow,
    host_owns_subject, item_rows, tool_row,
};

impl Applier<'_> {
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
                if !host_wake {
                    let anchor = self.seq();
                    self.views.state.wake = Some(Anchored {
                        anchor,
                        value: false,
                    });
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
