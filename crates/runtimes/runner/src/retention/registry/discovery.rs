use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use plakous_runner_protocol::retention::{Bind, Id, Line, Origin};

use super::super::{self as retention, Client, Codec, loss};
use super::{Registry, append, inspect_lines};
use loss::{NativeHolder, SocketIdentity, native_holder, native_lock_free};

struct Endpoint {
    build: Id,
    version: u32,
    socket: PathBuf,
}

struct Refused {
    endpoint: Endpoint,
    socket: Option<SocketIdentity>,
    error: io::Error,
}

enum Probe {
    Inventory(Origin, Vec<Line>, Option<u32>),
    Unreachable(io::Error),
}

pub(super) async fn current(
    registry: &Registry,
    scope: &Bind,
    own: &Origin,
    current: u32,
) -> io::Result<Vec<Line>> {
    let builds = &registry.builds;
    let mut originals = BTreeMap::new();
    // Saved provenance remains authoritative occupancy during an outage.
    for line in registry.inventory(scope.host_id.as_str()) {
        append(&mut originals, line, false);
    }
    let mut answered = BTreeMap::new();
    let mut deferred = Vec::new();
    for endpoint in endpoints(builds, own)? {
        let socket = SocketIdentity::at(&endpoint.socket).ok();
        match endpoint.probe(builds, scope, current).await? {
            Probe::Inventory(origin, lines, pid) => {
                for line in lines {
                    let direct = line.origin == origin;
                    append(&mut originals, line, direct);
                }
                if let Some(holder) = native_holder(builds, origin.build.as_str())
                    && pid == Some(holder.pid)
                {
                    answered.insert(holder, origin);
                }
            }
            Probe::Unreachable(error) => {
                if !native_lock_free(builds, endpoint.build.as_str()) {
                    deferred.push(Refused {
                        endpoint,
                        socket,
                        error,
                    });
                }
            }
        }
    }
    verify_refusals(builds, scope, current, &answered, deferred).await?;
    Ok(originals.into_values().map(|(line, _)| line).collect())
}

async fn verify_refusals(
    builds: &Path,
    scope: &Bind,
    current: u32,
    answered: &BTreeMap<NativeHolder, Origin>,
    deferred: Vec<Refused>,
) -> io::Result<()> {
    let mut proven = BTreeMap::new();
    for refused in deferred {
        let build = refused.endpoint.build.as_str();
        if native_lock_free(builds, build) {
            continue;
        }
        let Some(holder) = native_holder(builds, build) else {
            return Err(refused.error);
        };
        let Some(sibling) = answered
            .get(&holder)
            .filter(|other| other.build != refused.endpoint.build)
        else {
            return Err(refused.error);
        };
        // A legacy lock may have aliases from many builds. Only a fresh
        // answer from the distinct, authenticated native lock holder proves
        // that the refused socket did not own that lock.
        let proof = if let Some(proof) = proven.get(&holder) {
            *proof
        } else {
            let proof = proves_sibling(builds, scope, current, sibling, holder).await;
            proven.insert(holder, proof);
            proof
        };
        if !proof
            || native_holder(builds, build) != Some(holder)
            || refused.socket.is_none()
            || SocketIdentity::at(&refused.endpoint.socket).ok() != refused.socket
        {
            return Err(refused.error);
        }
    }
    Ok(())
}

async fn proves_sibling(
    builds: &Path,
    scope: &Bind,
    current: u32,
    sibling: &Origin,
    holder: NativeHolder,
) -> bool {
    if native_holder(builds, sibling.build.as_str()) != Some(holder) {
        return false;
    }
    let Ok(mut client) =
        Client::inspect(builds, &sibling.build, scope, sibling.version, current).await
    else {
        return false;
    };
    client.origin == *sibling
        && client.peer_pid() == Some(holder.pid)
        && inspect_lines(&mut client).await.is_ok()
        && native_holder(builds, sibling.build.as_str()) == Some(holder)
}

fn endpoints(builds: &Path, own: &Origin) -> io::Result<Vec<Endpoint>> {
    let mut directories = std::fs::read_dir(builds)?.collect::<Result<Vec<_>, _>>()?;
    directories.sort_by_key(std::fs::DirEntry::file_name);
    let mut endpoints = Vec::new();
    for directory in directories {
        let name = directory.file_name();
        let Some(name) = name.to_str().filter(|name| retention::build_key(name)) else {
            continue;
        };
        if name == own.build.as_str() {
            continue;
        }
        let build = Id::try_from(name.to_owned()).map_err(retention::invalid)?;
        retention::private_parent(&directory.path())?;
        for entry in std::fs::read_dir(directory.path())? {
            let entry = entry?;
            let name = entry.file_name();
            let Some(version) = name
                .to_str()
                .and_then(|name| name.strip_prefix("retention-v"))
                .and_then(|name| name.strip_suffix(".sock"))
                .and_then(|version| version.parse::<u32>().ok())
            else {
                continue;
            };
            endpoints.push(Endpoint {
                build: build.clone(),
                version,
                socket: entry.path(),
            });
        }
    }
    Ok(endpoints)
}

impl Endpoint {
    async fn probe(&self, builds: &Path, scope: &Bind, current: u32) -> io::Result<Probe> {
        if Codec::select(self.version, current).is_err() {
            let probe = async {
                retention::check_socket(&std::fs::symlink_metadata(&self.socket)?)?;
                tokio::time::timeout(
                    Duration::from_millis(250),
                    tokio::net::UnixStream::connect(&self.socket),
                )
                .await
                .map_err(retention::timed_out)?
            }
            .await;
            return match probe {
                Err(error) if possibly_ended(&error) => Ok(Probe::Unreachable(error)),
                _ => Err(retention::invalid(
                    "finish/consume/disown jobs through a retained compatible build, then retry",
                )),
            };
        }
        match Client::inspect(builds, &self.build, scope, self.version, current).await {
            Ok(mut client) => {
                let lines = inspect_lines(&mut client).await?;
                Ok(Probe::Inventory(
                    client.origin.clone(),
                    lines,
                    client.peer_pid(),
                ))
            }
            Err(error) if possibly_ended(&error) => Ok(Probe::Unreachable(error)),
            Err(error) => Err(error),
        }
    }
}

fn possibly_ended(error: &io::Error) -> bool {
    // A full listen queue can refuse connections to a live endpoint.
    matches!(
        error.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused | io::ErrorKind::TimedOut
    )
}
