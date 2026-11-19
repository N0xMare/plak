use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::shipping_artifact::{self as shipping, FileFacts, Manifest, NAMES, check, fail, io};
use crate::{CliResult, ProcessOutcome, snapshot_install};

use super::artifacts::ClientManifest;
use super::inventory::{self, Node, Route, Stage};
use super::probe::{self, Target, UNIT_DRIFT};
use super::protocol::{
    Call, Canary, Candidate, ClientOp, ClientSpec, Connection, Health, HostOp, Observed, Reply,
    Request, StageIdentity,
};

pub(crate) fn serve(input: &mut dyn Read, output: &mut dyn Write) -> CliResult<ProcessOutcome> {
    let mut input = BufReader::new(input);
    let mut header = Vec::new();
    Read::by_ref(&mut input)
        .take(shipping::LIMIT + 1)
        .read_until(b'\n', &mut header)
        .map_err(io)?;
    let result = if header.len() as u64 > shipping::LIMIT || !header.ends_with(b"\n") {
        Err(fail("SSH request exceeds its size bound"))
    } else {
        probe::decode(&header).and_then(|request| execute(request, &mut input))
    };
    let reply = match result {
        Ok(reply) => Reply::Ok(reply),
        Err(error) => Reply::Error(error.message().to_owned()),
    };
    output
        .write_all(&shipping::canonical(&reply)?)
        .map_err(io)?;
    Ok(ProcessOutcome::Success)
}

fn execute(request: Request, body: &mut dyn Read) -> CliResult<serde_json::Value> {
    match request.call {
        Call::Host { host, op } => {
            let target = Target::new(&host, request.operator_machine.as_deref())?;
            check(
                request.stage != Stage::Staging
                    || matches!(op, HostOp::Observe)
                    || !target.production_history(),
                "target has production history; use an isolated staging host",
            )?;
            if !matches!(op, HostOp::StageHost { .. }) {
                eof(body)?;
            }
            execute_host(&target, op, body)
        }
        Call::Client { node, client, op } => {
            probe::validate_node(&node)?;
            let client = ClientTarget::new(&node, client)?;
            if !matches!(op, ClientOp::Stage) {
                eof(body)?;
            }
            execute_client(&client, op, body)
        }
    }
}

fn execute_host(
    target: &Target<'_>,
    op: HostOp,
    body: &mut dyn Read,
) -> CliResult<serde_json::Value> {
    match op {
        HostOp::Observe => data(&target.observe()?.1),
        HostOp::Health { expected, routes } => data(&health(target, &expected, &routes)?),
        HostOp::CanaryPreflight { expected, route } => {
            qualify(target, &current(target, &expected)?, &route)?;
            data(&())
        }
        HostOp::Canary { expected, route } => data(&run_canary(target, &expected, &route)?),
        HostOp::StageHost {
            manifest,
            manifest_sha256,
        } => data(&stage_host(manifest.as_bytes(), &manifest_sha256, body)?),
        HostOp::DestinationUnit { candidate } => {
            check_unit(target, &staged(&candidate)?)?;
            data(&())
        }
        HostOp::DestinationCanaryPreflight { candidate, route } => {
            qualify(target, &staged(&candidate)?, &route)?;
            data(&())
        }
        HostOp::Upgrade {
            candidate,
            drain,
            previous,
        } => {
            upgrade(target, &candidate, &previous, drain)?;
            data(&())
        }
        HostOp::HostLink {
            candidate,
            instance,
        } => {
            staged(&candidate)?;
            let (_, observed) = target.observe()?;
            check(
                observed.sha256 == candidate.sha256 && observed.instance == instance,
                "replacement is no longer Ready",
            )?;
            snapshot_install::link_snapshot(&candidate.snapshot)?;
            data(&())
        }
        HostOp::MarkProduction { expected } => {
            mark_production(target, &expected)?;
            data(&())
        }
    }
}

fn data(result: &impl Serialize) -> CliResult<serde_json::Value> {
    serde_json::to_value(result).map_err(|_| fail("cannot encode target response"))
}

