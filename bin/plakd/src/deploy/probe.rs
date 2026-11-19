use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs;
use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::os::unix::fs::{FileTypeExt as _, MetadataExt as _};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;

use crate::CliResult;
use crate::operator_process;
use crate::shipping_artifact::{
    self as shipping, BuildInfo, FileFacts, NAMES, Upgrade, Wire, check, fail, io,
};

use super::inventory::{self, Host, Node};
use super::protocol::{Export, Facts, Observed, RunnerTriple, StageIdentity};
use super::ssh;

const UNIT: &str = "plakd-host.service";
pub(super) const NORMALIZE: &str = "host unit is not the generated stable-link unit: verify ~/.local/bin/plakd resolves to the running snapshot, move the pinned ExecStart drop-in aside, then run systemctl --user daemon-reload; do not restart";
pub(super) const UNIT_DRIFT: &str = "candidate unit template differs: review the candidate's host install --dry-run output and normalize the unit using the manual same-key procedure before retrying";
const GATEWAY_IMAGE: &str = "gateway image change: restore the enrolled gateway or use S3";
const CUSTODY: &str = "custody change: restore the enrolled local gateway; use the custody runbook";

pub(super) struct Target<'a> {
    pub(super) host: &'a Host,
    pub(super) machine: String,
    proc_root: PathBuf,
}

#[derive(Deserialize)]
struct Probe {
    protocol: u32,
    receipt_version: u32,
    binary_id: String,
    state_key: String,
    export_versions: Vec<u32>,
    host_available: bool,
    runner_protocol: u32,
    runner_version: RunnerTriple,
    config_error: Option<String>,
}

#[derive(Debug, Deserialize, Eq, PartialEq)]
struct Identity {
    protocol: u32,
    key: String,
    instance: String,
    binary: String,
    #[serde(default)]
    deploy_idle: Option<bool>,
}

struct Gateway {
    instance: String,
    versions: Vec<u32>,
    custody: String,
    accounts: Vec<String>,
}

pub(super) fn validate_node(node: &Node) -> CliResult<()> {
    node.validate()?;
    check(
        std::env::consts::OS == "linux" && std::env::consts::ARCH == "x86_64",
        "target must be Linux x86_64",
    )?;
    let uid = rustix::process::geteuid().as_raw();
    check(uid == node.uid && uid != 0, "unexpected SSH effective UID")?;
    let inherited =
        crate::environment::inherited_home().ok_or_else(|| fail("SSH HOME is unavailable"))?;
    let mut login = operator_process::command("/usr/bin/id".as_ref());
    login.env_clear().arg("-un");
    check(
        Path::new(&inherited) == node.home
            && node.home.canonicalize().map_err(io)? == node.home
            && fs::metadata(&node.home).map_err(io)?.uid() == uid
            && line(&capture(login, 5)?)? == node.login,
        "unexpected SSH login or home",
    )
}

impl<'a> Target<'a> {
    pub(super) fn new(host: &'a Host, operator: Option<&str>) -> CliResult<Self> {
        host.validate()?;
        validate_node(&host.node)?;
        let machine = ssh::machine_id()?;
        check(
            operator != Some(machine.as_str()),
            "run deploy from an operator machine independent of the target host",
        )?;
        check(
            host.product_home.canonicalize().map_err(io)? == host.product_home
                && fs::metadata(&host.product_home).map_err(io)?.uid() == host.node.uid,
            "unexpected product home",
        )?;
        Ok(Self {
            host,
            machine,
            proc_root: PathBuf::from("/proc"),
        })
    }

    pub(super) fn stage_identity(&self) -> StageIdentity {
        StageIdentity {
            machine_sha256: self.machine.clone(),
            product_home: self.host.product_home.clone(),
        }
    }

    pub(super) fn production_marker(&self) -> PathBuf {
        self.host
            .node
            .home
            .join(".local/lib/plak/deploy-production")
    }

