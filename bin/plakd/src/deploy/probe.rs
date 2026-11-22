use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs;
use std::io::{BufRead, BufReader, Read as _, Write as _};
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

use super::inventory::{self, Host, Node, Platform};
use super::protocol::{
    AccountIdentity, Export, Facts, HostImages, Observed, RunnerPlacement, RunnerTriple,
    StageIdentity,
};
use super::ssh;
use super::timing;
use super::{DeployRunnerInspection, DeployRunnerVerdict, DeployRunnerWarning};

const UNIT: &str = "plakd-host.service";
const HOST_AGENT: &str = "com.plakd.host";
pub(super) const NORMALIZE: &str = "host unit is not the generated stable-link unit: verify ~/.local/bin/plakd resolves to the running snapshot, move the pinned ExecStart drop-in aside, then run systemctl --user daemon-reload; do not restart";
pub(super) const UNIT_DRIFT: &str = "candidate unit template differs: review the candidate's host install --dry-run output and normalize the unit using the manual same-key procedure before retrying";
const CUSTODY: &str = "custody change: restore the enrolled local gateway; use the custody runbook";

mod gateway;

pub(super) struct Target<'a> {
    pub(super) host: &'a Host,
    pub(super) machine: String,
    proc_root: PathBuf,
    launchd_tools: Option<(&'a Path, &'a Path)>,
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

pub(super) fn validate_node(node: &Node, operator: Option<&str>) -> CliResult<AccountIdentity> {
    node.validate()?;
    check(
        Platform::native().is_some_and(|native| {
            native == node.platform || native.is_linux() && node.platform.is_linux()
        }),
        "unexpected target platform",
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
            && line(&capture(login, timing::LOGIN)?)? == node.login,
        "unexpected SSH login or home",
    )?;
    let machine_sha256 = ssh::machine_id(node.platform)?;
    check(
        operator != Some(machine_sha256.as_str()),
        "run deploy from an operator machine independent of the targets",
    )?;
    Ok(AccountIdentity {
        machine_sha256,
        platform: node.platform,
        uid,
        home: node.home.clone(),
    })
}

impl<'a> Target<'a> {
    pub(super) fn new(host: &'a Host, operator: Option<&str>) -> CliResult<Self> {
        host.validate()?;
        let account = validate_node(&host.node, operator)?;
        check(
            host.product_home.canonicalize().map_err(io)? == host.product_home
                && fs::metadata(&host.product_home).map_err(io)?.uid() == host.node.uid,
            "unexpected product home",
        )?;
        Ok(Self {
            host,
            machine: account.machine_sha256,
            proc_root: PathBuf::from("/proc"),
            launchd_tools: None,
        })
    }

    pub(super) fn launchctl(&self) -> &Path {
        if let Some((tool, _)) = self.launchd_tools {
            return tool;
        }
        "/bin/launchctl".as_ref()
    }

    pub(super) fn plutil(&self) -> &Path {
        if let Some((_, tool)) = self.launchd_tools {
            return tool;
        }
        "/usr/bin/plutil".as_ref()
    }

    pub(super) fn stage_identity(&self) -> StageIdentity {
        StageIdentity {
            machine_sha256: self.machine.clone(),
            platform: self.host.node.platform,
            uid: self.host.node.uid,
            home: self.host.node.home.clone(),
            product_home: self.host.product_home.clone(),
        }
    }

    pub(super) fn production_history(&self) -> bool {
        production_history(&self.host.node)
    }

    pub(super) fn unit_path(&self) -> PathBuf {
        match self.host.node.platform {
            Platform::Linux | Platform::LinuxGnu => {
                self.host.node.home.join(".config/systemd/user").join(UNIT)
            }
            Platform::Mac => self.plist(HOST_AGENT),
        }
    }

