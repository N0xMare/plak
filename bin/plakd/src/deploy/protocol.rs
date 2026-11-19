use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::shipping_artifact::{Upgrade, Wire};

use super::inventory::{Host, Node, Route, Stage};

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Request {
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
    Stage,
    Link { connection: Connection },
    Check { connection: Connection },
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
    pub(super) product_home: PathBuf,
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
    pub(super) paired: bool,
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
    pub(super) gateway_versions: Vec<u32>,
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
}