    pub(super) fn production_history(&self) -> bool {
        !fs::symlink_metadata(self.production_marker())
            .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
    }

    pub(super) fn unit_path(&self) -> PathBuf {
        self.host.node.home.join(".config/systemd/user").join(UNIT)
    }

    pub(super) fn render_unit(&self, binary: &Path) -> CliResult<Vec<u8>> {
        let web = format!("unix:{}", self.host.web_socket.display());
        self.product(
            binary,
            [
                "host".as_ref(),
                "install".as_ref(),
                "--dry-run".as_ref(),
                "--web".as_ref(),
                web.as_ref(),
                "--ui".as_ref(),
                self.host.ui.as_os_str(),
            ],
            30,
        )
    }

    pub(super) fn product(
        &self,
        binary: &Path,
        args: impl IntoIterator<Item = impl AsRef<OsStr>>,
        seconds: u64,
    ) -> CliResult<Vec<u8>> {
        let mut cmd = command(binary.as_os_str(), &self.host.node, &self.host.product_home);
        cmd.args(args);
        capture(cmd, seconds)
    }

    pub(super) fn observe(&self) -> CliResult<(PathBuf, Observed)> {
        let unit = self.unit_path();
        let host_props = self.properties(
            UNIT,
            &[
                "ActiveState",
                "MainPID",
                "FragmentPath",
                "DropInPaths",
                "NeedDaemonReload",
            ],
        )?;
        check(
            host_props
                .get("FragmentPath")
                .is_some_and(|path| Path::new(path) == unit)
                && host_props.get("DropInPaths").is_some_and(String::is_empty)
                && host_props
                    .get("NeedDaemonReload")
                    .is_some_and(|value| value == "no"),
            NORMALIZE,
        )?;
        let pid = active_pid(&host_props)?;
        let process = self.proc_root.join(pid).join("exe");
        let binary = process.canonicalize().map_err(io)?;
        let link = ssh::stable(&self.host.node);
        let linked = link.canonicalize().map_err(io)?;
        check(
            self.snapshot_binary(&binary) && link.is_symlink() && self.snapshot_binary(&linked),
            NORMALIZE,
        )?;
        let image = FileFacts::read(&binary)?;
        let unit = shipping::read(&unit, shipping::LIMIT)?;
        check(self.render_unit(&binary)? == unit, NORMALIZE)?;
        let facts = self.facts(&binary)?;
        let identity = self.identity()?;
        check(
            identity.binary == image.display_id
                && identity.binary == facts.binary_id
                && identity
                    .instance
                    .split_once('-')
                    .is_some_and(|(owner, nonce)| owner == pid && shipping::hex(nonce, 16))
                && identity.key == facts.state_key
                && identity.protocol == facts.host_upgrade.coordinator,
            "systemd image and coordinator identity differ",
        )?;
        self.welcome(&facts)?;
        let gateway = self.gateway(&binary)?;
        let accounts = self.accounts(&binary, &gateway.accounts)?;
        let paired = self.paired()?;
        let canary = self.canary_routes(&binary);
        check(
            self.identity()? == identity
                && process.canonicalize().map_err(io)? == binary
                && FileFacts::read(&binary)? == image,
            "host changed during observation",
        )?;
        Ok((
            binary,
            Observed {
                sha256: image.sha256,
                link_sha256: FileFacts::read(&linked)?.sha256,
                link_sidecars: link_sidecars(&link)?,
                instance: identity.instance,
                facts,
                unit_sha256: shipping::digest(&unit),
                custody_sha256: gateway.custody,
                gateway_instance: gateway.instance,
                gateway_versions: gateway.versions,
                stage_identity: self.stage_identity(),
                paired,
                canary,
                accounts,
                upgrade_idle: identity.deploy_idle == Some(true),
                production_history: self.production_history(),
            },
        ))
    }

