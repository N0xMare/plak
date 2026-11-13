//! Reconstructed identities are current-native data, never serialized source bytes.
use super::{Capsule, HeadContext, Line, Origin, Session, Terminal};
use plakous::model::{LineId, Timestamp};
use plakous::sessions::SessionLocator;
use plakous::sessions::host_move::{
    MoveLine, MoveLineOrigin, MoveLineRecord, MoveSessionHeader, MoveSessionRef,
    MoveSessionRevision,
};

impl Capsule {
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