fn staged(candidate: &Candidate) -> CliResult<PathBuf> {
    let binary = snapshot_install::verified_snapshot(&candidate.snapshot)?.join("plakd");
    check(
        FileFacts::read(&binary)?.sha256 == candidate.sha256,
        "staged binary digest differs",
    )?;
    Ok(binary)
}

fn current(target: &Target<'_>, expected: &str) -> CliResult<PathBuf> {
    let (binary, observed) = target.observe()?;
    check(observed.sha256 == expected, "serving host changed; replan")?;
    Ok(binary)
}

pub(super) fn check_unit(target: &Target<'_>, binary: &Path) -> CliResult<()> {
    check(
        target.render_unit(binary)? == shipping::read(&target.unit_path(), shipping::LIMIT)?,
        UNIT_DRIFT,
    )
}

fn upgrade(
    target: &Target<'_>,
    candidate: &Candidate,
    previous: &Observed,
    drain: u32,
) -> CliResult<()> {
    check(drain <= 120, "invalid drain bound")?;
    check(
        &target.observe()?.1 == previous,
        "serving facts changed before handoff",
    )?;
    let binary = staged(candidate)?;
    check_handoff_image(target, &binary, &previous.facts.state_key)?;
    let expected = candidate
        .sha256
        .get(..12)
        .ok_or_else(|| fail("invalid expected binary"))?;
    target.product(
        &binary,
        [
            "host",
            "upgrade",
            "--expect-binary",
            expected,
            "--drain",
            &format!("{drain}s"),
        ],
        720 + u64::from(drain),
    )?;
    Ok(())
}

pub(super) fn check_handoff_image(target: &Target<'_>, binary: &Path, key: &str) -> CliResult<()> {
    check_unit(target, binary)?;
    let found = target.product(binary, ["version", "--state-key"], 30)?;
    check(
        probe::line(&found)? == key,
        "cross-key: staged binary does not match the serving state key; use the settled host upgrade runbook",
    )
}

fn stage_host(raw: &[u8], pin: &str, body: &mut dyn Read) -> CliResult<PathBuf> {
    check(
        shipping::digest(raw) == pin,
        "transferred manifest digest differs",
    )?;
    let manifest = Manifest::parse(raw)?;
    let work = tempfile::tempdir().map_err(io)?;
    let bootstrap = crate::build_identity::running_executable()
        .ok_or_else(|| fail("cannot identify staged installer image"))?;
    let dest = work.path().join("plakd");
    let mut output = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o500)
        .open(&dest)
        .map_err(io)?;
    shipping::copy(bootstrap, &mut output, &manifest.files["plakd"])?;
    drop(output);
    for name in NAMES.into_iter().skip(1) {
        receive(body, &work.path().join(name), &manifest.files[name])?;
    }
    eof(body)?;
    fs::write(work.path().join("manifest.json"), raw).map_err(io)?;
    snapshot_install::install_dir(work.path(), true)
}

fn receive(body: &mut dyn Read, path: &Path, facts: &FileFacts) -> CliResult<()> {
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o500)
        .open(path)
        .map_err(io)?;
    let amount = std::io::copy(&mut body.take(facts.length), &mut file).map_err(io)?;
    check(
        amount == facts.length,
        "transferred artifact length differs",
    )?;
    facts.verify(path)
}

fn eof(input: &mut dyn Read) -> CliResult<()> {
    check(
        input.read(&mut [0]).map_err(io)? == 0,
        "trailing SSH transfer data",
    )
}