    fn snapshot_binary(&self, path: &Path) -> bool {
        path.file_name() == Some("plakd".as_ref())
            && path.parent().and_then(Path::parent).is_some_and(|root| {
                ["release", "maintainer"]
                    .into_iter()
                    .any(|kind| root == self.host.node.home.join(".local/lib/plak").join(kind))
            })
    }

    pub(super) fn facts(&self, binary: &Path) -> CliResult<Facts> {
        let raw = self.product(binary, ["version", "--host-upgrade-probe"], 30)?;
        let probe: Probe = decode(&raw)?;
        let key = self.product(binary, ["version", "--state-key"], 30)?;
        let export = self.product(binary, ["version", "--export-version"], 30)?;
        let wire = self.product(binary, ["wire", "schema"], 30)?;
        let mut facts =
            facts_from_probes(probe, line(&key)?, line(&export)?, &wire, "legacy facts")?;
        if let Ok(raw) = self.product(binary, ["version", "--build-info"], 30) {
            let info: BuildInfo = decode(&raw)?;
            check(
                info.state_key == facts.state_key
                    && info.host_upgrade == facts.host_upgrade
                    && info.wire == facts.wire
                    && info.export.read == facts.export.read
                    && info.export.write == facts.export.write
                    && info.runner.protocol == facts.runner_triple.protocol
                    && info.revision == facts.runner_triple.source_revision
                    && info.flavor == facts.runner_triple.flavor,
                "build-info disagrees with independent product probes",
            )?;
            facts.source = "build_info".to_owned();
        }
        Ok(facts)
    }

    pub(super) fn canary_routes(&self, binary: &Path) -> Option<Vec<String>> {
        let raw = self
            .product(binary, ["deploy-canary", "--capabilities"], 30)
            .ok()?;
        let mut lines = std::str::from_utf8(&raw).ok()?.lines();
        (lines.next()? == "plakd-deploy-canary-v1").then(|| lines.map(str::to_owned).collect())
    }

    fn paired(&self) -> CliResult<bool> {
        let config = self.host.product_home.join("config.toml");
        if !config.exists() {
            return Ok(false);
        }
        let data = shipping::read(&config, shipping::LIMIT)?;
        let text = std::str::from_utf8(&data).map_err(|_| fail("host config is not UTF-8"))?;
        let config: toml::Table =
            toml::from_str(text).map_err(|_| fail("cannot inspect host runners"))?;
        Ok(config
            .get("runners")
            .and_then(toml::Value::as_table)
            .is_some_and(|runners| !runners.is_empty()))
    }

    fn properties(&self, unit: &str, names: &[&str]) -> CliResult<BTreeMap<String, String>> {
        let mut cmd = command(
            "systemctl".as_ref(),
            &self.host.node,
            &self.host.product_home,
        );
        cmd.args(["--user", "show", unit])
            .args(names.iter().map(|name| format!("--property={name}")));
        let result = capture(cmd, 30)?;
        text(&result)?
            .lines()
            .map(|line| {
                let (key, value) = line
                    .split_once('=')
                    .ok_or_else(|| fail("invalid systemd property reply"))?;
                Ok((key.to_owned(), value.to_owned()))
            })
            .collect()
    }

