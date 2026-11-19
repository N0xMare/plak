use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::CliResult;
use crate::operator_process;
use crate::shipping_artifact::{self as shipping, FileFacts, Manifest, Wire, check, fail, hex};

use super::inventory::{Client, Inventory, Selected};
use super::protocol::{Candidate, ClientSpec};

const PLATFORM: &str = "x86_64-unknown-linux-musl";

pub(super) struct Artifacts {
    pub(super) manifest: Manifest,
    pub(super) raw: String,
    pub(super) pins: Pins,
    pub(super) root: PathBuf,
    pub(super) repo: PathBuf,
    pub(super) clients: BTreeMap<String, ClientArtifact>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Pins {
    pub(super) host: String,
    pub(super) clients: BTreeMap<String, String>,
}

impl Pins {
    pub(super) fn same_digests(&self, expected: &Self) -> bool {
        self.host == expected.host
            && self.clients.values().collect::<BTreeSet<_>>()
                == expected.clients.values().collect::<BTreeSet<_>>()
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ClientManifest {
    schema: u32,
    platform: String,
    pub(super) wire: Wire,
    file: ClientFile,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ClientFile {
    length: u64,
    sha256: String,
}

pub(super) struct ClientArtifact {
    raw: String,
    pub(super) file: FileFacts,
}

impl ClientManifest {
    pub(super) fn read(raw: &[u8], pin: &str) -> CliResult<Self> {
        check(
            shipping::digest(raw) == pin,
            "client manifest digest differs",
        )?;
        let manifest: Self =
            serde_json::from_slice(raw).map_err(|_| fail("invalid client manifest"))?;
        check(
            manifest.schema == 1
                && manifest.platform == PLATFORM
                && manifest.file.length > 0
                && manifest.file.length <= shipping::BLOB_LIMIT
                && hex(&manifest.file.sha256, 64)
                && manifest.wire.number > 0
                && hex(&manifest.wire.schema_pin, 64),
            "invalid client manifest facts",
        )?;
        Ok(manifest)
    }

    pub(super) fn file(&self) -> FileFacts {
        FileFacts {
            length: self.file.length,
            sha256: self.file.sha256.clone(),
            display_id: self.file.sha256.chars().take(12).collect(),
        }
    }
}

impl ClientArtifact {
    pub(super) fn request(&self, client: &Client) -> ClientSpec {
        ClientSpec {
            link: client.link.clone(),
            manifest: self.raw.clone(),
            manifest_sha256: client.manifest_sha256.clone(),
            source_sha256: client.source_sha256.clone(),
        }
    }
}

impl Artifacts {
    pub(super) fn import(data: &Inventory, config: &Selected<'_>) -> CliResult<Self> {
        from_main(&data.repo, &data.revision)?;
        let raw = shipping::read(&data.artifact.join("manifest.json"), shipping::LIMIT)?;
        check(
            shipping::digest(&raw) == data.manifest_sha256,
            "host manifest digest differs",
        )?;
        let manifest = Manifest::parse(&raw)?;
        check(
            manifest.revision == data.revision && manifest.platform == PLATFORM,
            "wrong host revision or target",
        )?;
        manifest.verify_dir(&data.artifact)?;
        let mut pins = Pins {
            host: data.manifest_sha256.clone(),
            clients: BTreeMap::new(),
        };
        let mut clients = BTreeMap::new();
        for client in config.clients {
            let raw = shipping::read(&client.manifest, shipping::LIMIT)?;
            let client_manifest = ClientManifest::read(&raw, &client.manifest_sha256)?;
            check(
                client_manifest.wire == manifest.build_info.wire,
                "client wire/schema pin differs; build an exact-match client",
            )?;
            let file = client_manifest.file();
            file.verify(&client.blob)?;
            pins.clients
                .insert(client.id.clone(), client.manifest_sha256.clone());
            clients.insert(
                client.id.clone(),
                ClientArtifact {
                    file,
                    raw: String::from_utf8(raw)
                        .map_err(|_| fail("client manifest is not UTF-8"))?,
                },
            );
        }
        Ok(Self {
            manifest,
            raw: String::from_utf8(raw).map_err(|_| fail("host manifest is not UTF-8"))?,
            pins,
            clients,
            root: data.artifact.clone(),
            repo: data.repo.clone(),
        })
    }

    pub(super) fn candidate(&self) -> Candidate {
        Candidate {
            snapshot: self.manifest.snapshot_id.clone(),
            sha256: self.manifest.files["plakd"].sha256.clone(),
        }
    }

    pub(super) fn check_ancestor(&self, source: &str) -> CliResult<()> {
        check(
            shipping::revision(source),
            "serving revision is unavailable; enroll the shipping source first",
        )?;
        let result = git(
            &self.repo,
            &[
                "merge-base",
                "--is-ancestor",
                source,
                &self.manifest.revision,
            ],
        )?;
        match result.status.code() {
            Some(0) => Ok(()),
            Some(1) => Err(fail(
                "revision downgrade or divergence: choose a descendant of the serving revision",
            )),
            _ => Err(fail(
                "serving revision cannot be verified; fetch its history before deploying",
            )),
        }
    }
}

fn from_main(repo: &Path, revision: &str) -> CliResult<()> {
    let found = git(
        repo,
        &["ls-remote", "--exit-code", "origin", "refs/heads/main"],
    )?;
    let found = found.text()?;
    let row: Vec<_> = found.split_whitespace().collect();
    let [tip, "refs/heads/main"] = row.as_slice() else {
        return Err(fail("ambiguous origin/main"));
    };
    check(
        shipping::revision(tip),
        "invalid advertised origin/main revision",
    )?;
    let history = git(repo, &["rev-list", "--first-parent", tip])?;
    check(
        history.text()?.lines().any(|line| line == revision),
        "revision is not on origin/main; build an exact origin/main commit",
    )
}

fn git(repo: &Path, args: &[&str]) -> CliResult<operator_process::Output> {
    let mut command = operator_process::command("git".as_ref());
    command
        .arg("-C")
        .arg(repo)
        .args(args)
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .env("GIT_TERMINAL_PROMPT", "0");
    operator_process::capture(command, None, Duration::from_secs(30), 16 * shipping::LIMIT)
        .map_err(|_| fail("cannot verify origin/main; fetch origin main and retry"))
}

trait GitText {
    fn text(&self) -> CliResult<&str>;
}

impl GitText for operator_process::Output {
    fn text(&self) -> CliResult<&str> {
        check(
            self.status.success(),
            "cannot verify origin/main; fetch origin main and retry",
        )?;
        std::str::from_utf8(&self.stdout).map_err(|_| fail("invalid Git revision reply"))
    }
}