    fn plist(&self, label: &str) -> PathBuf {
        self.host
            .node
            .home
            .join("Library/LaunchAgents")
            .join(format!("{label}.plist"))
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
            timing::PROBE,
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

    pub(super) fn product_reply(
        &self,
        binary: &Path,
        args: impl IntoIterator<Item = impl AsRef<OsStr>>,
        seconds: u64,
    ) -> CliResult<operator_process::Output> {
        let mut cmd = command(binary.as_os_str(), &self.host.node, &self.host.product_home);
        cmd.args(args);
        operator_process::capture(cmd, None, Duration::from_secs(seconds), shipping::LIMIT)
    }

    fn host_pid(&self) -> CliResult<String> {
        if self.host.node.platform == Platform::Mac {
            return super::launchd::observe_service(self, HOST_AGENT, &self.unit_path())?
                .pid
                .ok_or_else(|| fail("expected an active LaunchAgent"));
        }
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
        Ok(active_pid(&host_props)?.to_owned())
    }

    fn process_image(&self, pid: &str) -> CliResult<PathBuf> {
        match self.host.node.platform {
            Platform::Linux | Platform::LinuxGnu => {
                let exe = self.proc_root.join(pid).join("exe");
                let path = exe.canonicalize().map_err(io)?;
                let running = fs::metadata(exe).map_err(io)?;
                let found = fs::metadata(&path).map_err(io)?;
                check(
                    running.dev() == found.dev() && running.ino() == found.ino(),
                    "running executable inode differs from the image on disk",
                )?;
                Ok(path)
            }
            Platform::Mac => super::launchd::process_image(self, pid),
        }
    }

    pub(super) fn observe(&self) -> CliResult<(PathBuf, Observed)> {
        let pid = self.host_pid()?;
        let (binary, linked) = self.image_paths(&pid)?;
        let link = ssh::stable(&self.host.node);
        let image = FileFacts::read(&binary)?;
        let unit = shipping::read(&self.unit_path(), shipping::LIMIT)?;
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
            "service process image and coordinator identity differ",
        )?;
        self.welcome(&facts)?;
        let (gateway, listed) = self.observe_gateway(&binary, &facts.gateway_spoken)?;
        let accounts = self.accounts(&binary, &listed)?;
        let paired = self.paired()?;
        let canary = self.canary_routes(&binary)?;
        check(
            self.identity()? == identity
                && self.process_image(&pid)? == binary
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
                gateway,
                stage_identity: self.stage_identity(),
                paired,
                canary,
                accounts,
                upgrade_idle: identity.deploy_idle == Some(true),
                production_history: self.production_history(),
            },
        ))
    }

    pub(super) fn host_images(&self) -> CliResult<HostImages> {
        let pid = self.running_host_pid()?;
        let (binary, linked) = self.image_paths(&pid)?;
        let image = FileFacts::read(&binary)?;
        let link = FileFacts::read(&linked)?;
        check(
            self.running_host_pid()? == pid && self.image_paths(&pid)? == (binary, linked),
            "host changed during runner withdrawal observation",
        )?;
        Ok(HostImages {
            sha256: image.sha256,
            link_sha256: link.sha256,
            stage_identity: self.stage_identity(),
            production_history: self.production_history(),
        })
    }

    fn running_host_pid(&self) -> CliResult<String> {
        match self.host.node.platform {
            Platform::Mac => super::launchd::running_pid(self, HOST_AGENT),
            Platform::Linux | Platform::LinuxGnu => {
                Ok(active_pid(&self.properties(UNIT, &["ActiveState", "MainPID"])?)?.to_owned())
            }
        }
    }

    fn image_paths(&self, pid: &str) -> CliResult<(PathBuf, PathBuf)> {
        let binary = self.process_image(pid)?;
        let link = ssh::stable(&self.host.node);
        let linked = link.canonicalize().map_err(io)?;
        check(
            self.snapshot_binary(&binary) && link.is_symlink() && self.snapshot_binary(&linked),
            NORMALIZE,
        )?;
        Ok((binary, linked))
    }

    fn snapshot_binary(&self, path: &Path) -> bool {
        path.file_name() == Some("plakd".as_ref())
            && path.parent().and_then(Path::parent).is_some_and(|root| {
                crate::snapshot_install::physical_path(&self.host.node.home.join(".local/lib/plak"))
                    .is_ok_and(|installed| {
                        ["release", "maintainer"]
                            .into_iter()
                            .any(|kind| root == installed.join(kind))
                    })
            })
    }

    pub(super) fn facts(&self, binary: &Path) -> CliResult<Facts> {
        let raw = self.product(binary, ["version", "--host-upgrade-probe"], timing::PROBE)?;
        let probe: Probe = decode(&raw)?;
        let key = self.product(binary, ["version", "--state-key"], timing::PROBE)?;
        let export = self.product(binary, ["version", "--export-version"], timing::PROBE)?;
        let wire = self.product(binary, ["wire", "schema"], timing::PROBE)?;
        let mut facts =
            facts_from_probes(probe, line(&key)?, line(&export)?, &wire, "legacy facts")?;
        if let Ok(raw) = self.product(binary, ["version", "--build-info"], timing::PROBE) {
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
            facts.gateway_spoken = info.gateway.spoken;
        }
        Ok(facts)
    }

    pub(super) fn canary_routes(&self, binary: &Path) -> CliResult<Option<Vec<String>>> {
        let mut cmd = command(binary.as_os_str(), &self.host.node, &self.host.product_home);
        cmd.args(["deploy-canary", "--capabilities"]);
        let reply = operator_process::capture(
            cmd,
            None,
            Duration::from_secs(timing::PROBE),
            shipping::LIMIT,
        )?;
        if reply.stdout.is_empty() && reply.status.code() == Some(2) {
            return Ok(None);
        }
        let mut lines = text(&reply.stdout)?.lines();
        check(
            reply.status.success() && lines.next() == Some("plakd-deploy-canary-v1"),
            "invalid deploy-canary --capabilities reply; use a matching deploy build",
        )?;
        Ok(Some(lines.map(str::to_owned).collect()))
    }

    fn paired(&self) -> CliResult<BTreeMap<String, RunnerPlacement>> {
        #[derive(Deserialize)]
        struct Placements {
            #[serde(default)]
            runners: BTreeMap<String, RunnerPlacement>,
        }
        let config = self.host.product_home.join("config.toml");
        if fs::symlink_metadata(&config)
            .is_err_and(|err| err.kind() == std::io::ErrorKind::NotFound)
        {
            return Ok(BTreeMap::new());
        }
        let data = shipping::read(&config, shipping::LIMIT)?;
        let text = std::str::from_utf8(&data).map_err(|_| fail("host config is not UTF-8"))?;
        let cfg: Placements =
            toml::from_str(text).map_err(|_| fail("cannot inspect host runners"))?;
        Ok(cfg.runners)
    }

    fn properties(&self, unit: &str, names: &[&str]) -> CliResult<BTreeMap<String, String>> {
        let mut cmd = command(
            "systemctl".as_ref(),
            &self.host.node,
            &self.host.product_home,
        );
        cmd.args(["--user", "show", unit])
            .args(names.iter().map(|name| format!("--property={name}")));
        let result = capture(cmd, timing::PROBE)?;
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

    fn accounts(&self, binary: &Path, listed: &[String]) -> CliResult<BTreeMap<String, String>> {
        let output = self.product(binary, ["status"], timing::PROBE)?;
        let custody = format!("via gateway unix:{}", self.host.gateway_socket.display());
        let contents = text(&output)?;
        check(contents.lines().any(|line| line == custody), CUSTODY)?;
        account_rows(contents, listed)
    }

    fn identity(&self) -> CliResult<Identity> {
        let socket = connect(&self.host.product_home.join("host/upgrade.sock"))?;
        frame(socket)
    }

    pub(super) fn runner_preflight(
        &self,
        observed: &Observed,
    ) -> CliResult<Vec<DeployRunnerWarning>> {
        let mut socket = connect(&self.host.product_home.join("host/upgrade.sock"))?;
        let mut reader = BufReader::new(socket.try_clone().map_err(io)?);
        let identity: Identity = read_frame(&mut reader)?;
        check(
            identity.instance == observed.instance
                && identity.binary == observed.facts.binary_id
                && identity.deploy_idle == Some(true),
            "source coordinator changed or is not idle; replan",
        )?;
        reader
            .get_ref()
            .set_read_timeout(Some(Duration::from_secs(timing::RUNNER_INSPECTION)))
            .map_err(io)?;
        let request = DeployRunnerInspection {
            source_instance: identity.instance,
            deploy_inspect_runners: observed.paired.keys().cloned().collect(),
        };
        socket
            .write_all(&shipping::canonical(&request)?)
            .map_err(io)?;
        let answer: DeployRunnerVerdict = read_frame(&mut reader)
            .map_err(|_| fail("source cannot prove paired-runner inspection; use a source with deploy runner inspection or the manual paired upgrade"))?;
        match answer {
            DeployRunnerVerdict::Settled { warnings } => Ok(warnings),
            DeployRunnerVerdict::Blocked { detail } => Err(fail(&detail)),
        }
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
        gateway_spoken: Vec::new(),
    })
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
        .set_read_timeout(Some(Duration::from_secs(timing::SOCKET)))
        .map_err(io)?;
    stream
        .set_write_timeout(Some(Duration::from_secs(timing::SOCKET)))
        .map_err(io)?;
    Ok(stream)
}

