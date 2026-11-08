use super::{
    AgentSessions, AgentSessionsError, AgentSessionsErrorKind, CallId, ExactSource,
    OutputBoundDisposition, RuntimeLifecycle, RuntimeWorkingRoot, SessionCallDetailOutcome,
    SessionHistoryPageOutcome, SessionHistoryPageRequest, SessionLocator, SessionTranscriptSource,
    TurnId, call_detail, checkpoint_instruction_sources, checkpoint_job_config,
    checkpoint_registers_tool, committed_compaction_facts, history_page_preflight,
    project_history_page, runtime_error, store_error,
};
#[cfg(feature = "sessions-sqlite")]
use {
    super::{PortableSessionHistory, require_export_job_terminals},
    plakous_store::SqliteSessionStore,
    std::path::Path,
};

#[cfg(feature = "sessions-sqlite")]
impl AgentSessions {
    /// Read the current durable checkpoint revision of a SQLite branch.
    ///
    /// Only the locator's session and branch identify the read; its checkpoint
    /// may have advanced. This acquires no runtime lease and never performs
    /// recovery or a write. Lease release and pruning do not advance this value.
    ///
    /// # Errors
    /// Returns typed schema, lookup, integrity or backend read failure.
    pub fn inspect_branch_revision_sqlite(
        path: impl AsRef<Path>,
        locator: SessionLocator,
    ) -> Result<u64, AgentSessionsError> {
        let (head, _) = SqliteSessionStore::inspect_branch_metadata(path, locator.session_ref())
            .map_err(store_error)?;
        Ok(head.revision().get())
    }

    /// Read directory activity revisions with one read-only SQLite snapshot.
    /// Missing or unreadable branches return `None` independently of other rows.
    /// No runtime, lease, recovery or write is opened.
    ///
    /// # Errors
    /// The shared database snapshot cannot be read or admitted.
    pub fn inspect_branch_revisions_sqlite(
        path: impl AsRef<Path>,
        locators: &[SessionLocator],
    ) -> Result<Vec<Option<u64>>, AgentSessionsError> {
        let selectors = locators
            .iter()
            .map(|locator| locator.session_ref())
            .collect::<Vec<_>>();
        SqliteSessionStore::inspect_branch_revisions(path, &selectors).map_err(store_error)
    }

    /// Pages committed canonical history at one exact SQLite branch head,
    /// including Working heads. This reads no uncommitted stream items, opens
    /// no mutable runtime, takes no lease, and performs no recovery or writes.
    /// Cursor, visibility and page bounds are the same as [`Self::history_page`].
    /// A committed compaction segment projects as kind `compaction` with a short
    /// display phrase; its summary and provider replay envelope are omitted.
    ///
    /// # Errors
    ///
    /// Returns typed lookup, stale-locator, integrity, schema, invalid-input,
    /// or storage failure. Cursor/revision mismatch requests a resnapshot.
    pub fn inspect_history_page_sqlite(
        path: impl AsRef<Path>,
        request: &SessionHistoryPageRequest,
    ) -> Result<SessionHistoryPageOutcome, AgentSessionsError> {
        if let Some(outcome) = history_page_preflight(request)? {
            return Ok(outcome);
        }
        let loaded = SqliteSessionStore::inspect_branch(path, request.locator.session_ref())
            .map_err(store_error)?;
        let exact = ExactSource::validate(request.locator, &loaded)?;
        Ok(project_history_page(
            exact.checkpoint.manifest().canonical().history(),
            request,
            committed_compaction_facts(&exact.checkpoint),
        ))
    }
}

impl AgentSessions {
    /// Reads the committed conversation and named state of one exact quiescent SQLite branch
    /// head so it can seed a new session elsewhere, without opening a mutable
    /// runtime, acquiring a lease, or calling a provider or tool.
    ///
    /// # Errors
    ///
    /// Returns typed lookup, stale-locator, integrity, schema, or storage
    /// failure, and an invalid-input refusal when the head is an unfinished
    /// turn.
    #[cfg(feature = "sessions-sqlite")]
    pub fn export_history_sqlite(
        path: impl AsRef<Path>,
        locator: SessionLocator,
    ) -> Result<PortableSessionHistory, AgentSessionsError> {
        let loaded =
            SqliteSessionStore::inspect_branch(path, locator.session_ref()).map_err(store_error)?;
        let exact = ExactSource::validate(locator, &loaded)?;
        let manifest = exact.checkpoint.manifest();
        if !matches!(manifest.lifecycle(), RuntimeLifecycle::Quiescent) {
            return Err(AgentSessionsError::new(
                AgentSessionsErrorKind::InvalidInput,
                "session head is an unfinished turn; recover it before carrying its history",
            ));
        }
        require_export_job_terminals(
            manifest
                .jobs()
                .rows()
                .iter()
                .any(|row| row.terminal.is_none()),
        )?;
        let mut history = PortableSessionHistory::new(
            manifest.canonical().history().segments().to_vec(),
            manifest.binding().account().subject().to_owned(),
            manifest.binding().requested_model().clone(),
        );
        history.named_state = manifest.named_state().clone();
        Ok(history)
    }