    fn gateway(&self, binary: &Path) -> CliResult<Gateway> {
        let expected = format!("via gateway unix:{}", self.host.gateway_socket.display());
        let status = self
            .product(binary, ["gateway", "status"], 30)
            .map_err(|_| fail(CUSTODY))?;
        let status = text(&status)?;
        check(status.lines().any(|line| line == expected), CUSTODY)?;
        let instance = status
            .lines()
            .find_map(|line| line.strip_prefix("instance "))
            .filter(|instance| uuid::Uuid::parse_str(instance).is_ok())
            .ok_or_else(|| fail("gateway instance is unavailable"))?;
        let raw = self.product(binary, ["gateway", "check"], 30)?;
        let (versions, accounts) = gateway_reply(text(&raw)?, &expected, instance)?;
        let props = self.properties(
            &self.host.gateway_unit,
            &["ActiveState", "MainPID", "ExecStart", "NeedDaemonReload"],
        )?;
        let pid = active_pid(&props)?;
        let process = self
            .proc_root
            .join(pid)
            .join("exe")
            .canonicalize()
            .map_err(io)?;
        check(
            FileFacts::read(&process)?.sha256 == self.host.gateway_sha256,
            GATEWAY_IMAGE,
        )?;
        let start = props
            .get("ExecStart")
            .and_then(|row| row.strip_prefix("{ path="))
            .and_then(|row| row.split_once(" ; argv[]="))
            .map(|row| Path::new(row.0))
            .filter(|path| path.is_absolute())
            .and_then(|path| path.canonicalize().ok());
        check(
            start.as_ref() == Some(&process)
                && props
                    .get("NeedDaemonReload")
                    .is_some_and(|value| value == "no"),
            "gateway image change: ExecStart does not resolve to the running image; restore the enrolled gateway or use S3",
        )?;
        let socket = fs::metadata(&self.host.gateway_socket).map_err(|_| fail(CUSTODY))?;
        check(socket.file_type().is_socket(), CUSTODY)?;
        let configs = ["config.toml", "gateway.toml"]
            .into_iter()
            .map(|name| {
                let file = self.host.product_home.join(name);
                file.exists()
                    .then(|| {
                        shipping::read(&file, shipping::LIMIT).map(|bytes| shipping::digest(&bytes))
                    })
                    .transpose()
            })
            .collect::<CliResult<Vec<_>>>()?;
        let mut custody = shipping::canonical(&(configs, socket.dev(), socket.ino(), pid))?;
        custody.pop();
        Ok(Gateway {
            instance: instance.to_owned(),
            versions,
            custody: shipping::digest(&custody),
            accounts,
        })
    }

    fn accounts(&self, binary: &Path, listed: &[String]) -> CliResult<BTreeMap<String, String>> {
        let output = self.product(binary, ["status"], 30)?;
        let custody = format!("via gateway unix:{}", self.host.gateway_socket.display());
        let contents = text(&output)?;
        check(contents.lines().any(|line| line == custody), CUSTODY)?;
        account_rows(contents, listed)
    }

    fn identity(&self) -> CliResult<Identity> {
        let socket = connect(&self.host.product_home.join("host/upgrade.sock"))?;
        frame(socket)
    }

    fn welcome(&self, facts: &Facts) -> CliResult<()> {
        #[derive(Deserialize)]
        struct Welcome {
            frame: String,
            proto: u32,
            binary_id: String,
            state_key: String,
        }
        let mut socket = connect(
            &self
                .host
                .product_home
                .join("host/run")
                .join(format!("{}.sock", facts.state_key)),
        )?;
        writeln!(
            socket,
            "{{\"frame\":\"hello\",\"proto\":{},\"binary_id\":\"{}\",\"caps\":[]}}",
            facts.wire.number, facts.binary_id
        )
        .map_err(io)?;
        let found: Welcome = frame(socket)?;
        check(
            found.frame == "welcome"
                && found.proto == facts.wire.number
                && found.binary_id == facts.binary_id
                && found.state_key == facts.state_key
                && fs::metadata(&self.host.web_socket)
                    .map_err(io)?
                    .file_type()
                    .is_socket(),
            "host wire Welcome does not identify the serving coordinator",
        )
    }
}