fn frame<T: serde::de::DeserializeOwned>(stream: UnixStream) -> CliResult<T> {
    read_frame(&mut BufReader::new(stream))
}

fn read_frame<T: serde::de::DeserializeOwned>(reader: &mut impl BufRead) -> CliResult<T> {
    let mut buffer = Vec::new();
    reader
        .take(shipping::LIMIT + 1)
        .read_until(b'\n', &mut buffer)
        .map_err(io)?;
    check(
        buffer.len() as u64 <= shipping::LIMIT && buffer.ends_with(b"\n"),
        "invalid service identity frame",
    )?;
    decode(&buffer)
}

pub(super) fn production_marker(node: &Node) -> PathBuf {
    node.home.join(".local/lib/plak/deploy-production")
}

pub(super) fn production_history(node: &Node) -> bool {
    !fs::symlink_metadata(production_marker(node))
        .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
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
        .env(
            "LANG",
            if node.platform == Platform::Mac {
                "en_US.UTF-8"
            } else {
                "C.UTF-8"
            },
        )
        .current_dir(&node.home);
    if node.platform.is_linux() {
        cmd.env("XDG_RUNTIME_DIR", &runtime).env(
            "DBUS_SESSION_BUS_ADDRESS",
            format!("unix:path={runtime}/bus"),
        );
    }
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
