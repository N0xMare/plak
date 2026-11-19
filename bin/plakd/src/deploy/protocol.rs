use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::shipping_artifact::{Upgrade, Wire};

use super::inventory::{Host, Node, Platform, Route, Runner, Stage};

pub(super) const VERSION: u32 = 2;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DeployRunnerInspection {
    pub(crate) source_instance: String,
    pub(crate) deploy_inspect_runners: Vec<String>,
}

#[derive(Deserialize, Serialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum DeployRunnerVerdict {
    Settled,
    Blocked { detail: String },
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Request {
    pub(super) deploy_protocol: u32,
    pub(super) stage: Stage,
    pub(super) operator_machine: Option<String>,
    pub(super) call: Call,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(
    tag = "role",
    content = "request",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub(super) enum Call {
    Host {
        host: Host,
        op: HostOp,
    },
    Runner {
        runner: Runner,
        op: RunnerOp,
    },
    Client {
        node: Node,
        client: ClientSpec,
        op: ClientOp,
    },
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum HostOp {
    Observe,
    RunnerPreflight {
        expected: String,
        placements: BTreeMap<String, RunnerPlacement>,
    },
    Health {
        expected: String,
        routes: Vec<Route>,
    },
    CanaryPreflight {
        expected: String,
        route: Route,
    },
    Canary {
        expected: String,
        route: Route,
    },
    StageHost {
        manifest: String,
        manifest_sha256: String,
    },
    DestinationUnit {
        candidate: Candidate,
    },
    DestinationCanaryPreflight {
        candidate: Candidate,
        route: Route,
    },
    Upgrade {
        candidate: Candidate,
        drain: u32,
        previous: Box<Observed>,
        ticket: uuid::Uuid,
    },
    WithdrawUpgrade {
        ticket: uuid::Uuid,
        instance: String,
        source_sha256: String,
    },
    HostLink {
        candidate: Candidate,
        instance: String,
    },
    MarkProduction {
        expected: StageIdentity,
    },
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum ClientOp {
    Observe,
    Stage,
    Link { connection: Connection },
    Check { connection: Connection },
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum RunnerOp {
    Observe,
    Stage {
        manifest: String,
        manifest_sha256: String,
    },
    Advance {
        ticket: uuid::Uuid,
        candidate: Candidate,
        source: RunnerLink,
    },
    Restore {
        ticket: uuid::Uuid,
        candidate: Candidate,
        source: RunnerLink,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Candidate {
    pub(super) snapshot: String,
    pub(super) sha256: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct StageIdentity {
    pub(super) machine_sha256: String,
    pub(super) platform: Platform,
    pub(super) uid: u32,
    pub(super) home: PathBuf,
    pub(super) product_home: PathBuf,
}

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct AccountIdentity {
    pub(super) machine_sha256: String,
    pub(super) platform: Platform,
    pub(super) uid: u32,
    pub(super) home: PathBuf,
}

impl AccountIdentity {
    pub(super) fn isolated_from(&self, other: &Self) -> bool {
        self.machine_sha256 != other.machine_sha256
            || (self.platform == Platform::Mac
                && other.platform == Platform::Mac
                && self.uid != other.uid
                && self.home != other.home)
    }
}

impl StageIdentity {
    pub(super) fn account(&self) -> AccountIdentity {
        AccountIdentity {
            machine_sha256: self.machine_sha256.clone(),
            platform: self.platform,
            uid: self.uid,
            home: self.home.clone(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RunnerPlacement {
    pub(super) ssh: String,
    pub(super) command: PathBuf,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RunnerLink {
    pub(super) path: PathBuf,
    pub(super) sha256: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RunnerObserved {
    pub(super) account: AccountIdentity,
    pub(super) production_history: bool,
    pub(super) link: RunnerLink,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ClientObserved {
    pub(super) account: AccountIdentity,
    pub(super) production_history: bool,
    pub(super) current: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Facts {
    pub(super) source: String,
    pub(super) binary_id: String,
    pub(super) state_key: String,
    pub(super) host_upgrade: Upgrade,
    pub(super) runner_triple: RunnerTriple,
    pub(super) wire: Wire,
    pub(super) export: Export,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Export {
    pub(super) read: Vec<u32>,
    pub(super) write: u32,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RunnerTriple {
    pub(super) protocol: u32,
    pub(super) source_revision: String,
    pub(super) flavor: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Observed {
    pub(super) sha256: String,
    pub(super) link_sha256: String,
    pub(super) link_sidecars: BTreeMap<String, String>,
    pub(super) instance: String,
    pub(super) facts: Facts,
    pub(super) unit_sha256: String,
    pub(super) custody_sha256: String,
    pub(super) gateway_instance: String,
    pub(super) gateway_versions: Vec<u32>,
    pub(super) stage_identity: StageIdentity,
    pub(super) paired: BTreeMap<String, RunnerPlacement>,
    pub(super) canary: Option<Vec<String>>,
    pub(super) accounts: BTreeMap<String, String>,
    pub(super) upgrade_idle: bool,
    pub(super) production_history: bool,
}

impl Observed {
    pub(super) fn dependencies(&self) -> Dependencies {
        Dependencies {
            unit_sha256: self.unit_sha256.clone(),
            custody_sha256: self.custody_sha256.clone(),
            gateway_instance: self.gateway_instance.clone(),
            gateway_versions: self.gateway_versions.clone(),
            stage_identity: self.stage_identity.clone(),
        }
    }

    pub(super) fn source_transition(&self) -> SourceTransition {
        SourceTransition {
            wire: self.facts.wire.clone(),
            coordinator: self.facts.host_upgrade.clone(),
            runner_protocol: self.facts.runner_triple.protocol,
            runners: Vec::new(),
            gateway_versions: self.gateway_versions.clone(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Dependencies {
    pub(super) unit_sha256: String,
    pub(super) custody_sha256: String,
    pub(super) gateway_instance: String,
    pub(super) gateway_versions: Vec<u32>,
    pub(super) stage_identity: StageIdentity,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SourceTransition {
    pub(super) wire: Wire,
    pub(super) coordinator: Upgrade,
    pub(super) runner_protocol: u32,
    pub(super) runners: Vec<(Platform, RunnerTransition)>,
    pub(super) gateway_versions: Vec<u32>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum RunnerTransition {
    Unchanged,
    SameTriple,
    ChangedTriple,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Health {
    pub(super) doctor: String,
    pub(super) status: String,
    pub(super) bindings: String,
    pub(super) binding_count: u64,
    pub(super) generations: BTreeMap<String, Option<u64>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) canaries: Option<BTreeMap<String, String>>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Canary {
    pub(super) adapter: String,
    pub(super) generation_before: Option<u64>,
    pub(super) generation_after: Option<u64>,
}

#[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Connection {
    pub(super) binary_id: String,
    pub(super) wire: Wire,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ClientSpec {
    pub(super) link: PathBuf,
    pub(super) manifest: String,
    pub(super) manifest_sha256: String,
    pub(super) source_sha256: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Reply {
    Ok(serde_json::Value),
    Error(String),
    ProtocolMismatch { supported: u32, remedy: String },
}
