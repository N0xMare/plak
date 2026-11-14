//! Ephemeral quota witnesses belong to one live native connection, not a discovered namespace.
use super::{Bind, Delegation, Line};
use std::{collections::BTreeMap, io};

type Key = (String, String, String);

#[derive(Default)]
pub(super) struct Observations {
    peers: BTreeMap<(String, String), u64>,
    pub(super) lines: BTreeMap<Key, Vec<Line>>,
}

impl Observations {
    pub(super) fn connection(&self, scope: &Bind) -> Option<u64> {
        self.peers
            .get(&(
                scope.host_id.as_str().into(),
                scope.host_instance.as_str().into(),
            ))
            .copied()
    }

    pub(super) fn connected(&mut self, host: &str, instance: &str, connection: u64) {
        self.peers.retain(|(held, _), _| held != host);
        self.lines.retain(|(held, _, _), _| held != host);
        self.peers
            .insert((host.into(), instance.into()), connection);
    }

    pub(super) fn disconnected(&mut self, host: &str, instance: &str, connection: u64) {
        if self.peers.get(&(host.into(), instance.into())) == Some(&connection) {
            self.peers.remove(&(host.into(), instance.into()));
            self.lines
                .retain(|(held, peer, _), _| held != host || peer != instance);
        }
    }

    pub(super) fn forget(&mut self, host: &str, instance: &str, lines: &[String]) {
        self.lines.retain(|(held, peer, line), _| {
            held != host || peer != instance || !lines.contains(line)
        });
    }

    pub(super) fn for_transfer(&self, scope: &Bind, line: &str) -> Vec<Line> {
        self.lines
            .get(&(
                scope.host_id.as_str().into(),
                scope.host_instance.as_str().into(),
                line.into(),
            ))
            .cloned()
            .unwrap_or_default()
    }

    pub(super) fn remember(
        &mut self,
        scope: &Bind,
        connection: Option<u64>,
        lines: impl IntoIterator<Item = Line>,
        entries: &[Delegation],
    ) -> io::Result<()> {
        if self.connection(scope) != connection {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "read-witness connection changed",
            ));
        }
        let mut next = Self {
            peers: self.peers.clone(),
            lines: self.lines.clone(),
        };
        for line in lines {
            let rows = next
                .lines
                .entry((
                    scope.host_id.as_str().into(),
                    scope.host_instance.as_str().into(),
                    line.owner.line.as_str().into(),
                ))
                .or_default();
            if let Some(previous) = rows.iter_mut().find(|old| old.origin == line.origin) {
                *previous = line;
            } else {
                rows.push(line);
            }
        }
        next.prune(entries);
        let host = scope.host_id.as_str();
        let own = next
            .lines
            .iter()
            .filter(|((held, _, _), _)| held == host)
            .collect::<Vec<_>>();
        if own.len() > plakous_runner_protocol::OWNER_FLOOR_QUERY_LIMIT {
            return Err(super::super::invalid(
                "host read-witness line capacity exceeded (256); dispose observed groups before retrying",
            ));
        }
        if own
            .iter()
            .any(|(_, rows)| rows.len() > plakous_runner_protocol::OWNER_FLOOR_QUERY_LIMIT)
        {
            return Err(super::super::invalid(
                "line read-witness origin capacity exceeded (256); finish or consume jobs before retrying",
            ));
        }
        let rows = own
            .iter()
            .flat_map(|(_, rows)| rows.iter())
            .collect::<Vec<_>>();
        if serde_json::to_vec(&rows)
            .map_err(super::super::invalid)?
            .len()
            > 16 * plakous_runner_protocol::retention::MAX_FRAME_BYTES
        {
            return Err(super::super::invalid(
                "host read-witness byte capacity exceeded (16 MiB); dispose observed groups before retrying",
            ));
        }
        self.lines = next.lines;
        Ok(())
    }

    pub(super) fn prune(&mut self, entries: &[Delegation]) {
        self.lines.retain(|(host, _, _), rows| {
            rows.retain_mut(|line| {
                line.jobs.retain(|job| {
                    !line.lost
                        && !job.terminal
                        && !entries.iter().any(|entry| {
                            entry.host == *host
                                && entry.inventory.origin == line.origin
                                && (entry.inventory.lost
                                    || entry.allocations.contains(&job.session)
                                    || entry.consumed.contains(&job.session))
                        })
                });
                !line.jobs.is_empty()
            });
            !rows.is_empty()
        });
    }
}
