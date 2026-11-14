//! Durable direct-worker references belong to this build, never to a foreign
//! native store. Intent is synced before any remote owner CAS is attempted.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};

use plakous_runner_protocol::retention::{Bind, Line, Origin, Owner};
use serde::{Deserialize, Serialize};

mod observations;
use observations::Observations;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Delegation {
    pub(crate) host: String,
    pub(crate) holder: String,
    pub(crate) from: Owner,
    pub(crate) to: Owner,
    source: Owner,
    proven: Owner,
    previous: Vec<Owner>,
    pub(crate) inventory: Line,
    pub(crate) confirmed: bool,
    pub(crate) allocations: BTreeSet<plakous_runner_protocol::retention::Id>,
    consumed: BTreeSet<plakous_runner_protocol::retention::Id>,
    pub(crate) controls: BTreeSet<plakous_runner_protocol::retention::Id>,
    socket: Option<super::loss::SocketIdentity>,
}

impl Delegation {
    pub(crate) fn permits_observed(&self, owner: &Owner) -> bool {
        owner == &self.proven || owner == &self.to || self.previous.contains(owner)
    }

    pub(crate) fn permits_native_from(&self, owner: &plakous_runner_protocol::LineOwner) -> bool {
        [&self.source, &self.from, &self.to]
            .into_iter()
            .chain(&self.previous)
            .any(|allowed| super::native_owner(allowed) == *owner)
    }
}

#[derive(Clone, Copy)]
pub(crate) enum ControlReceipt {
    Sent,
    Acknowledged,
}

pub(crate) struct Registry {
    pub(crate) builds: PathBuf,
    path: PathBuf,
    entries: Mutex<Vec<Delegation>>,
    // Read witnesses are not authority. A transfer consumes these once so an
    // original disappearing after the read still reserves occupancy before CAS.
    observed: Mutex<Observations>,
}

impl Registry {
    pub(crate) fn open(directory: &Path) -> io::Result<Self> {
        super::private_parent(directory)?;
        let path = directory.join("retained-jobs.json");
        let entries = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => {
                use std::os::unix::fs::MetadataExt as _;
                if !metadata.is_file()
                    || metadata.uid() != rustix::process::geteuid().as_raw()
                    || metadata.mode() & 0o077 != 0
                {
                    return Err(super::invalid(
                        "retention inventory must be an owned private file",
                    ));
                }
                serde_json::from_slice(&std::fs::read(&path)?).map_err(super::invalid)?
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Vec::new(),
            Err(error) => return Err(error),
        };
        Ok(Self {
            builds: directory
                .parent()
                .ok_or_else(|| super::invalid("build directory"))?
                .into(),
            path,
            entries: Mutex::new(entries),
            observed: Mutex::default(),
        })
    }

    pub(crate) async fn call(
        &self,
        scope: &Bind,
        origin: &Origin,
        call: &plakous_runner_protocol::retention::Call,
    ) -> io::Result<plakous_runner_protocol::retention::Answer> {
        // Registry-owned, request-scoped clients do not occupy an original's
        // bounded connection slots while idle. This is not a native bind and
        // never resends a possibly dispatched control after I/O loss.
        let result = async {
            let mut client = super::Client::connect(&self.builds, origin, scope).await?;
            client.call(call).await
        }
        .await;
        if result.is_err() {
            let socket = lock(&self.entries)
                .iter()
                .find(|entry| {
                    entry.host == scope.host_id.as_str() && entry.inventory.origin == *origin
                })
                .and_then(|entry| entry.socket);
            if super::loss::positive(&self.builds, origin, socket) {
                return Err(super::loss::error());
            }
        }
        result
    }

    pub(crate) async fn discover(&self, scope: &Bind, own: &Origin) -> io::Result<Vec<Line>> {
        self.discover_current(scope, own, plakous_runner_protocol::retention::VERSION)
            .await
    }

