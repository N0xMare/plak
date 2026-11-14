//! Reconstructed identities are current-native data, never serialized source bytes.
use super::{Capsule, HeadContext, Line, Origin, Session, Terminal};
use plakous::model::{LineId, Timestamp};
use plakous::sessions::SessionLocator;
use plakous::sessions::host_move::{
    MoveLine, MoveLineOrigin, MoveLineRecord, MoveSessionHeader, MoveSessionRef,
    MoveSessionRevision,
};
use std::collections::BTreeMap;

impl Capsule {
    pub(super) fn verify_runner_work(
        &self,
        lines: &[String],
        statuses: &[plakous::runner::protocol::LineStatus],
    ) -> Result<BTreeMap<String, Vec<plakous::tools::JobView>>, String> {
        let mut inventory = BTreeMap::new();
        for line in lines {
            let mut jobs = Vec::new();
            let mut found = false;
            for status in statuses.iter().filter(|status| &status.line == line) {
                if found || status.foreground || status.host_id != self.source_host {
                    return Err("runner line is duplicated, foreign or has foreground work".into());
                }
                found = true;
                jobs = plakous::tools::runner::decode_jobs_snapshot(status.jobs.clone())
                    .map_err(|error| error.to_string())?
                    .1;
            }
            self.verify_jobs(line, &jobs)?;
            inventory.insert(line.clone(), jobs);
        }
        Ok(inventory)
    }

    pub(super) fn verify_jobs(
        &self,
        line: &str,
        observed: &[plakous::tools::JobView],
    ) -> Result<(), String> {
        self.current_jobs(line)?
            .verify_inventory(observed, self.job_cut)
    }

    pub(super) fn needs_runner(&self, line: &str) -> Result<bool, String> {
        Ok(self.current_jobs(line)?.needs_runner())
    }

    fn current_jobs(&self, line: &str) -> Result<&super::Jobs, String> {
        let current = self
            .lines
            .iter()
            .find(|current| current.id == line)
            .ok_or("runner reports an uncarried line")?;
        // Historical branches may still contain rows already consumed by a
        // continued head. Only the current ledger owes live observations.
        let cuts = self
            .sessions
            .iter()
            .flat_map(|session| &session.heads)
            .filter(|head| {
                head.line == line
                    && head.source.parse::<SessionLocator>().is_ok_and(|source| {
                        source.session_id().to_string() == current.session
                            && source.branch_id().to_string() == current.branch
                    })
            })
            .map(|head| &head.jobs)
            .collect::<Vec<_>>();
        if cuts.len() != 1 {
            return Err("move line has no unique current job ledger".into());
        }
        Ok(cuts[0])
    }

    pub(super) fn current_context(&self, line: &Line) -> Result<&HeadContext, String> {
        self.sessions
            .iter()
            .filter(|session| session.id == line.session)
            .flat_map(|session| &session.heads)
            .find(|head| {
                head.source
                    .parse::<SessionLocator>()
                    .is_ok_and(|source| source.branch_id().to_string() == line.branch)
            })
            .map(|head| &head.context)
            .ok_or_else(|| "move line has no exact current context".into())
    }
}

impl Session {
    pub(super) fn header(&self) -> Result<MoveSessionHeader, String> {
        Ok(MoveSessionHeader::from_store(
            self.id.parse().map_err(|_| "invalid move session")?,
            self.initial_branch
                .parse()
                .map_err(|_| "invalid move initial branch")?,
            MoveSessionRevision::from_store(self.revision),
            Timestamp::new(self.created_seconds, self.created_nanos).map_err(|e| e.to_string())?,
            self.durability.decode(),
            false,
        ))
    }
}

impl Line {
    pub(super) fn decode(self) -> Result<MoveLine, String> {
        let line: LineId = self.id.parse().map_err(|_| "invalid move line")?;
        let origin = match self.origin {
            Origin::Created => MoveLineOrigin::Created,
            Origin::Imported { provenance } => {
                MoveLineOrigin::imported(provenance).ok_or("invalid move line import provenance")?
            }
            Origin::Forked { line, source } => {
                let source: SessionLocator =
                    source.parse().map_err(|_| "invalid move line fork")?;
                MoveLineOrigin::Forked {
                    line: line.parse().map_err(|_| "invalid move fork line")?,
                    source: MoveSessionRef::new(source.session_id(), source.branch_id()),
                    checkpoint: source.checkpoint_id(),
                }
            }
        };
        Ok(MoveLine {
            line: MoveLineRecord::from_store(
                line,
                MoveSessionRef::new(
                    self.session
                        .parse()
                        .map_err(|_| "invalid move line head session")?,
                    self.branch
                        .parse()
                        .map_err(|_| "invalid move line head branch")?,
                ),
                self.generation,
                origin,
            ),
            terminals: self
                .terminals
                .into_iter()
                .map(Terminal::decode)
                .collect::<Result<_, _>>()?,
        })
    }
}
