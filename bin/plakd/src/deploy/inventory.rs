use std::collections::BTreeSet;
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
    pub(super) artifact: PathBuf,
    pub(super) manifest_sha256: String,
    staging: Option<Config>,
    production: Option<Config>,
}

#[derive(Debug, Deserialize, Serialize, Default)]
#[serde(deny_unknown_fields)]
pub(super) struct Config {
    pub(super) host: Option<Host>,
    pub(super) routes: Option<Vec<Route>>,
    pub(super) clients: Option<Vec<Client>>,
    pub(super) drain_seconds: Option<u32>,
    pub(super) evidence: Option<PathBuf>,
    pub(super) receipt: Option<PathBuf>,
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

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Node {
    pub(super) ssh: String,
    pub(super) login: String,
    pub(super) home: PathBuf,
    pub(super) uid: u32,
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

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Client {
    pub(super) id: String,
    pub(super) node: Node,
    pub(super) link: PathBuf,
    pub(super) blob: PathBuf,
    pub(super) manifest: PathBuf,
    pub(super) manifest_sha256: String,
    pub(super) source_sha256: String,
}

#[derive(Debug, Serialize)]
pub(super) struct Selected<'a> {
    pub(super) host: &'a Host,
    pub(super) routes: &'a [Route],
    pub(super) clients: &'a [Client],
    pub(super) drain_seconds: u32,
    pub(super) evidence: &'a Path,
    pub(super) receipt: &'a Path,
}

impl Inventory {
    pub(super) fn read(path: &Path) -> CliResult<Self> {
        let bytes = shipping::read(path, shipping::LIMIT)?;
        let text = std::str::from_utf8(&bytes).map_err(|_| fail("inventory is not UTF-8"))?;
        let data: Self = toml::from_str(text).map_err(|_| fail("invalid strict inventory TOML"))?;
        check(
            data.schema == 1 && revision(&data.revision) && hex(&data.manifest_sha256, 64),
            "invalid inventory schema, revision or digest",
        )?;
        for path in [&data.repo, &data.artifact] {
            absolute(path)?;
        }
        let stage = data
            .staging
            .as_ref()
            .map(Config::select)
            .transpose()?
            .flatten();
        let prod = data
            .production
            .as_ref()
            .map(Config::select)
            .transpose()?
            .flatten();
        if let (Some(stage), Some(prod)) = (stage, prod) {
            check(
                stage.host.node.ssh != prod.host.node.ssh
                    && stage.evidence != prod.evidence
                    && stage.receipt == prod.receipt
                    && stage.receipt != stage.evidence
                    && stage.receipt != prod.evidence,
                "staging and production must have distinct hosts/evidence and share only the staging receipt",
            )?;
        }
        Ok(data)
    }

    pub(super) fn select(&self, stage: Stage) -> CliResult<Selected<'_>> {
        let config = match stage {
            Stage::Staging => &self.staging,
            Stage::Production => &self.production,
        };
        config
            .as_ref()
            .map(Config::select)
            .transpose()?
            .flatten()
            .ok_or_else(|| fail("selected stage is not enrolled"))
    }
}

impl Config {
    fn select(&self) -> CliResult<Option<Selected<'_>>> {
        let (host, routes, clients, drain_seconds, evidence, receipt) = (
            self.host.as_ref(),
            self.routes.as_deref(),
            self.clients.as_deref(),
            self.drain_seconds,
            self.evidence.as_deref(),
            self.receipt.as_deref(),
        );
        if matches!(
            (host, routes, clients, drain_seconds, evidence, receipt),
            (None, None, None, None, None, None)
        ) {
            return Ok(None);
        }
        let (
            Some(host),
            Some(routes),
            Some(clients),
            Some(drain_seconds),
            Some(evidence),
            Some(receipt),
        ) = (host, routes, clients, drain_seconds, evidence, receipt)
        else {
            return Err(fail("missing required stage fields"));
        };
        check(
            evidence != receipt && drain_seconds <= 120 && routes.len() <= 32 && clients.len() <= 8,
            "invalid stage paths or bounds",
        )?;
        absolute(evidence)?;
        absolute(receipt)?;
        host.validate()?;
        validate_routes(routes)?;
        validate_clients(clients)?;
        Ok(Some(Selected {
            host,
            routes,
            clients,
            drain_seconds,
            evidence,
            receipt,
        }))
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
        check(
            self.gateway_unit != "plakd-host.service"
                && self
                    .gateway_unit
                    .strip_suffix(".service")
                    .is_some_and(identifier),
            "invalid gateway unit",
        )
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
        check(
            identifier(&client.id)
                && client.link.parent() == Some(client.node.home.join(".local/bin").as_path())
                && client
                    .link
                    .file_name()
                    .is_some_and(|name| !shipping::NAMES.iter().any(|product| *product == name))
                && ids.insert(&client.id)
                && links.insert((&client.node.ssh, &client.link))
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
