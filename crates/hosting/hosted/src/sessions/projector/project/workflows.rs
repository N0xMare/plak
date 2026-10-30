use super::{
    Anchored, Applier, FeedBranchTransition, FeedChange, FeedFact, FeedLocator, FeedWorkflowKind,
    FeedWorkflowOutcome, LineageStep, NoticeScope, PauseCause, ProductCompactReason, ProjectError,
    RowKey, Successor, TranscriptRow, WorkflowCompletion, WorkflowSettlement, compact_pass_label,
    compact_terminal_subject, project, tool_row, workflow_hex,
};

impl Applier<'_> {
    /// A `Branched` record whose source is this session's branch: the source
    /// settles its request, and the successor starts from a copy of these
    /// views with the record applied on its own branch.
    pub(super) fn publish_successor(
        &mut self,
        transition: FeedBranchTransition,
    ) -> Result<Successor, ProjectError> {
        let header = *self.record.header();
        let FeedChange::Branched { target, .. } = header.change else {
            unreachable!("only a branch record publishes a successor")
        };
        let mut operation_id = None;
        for fact in self.record.facts() {
            if let FeedFact::WorkflowAdmitted { .. } | FeedFact::WorkflowSettled { .. } = fact {
                self.apply_fact(fact)?;
            }
            if let FeedFact::WorkflowSettled {
                operation_id: Some(id),
                ..
            } = fact
            {
                operation_id = Some(id.clone());
            }
        }
        // Retention seeds and intent-only pause joins are not a complete
        // fact-derived prefix for the successor's independently owned intents.
        let can_seed = self.views.state.retained_from.is_none()
            && !matches!(
                &self.views.state.pause,
                Some(Anchored {
                    value: Some(PauseCause::CompactionNotApplied { .. }),
                    ..
                })
            );
        let seed = if can_seed {
            let mut inherited = self.views.clone();
            for request in &self.intents.workflow_requests {
                if request.kind == crate::sessions::SessionWorkflowKind::Clear {
                    inherited.state.workflows.remove(&request.operation_id);
                }
            }
            inherited.state.lineage.push(LineageStep {
                branch_id: header.branch_id,
                from_seq: header.seq.get(),
            });
            project(
                inherited,
                self.intents,
                std::slice::from_ref(self.record),
                self.reads,
            )
            .ok()
            .map(|run| run.views)
        } else {
            None
        };
        Ok(Successor {
            operation_id,
            transition,
            target: FeedLocator {
                session_id: header.session_id,
                branch_id: header.branch_id,
                checkpoint_id: target,
            },
            published_seq: header.seq,
            created_at_unix: header.committed_at_ms / 1_000,
            seed,
        })
    }

    fn compact_row(&mut self, workflow_id: &[u8]) -> RowKey {
        let hex = workflow_hex(workflow_id);
        if let Some(key) = self.views.state.compactions.get(&hex) {
            return *key;
        }
        let key = self.next_key();
        self.views.state.compactions.insert(hex, key);
        let mut tool = tool_row(
            &super::compaction_call_id(workflow_id),
            "compact",
            "compact",
            None,
        );
        tool.started_at_ms = Some(self.committed_at());
        self.replace(key, TranscriptRow::Tool(tool));
        key
    }

    pub(super) fn workflow_admitted(
        &mut self,
        workflow_id: &[u8],
        operation_id: Option<&String>,
        kind: FeedWorkflowKind,
    ) {
        let hex = workflow_hex(workflow_id);
        let key = operation_id.cloned().unwrap_or_else(|| hex.clone());
        self.views
            .state
            .workflows
            .entry(key)
            .or_insert(WorkflowCompletion {
                admitted_at: None,
                settled: None,
            })
            .admitted_at = Some(self.seq());
        if matches!(kind, FeedWorkflowKind::Compact { .. }) {
            self.compact_row(workflow_id);
        }
    }

    pub(super) fn workflow_settled(
        &mut self,
        workflow_id: &[u8],
        operation_id: Option<&str>,
        kind: FeedWorkflowKind,
        outcome: FeedWorkflowOutcome,
    ) {
        let hex = workflow_hex(workflow_id);
        let settlement = match outcome {
            FeedWorkflowOutcome::Compacted {
                tokens_before,
                tokens_after,
                ..
            } => WorkflowSettlement::Compacted {
                tokens_before,
                tokens_after,
            },
            FeedWorkflowOutcome::NoEligibleRange => WorkflowSettlement::NoEligibleRange,
            FeedWorkflowOutcome::Abandoned { .. } => WorkflowSettlement::Abandoned,
            FeedWorkflowOutcome::Published { target } => WorkflowSettlement::Published { target },
            _ => WorkflowSettlement::Failed,
        };
        let key = operation_id.map_or_else(|| hex.clone(), str::to_owned);
        self.views
            .state
            .workflows
            .entry(key)
            .or_insert(WorkflowCompletion {
                admitted_at: None,
                settled: None,
            })
            .settled = Some(settlement.clone());
        if !matches!(kind, FeedWorkflowKind::Compact { .. }) {
            return;
        }
        let row_key = self.compact_row(workflow_id);
        let committed_at = self.committed_at();
        if let Some(TranscriptRow::Tool(mut tool)) = self.views.rows.get(&row_key).cloned() {
            tool.finished_at_ms = Some(committed_at);
            match outcome {
                FeedWorkflowOutcome::Compacted {
                    pass,
                    selected_segments,
                    tokens_before,
                    tokens_after,
                    ..
                } => {
                    "succeeded".clone_into(&mut tool.state);
                    tool.subject = compact_terminal_subject(
                        Some(tokens_before),
                        Some(tokens_after),
                        true,
                        true,
                    );
                    tool.detail = Some(format!(
                        "{} pass · {selected_segments} segments",
                        compact_pass_label(pass)
                    ));
                }
                FeedWorkflowOutcome::NoEligibleRange => "succeeded".clone_into(&mut tool.state),
                FeedWorkflowOutcome::Abandoned { .. } => "interrupted".clone_into(&mut tool.state),
                _ => "failed".clone_into(&mut tool.state),
            }
            self.replace(row_key, TranscriptRow::Tool(tool));
        }
        match outcome {
            FeedWorkflowOutcome::Compacted { .. } => self.views.state.counters.compactions += 1,
            FeedWorkflowOutcome::NoEligibleRange => self.push(TranscriptRow::info(
                NoticeScope::Compaction,
                crate::runtime::COMPACT_NO_ELIGIBLE_RANGE_DETAIL,
            )),
            FeedWorkflowOutcome::Abandoned { summary_unsent } => {
                self.push(TranscriptRow::warning(
                    NoticeScope::Recovery,
                    if summary_unsent {
                        crate::runtime::COMPACT_ABANDONED_UNSENT_NOTE
                    } else {
                        crate::runtime::COMPACT_ABANDONED_BILLED_NOTE
                    },
                ));
            }
            _ => self.push(TranscriptRow::warning(
                NoticeScope::Compaction,
                crate::runtime::COMPACT_FAILED_DETAIL,
            )),
        }
        let pass_pauses = operation_id
            .and_then(|id| self.intents.workflow_request(id))
            .and_then(super::intents::WorkflowRequest::compact_reason)
            .is_some_and(|reason| {
                matches!(
                    reason,
                    ProductCompactReason::Scheduled | ProductCompactReason::CapacityRecovery
                )
            });
        if pass_pauses
            && !matches!(outcome, FeedWorkflowOutcome::Compacted { .. })
            && let Some(operation_id) = operation_id
        {
            self.views.state.pause = Some(Anchored {
                anchor: self.seq(),
                value: Some(PauseCause::CompactionNotApplied {
                    operation_id: operation_id.to_owned(),
                }),
            });
        }
    }
}