    /// Whether the head at this locator is an unfinished turn awaiting recovery.
    ///
    /// A caller that has just been refused for a stale locator uses this to say
    /// something true about why. A head that is simply further along is one
    /// thing; a head that is an unfinished turn this release cannot close is
    /// another, and it will still be one after any amount of waiting. Saying so
    /// is the difference between a caller that stops and a caller that spends
    /// its whole patience on a refusal that cannot change.
    ///
    /// # Errors
    ///
    /// Returns the same typed lookup, integrity or backend failure an open
    /// would.
    pub async fn head_requires_recovery(
        &self,
        head: SessionLocator,
    ) -> Result<bool, AgentSessionsError> {
        Ok(self.load_exact(head).await?.recovery_report().is_some())
    }

    /// Whether the head at this locator is quiescent: no turn or session
    /// workflow is unsettled on it.
    ///
    /// A host deleting the sessions a finished agent tree left behind uses this
    /// to confirm a child's runtime session is at rest rather than trusting the
    /// tree's record of it.
    ///
    /// # Errors
    ///
    /// Returns the same typed lookup, stale-locator, integrity or backend
    /// failure an open would.
    pub async fn head_is_quiescent(
        &self,
        head: SessionLocator,
    ) -> Result<bool, AgentSessionsError> {
        Ok(matches!(
            self.load_exact(head)
                .await?
                .checkpoint
                .manifest()
                .lifecycle(),
            RuntimeLifecycle::Quiescent
        ))
    }

    /// Reads the running ordinary turn of an exact head without acquiring a
    /// lease. Quiescent heads and session workflow roots have no ordinary turn.
    ///
    /// # Errors
    /// Returns typed exact-head lookup, stale-locator, integrity or backend failure.
    pub async fn head_running_turn(
        &self,
        head: SessionLocator,
    ) -> Result<Option<TurnId>, AgentSessionsError> {
        Ok(
            match self
                .load_exact(head)
                .await?
                .checkpoint
                .manifest()
                .lifecycle()
            {
                RuntimeLifecycle::Working {
                    root: RuntimeWorkingRoot::Turn { turn_id, .. },
                    ..
                } => Some(*turn_id),
                _ => None,
            },
        )
    }

    /// The process budgets bound to the session's stored runtime environment.
    ///
    /// Read these before compiling tool descriptions or composition attachments
    /// for an exact reopen. Current product defaults are not resume authority.
    ///
    /// # Errors
    /// Returns typed lookup, stale-locator, integrity, compatibility or backend
    /// failures, without substituting defaults for invalid stored values.
    pub async fn created_job_config(
        &self,
        locator: SessionLocator,
    ) -> Result<plakous_tools::JobConfig, AgentSessionsError> {
        let exact = self.load_exact(locator).await?;
        checkpoint_job_config(&exact.checkpoint)
            .map_err(|error| runtime_error(&error).for_session(locator.session_id))
    }

    /// Verifies and loads the exact recorded evaluator before runtime compilation.
    /// No current sidecar locator participates, and this read never changes a head.
    ///
    /// # Errors
    /// Returns typed exact-lookup failures or incompatibility when the recorded
    /// executable, canonical path, hash or engine contract no longer agrees.
    #[cfg(feature = "composition")]
    pub async fn created_composition_artifact(
        &self,
        locator: SessionLocator,
    ) -> Result<Option<crate::local_composition::LocalCompositionEngineArtifact>, AgentSessionsError>
    {
        let exact = self.load_exact(locator).await?;
        plakous_agent::internal::facade_support::checkpoint_composition_artifact(&exact.checkpoint)
            .and_then(|stored| {
                stored
                    .as_ref()
                    .map(crate::local_composition::restore_stored_artifact)
                    .transpose()
            })
            .map_err(|error| runtime_error(&error).for_session(locator.session_id))
    }