fn facts_from_probes(
    probe: Probe,
    key: &str,
    export: &str,
    wire: &[u8],
    source: &str,
) -> CliResult<Facts> {
    let release: u32 = export.parse().map_err(|_| fail("invalid export probe"))?;
    check(
        probe.host_available && probe.config_error.is_none(),
        "serving image cannot qualify its host config",
    )?;
    check(
        probe.state_key == key
            && crate::carry::upgrade::state_key_valid(key)
            && shipping::hex(&probe.binary_id, 12)
            && probe.runner_protocol == probe.runner_version.protocol
            && probe.export_versions.contains(&release),
        "inconsistent serving build facts",
    )?;
    let mut schema: serde_json::Value = decode(wire)?;
    let schema_object = schema
        .as_object_mut()
        .ok_or_else(|| fail("invalid wire schema"))?;
    check(
        schema_object
            .remove("binary_id")
            .and_then(|id| id.as_str().map(str::to_owned))
            .as_deref()
            == Some(probe.binary_id.as_str()),
        "wire schema identifies a different installed binary",
    )?;
    let number = schema_object
        .get("wire_version")
        .and_then(serde_json::Value::as_u64)
        .and_then(|number| u32::try_from(number).ok())
        .filter(|number| *number > 0)
        .ok_or_else(|| fail("invalid wire version"))?;
    let mut raw = shipping::canonical(&schema)?;
    raw.pop();
    Ok(Facts {
        source: source.to_owned(),
        binary_id: probe.binary_id,
        state_key: probe.state_key,
        host_upgrade: Upgrade {
            coordinator: probe.protocol,
            receipt: probe.receipt_version,
        },
        runner_triple: probe.runner_version,
        wire: Wire {
            number,
            schema_pin: shipping::digest(&raw),
        },
        export: Export {
            read: probe.export_versions,
            write: release,
        },
    })
}

pub(super) fn legacy(bytes: &[u8]) -> CliResult<Facts> {
    let mut rows = text(bytes)?.splitn(4, '\n');
    let probe = rows
        .next()
        .ok_or_else(|| fail("legacy facts unavailable"))?;
    let key = rows
        .next()
        .ok_or_else(|| fail("legacy state key unavailable"))?;
    let export = rows
        .next()
        .ok_or_else(|| fail("legacy export version unavailable"))?;
    let wire = rows
        .next()
        .ok_or_else(|| fail("legacy wire schema unavailable"))?;
    facts_from_probes(
        decode(probe.as_bytes())?,
        key,
        export,
        wire.as_bytes(),
        "legacy facts",
    )
}

fn account_rows(status: &str, listed: &[String]) -> CliResult<BTreeMap<String, String>> {
    let mut accounts = BTreeMap::new();
    for line in status.lines() {
        let Some((account, detail)) = line.split_once(": ") else {
            continue;
        };
        if !listed.iter().any(|known| known == account) {
            continue;
        }
        let state = detail
            .split_whitespace()
            .find_map(|field| field.strip_prefix("credential="))
            .ok_or_else(|| fail("cannot enumerate all gateway account statuses"))?;
        let state = if state == "signed-out" {
            "not_enrolled"
        } else {
            state
        };
        check(
            matches!(
                state,
                "not_enrolled"
                    | "ready"
                    | "refresh-required"
                    | "refreshing"
                    | "quarantined"
                    | "login-required"
                    | "profile-changed"
                    | "written-by-newer-build"
                    | "revoked"
            ),
            "unknown gateway account readiness",
        )?;
        check(
            accounts
                .insert(account.to_owned(), state.to_owned())
                .is_none(),
            "duplicate gateway account status",
        )?;
    }
    check(
        accounts.len() == listed.len(),
        "cannot enumerate all gateway account statuses",
    )?;
    Ok(accounts)
}

fn gateway_reply(raw: &str, custody: &str, instance: &str) -> CliResult<(Vec<u32>, Vec<String>)> {
    check(raw.lines().any(|line| line == custody), CUSTODY)?;
    let prefix = format!("gateway {instance} protocol ");
    let mut versions: Vec<u32> = raw
        .lines()
        .find_map(|line| line.strip_prefix(&prefix))
        .ok_or_else(|| fail("cannot prove unchanged gateway holder/protocol"))?
        .split(',')
        .map(|version| {
            version
                .parse()
                .map_err(|_| fail("invalid gateway protocol"))
        })
        .collect::<CliResult<_>>()?;
    versions.sort_unstable();
    versions.dedup();
    check(
        versions.first().is_some_and(|value| *value > 0),
        "invalid gateway protocol",
    )?;
    let mut accounts = Vec::new();
    for account in raw.lines().filter_map(|row| row.strip_prefix("account ")) {
        inventory::account(account)?;
        check(
            !accounts.iter().any(|name| name == account),
            "duplicate gateway account",
        )?;
        accounts.push(account.to_owned());
    }
    Ok((versions, accounts))
}

