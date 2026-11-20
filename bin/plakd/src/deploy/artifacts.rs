use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::CliResult;
use crate::operator_process;
use crate::shipping_artifact::{
    self as shipping, BuildInfo, FileFacts, Manifest, NAMES, Wire, check, fail, hex,
};
use crate::snapshot_install::{Flavor, maintainer};

use super::inventory::{ArtifactPin, Client, Inventory, Platform, Selected};
use super::protocol::{Candidate, ClientSpec, MaintainerBuild};

pub(super) struct Artifacts {
    host: Platform,
    images: BTreeMap<Platform, ProductArtifact>,
    pub(super) pins: Pins,
    pub(super) repo: PathBuf,
    pub(super) clients: BTreeMap<String, ClientArtifact>,
}

pub(super) struct ProductArtifact {
    pub(super) manifest: ProductManifest,
    pub(super) raw: String,
    pub(super) pin: String,
    pub(super) paths: BTreeMap<String, PathBuf>,
}

pub(super) struct ProductManifest {
    pub(super) files: BTreeMap<String, FileFacts>,
    pub(super) build_info: BuildInfo,
    pub(super) snapshot_id: String,
    pub(super) flavor: Flavor,
    pub(super) glibc: Option<[u32; 3]>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Pins {
    pub(super) host: String,
    pub(super) platforms: BTreeMap<Platform, String>,
    pub(super) clients: BTreeMap<String, String>,
}

impl Pins {
    pub(super) fn same_digests(&self, expected: &Self) -> bool {
        self.host == expected.host
            && self.platforms == expected.platforms
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
    pub(super) fn read(raw: &[u8], pin: &str, platform: Platform) -> CliResult<Self> {
        check(
            shipping::digest(raw) == pin,
            "client manifest digest differs",
        )?;
        let manifest: Self =
            serde_json::from_slice(raw).map_err(|_| fail("invalid client manifest"))?;
        check(
            manifest.schema == 1
                && manifest.platform == platform.triple()
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
        let host = config.host.node.platform;
        let mut images = BTreeMap::new();
        for platform in config
            .nodes()
            .map(|node| node.platform)
            .collect::<BTreeSet<_>>()
        {
            images.insert(
                platform,
                ProductArtifact::read(&data.manifests[&platform], platform, &data.revision)?,
            );
        }
        let manifest = &images[&host].manifest;
        for runner in config.runners {
            check(
                images[&runner.node.platform]
                    .manifest
                    .build_info
                    .runner
                    .protocol
                    == manifest.build_info.runner.protocol,
                "runner platform artifact has a different runner protocol",
            )?;
        }
        let mut pins = Pins {
            host: images[&host].pin.clone(),
            platforms: images
                .iter()
                .map(|(platform, image)| (*platform, image.pin.clone()))
                .collect(),
            clients: BTreeMap::new(),
        };
        let mut clients = BTreeMap::new();
        for client in config.clients {
            let raw = shipping::read(&client.manifest, shipping::LIMIT)?;
            let client_manifest =
                ClientManifest::read(&raw, &client.manifest_sha256, client.node.platform)?;
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
            host,
            images,
            pins,
            clients,
            repo: data.repo.clone(),
        })
    }

    pub(super) fn host(&self) -> &ProductArtifact {
        self.image(self.host)
    }

    pub(super) fn image(&self, platform: Platform) -> &ProductArtifact {
        &self.images[&platform]
    }

    pub(super) fn candidate(&self) -> Candidate {
        self.host().candidate()
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
                &self.host().manifest.build_info.revision,
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

impl ProductArtifact {
    fn read(pin: &ArtifactPin, platform: Platform, revision: &str) -> CliResult<Self> {
        let flavor = pin.flavor.unwrap_or(Flavor::Release);
        let filename = match flavor {
            Flavor::Release => "manifest.json",
            Flavor::Maintainer => maintainer::MANIFEST,
        };
        let bytes = shipping::read(&pin.artifact.join(filename), shipping::LIMIT)?;
        check(
            shipping::digest(&bytes) == pin.manifest_sha256,
            "platform manifest digest differs",
        )?;
        let (manifest, paths) = match flavor {
            Flavor::Release => {
                let manifest = Manifest::parse(&bytes)?;
                check(
                    platform != Platform::LinuxGnu
                        && manifest.revision == revision
                        && manifest.platform == platform.triple(),
                    "wrong artifact revision or platform",
                )?;
                manifest.verify_dir(&pin.artifact)?;
                (
                    ProductManifest {
                        files: manifest.files,
                        build_info: manifest.build_info,
                        snapshot_id: manifest.snapshot_id,
                        flavor,
                        glibc: None,
                    },
                    NAMES
                        .into_iter()
                        .map(|name| (name.to_owned(), pin.artifact.join(name)))
                        .collect(),
                )
            }
            Flavor::Maintainer => {
                let manifest = maintainer::Manifest::parse(&bytes)?;
                check(
                    manifest.build_info.revision == revision
                        && manifest.platform == platform.triple(),
                    "wrong maintainer snapshot revision or platform",
                )?;
                let paths = manifest.inputs(&pin.artifact)?;
                (
                    ProductManifest {
                        files: manifest.files,
                        build_info: manifest.build_info,
                        snapshot_id: manifest.snapshot_id,
                        flavor,
                        glibc: manifest.glibc,
                    },
                    paths,
                )
            }
        };
        Ok(Self {
            manifest,
            pin: pin.manifest_sha256.clone(),
            paths,
            raw: String::from_utf8(bytes).map_err(|_| fail("manifest is not UTF-8"))?,
        })
    }

    pub(super) fn candidate(&self) -> Candidate {
        Candidate {
            snapshot: self.manifest.snapshot_id.clone(),
            sha256: self.manifest.files["plakd"].sha256.clone(),
            flavor: self.manifest.flavor,
        }
    }

    pub(super) fn maintainer_build(&self, platform: Platform) -> Option<MaintainerBuild> {
        (self.manifest.flavor == Flavor::Maintainer).then(|| MaintainerBuild {
            platform,
            revision: self.manifest.build_info.revision.clone(),
            state_key: self.manifest.build_info.state_key.clone(),
        })
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