    pub(super) async fn discover_current(
        &self,
        scope: &Bind,
        own: &Origin,
        current: u32,
    ) -> io::Result<Vec<Line>> {
        let mut originals = BTreeMap::new();
        // Saved provenance remains authoritative occupancy during an outage.
        for line in self.inventory(scope.host_id.as_str()) {
            append(&mut originals, line, false);
        }
        let mut directories = std::fs::read_dir(&self.builds)?.collect::<Result<Vec<_>, _>>()?;
        directories.sort_by_key(std::fs::DirEntry::file_name);
        for directory in directories {
            let name = directory.file_name();
            let Some(name) = name.to_str().filter(|name| super::build_key(name)) else {
                continue;
            };
            if name == own.build.as_str() {
                continue;
            }
            super::private_parent(&directory.path())?;
            for socket in std::fs::read_dir(directory.path())? {
                let socket = socket?;
                let name = socket.file_name();
                let Some(version) = name
                    .to_str()
                    .and_then(|name| name.strip_prefix("retention-v"))
                    .and_then(|name| name.strip_suffix(".sock"))
                    .and_then(|version| version.parse::<u32>().ok())
                else {
                    continue;
                };
                if super::Codec::select(version, current).is_err() {
                    super::check_socket(&std::fs::symlink_metadata(socket.path())?)?;
                    match tokio::time::timeout(
                        std::time::Duration::from_millis(250),
                        tokio::net::UnixStream::connect(socket.path()),
                    )
                    .await
                    {
                        Ok(Err(error))
                            if matches!(
                                error.kind(),
                                io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound
                            ) =>
                        {
                            continue;
                        }
                        _ => {
                            return Err(super::invalid(
                                "finish/consume/disown jobs through a retained compatible build, then retry",
                            ));
                        }
                    }
                }
                let build = plakous_runner_protocol::retention::Id::try_from(
                    directory.file_name().to_string_lossy().into_owned(),
                )
                .map_err(super::invalid)?;
                let mut client =
                    match super::Client::inspect(&self.builds, &build, scope, version, current)
                        .await
                    {
                        Ok(client) => client,
                        Err(error)
                            if matches!(
                                error.kind(),
                                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
                            ) =>
                        {
                            continue;
                        }
                        Err(error) => return Err(error),
                    };
                for line in inspect_lines(&mut client).await? {
                    let direct = line.origin == client.origin;
                    append(&mut originals, line, direct);
                }
            }
        }
        Ok(originals.into_values().map(|(line, _)| line).collect())
    }

    pub(crate) fn entries(&self, host: &str, line: Option<&str>) -> Vec<Delegation> {
        lock(&self.entries)
            .iter()
            .filter(|entry| {
                entry.host == host && line.is_none_or(|line| entry.to.line.as_str() == line)
            })
            .cloned()
            .collect()
    }

    /// Read the original, not a receiver's cached snapshot. Failure withholds
    /// activation without declaring a worker lost or changing its occupancy.
    pub(crate) async fn fresh_lines(
        &self,
        scope: &Bind,
        own: &Origin,
        lines: &[String],
        connection: u64,
    ) -> io::Result<Vec<Line>> {
        let discovered = self.discover(scope, own).await?;
        self.remember_requested(scope, own, lines, Some(connection), &discovered)?;
        let origins = discovered
            .iter()
            .filter(|line| lines.iter().any(|id| id == line.owner.line.as_str()))
            .filter(|line| line.origin != *own && !line.jobs.is_empty())
            .map(|line| (line.origin.build.clone(), line.origin.clone()))
            .collect::<BTreeMap<_, _>>();
        let mut fresh = Vec::new();
        for origin in origins.into_values() {
            let mut client = super::Client::connect(&self.builds, &origin, scope).await?;
            let direct = inspect_lines(&mut client).await?;
            for previous in discovered.iter().filter(|line| {
                line.origin == origin
                    && !line.jobs.is_empty()
                    && lines.iter().any(|id| id == line.owner.line.as_str())
            }) {
                let current = direct
                    .iter()
                    .find(|line| line.origin == origin && line.owner.line == previous.owner.line)
                    .ok_or_else(|| super::invalid("original worker no longer proves its line"))?;
                if current.lost {
                    return Err(super::invalid("original worker has been lost"));
                }
                fresh.push(current.clone());
                if fresh.len() > plakous_runner_protocol::OWNER_FLOOR_QUERY_LIMIT {
                    return Err(super::invalid("too many retained owner witnesses"));
                }
            }
        }
        self.remember_at(scope, Some(connection), fresh.iter().cloned())?;
        Ok(fresh)
    }

    fn remember_requested(
        &self,
        scope: &Bind,
        own: &Origin,
        requested: &[String],
        connection: Option<u64>,
        discovered: &[Line],
    ) -> io::Result<()> {
        self.remember_at(
            scope,
            connection,
            discovered
                .iter()
                .filter(|line| {
                    line.origin != *own && requested.iter().any(|id| id == line.owner.line.as_str())
                })
                .cloned(),
        )
    }