    /// The instruction sources (`AGENTS.md` and the like) the session at this
    /// exact locator was compiled from, without opening it.
    ///
    /// # Errors
    /// Returns typed lookup, stale-locator, integrity, compatibility or backend
    /// failures.
    pub async fn created_instruction_sources(
        &self,
        locator: SessionLocator,
    ) -> Result<Vec<plakous_agent::ResolvedContextSourceFact>, AgentSessionsError> {
        let exact = self.load_exact(locator).await?;
        checkpoint_instruction_sources(&exact.checkpoint)
            .map_err(|error| runtime_error(&error).for_session(locator.session_id))
    }

    /// Whether the session at this exact locator froze a tool registration
    /// named `name`, and the instruction sources it was compiled from, from
    /// one exact load and without opening it. A host whose optional tools
    /// depend on a live setting reads this before an exact reopen, so the
    /// reopen rebuilds exactly the tools the session sealed.
    ///
    /// # Errors
    /// Returns typed lookup, stale-locator, integrity, compatibility or backend
    /// failures.
    pub async fn created_tool_registration_and_sources(
        &self,
        locator: SessionLocator,
        name: &plakous_model::ToolName,
    ) -> Result<(bool, Vec<plakous_agent::ResolvedContextSourceFact>), AgentSessionsError> {
        let exact = self.load_exact(locator).await?;
        let registered = checkpoint_registers_tool(&exact.checkpoint, name)
            .map_err(|error| runtime_error(&error).for_session(locator.session_id))?;
        let sources = checkpoint_instruction_sources(&exact.checkpoint)
            .map_err(|error| runtime_error(&error).for_session(locator.session_id))?;
        Ok((registered, sources))
    }

    /// The per-request output bound the session at this locator was created with.
    ///
    /// A route that derives its output bound from its own attestation moves the
    /// figure when the provider's catalog moves, and that figure is
    /// identity-bearing: it reaches the configuration identity, and from there
    /// the persisted harness plan, tool projection target, request prefix and
    /// admission evidence. Those artefacts cannot be re-signed on reopen, and
    /// the request prefix has to satisfy the persisted projection and the live
    /// adapter at once, so a reopened session has to be compiled with the bound
    /// it was created under rather than with today's. The caller reads it here,
    /// before it compiles anything.
    ///
    /// The disposition is returned whole, not just the figure: a session
    /// created before this route derived anything spelled the same figure
    /// `Enforced`, and the two spellings digest differently.
    ///
    /// # Errors
    ///
    /// Returns the same typed lookup, stale-locator, integrity or backend
    /// failure an open would.
    pub async fn created_request_output_bound(
        &self,
        locator: SessionLocator,
    ) -> Result<OutputBoundDisposition, AgentSessionsError> {
        Ok(self
            .load_exact(locator)
            .await?
            .checkpoint
            .manifest()
            .provider_configuration()
            .effective()
            .behavior()
            .output_bound)
    }

    /// The provider semantic evidence the session at this locator was created
    /// with: the canonical bytes its configuration identity binds.
    ///
    /// A route that binds admitted model facts into that evidence reads them
    /// here before it compiles a reopen, so a moved fact is refused with its
    /// own reason instead of an identity mismatch.
    ///
    /// # Errors
    ///
    /// Returns the same typed lookup, stale-locator, integrity or backend
    /// failure an open would.
    pub async fn created_provider_semantic_evidence(
        &self,
        locator: SessionLocator,
    ) -> Result<Vec<u8>, AgentSessionsError> {
        Ok(self
            .load_exact(locator)
            .await?
            .checkpoint
            .manifest()
            .provider_configuration()
            .effective()
            .authority()
            .semantic_evidence()
            .canonical_bytes()
            .to_vec())
    }

