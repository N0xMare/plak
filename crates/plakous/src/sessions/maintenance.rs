use super::{
    AgentSessions, AgentSessionsError, AgentSessionsErrorKind, ArchiveSession,
    ArchiveSessionReport, ArchivedSessionReport, DeleteSession, DeleteSessionReport,
    DurableSessionAddress, PruneHistoryOutcome, PruneHistoryReport, ReclaimReport, SessionId,
    SessionLocator, store_error,
};

impl AgentSessions {
    /// Drains deferred object reclaim in bounded, crash-safe transactions.
    /// Does not prune checkpoints or delete session authority.
    ///
    /// # Errors
    /// Returns a typed backend or integrity failure.
    pub async fn reclaim(&self) -> Result<ReclaimReport, AgentSessionsError> {
        self.store
            .reclaim(plakous_agent::internal::StoreReclaimMode::Drain)
            .await
            .map(ReclaimReport::from_store)
            .map_err(store_error)
    }

    /// Runs passive backend checkpoint maintenance after a consumer is promptable.
    ///
    /// # Errors
    /// Returns a typed backend failure, without changing runtime authority.
    pub async fn passive_checkpoint(&self) -> Result<(), AgentSessionsError> {
        self.store.flush().await.map(|_| ()).map_err(store_error)
    }

    /// Archives one exact released session revision.
    ///
    /// The consumer owns discovery, retention policy, and confirmation. Plakous
    /// owns the exact compare-and-swap transition and returns the revision that
    /// can authorize a later semantic deletion. Archived sessions are listed by
    /// [`Self::archived_sessions`]; a consumer that also archives for reasons
    /// other than deletion must not finish every listed session as a delete.
    ///
    /// # Errors
    ///
    /// Returns a typed ownership, stale-locator, integrity, or backend failure.
    pub async fn archive(
        &self,
        locator: SessionLocator,
    ) -> Result<ArchiveSessionReport, AgentSessionsError> {
        let _reservation = self.drivers.reserve_session(locator.session_id)?;
        let source = self.load_exact(locator).await?;
        let receipt = self
            .store
            .archive(ArchiveSession::new(
                locator.session_id,
                source.session_revision,
            ))
            .await
            .map_err(store_error)?;
        Ok(ArchiveSessionReport::from_receipt(receipt))
    }

    /// Deletes one exact already-archived session revision.
    ///
    /// Listing, archival, user confirmation, and retention policy remain the
    /// consumer's workflow. An attached private driver always refuses deletion.
    ///
    /// # Errors
    ///
    /// Returns a typed ownership, revision, capacity, integrity, or backend
    /// failure without partially deleting authority.
    pub async fn delete(
        &self,
        session_id: SessionId,
        expected_archived_revision: u64,
    ) -> Result<DeleteSessionReport, AgentSessionsError> {
        let _reservation = self.drivers.reserve_session(session_id)?;
        let receipt = self
            .store
            .delete(DeleteSession::new(
                session_id,
                plakous_agent::internal::SessionRevision::from_store(expected_archived_revision),
            ))
            .await
            .map_err(store_error)?;
        Ok(DeleteSessionReport::from_receipt(receipt))
    }

    /// Deletes the session `session_id` if it is retained only as an archived
    /// session: an archive that was accepted but whose deletion did not
    /// complete. Returns `None` when the store retains no session under this
    /// identity, live or archived, which is the case once it was deleted.
    ///
    /// # Errors
    ///
    /// Returns `InvalidInput` when the session is retained but not archived,
    /// or a typed ownership, integrity, or backend failure.
    pub async fn delete_archived(
        &self,
        session_id: SessionId,
    ) -> Result<Option<DeleteSessionReport>, AgentSessionsError> {
        let page_size = std::num::NonZeroUsize::MIN.saturating_add(15);
        let mut query =
            plakous_agent::internal::SessionQuery::new(page_size).include_archived(true);
        let head = loop {
            let page = self.store.list(query).await.map_err(store_error)?;
            if let Some(summary) = page
                .items()
                .iter()
                .find(|summary| summary.head().session_id() == session_id)
            {
                break summary.head();
            }
            match page.next_after() {
                Some(after) => query = query.after(after),
                None => return Ok(None),
            }
        };
        if !head.is_archived() {
            return Err(AgentSessionsError::new(
                AgentSessionsErrorKind::InvalidInput,
                "session is retained but not archived",
            ));
        }
        self.delete(session_id, head.revision().get())
            .await
            .map(Some)
    }

    /// Reclaims the superseded checkpoint history of one released session at
    /// its exact head. The head, its parent, create and fork checkpoints,
    /// checkpoints other sessions depend on, and terminal results the head
    /// still names are kept; the locator stays valid. Repeating it reclaims
    /// nothing more.
    ///
    /// # Errors
    ///
    /// Returns a typed ownership failure while the session is attached or
    /// leased, a stale-locator failure naming the current head, or an
    /// integrity or backend failure. Never refused for store capacity.
    pub async fn prune_history(
        &self,
        locator: SessionLocator,
    ) -> Result<PruneHistoryOutcome, AgentSessionsError> {
        const OPERATION: &str = "session-history-prune";
        let _reservation = self.drivers.reserve_session(locator.session_id)?;
        self.load_exact(locator).await?;
        let address = DurableSessionAddress::new(
            locator.session_id,
            locator.branch_id,
            locator.checkpoint_id,
        );
        match self.runtime.prune_history(OPERATION, address).await {
            Ok(Some(receipt)) => Ok(PruneHistoryOutcome::Pruned(
                PruneHistoryReport::from_receipt(locator, receipt),
            )),
            Ok(None) => Ok(PruneHistoryOutcome::RecoveryRequired),
            Err(error) => Err(self.map_branch_runtime_error(locator, &error).await),
        }
    }

    /// Lists the sessions this host's store holds archived but not deleted,
    /// each with the archived revision [`Self::delete`] needs. A consumer that
    /// archives only as the first half of a deletion can finish every listed
    /// session as an interrupted delete; one that archives for other reasons
    /// must tell them apart itself.
    ///
    /// # Errors
    ///
    /// Returns a typed integrity or backend failure.
    pub async fn archived_sessions(
        &self,
    ) -> Result<Vec<ArchivedSessionReport>, AgentSessionsError> {
        const PAGE: std::num::NonZeroUsize = std::num::NonZeroUsize::new(128).unwrap();
        let mut archived = Vec::new();
        let mut query = plakous_agent::internal::SessionQuery::new(PAGE).include_archived(true);
        loop {
            let page = self.store.list(query).await.map_err(store_error)?;
            archived.extend(
                page.items()
                    .iter()
                    .map(|summary| summary.head())
                    .filter(|head| head.is_archived())
                    .map(|head| {
                        ArchivedSessionReport::new(head.session_id(), head.revision().get())
                    }),
            );
            let Some(after) = page.next_after() else {
                return Ok(archived);
            };
            query = query.after(after);
        }
    }

    /// Recovers the exact accepted deletion after acknowledgement loss.
    ///
    /// # Errors
    ///
    /// Returns a typed not-found, revision-conflict, integrity, or backend
    /// failure.
    pub async fn recover_delete(
        &self,
        session_id: SessionId,
        expected_archived_revision: u64,
    ) -> Result<DeleteSessionReport, AgentSessionsError> {
        let receipt = self
            .store
            .recover_delete(DeleteSession::new(
                session_id,
                plakous_agent::internal::SessionRevision::from_store(expected_archived_revision),
            ))
            .await
            .map_err(store_error)?;
        Ok(DeleteSessionReport::from_receipt(receipt))
    }
}