    fn remember_at(
        &self,
        scope: &Bind,
        connection: Option<u64>,
        lines: impl IntoIterator<Item = Line>,
    ) -> io::Result<()> {
        let entries = lock(&self.entries);
        lock(&self.observed).remember(scope, connection, lines, &entries)
    }

    #[cfg(test)]
    fn remember(&self, scope: &Bind, lines: impl IntoIterator<Item = Line>) -> io::Result<()> {
        let connection = lock(&self.observed).connection(scope);
        self.remember_at(scope, connection, lines)
    }

    pub(crate) fn observation_connection(&self, scope: &Bind) -> Option<u64> {
        lock(&self.observed).connection(scope)
    }

    pub(crate) fn observation_connected(&self, host: &str, instance: &str, connection: u64) {
        lock(&self.observed).connected(host, instance, connection);
    }

    pub(crate) fn observation_disconnected(&self, host: &str, instance: &str, connection: u64) {
        lock(&self.observed).disconnected(host, instance, connection);
    }

    pub(crate) fn forget_observations(&self, host: &str, instance: &str, lines: &[String]) {
        lock(&self.observed).forget(host, instance, lines);
    }

    pub(crate) async fn transfer_inventory(
        &self,
        scope: &Bind,
        own: &Origin,
        line: &str,
    ) -> io::Result<Vec<Line>> {
        let connection = self
            .observation_connection(scope)
            .ok_or_else(|| super::invalid("read-witness connection is no longer bound"))?;
        let discovered = self.discover(scope, own).await?;
        self.remember_requested(scope, own, &[line.into()], Some(connection), &discovered)?;
        let fresh = discovered
            .into_iter()
            .filter(|row| row.owner.line.as_str() == line)
            .collect::<Vec<_>>();
        let observed = lock(&self.observed).for_transfer(scope, line);
        let mut inventory = BTreeMap::new();
        for row in observed {
            append(&mut inventory, row, false);
        }
        for row in fresh {
            append(&mut inventory, row, true);
        }
        Ok(inventory.into_values().map(|(row, _)| row).collect())
    }

    pub(crate) fn all_entries(&self) -> Vec<Delegation> {
        lock(&self.entries).clone()
    }

    pub(crate) fn control_targets(
        &self,
        host: &str,
        instance: &str,
        request_id: &str,
    ) -> Vec<Delegation> {
        self.entries(host, None)
            .into_iter()
            .filter(|entry| {
                entry.holder == instance
                    && !entry.inventory.lost
                    && !entry.inventory.jobs.is_empty()
                    && entry.controls.iter().any(|id| id.as_str() == request_id)
            })
            .collect()
    }

    pub(crate) fn intent(
        &self,
        scope: &Bind,
        from: Owner,
        to: Owner,
        inventory: Line,
    ) -> io::Result<Delegation> {
        self.record_intent(scope, from, to, inventory, None)
    }

    pub(crate) fn retry_intent(
        &self,
        scope: &Bind,
        previous: &Owner,
        from: Owner,
        to: Owner,
        inventory: Line,
    ) -> io::Result<Delegation> {
        self.record_intent(scope, from, to, inventory, Some(previous))
    }