    /// Pages presentation items from exact canonical history without opening a
    /// mutable driver or store write. A committed compaction segment projects
    /// as kind `compaction`; its summary and provider replay envelope are omitted.
    ///
    /// # Errors
    ///
    /// Returns typed lookup, stale-locator, invalid-input, or backend failure.
    pub async fn history_page(
        &self,
        request: SessionHistoryPageRequest,
    ) -> Result<SessionHistoryPageOutcome, AgentSessionsError> {
        if let Some(outcome) = history_page_preflight(&request)? {
            return Ok(outcome);
        }
        let exact = self.load_exact(request.locator).await?;
        Ok(project_history_page(
            exact.checkpoint.manifest().canonical().history(),
            &request,
            committed_compaction_facts(&exact.checkpoint),
        ))
    }

    /// Reads one tool call's input and result from the exact checkpoint's
    /// committed canonical history by call id, without opening a mutable
    /// driver or store write. Each side is redacted with the shared preview
    /// redactor and bounded at [`MAX_CALL_DETAIL_SIDE_BYTES`](super::MAX_CALL_DETAIL_SIDE_BYTES). Parsed arguments
    /// are pretty-printed only up to the redaction window; the unread tail is
    /// not copied. Only this session's history is searched: a call that is
    /// absent (compacted, pruned, not carried, not committed yet, or another
    /// session's) is [`SessionCallDetailOutcome::NotInCanonicalHistory`].
    ///
    /// # Errors
    ///
    /// Returns typed lookup, stale-locator, integrity, or backend failure.
    pub async fn call_detail(
        &self,
        locator: SessionLocator,
        call_id: CallId,
    ) -> Result<SessionCallDetailOutcome, AgentSessionsError> {
        let exact = self.load_exact(locator).await?;
        Ok(call_detail::project_call_detail(
            exact.checkpoint.manifest().canonical().history(),
            call_id,
        ))
    }

    /// Tests whether each id has both a call and a result in this exact head's
    /// committed canonical history, preserving request order and duplicates.
    /// Authenticates the head in one load, including an empty batch, without
    /// opening a driver or rendering call previews.
    ///
    /// # Errors
    /// Returns typed lookup, stale-locator, integrity or backend failure.
    pub async fn committed_calls(
        &self,
        locator: SessionLocator,
        ids: Vec<CallId>,
    ) -> Result<Vec<(CallId, bool)>, AgentSessionsError> {
        use plakous_model::{ContentBlock, ConversationItem, HistorySegment};
        use std::collections::BTreeMap;
        let exact = self.load_exact(locator).await?;
        let mut calls: BTreeMap<_, _> = ids.iter().map(|id| (*id, (false, false))).collect();
        for block in exact
            .checkpoint
            .manifest()
            .canonical()
            .history()
            .segments()
            .iter()
            .flat_map(HistorySegment::items)
            .flat_map(ConversationItem::blocks)
        {
            match block {
                ContentBlock::ToolCall(call) => {
                    if let Some(seen) = calls.get_mut(&call.call_id()) {
                        seen.0 = true;
                    }
                }
                ContentBlock::ToolResult(result) => {
                    if let Some(seen) = calls.get_mut(&result.call_id()) {
                        seen.1 = true;
                    }
                }
                _ => {}
            }
        }
        Ok(ids
            .into_iter()
            .map(|id| {
                let (call, result) = calls[&id];
                (id, call && result)
            })
            .collect())
    }

    /// Reads the committed canonical history of `locator`'s session and
    /// branch at its current head, in one load and without opening a mutable
    /// driver or store write. A head that moved on the same session and
    /// branch is read where it is now; one on another session or branch is
    /// refused as stale.
    ///
    /// # Errors
    ///
    /// Returns typed lookup, stale-locator, integrity, or backend failure.
    pub async fn transcript_source(
        &self,
        locator: SessionLocator,
    ) -> Result<SessionTranscriptSource, AgentSessionsError> {
        let loaded = self
            .store
            .load_branch(locator.session_ref())
            .await
            .map_err(|error| store_error(error).for_session(locator.session_id))?;
        let (current, exact) = match ExactSource::validate(locator, &loaded) {
            Ok(exact) => (locator, exact),
            Err(error) => match error.current_locator() {
                Some(current)
                    if error.kind() == AgentSessionsErrorKind::StaleLocator
                        && current.session_id == locator.session_id
                        && current.branch_id == locator.branch_id =>
                {
                    (current, ExactSource::validate(current, &loaded)?)
                }
                _ => return Err(error),
            },
        };
        Ok(SessionTranscriptSource::new(
            current,
            exact.checkpoint.manifest().canonical().history().clone(),
            exact.checkpoint.manifest().ledger().clone(),
        ))
    }
}