fn active_pid(props: &BTreeMap<String, String>) -> CliResult<&str> {
    check(
        props
            .get("ActiveState")
            .is_some_and(|state| state == "active"),
        "expected an active user service",
    )?;
    props
        .get("MainPID")
        .filter(|pid| pid.parse::<u32>().is_ok_and(|pid| pid > 0))
        .map(String::as_str)
        .ok_or_else(|| fail("missing systemd MainPID"))
}

fn link_sidecars(link: &Path) -> CliResult<BTreeMap<String, String>> {
    let mut result = BTreeMap::new();
    for name in NAMES.into_iter().skip(1) {
        let worker = link.with_file_name(name);
        if worker.is_symlink() {
            result.insert(
                name.to_owned(),
                FileFacts::read(&worker.canonicalize().map_err(io)?)?.sha256,
            );
        }
    }
    Ok(result)
}

fn connect(path: &Path) -> CliResult<UnixStream> {
    let stream = UnixStream::connect(path).map_err(io)?;
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(io)?;
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .map_err(io)?;
    Ok(stream)
}

fn frame<T: serde::de::DeserializeOwned>(stream: UnixStream) -> CliResult<T> {
    let mut buffer = Vec::new();
    BufReader::new(stream)
        .take(shipping::LIMIT + 1)
        .read_until(b'\n', &mut buffer)
        .map_err(io)?;
    check(
        buffer.len() as u64 <= shipping::LIMIT && buffer.ends_with(b"\n"),
        "invalid service identity frame",
    )?;
    decode(&buffer)
}

pub(super) fn command(binary: &OsStr, node: &Node, product_home: &Path) -> std::process::Command {
    let runtime = format!("/run/user/{}", node.uid);
    let mut cmd = operator_process::command(binary);
    cmd.env_clear()
        .env("HOME", &node.home)
        .env("PLAK_HOME", product_home)
        .env(
            "PATH",
            format!(
                "{}/.local/bin:/usr/local/bin:/usr/bin:/bin",
                node.home.display()
            ),
        )
        .env("LANG", "C.UTF-8")
        .env("XDG_RUNTIME_DIR", &runtime)
        .env(
            "DBUS_SESSION_BUS_ADDRESS",
            format!("unix:path={runtime}/bus"),
        )
        .current_dir(&node.home);
    cmd
}

pub(super) fn capture(cmd: std::process::Command, seconds: u64) -> CliResult<Vec<u8>> {
    let reply =
        operator_process::capture(cmd, None, Duration::from_secs(seconds), shipping::LIMIT)?;
    check(
        reply.status.success(),
        "target command failed; inspect the documented command on the target",
    )?;
    Ok(reply.stdout)
}

pub(super) fn decode<T: serde::de::DeserializeOwned>(raw: &[u8]) -> CliResult<T> {
    serde_json::from_slice(raw).map_err(|_| fail("target returned invalid typed data"))
}

pub(super) fn text(raw: &[u8]) -> CliResult<&str> {
    std::str::from_utf8(raw).map_err(|_| fail("target returned non-UTF-8 data"))
}

pub(super) fn line(raw: &[u8]) -> CliResult<&str> {
    let text = text(raw)?;
    text.strip_suffix('\n')
        .filter(|line| !line.contains('\n'))
        .ok_or_else(|| fail("expected one product reply line"))
}

#[cfg(test)]
mod tests;
