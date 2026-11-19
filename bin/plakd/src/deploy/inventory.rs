use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use clap::ValueEnum;
use serde::{Deserialize, Serialize};

use crate::CliResult;
use crate::shipping_artifact::{self as shipping, check, fail, hex, revision};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, ValueEnum)]
#[serde(rename_all = "snake_case")]
pub(super) enum Stage {
    Staging,
    Production,
}

impl Stage {
    pub(super) fn name(self) -> &'static str {
        match self {
            Self::Staging => "staging",
            Self::Production => "production",
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Inventory {
    pub(super) schema: u32,
    pub(super) repo: PathBuf,
    pub(super) revision: String,
    pub(super) manifests: BTreeMap<Platform, ArtifactPin>,
    staging: Option<StageFile>,
    production: Option<StageFile>,
    #[serde(skip)]
    stages: BTreeMap<&'static str, Config>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub(super) enum Platform {
    #[serde(rename = "x86_64-unknown-linux-musl")]
    Linux,
    #[serde(rename = "aarch64-apple-darwin")]
    Mac,
}

impl Platform {
    pub(super) fn triple(self) -> &'static str {
        match self {
            Self::Linux => "x86_64-unknown-linux-musl",
            Self::Mac => "aarch64-apple-darwin",
        }
    }

    pub(super) fn native() -> Option<Self> {
        match (std::env::consts::OS, std::env::consts::ARCH) {
            ("linux", "x86_64") => Some(Self::Linux),
            ("macos", "aarch64") => Some(Self::Mac),
            _ => None,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ArtifactPin {
    pub(super) artifact: PathBuf,
    pub(super) manifest_sha256: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StageFile {
    machines: Vec<Machine>,
    routes: Vec<Route>,
    drain_seconds: u32,
    evidence: PathBuf,
    receipt: PathBuf,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Machine {
    node: Node,
    host: Option<HostRole>,
    gateway: Option<GatewayRole>,
    #[serde(default)]
    runners: Vec<RunnerRole>,
    #[serde(default)]
    clients: Vec<ClientRole>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct HostRole {
    product_home: PathBuf,
    source_sha256: String,
    web_socket: PathBuf,
    ui: PathBuf,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct GatewayRole {
    service: String,
    socket: PathBuf,
    sha256: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RunnerRole {
    id: String,
    link: PathBuf,
    source_sha256: String,
}

#[derive(Debug)]
struct Config {
    host: Host,
    routes: Vec<Route>,
    runners: Vec<Runner>,
    clients: Vec<Client>,
    drain_seconds: u32,
    evidence: PathBuf,
    receipt: PathBuf,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Host {
    pub(super) node: Node,
    pub(super) product_home: PathBuf,
    pub(super) source_sha256: String,
    pub(super) web_socket: PathBuf,
    pub(super) ui: PathBuf,
    pub(super) gateway_unit: String,
    pub(super) gateway_socket: PathBuf,
    pub(super) gateway_sha256: String,
}

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Node {
    pub(super) ssh: String,
    pub(super) login: String,
    pub(super) home: PathBuf,
    pub(super) uid: u32,
    pub(super) platform: Platform,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Route {
    pub(super) account: String,
    pub(super) model: String,
    pub(super) effort: String,
}

impl Route {
    pub(super) fn kind(&self) -> &str {
        kind(&self.account)
    }
}

pub(super) fn kind(account: &str) -> &str {
    account.split_once('/').map_or(account, |row| row.0)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ClientRole {
    id: String,
    link: PathBuf,
    blob: PathBuf,
    manifest: PathBuf,
    manifest_sha256: String,
    source_sha256: String,
}

#[derive(Debug, Serialize)]
pub(super) struct Client {
    pub(super) id: String,
    pub(super) node: Node,
    pub(super) link: PathBuf,
    pub(super) blob: PathBuf,
    pub(super) manifest: PathBuf,
    pub(super) manifest_sha256: String,
    pub(super) source_sha256: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Runner {
    pub(super) id: String,
    pub(super) node: Node,
    pub(super) link: PathBuf,
    pub(super) source_sha256: String,
}

#[derive(Debug, Serialize)]
pub(super) struct Selected<'a> {
    pub(super) host: &'a Host,
    pub(super) routes: &'a [Route],
    pub(super) clients: &'a [Client],
    pub(super) runners: &'a [Runner],
    pub(super) drain_seconds: u32,
    pub(super) evidence: &'a Path,
    pub(super) receipt: &'a Path,
}

impl Inventory {
    pub(super) fn read(path: &Path) -> CliResult<Self> {
        let bytes = shipping::read(path, shipping::LIMIT)?;
        let text = std::str::from_utf8(&bytes).map_err(|_| fail("inventory is not UTF-8"))?;
        let mut data: Self =
            toml::from_str(text).map_err(|_| fail("invalid strict inventory TOML"))?;
        check(
            data.schema == 2 && revision(&data.revision) && !data.manifests.is_empty(),
            "invalid inventory schema, revision or platform manifests",
        )?;
        absolute(&data.repo)?;
        for pin in data.manifests.values() {
            absolute(&pin.artifact)?;
            check(
                hex(&pin.manifest_sha256, 64),
                "invalid platform manifest digest",
            )?;
        }
        for (stage, file) in [
            (Stage::Staging, data.staging.take()),
            (Stage::Production, data.production.take()),
        ] {
            if let Some(file) = file {
                let config = file.validate()?;
                check(
                    config
                        .select()
                        .nodes()
                        .all(|node| data.manifests.contains_key(&node.platform)),
                    "missing platform manifest for an enrolled machine",
                )?;
                data.stages.insert(stage.name(), config);
            }
        }
        if let (Some(stage), Some(prod)) =
            (data.stages.get("staging"), data.stages.get("production"))
        {
            check(
                stage.evidence != prod.evidence
                    && stage.receipt == prod.receipt
                    && stage.select().nodes().all(|left| {
                        prod.select().nodes().all(|right| {
                            left.ssh != right.ssh
                                || (left.platform == Platform::Mac
                                    && right.platform == Platform::Mac
                                    && left.uid != right.uid
                                    && left.login != right.login
                                    && left.home != right.home)
                        })
                    }),
                "stages must have distinct evidence and logins (only macOS permits separate users on one machine)",
            )?;
        }
        Ok(data)
    }

    pub(super) fn select(&self, stage: Stage) -> CliResult<Selected<'_>> {
        self.stages
            .get(stage.name())
            .map(Config::select)
            .ok_or_else(|| fail("selected stage is not enrolled"))
    }
}

impl Selected<'_> {
    pub(super) fn nodes(&self) -> impl Iterator<Item = &Node> {
        std::iter::once(&self.host.node)
            .chain(self.runners.iter().map(|runner| &runner.node))
            .chain(self.clients.iter().map(|client| &client.node))
    }
}

impl StageFile {
    fn validate(self) -> CliResult<Config> {
        check(
            self.evidence != self.receipt
                && self.drain_seconds <= 120
                && self.routes.len() <= 32
                && self.machines.len() <= 17,
            "invalid stage paths or bounds",
        )?;
        absolute(&self.evidence)?;
        absolute(&self.receipt)?;
        validate_routes(&self.routes)?;
        let mut host = None;
        let mut runners = Vec::new();
        let mut clients = Vec::new();
        let mut accounts = BTreeSet::new();
        for Machine {
            node,
            host: service,
            gateway,
            runners: runner_roles,
            clients: client_roles,
        } in self.machines
        {
            node.validate()?;
            check(
                accounts.insert((node.ssh.clone(), node.login.clone())),
                "duplicate machine login",
            )?;
            match (service, gateway) {
                (Some(role), Some(gateway)) if host.is_none() => {
                    let enrolled = Host {
                        node: node.clone(),
                        product_home: role.product_home,
                        source_sha256: role.source_sha256,
                        web_socket: role.web_socket,
                        ui: role.ui,
                        gateway_unit: gateway.service,
                        gateway_socket: gateway.socket,
                        gateway_sha256: gateway.sha256,
                    };
                    enrolled.validate()?;
                    host = Some(enrolled);
                }
                (None, None) if !runner_roles.is_empty() || !client_roles.is_empty() => {}
                _ => {
                    return Err(fail(
                        "each stage needs one host and its co-located gateway; other logins must have a role",
                    ));
                }
            }
            runners.extend(runner_roles.into_iter().map(|r| Runner {
                node: node.clone(),
                id: r.id,
                link: r.link,
                source_sha256: r.source_sha256,
            }));
            clients.extend(client_roles.into_iter().map(|c| Client {
                node: node.clone(),
                id: c.id,
                link: c.link,
                blob: c.blob,
                manifest: c.manifest,
                manifest_sha256: c.manifest_sha256,
                source_sha256: c.source_sha256,
            }));
        }
        let host = host.ok_or_else(|| fail("stage host is missing"))?;
        check(
            runners.len() <= 8 && clients.len() <= 8,
            "too many runners or clients",
        )?;
        validate_clients(&clients)?;
        let mut ids = BTreeSet::new();
        let mut links: BTreeSet<_> = clients
            .iter()
            .map(|c| (&c.node.ssh, &c.node.login, &c.link))
            .collect();
        for runner in &runners {
            runner.validate()?;
            check(
                ids.insert(&runner.id)
                    && links.insert((&runner.node.ssh, &runner.node.login, &runner.link)),
                "duplicate runner or command link",
            )?;
        }
        Ok(Config {
            host,
            runners,
            clients,
            routes: self.routes,
            evidence: self.evidence,
            receipt: self.receipt,
            drain_seconds: self.drain_seconds,
        })
    }
}

impl Config {
    fn select(&self) -> Selected<'_> {
        Selected {
            host: &self.host,
            routes: &self.routes,
            clients: &self.clients,
            runners: &self.runners,
            drain_seconds: self.drain_seconds,
            evidence: &self.evidence,
            receipt: &self.receipt,
        }
    }
}

impl Host {
    pub(super) fn validate(&self) -> CliResult<()> {
        self.node.validate()?;
        for path in [
            &self.product_home,
            &self.web_socket,
            &self.ui,
            &self.gateway_socket,
        ] {
            absolute(path)?;
        }
        check(
            hex(&self.source_sha256, 64) && hex(&self.gateway_sha256, 64),
            "invalid host image digest",
        )?;
        let valid = match self.node.platform {
            Platform::Linux => {
                self.gateway_unit != "plakd-host.service"
                    && self
                        .gateway_unit
                        .strip_suffix(".service")
                        .is_some_and(identifier)
            }
            Platform::Mac => {
                self.gateway_unit != "com.plakd.host" && identifier(&self.gateway_unit)
            }
        };
        check(valid, "invalid gateway service")
    }
}

impl Node {
    pub(super) fn validate(&self) -> CliResult<()> {
        absolute(&self.home)?;
        check(
            identifier(&self.ssh)
                && !self.login.is_empty()
                && self.login.len() <= 64
                && self
                    .login
                    .bytes()
                    .next()
                    .is_some_and(|byte| byte.is_ascii_lowercase() || byte == b'_')
                && self.login.bytes().all(|byte| {
                    byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"_-".contains(&byte)
                })
                && (1..u32::MAX).contains(&self.uid),
            "invalid SSH host, login or UID",
        )
    }
}

impl Runner {
    pub(super) fn validate(&self) -> CliResult<()> {
        self.node.validate()?;
        dedicated_link(&self.node, &self.link)?;
        check(
            crate::product_config::validate_runner_name(&self.id).is_ok()
                && self
                    .link
                    .to_str()
                    .is_some_and(crate::product_config::is_runner_command)
                && hex(&self.source_sha256, 64),
            "invalid paired runner pin",
        )
    }
}

pub(super) fn dedicated_link(node: &Node, link: &Path) -> CliResult<()> {
    absolute(link)?;
    check(
        link.parent() == Some(node.home.join(".local/bin").as_path())
            && link
                .file_name()
                .is_some_and(|name| !shipping::NAMES.iter().any(|product| *product == name)),
        "expected a dedicated command link under ~/.local/bin",
    )
}

fn validate_routes(routes: &[Route]) -> CliResult<()> {
    let mut accounts = BTreeSet::new();
    for route in routes {
        account(&route.account)?;
        check(accounts.insert(&route.account), "duplicate canary account")?;
        check(
            !route.model.is_empty()
                && route.model.len() <= 128
                && route
                    .model
                    .bytes()
                    .next()
                    .is_some_and(|b| b.is_ascii_alphanumeric())
                && route
                    .model
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_./:-".contains(&b))
                && matches!(route.effort.as_str(), "" | "none" | "low"),
            "invalid canary model or effort",
        )?;
    }
    Ok(())
}

fn validate_clients(clients: &[Client]) -> CliResult<()> {
    let mut ids = BTreeSet::new();
    let mut links = BTreeSet::new();
    for client in clients {
        client.node.validate()?;
        for path in [&client.link, &client.blob, &client.manifest] {
            absolute(path)?;
        }
        dedicated_link(&client.node, &client.link)?;
        check(
            identifier(&client.id)
                && ids.insert(&client.id)
                && links.insert((&client.node.ssh, &client.node.login, &client.link))
                && hex(&client.manifest_sha256, 64)
                && hex(&client.source_sha256, 64),
            "invalid or duplicate client pin",
        )?;
    }
    Ok(())
}

pub(super) fn account(value: &str) -> CliResult<()> {
    value
        .parse::<plakous_hosted::account::AccountRef>()
        .map_err(|_| fail("invalid gateway account"))?;
    Ok(())
}

pub(super) fn absolute(path: &Path) -> CliResult<()> {
    let text = path
        .to_str()
        .ok_or_else(|| fail("expected a UTF-8 absolute path"))?;
    check(
        path.is_absolute()
            && !text.chars().any(char::is_control)
            && (text == "/"
                || text[1..]
                    .split('/')
                    .all(|part| !matches!(part, "" | "." | ".."))),
        "expected a normalized absolute path",
    )
}

fn identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_alphanumeric())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_.-".contains(&byte))
}
