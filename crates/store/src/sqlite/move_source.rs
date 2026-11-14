//! The source executable reads only its own native key before taking any move authority.
use super::*;
use plakous_agent::internal::host_move::{
    MoveBranch, MoveInstall, MoveJobCut, MoveLine, MoveSession, MoveSource,
};

impl SqliteSessionStore {
    /// Read-only complete-session dependencies; no driver, hold or retirement is created.
    ///
    /// # Errors
    /// Refuses unknown, archived, hidden or excessive source inventories.
    #[doc(hidden)]
    pub fn inspect_move_dependencies(
        path: impl AsRef<Path>,
        sessions: &[SessionId],
    ) -> Result<BTreeMap<SessionId, BTreeSet<SessionId>>, SessionStoreError> {
        validate_inventory(sessions)?;
        let connection =
            Self::open_inspection(path.as_ref())?.ok_or(SessionStoreError::NotFound)?;
        let snapshot = connection.unchecked_transaction().map_err(map_sqlite)?;
        admit_inspection_snapshot(&snapshot)?;
        sessions
            .iter()
            .map(|session| {
                live_session(&snapshot, *session)?;
                let mut dependencies = BTreeSet::new();
                for branch in host_move::branch_ids(&snapshot, *session)? {
                    let selector = SessionRef::new(*session, parse(&branch)?);
                    host_move::require_branch(&snapshot, selector)?;
                    let (line, _) = lines::branch_line(&snapshot, selector)?
                        .ok_or(SessionStoreError::corrupt())?;
                    dependencies.insert(
                        lines::load_line_unchecked(&snapshot, line)?
                            .head()
                            .session_id(),
                    );
                }
                Ok((*session, dependencies))
            })
            .collect()
    }

    /// Read-only semantic source snapshot of complete native sessions at their current heads.
    /// This is not a retirement; the coordinator rechecks it under holds before surrender.
    ///
    /// # Errors
    /// Refuses working heads, incomplete line closure, hidden state or malformed inventory.
    #[doc(hidden)]
    pub fn inspect_move_source(
        path: impl AsRef<Path>,
        sessions: &[SessionId],
    ) -> Result<MoveInstall, SessionStoreError> {
        Self::inspect_move_source_cut(path, sessions, MoveJobCut::Settled)
    }

    /// Read-only joined source snapshot with an explicit promoted-job policy.
    ///
    /// # Errors
    /// Refuses working heads, incomplete inventories or jobs outside the selected cut.
    #[doc(hidden)]
    pub fn inspect_move_source_cut(
        path: impl AsRef<Path>,
        sessions: &[SessionId],
        job_cut: MoveJobCut,
    ) -> Result<MoveInstall, SessionStoreError> {
        let source = Self::inspect_move_source_unsettled(path, sessions)?;
        MoveInstall::with_job_cut(source.sessions, source.lines, job_cut)
    }

    /// Reads source heads before terminal proof sampling, granting no installation authority.
    ///
    /// # Errors
    /// Refuses unknown, hidden or corrupt source inventories.
    #[doc(hidden)]
    pub fn inspect_move_source_unsettled(
        path: impl AsRef<Path>,
        sessions: &[SessionId],
    ) -> Result<MoveSource, SessionStoreError> {
        validate_inventory(sessions)?;
        let connection =
            Self::open_inspection(path.as_ref())?.ok_or(SessionStoreError::NotFound)?;
        let snapshot = connection.unchecked_transaction().map_err(map_sqlite)?;
        admit_inspection_snapshot(&snapshot)?;
        let mut line_ids = BTreeSet::new();
        let mut members = Vec::new();
        for sid in sessions {
            let session = live_session(&snapshot, *sid)?;
            let mut branches = Vec::new();
            let mut checkpoints = Vec::new();
            for branch in host_move::branch_ids(&snapshot, *sid)? {
                let selector = SessionRef::new(*sid, parse(&branch)?);
                host_move::require_branch(&snapshot, selector)?;
                let row = branch_row(&snapshot, selector)?.ok_or(SessionStoreError::NotFound)?;
                line_ids.insert(
                    lines::branch_line(&snapshot, selector)?
                        .ok_or(SessionStoreError::corrupt())?
                        .0,
                );
                checkpoints.push(
                    load_checkpoint(&snapshot, row.head.checkpoint().checkpoint_id())?
                        .checkpoint()
                        .as_ref()
                        .clone(),
                );
                branches.push(MoveBranch {
                    session: selector,
                    revision: row.head.revision(),
                    checkpoint: row.head.checkpoint().checkpoint_id(),
                    fence_floor: row.last_fence,
                });
            }
            members.push(MoveSession {
                session,
                feed_tail_floor: feed::tail(&snapshot, *sid)?.tail().get(),
                branches,
                checkpoints,
            });
        }
        let lines = line_ids
            .into_iter()
            .map(|line| {
                Ok(MoveLine {
                    line: lines::load_line_unchecked(&snapshot, line)?,
                    terminals: lines::line_job_terminals(&snapshot, line)?,
                })
            })
            .collect::<Result<_, SessionStoreError>>()?;
        Ok(MoveSource {
            sessions: members,
            lines,
        })
    }
}

fn validate_inventory(sessions: &[SessionId]) -> Result<(), SessionStoreError> {
    if sessions.is_empty()
        || sessions.len() > 16384
        || sessions.iter().collect::<BTreeSet<_>>().len() != sessions.len()
    {
        return Err(SessionStoreError::IdentityConflict);
    }
    Ok(())
}
