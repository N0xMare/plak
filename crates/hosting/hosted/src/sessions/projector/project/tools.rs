use super::{
    ActivityKind, ActivityState, Applier, CallRead, CallTarget, CallView, ProjectError, ResultRead,
    RowKey, ToolItem, TranscriptRow, compose_counts_subject, projected_tool_subject, settle,
    tool_row,
};

impl Applier<'_> {
    pub(super) fn hosted_result(&mut self, call_id: &str, result: &ResultRead) {
        let Some(call) = self.views.state.calls.get_mut(call_id) else {
            return;
        };
        if call.operated || call.settled {
            return;
        }
        call.settled = true;
        let key = call.key;
        let Some(TranscriptRow::Tool(mut tool)) = self.views.rows.get(&key).cloned() else {
            return;
        };
        settle(&mut tool, result);
        self.replace(key, TranscriptRow::Tool(tool));
    }

    pub(super) fn tool_result(
        &mut self,
        call: plakous::model::CallId,
        turn_id: Option<String>,
    ) -> Result<(), ProjectError> {
        let read = self.call_read(CallTarget::Call(call))?;
        let call_id = call.to_string();
        let CallRead::Tool {
            canonical,
            exposed,
            arguments,
            result,
        } = read
        else {
            return Ok(());
        };
        let key = self.call_key(&call_id, turn_id, || {
            tool_row(&call_id, &canonical, &exposed, arguments.as_deref())
        });
        self.count(&call_id);
        if result.is_some()
            && self
                .views
                .state
                .calls
                .get(&call_id)
                .is_some_and(|call| call.settled)
        {
            self.resolved_calls.push(call_id.clone());
        }
        let Some(TranscriptRow::Tool(mut tool)) = self.views.rows.get(&key).cloned() else {
            if result.is_some()
                && let Some(call) = self.views.state.calls.get_mut(&call_id)
            {
                call.settled = true;
            }
            return Ok(());
        };
        // The committed call block names the canonical tool and its
        // arguments; the effect's facts named only what the model called.
        if tool.name != canonical {
            let fresh = tool_row(&call_id, &canonical, &exposed, arguments.as_deref());
            tool.name = fresh.name;
            tool.kind = fresh.kind;
            tool.exposed_name = fresh.exposed_name;
            tool.detail = tool.detail.or(fresh.detail);
            tool.wait = tool.wait.or(fresh.wait);
        }
        if tool.subject.is_none() {
            tool.subject = projected_tool_subject(&canonical, arguments.as_deref());
        }
        let call = self.views.state.calls.get_mut(&call_id);
        if let Some(result) = result.as_ref() {
            if let Some(call) = call {
                call.settled = true;
            }
            settle(&mut tool, result);
        }
        if tool.kind == ActivityKind::Compose {
            let mut count = 0;
            let mut failed = 0;
            for child in self.views.rows.values().filter_map(TranscriptRow::as_tool) {
                if child.parent_call_id.as_deref() == Some(&call_id) {
                    count += 1;
                    failed += usize::from(matches!(
                        child.activity_state(),
                        ActivityState::Failed { .. }
                    ));
                }
            }
            tool.subject = Some(compose_counts_subject(
                count,
                failed,
                self.views.state.truncated,
            ));
        }
        self.replace(key, TranscriptRow::Tool(tool));
        Ok(())
    }

    /// The key of the call's row, creating it at this record when no fact
    /// mentioned the call before.
    pub(super) fn call_key(
        &mut self,
        call_id: &str,
        turn_id: Option<String>,
        create: impl FnOnce() -> ToolItem,
    ) -> RowKey {
        if let Some(call) = self.views.state.calls.get(call_id) {
            return call.key;
        }
        // A later fact can mention a canonical call whose live bookkeeping
        // was already pruned. Its retained row still owns the key.
        let existing = self.views.rows.iter().find_map(|(key, row)| {
            row.as_tool()
                .filter(|tool| tool.call_id == call_id)
                .map(|tool| {
                    (
                        *key,
                        tool.started_at_ms.is_some()
                            || matches!(
                                tool.activity_state(),
                                ActivityState::Succeeded | ActivityState::Failed { .. }
                            ),
                    )
                })
        });
        let key = existing.map_or_else(|| self.next_key(), |(key, _)| key);
        self.views.state.calls.insert(
            call_id.to_owned(),
            CallView {
                key,
                turn_id,
                counted: existing.is_some_and(|(_, counted)| counted),
                operated: false,
                settled: false,
            },
        );
        if existing.is_none() {
            self.replace(key, TranscriptRow::Tool(create()));
        }
        key
    }

    pub(super) fn count(&mut self, call_id: &str) {
        if let Some(call) = self.views.state.calls.get_mut(call_id)
            && !call.counted
        {
            call.counted = true;
            self.views.state.counters.tool_calls += 1;
        }
    }
}