    fn record_intent(
        &self,
        scope: &Bind,
        from: Owner,
        to: Owner,
        inventory: Line,
        previous: Option<&Owner>,
    ) -> io::Result<Delegation> {
        let mut entries = lock(&self.entries);
        let mut next = entries.clone();
        let desired = to.clone();
        let origin = inventory.origin.clone();
        let socket = super::loss::SocketIdentity::observe(&self.builds, &origin).ok();
        let allocations = inventory
            .jobs
            .iter()
            .map(|job| job.session.clone())
            .collect();
        let same = |entry: &&mut Delegation| {
            entry.host == scope.host_id.as_str()
                && entry.to.line == to.line
                && entry.inventory.origin == inventory.origin
        };
        if let Some(existing) = next.iter_mut().find(same) {
            if existing.to == to {
                if existing.holder != scope.host_instance.as_str() {
                    return Err(super::invalid(
                        "retention intent belongs to another host instance",
                    ));
                }
                return Ok(existing.clone());
            }
            if existing.to != from
                && !(previous == Some(&existing.to) && existing.permits_observed(&from))
            {
                return Err(super::invalid(
                    "retention intent cannot leapfrog a foreign owner",
                ));
            }
            let mut prior = existing.previous.clone();
            if !prior.contains(&existing.to) {
                prior.push(existing.to.clone());
            }
            if prior.len() > 64 {
                return Err(super::invalid(
                    "retention owner-intent history exceeds its bound",
                ));
            }
            *existing = Delegation {
                host: scope.host_id.as_str().into(),
                holder: scope.host_instance.as_str().into(),
                source: existing.source.clone(),
                proven: existing.proven.clone(),
                previous: prior,
                from,
                to,
                inventory,
                confirmed: false,
                allocations,
                consumed: existing.consumed.clone(),
                controls: existing.controls.clone(),
                socket: existing.socket.or(socket),
            };
        } else {
            next.push(Delegation {
                host: scope.host_id.as_str().into(),
                holder: scope.host_instance.as_str().into(),
                source: from.clone(),
                proven: from.clone(),
                previous: Vec::new(),
                from,
                to,
                inventory,
                confirmed: false,
                allocations,
                consumed: BTreeSet::new(),
                controls: BTreeSet::new(),
                socket,
            });
        }
        let accepted = next
            .iter()
            .find(|entry| {
                entry.host == scope.host_id.as_str()
                    && entry.to == desired
                    && entry.inventory.origin == origin
            })
            .cloned()
            .ok_or_else(|| super::invalid("retention intent"))?;
        crate::floors::write_atomically(
            &self.path,
            &serde_json::to_vec(&next).map_err(super::invalid)?,
        )?;
        *entries = next;
        lock(&self.observed).prune(&entries);
        Ok(accepted)
    }

    pub(crate) fn update(
        &self,
        scope: &Bind,
        origin: &Origin,
        owner: &Owner,
        jobs: Vec<plakous_runner_protocol::retention::Job>,
        next_number: u32,
    ) -> io::Result<()> {
        let mut entries = lock(&self.entries);
        let mut next = entries.clone();
        let entry = next
            .iter_mut()
            .find(|entry| {
                entry.host == scope.host_id.as_str()
                    && entry.holder == scope.host_instance.as_str()
                    && entry.to == *owner
                    && entry.inventory.origin == *origin
            })
            .ok_or_else(|| super::invalid("retention inventory has no accepted intent"))?;
        let mut jobs: Vec<_> = jobs
            .into_iter()
            .filter(|job| !entry.consumed.contains(&job.session))
            .collect();
        if entry.inventory.lost {
            jobs = entry
                .inventory
                .jobs
                .iter()
                .map(|old| {
                    jobs.iter()
                        .find(|new| {
                            new.session == old.session && new.terminal
                    && old.status == (plakous_runner_protocol::retention::Status::OutcomeUnknown {})
                    && new.status != (plakous_runner_protocol::retention::Status::OutcomeUnknown {})
                        })
                        .unwrap_or(old)
                        .clone()
                })
                .collect();
        }
        let changed = !entry.confirmed
            || entry.inventory.owner != *owner
            || next_number > entry.inventory.next_number
            || entry.inventory.jobs.len() != jobs.len()
            || entry.inventory.jobs.iter().zip(&jobs).any(|(old, new)| {
                old.session != new.session
                    || old.number != new.number
                    || old.status != new.status
                    || old.terminal != new.terminal
                    || old.owner_generation != new.owner_generation
            });
        // A response read before loss/consumption cannot resurrect an allocation
        // after the durable loss fence. Poll-only elapsed/tail refreshes stay in
        // memory; identity, status, floor and consumption changes are synced.
        entry.inventory.jobs = jobs;
        entry
            .allocations
            .extend(entry.inventory.jobs.iter().map(|job| job.session.clone()));
        entry.inventory.next_number = entry.inventory.next_number.max(next_number);
        entry.inventory.owner = owner.clone();
        entry.confirmed = true;
        entry.proven = owner.clone();
        entry.previous.clear();
        entry.inventory.native_disabled = true;
        if changed {
            crate::floors::write_atomically(
                &self.path,
                &serde_json::to_vec(&next).map_err(super::invalid)?,
            )?;
        }
        *entries = next;
        Ok(())
    }

    pub(crate) fn inventory(&self, host: &str) -> Vec<Line> {
        self.entries(host, None)
            .into_iter()
            .flat_map(|entry| {
                let mut line = entry.inventory;
                line.owner = entry.to;
                if line.jobs.is_empty() {
                    return vec![line];
                }
                std::mem::take(&mut line.jobs)
                    .into_iter()
                    .map(|job| Line {
                        jobs: vec![job],
                        ..line.clone()
                    })
                    .collect()
            })
            .collect()
    }