fn mark_production(target: &Target<'_>, expected: &StageIdentity) -> CliResult<()> {
    check(
        &target.stage_identity() == expected,
        "target stage identity changed",
    )?;
    let path = target.production_marker();
    snapshot_install::require_owned(snapshot_install::parent(&path)?)?;
    let bytes = shipping::canonical(expected)?;
    match OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(&path)
    {
        Ok(mut file) => {
            file.write_all(&bytes).map_err(io)?;
            file.sync_all().map_err(io)?;
            File::open(snapshot_install::parent(&path)?)
                .and_then(|dir| dir.sync_all())
                .map_err(io)
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => check(
            shipping::read(&path, shipping::LIMIT)? == bytes,
            "invalid production history marker",
        ),
        Err(error) => Err(io(error)),
    }
}

pub(super) fn health(target: &Target<'_>, expected: &str, routes: &[Route]) -> CliResult<Health> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Bindings {
        compatible: bool,
        count: u64,
    }

    let (binary, observed) = target.observe()?;
    check(observed.sha256 == expected, "serving host changed; replan")?;
    let actual: BTreeSet<_> = observed
        .accounts
        .iter()
        .filter(|(_, status)| *status != "not_enrolled")
        .map(|(account, _)| account.as_str())
        .collect();
    check(
        actual == routes.iter().map(|route| route.account.as_str()).collect(),
        "in-use gateway accounts changed; replan",
    )?;
    let doctor = target.product(&binary, ["doctor"], 45)?;
    let doctor = probe::text(&doctor)?;
    check(
        !doctor.trim().is_empty()
            && !doctor
                .lines()
                .any(|line| line.split('\t').skip(1).any(|word| word == "ERROR")),
        "doctor reported an error",
    )?;
    let report = target.product(
        &binary,
        ["session", "check-binding", "--all", "--summary"],
        75,
    )?;
    let bindings: Bindings = probe::decode(&report)?;
    check(
        bindings.compatible,
        "session check-binding --all is not compatible",
    )?;
    let generations = routes
        .iter()
        .map(|route| Ok((route.account.clone(), generation(target, &binary, route)?)))
        .collect::<CliResult<BTreeMap<_, _>>>()?;
    Ok(Health {
        doctor: "pass".into(),
        status: "pass".into(),
        bindings: "pass".into(),
        binding_count: bindings.count,
        generations,
        canaries: None,
    })
}

fn generation(target: &Target<'_>, binary: &Path, route: &Route) -> CliResult<Option<u64>> {
    inventory::account(&route.account)?;
    let status = target.product(binary, ["status", &route.account], 30)?;
    let account = format!("{}: ", route.account);
    let ready = probe::text(&status)?
        .lines()
        .filter_map(|line| line.strip_prefix(&account))
        .any(|fields| {
            fields
                .split_whitespace()
                .any(|word| word == "credential=ready")
        });
    check(ready, "credential not Ready for enrolled account")?;
    if !matches!(route.kind(), "codex" | "claude" | "grok") {
        return Ok(None);
    }
    let raw = target.product(binary, ["deploy-credential-generation", &route.account], 30)?;
    let value = probe::line(&raw)?
        .parse()
        .map_err(|_| fail("unavailable OAuth credential generation"))?;
    Ok(Some(value))
}

fn canary_args(route: &Route, preflight: bool) -> Vec<String> {
    let mut args = vec![
        "deploy-canary".to_owned(),
        format!("--account={}", route.account),
        format!("--model={}", route.model),
    ];
    if !route.effort.is_empty() {
        args.push(format!("--effort={}", route.effort));
    }
    if preflight {
        args.push("--preflight".into());
    }
    args
}

fn qualify(target: &Target<'_>, binary: &Path, route: &Route) -> CliResult<()> {
    check(
        target
            .canary_routes(binary)
            .is_some_and(|kinds| kinds.iter().any(|kind| kind == route.kind())),
        "route has no bounded canary adapter",
    )?;
    target.product(binary, canary_args(route, true), 30)?;
    Ok(())
}

fn run_canary(target: &Target<'_>, expected: &str, route: &Route) -> CliResult<Canary> {
    let binary = current(target, expected)?;
    qualify(target, &binary, route)?;
    let before = generation(target, &binary, route)?;
    let result = target.product(&binary, canary_args(route, false), 105)?;
    check(
        result == b"plakd-deploy-canary-pass\n",
        "invalid canary terminal/cleanup verdict",
    )?;
    Ok(Canary {
        adapter: "bounded_cli_v1".into(),
        generation_before: before,
        generation_after: generation(target, &binary, route)?,
    })
}

