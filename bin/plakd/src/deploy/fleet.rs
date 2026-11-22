use std::{cell::Cell, path::PathBuf};

use crate::shipping_artifact::{NAMES, fail};
use crate::{CliFailure, CliResult};

use super::artifacts::{Artifacts, ProductArtifact};
use super::inventory::{Client, Host, Node, Runner, Stage};
use super::protocol::{
    Call, ClientObserved, ClientOp, GatewayOp, HostOp, Observed, RunnerObserved, RunnerOp,
};
use super::ssh::{Blob, Image, Ssh, SshFailure};
use super::timing;

pub(super) trait Fleet {
    fn host(&self, op: HostOp) -> CliResult<serde_json::Value>;
    fn gateway(&self, op: GatewayOp) -> CliResult<serde_json::Value>;
    fn runner(&self, runner: &Runner, op: RunnerOp) -> Result<serde_json::Value, SshFailure>;
    fn client(&self, client: &Client, op: ClientOp) -> CliResult<serde_json::Value>;
    fn use_installed_candidate(&self);
}

pub(super) struct Remote<'a> {
    ssh: Ssh,
    artifacts: &'a Artifacts,
    host: &'a Host,
    staged: Cell<bool>,
}

impl<'a> Remote<'a> {
    pub(super) fn new(stage: Stage, artifacts: &'a Artifacts, host: &'a Host) -> CliResult<Self> {
        Ok(Self {
            ssh: Ssh::new(stage)?,
            artifacts,
            host,
            staged: Cell::new(false),
        })
    }

    fn product(
        &self,
        node: &Node,
        call: Call,
        staging: bool,
        installed: bool,
        timeout: u64,
    ) -> Result<serde_json::Value, SshFailure> {
        let artifact = self.artifacts.image(node.platform);
        let bootstrap = &artifact.paths["plakd"];
        let deployed = deployed(node, artifact);
        let files: Vec<_> = if staging {
            NAMES
                .into_iter()
                .skip(1)
                .map(|name| Blob {
                    path: &artifact.paths[name],
                    facts: &artifact.manifest.files[name],
                })
                .collect()
        } else {
            Vec::new()
        };
        let facts = &artifact.manifest.files["plakd"];
        let image = if installed && !staging {
            Image::Installed(&deployed, facts)
        } else {
            Ssh::require_glibc(node, artifact.manifest.glibc)?;
            Image::Transfer(bootstrap, facts)
        };
        self.ssh.call(
            node,
            image,
            call,
            artifact.maintainer_build(node.platform),
            &files,
            timeout,
        )
    }
}

pub(super) fn deployed(node: &Node, artifact: &ProductArtifact) -> PathBuf {
    node.home
        .join(".local/lib/plak")
        .join(artifact.manifest.flavor.name())
        .join(&artifact.manifest.snapshot_id)
        .join("plakd")
}

impl Fleet for Remote<'_> {
    fn host(&self, op: HostOp) -> CliResult<serde_json::Value> {
        let staging = matches!(op, HostOp::StageHost { .. });
        let timeout = timing::host(&op);
        let reply = self.product(
            &self.host.node,
            Call::Host {
                host: self.host.clone(),
                op,
            },
            staging,
            self.staged.get(),
            timeout,
        )?;
        if staging {
            self.use_installed_candidate();
        }
        Ok(reply)
    }

    fn runner(&self, runner: &Runner, op: RunnerOp) -> Result<serde_json::Value, SshFailure> {
        let staging = matches!(op, RunnerOp::Stage { .. });
        let installed = matches!(op, RunnerOp::Advance { .. } | RunnerOp::Restore { .. });
        let timeout = timing::runner(&op);
        self.product(
            &runner.node,
            Call::Runner {
                runner: runner.clone(),
                op,
            },
            staging,
            installed,
            timeout,
        )
    }

    fn gateway(&self, op: GatewayOp) -> CliResult<serde_json::Value> {
        let timeout = timing::gateway(&op);
        self.product(
            &self.host.node,
            Call::Gateway {
                host: self.host.clone(),
                op,
            },
            false,
            self.staged.get(),
            timeout,
        )
        .map_err(CliFailure::from)
    }

    fn client(&self, client: &Client, op: ClientOp) -> CliResult<serde_json::Value> {
        let artifact = &self.artifacts.clients[&client.id];
        let native = self.artifacts.image(client.node.platform);
        let bootstrap = &native.paths["plakd"];
        Ssh::require_glibc(&client.node, native.manifest.glibc)?;
        let files = matches!(op, ClientOp::Stage).then(|| {
            [Blob {
                path: &client.blob,
                facts: &artifact.file,
            }]
        });
        let timeout = timing::client(&op);
        self.ssh
            .call(
                &client.node,
                Image::Transfer(bootstrap, &native.manifest.files["plakd"]),
                Call::Client {
                    node: client.node.clone(),
                    client: artifact.request(client),
                    op,
                },
                native.maintainer_build(client.node.platform),
                files.as_ref().map_or(&[], |files| files.as_slice()),
                timeout,
            )
            .map_err(CliFailure::from)
    }

    fn use_installed_candidate(&self) {
        self.staged.set(true);
    }
}

pub(super) fn observed(fleet: &impl Fleet) -> CliResult<Observed> {
    evidence_decode(fleet.host(HostOp::Observe)?)
}

pub(super) fn runner(
    fleet: &impl Fleet,
    runner: &Runner,
    expected: super::protocol::Candidate,
) -> Result<RunnerObserved, SshFailure> {
    evidence_decode(fleet.runner(runner, RunnerOp::Observe { expected })?).map_err(SshFailure::from)
}

pub(super) fn client(fleet: &impl Fleet, client: &Client) -> CliResult<ClientObserved> {
    evidence_decode(fleet.client(client, ClientOp::Observe)?)
}

pub(super) fn evidence_decode<T: serde::de::DeserializeOwned>(
    value: serde_json::Value,
) -> CliResult<T> {
    serde_json::from_value(value).map_err(|_| fail("unexpected typed deployment reply"))
}