    pub(crate) fn lose(&self, scope: &Bind, origin: &Origin, owner: &Owner) -> io::Result<()> {
        let mut entries = lock(&self.entries);
        let mut next = entries.clone();
        let entry = next
            .iter_mut()
            .find(|entry| {
                entry.host == scope.host_id.as_str()
                    && entry.holder == scope.host_instance.as_str()
                    && entry.to == *owner
                    && entry.inventory.origin == *origin
            })
            .ok_or_else(|| super::invalid("unknown retained allocation"))?;
        entry.inventory.lost = true;
        for job in &mut entry.inventory.jobs {
            if !job.terminal {
                job.status = plakous_runner_protocol::retention::Status::OutcomeUnknown {};
                job.terminal = true;
            }
        }
        crate::floors::write_atomically(
            &self.path,
            &serde_json::to_vec(&next).map_err(super::invalid)?,
        )?;
        *entries = next;
        Ok(())
    }

    pub(crate) fn consume_lost(
        &self,
        scope: &Bind,
        origin: &Origin,
        owner: &Owner,
        numbers: &[u32],
    ) -> io::Result<()> {
        let mut entries = lock(&self.entries);
        let mut next = entries.clone();
        let entry = next
            .iter_mut()
            .find(|entry| {
                entry.host == scope.host_id.as_str()
                    && entry.holder == scope.host_instance.as_str()
                    && entry.to == *owner
                    && entry.inventory.origin == *origin
                    && entry.inventory.lost
            })
            .ok_or_else(|| super::invalid("original worker has not been lost"))?;
        entry.consumed.extend(
            entry
                .inventory
                .jobs
                .iter()
                .filter(|job| numbers.contains(&job.number))
                .map(|job| job.session.clone()),
        );
        entry
            .inventory
            .jobs
            .retain(|job| !numbers.contains(&job.number));
        crate::floors::write_atomically(
            &self.path,
            &serde_json::to_vec(&next).map_err(super::invalid)?,
        )?;
        *entries = next;
        Ok(())
    }

    pub(crate) fn acknowledged(
        &self,
        scope: &Bind,
        origin: &Origin,
        owner: &Owner,
        sessions: &[plakous_runner_protocol::retention::Id],
    ) -> io::Result<()> {
        let mut entries = lock(&self.entries);
        let mut next = entries.clone();
        let entry = next
            .iter_mut()
            .find(|entry| {
                entry.host == scope.host_id.as_str()
                    && entry.holder == scope.host_instance.as_str()
                    && entry.to == *owner
                    && entry.inventory.origin == *origin
            })
            .ok_or_else(|| super::invalid("unknown retained acknowledgement"))?;
        entry.consumed.extend(sessions.iter().cloned());
        entry
            .inventory
            .jobs
            .retain(|job| !entry.consumed.contains(&job.session));
        crate::floors::write_atomically(
            &self.path,
            &serde_json::to_vec(&next).map_err(super::invalid)?,
        )?;
        *entries = next;
        Ok(())
    }

    pub(crate) fn control_receipt(
        &self,
        scope: &Bind,
        origin: &Origin,
        owner: &Owner,
        id: &plakous_runner_protocol::retention::Id,
        state: ControlReceipt,
    ) -> io::Result<()> {
        let mut entries = lock(&self.entries);
        let mut next = entries.clone();
        let entry = next
            .iter_mut()
            .find(|entry| {
                entry.host == scope.host_id.as_str()
                    && entry.holder == scope.host_instance.as_str()
                    && entry.to == *owner
                    && entry.inventory.origin == *origin
            })
            .ok_or_else(|| super::invalid("unknown relayed control"))?;
        match state {
            ControlReceipt::Sent => {
                entry.controls.insert(id.clone());
            }
            ControlReceipt::Acknowledged => {
                entry.controls.remove(id);
            }
        }
        crate::floors::write_atomically(
            &self.path,
            &serde_json::to_vec(&next).map_err(super::invalid)?,
        )?;
        *entries = next;
        Ok(())
    }