struct ClientTarget<'a> {
    node: &'a Node,
    link: PathBuf,
    blob: PathBuf,
    source: String,
    manifest: ClientManifest,
}

impl<'a> ClientTarget<'a> {
    fn new(node: &'a Node, client: ClientSpec) -> CliResult<Self> {
        inventory::absolute(&client.link)?;
        check(
            client.link.parent() == Some(node.home.join(".local/bin").as_path())
                && client
                    .link
                    .file_name()
                    .is_some_and(|name| !NAMES.iter().any(|product| *product == name)),
            "client link must be a dedicated ~/.local/bin link",
        )?;
        let manifest = ClientManifest::read(client.manifest.as_bytes(), &client.manifest_sha256)?;
        let blob = node
            .home
            .join(".local/lib/plak/clients")
            .join(&manifest.file().sha256)
            .join("client");
        Ok(Self {
            node,
            blob,
            manifest,
            link: client.link,
            source: client.source_sha256,
        })
    }

    fn observe(&self) -> CliResult<bool> {
        snapshot_install::require_owned(snapshot_install::parent(&self.link)?)?;
        let link = self.link.canonicalize().map_err(io)?;
        let found = FileFacts::read(&link)?;
        let want = self.manifest.file();
        check(
            self.link.is_symlink() && (found.sha256 == self.source || found == want),
            "unexpected client link digest; inspect and re-enroll",
        )?;
        Ok(found == want && link == self.blob)
    }

    fn stage(&self, body: &mut dyn Read) -> CliResult<()> {
        let temporary = tempfile::tempdir().map_err(io)?;
        let source = temporary.path().join("client");
        let facts = self.manifest.file();
        receive(body, &source, &facts)?;
        eof(body)?;
        let parent = snapshot_install::parent(&self.blob)?;
        snapshot_install::ensure(parent)?;
        snapshot_install::require_owned(parent)?;
        let mut temp = tempfile::NamedTempFile::new_in(parent).map_err(io)?;
        shipping::copy(&source, &mut temp, &facts)?;
        temp.as_file()
            .set_permissions(fs::Permissions::from_mode(0o500))
            .map_err(io)?;
        temp.as_file().sync_all().map_err(io)?;
        match temp.persist_noclobber(&self.blob) {
            Ok(_) => {}
            Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(io(error.error)),
        }
        facts.verify(&self.blob)
    }

    fn connection(&self, connection: &Connection) -> CliResult<()> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Verdict {
            connected: bool,
            binary_id: String,
            wire: shipping::Wire,
        }

        snapshot_install::require_owned(snapshot_install::parent(&self.blob)?)?;
        let image = fs::symlink_metadata(&self.blob).map_err(io)?;
        check(
            image.uid() == self.node.uid && image.mode() & 0o7777 == 0o500,
            "unexpected client executable owner or mode",
        )?;
        self.manifest.file().verify(&self.blob)?;
        check(
            connection.wire == self.manifest.wire,
            "serving host is not an exact wire match for this client",
        )?;
        let mut command = probe::command(
            self.blob.as_os_str(),
            self.node,
            &self.node.home.join(".plak"),
        );
        command.arg("--check-connection");
        let raw = probe::capture(command, 30)?;
        let reply: Verdict = probe::decode(&raw)?;
        check(
            reply.connected
                && reply.binary_id == connection.binary_id
                && reply.wire == connection.wire,
            "client did not prove its exact host connection",
        )
    }
}

fn execute_client(
    target: &ClientTarget<'_>,
    op: ClientOp,
    body: &mut dyn Read,
) -> CliResult<serde_json::Value> {
    match op {
        ClientOp::Stage => target.stage(body)?,
        ClientOp::Link { connection } => {
            target.observe()?;
            target.connection(&connection)?;
            snapshot_install::atomic_link(&target.link, &target.blob)?;
        }
        ClientOp::Check { connection } => {
            check(target.observe()?, "client link did not converge")?;
            target.connection(&connection)?;
        }
    }
    data(&())
}
