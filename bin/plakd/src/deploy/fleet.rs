use std::{
    cell::Cell,
    path::{Path, PathBuf},
};

use crate::CliResult;
use crate::shipping_artifact::{NAMES, check, fail};

use super::artifacts::Artifacts;
use super::inventory::{Client, Host, Stage};
use super::probe;
use super::protocol::{Call, ClientOp, HostOp, Observed};
use super::ssh::{Blob, Image, Ssh};
use super::timing;

pub(super) trait Fleet {
    fn host(&self, op: HostOp) -> CliResult<serde_json::Value>;
    fn client_current(&self, client: &Client) -> CliResult<bool>;
    fn client(&self, client: &Client, op: ClientOp) -> CliResult<()>;
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

    fn deployed(&self) -> PathBuf {
        self.host
            .node
            .home
            .join(".local/lib/plak/release")
            .join(&self.artifacts.manifest.snapshot_id)
            .join("plakd")
    }

    fn transfer<'b>(&'b self, path: &'b Path) -> Image<'b> {
        Image::Transfer(path, &self.artifacts.manifest.files["plakd"])
    }
}

impl Fleet for Remote<'_> {
    fn host(&self, op: HostOp) -> CliResult<serde_json::Value> {
        let deployed = self.deployed();
        let bootstrap = self.artifacts.root.join("plakd");
        let sidecars = NAMES.map(|name| self.artifacts.root.join(name));
        let mut files = Vec::new();
        let staging = matches!(op, HostOp::StageHost { .. });
        if staging {
            for (name, file) in NAMES.into_iter().zip(&sidecars).skip(1) {
                files.push(Blob {
                    path: file,
                    facts: &self.artifacts.manifest.files[name],
                });
            }
        }
        let image = if self.staged.get() && !staging {
            Image::Installed(&deployed, &self.artifacts.manifest.files["plakd"])
        } else {
            self.transfer(&bootstrap)
        };
        let timeout = timing::host(&op);
        let reply = self.ssh.call(
            &self.host.node,
            image,
            Call::Host {
                host: self.host.clone(),
                op,
            },
            &files,
            timeout,
        )?;
        if staging {
            self.use_installed_candidate();
        }
        Ok(reply)
    }

    fn client_current(&self, client: &Client) -> CliResult<bool> {
        let artifact = &self.artifacts.clients[&client.id];
        let raw = Ssh::script(
            &client.node,
            CLIENT_OBSERVE,
            &[
                path(&client.node.home)?,
                &client.node.uid.to_string(),
                path(&client.link)?,
                &client.node.login,
            ],
        )?;
        let value = probe::text(&raw)?;
        let (hash, resolved) = value
            .split_once('\n')
            .ok_or_else(|| fail("invalid client link verdict"))?;
        let resolved = probe::line(resolved.as_bytes())?;
        check(
            hash == client.source_sha256 || hash == artifact.file.sha256,
            "unexpected client link digest; inspect and re-enroll",
        )?;
        let desired = client
            .node
            .home
            .join(".local/lib/plak/clients")
            .join(&artifact.file.sha256)
            .join("client");
        Ok(Path::new(resolved) == desired && hash == artifact.file.sha256)
    }

    fn client(&self, client: &Client, op: ClientOp) -> CliResult<()> {
        let artifact = &self.artifacts.clients[&client.id];
        let bootstrap = self.artifacts.root.join("plakd");
        let files = matches!(op, ClientOp::Stage).then(|| {
            [Blob {
                path: &client.blob,
                facts: &artifact.file,
            }]
        });
        let timeout = timing::client(&op);
        self.ssh.call(
            &client.node,
            self.transfer(&bootstrap),
            Call::Client {
                node: client.node.clone(),
                client: artifact.request(client),
                op,
            },
            files.as_ref().map_or(&[], |files| files.as_slice()),
            timeout,
        )
    }

    fn use_installed_candidate(&self) {
        self.staged.set(true);
    }
}

pub(super) fn observed(fleet: &impl Fleet) -> CliResult<Observed> {
    evidence_decode(fleet.host(HostOp::Observe)?)
}

pub(super) fn evidence_decode<T: serde::de::DeserializeOwned>(
    value: serde_json::Value,
) -> CliResult<T> {
    serde_json::from_value(value).map_err(|_| fail("unexpected typed deployment reply"))
}

fn path(path: &Path) -> CliResult<&str> {
    path.to_str()
        .ok_or_else(|| fail("deployment path is not UTF-8"))
}

const CLIENT_OBSERVE: &str = r#"sh -c '
set -eu
IFS= read -r home
IFS= read -r uid
IFS= read -r link
IFS= read -r login
test "$HOME" = "$home" && test "$(id -u)" = "$uid" && test "$(id -un)" = "$login"
for dir in "$home" "$home/.local" "$home/.local/bin"; do
    test -d "$dir" && test ! -L "$dir"
    test "$(stat -c %u -- "$dir")" = "$uid"
    mode=$(stat -c %a -- "$dir")
    test "$((0$mode & 022))" -eq 0
done
test -L "$link"
image=$(readlink -f -- "$link")
test -f "$image"
hash=$(sha256sum -- "$image")
printf "%s\n%s\n" "${hash%% *}" "$image"
'"#;