    pub(crate) fn occupancy(
        &self,
        included: impl Fn(&str, &str) -> bool,
    ) -> BTreeMap<String, usize> {
        let entries = lock(&self.entries);
        let mut observed = lock(&self.observed);
        observed.prune(&entries);
        let mut allocations = BTreeMap::<String, BTreeSet<_>>::new();
        let mut blocked = BTreeMap::<String, BTreeSet<_>>::new();
        let mut lost = BTreeSet::new();
        for entry in entries.iter() {
            let original = original_key(&entry.host, &entry.inventory.origin);
            blocked
                .entry(original.clone())
                .or_default()
                .extend(entry.consumed.iter().cloned());
            blocked.entry(original.clone()).or_default().extend(
                entry
                    .inventory
                    .jobs
                    .iter()
                    .filter(|job| job.terminal)
                    .map(|job| job.session.clone()),
            );
            if entry.inventory.lost {
                lost.insert(original.clone());
            }
            allocations.entry(original.clone()).or_default();
            for job in entry
                .inventory
                .jobs
                .iter()
                .filter(|_| included(&entry.host, entry.to.line.as_str()))
            {
                if !job.terminal {
                    allocations
                        .entry(original.clone())
                        .or_default()
                        .insert(job.session.clone());
                }
            }
        }
        for ((host, _, line), rows) in &observed.lines {
            if !included(host, line) {
                continue;
            }
            for row in rows {
                let key = original_key(host, &row.origin);
                let counted = allocations.entry(key.clone()).or_default();
                if lost.contains(&key) {
                    continue;
                }
                counted.extend(
                    row.jobs
                        .iter()
                        .filter(|job| {
                            !job.terminal
                                && !blocked
                                    .get(&key)
                                    .is_some_and(|ids| ids.contains(&job.session))
                        })
                        .map(|job| job.session.clone()),
                );
            }
        }
        allocations
            .into_iter()
            .map(|(origin, jobs)| (origin, jobs.len()))
            .collect()
    }
}

fn original_key(host: &str, origin: &Origin) -> String {
    let build = origin.build.as_str();
    let instance = origin.instance.as_str();
    format!(
        "{}:{}{}:{}{}:{}",
        host.len(),
        host,
        build.len(),
        build,
        instance.len(),
        instance
    )
}

type Originals = BTreeMap<
    (
        plakous_runner_protocol::retention::Id,
        plakous_runner_protocol::retention::Id,
        plakous_runner_protocol::retention::Id,
    ),
    (Line, bool),
>;

fn append(originals: &mut Originals, line: Line, direct: bool) {
    let key = (
        line.origin.build.clone(),
        line.origin.instance.clone(),
        line.owner.line.clone(),
    );
    let (previous, authoritative) = originals
        .entry(key)
        .or_insert_with(|| (line.clone(), direct));
    if *authoritative && !direct {
        return;
    }
    if !direct && previous.owner != line.owner {
        if !super::native_owner(&line.owner).is_newer_than(&super::native_owner(&previous.owner)) {
            return;
        }
        *previous = line.clone();
    }
    if direct && !*authoritative {
        *previous = line.clone();
        *authoritative = true;
    }
    previous.owner = line.owner;
    previous.next_number = previous.next_number.max(line.next_number);
    for job in line.jobs {
        if let Some(existing) = previous
            .jobs
            .iter_mut()
            .find(|existing| existing.session == job.session)
        {
            if job.status != (plakous_runner_protocol::retention::Status::OutcomeUnknown {})
                || existing.status
                    == (plakous_runner_protocol::retention::Status::OutcomeUnknown {})
                || !existing.terminal
            {
                *existing = job;
            }
        } else {
            previous.jobs.push(job);
        }
    }
}

async fn inspect_lines(client: &mut super::Client) -> io::Result<Vec<Line>> {
    use plakous_runner_protocol::retention::{Answer, Call, Id, Request};
    let mut inventory = Vec::new();
    let mut after = 0;
    loop {
        let call = Call {
            id: Id::try_from("inventory".to_owned()).map_err(super::invalid)?,
            instance: client.origin.instance.clone(),
            mutation: None,
            request: Request::Lines { after },
        };
        let Answer::Lines { lines, next } = client.call(&call).await? else {
            return Err(super::invalid("retention inventory refused"));
        };
        inventory.extend(lines);
        match next {
            Some(next) if next > after => after = next,
            Some(_) => return Err(super::invalid("retention inventory did not advance")),
            None => return Ok(inventory),
        }
    }
}

fn lock<T>(value: &Mutex<T>) -> MutexGuard<'_, T> {
    value.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests;
