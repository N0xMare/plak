//! The schema-1 shipping manifest, shared by deploy and the snapshot installer.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{CliFailure, CliResult};

pub(crate) const NAMES: [&str; 3] = ["plakd", "plakous-js-host", "plakous-py-host"];
pub(crate) const LIMIT: u64 = 1 << 20;
pub(crate) const BLOB_LIMIT: u64 = 2 << 30;

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Manifest {
    pub(crate) schema: u32,
    pub(crate) revision: String,
    pub(crate) platform: String,
    flavor: String,
    product_version: String,
    lock_sha256: String,
    toolchain: Toolchain,
    pub(crate) files: BTreeMap<String, FileFacts>,
    pub(crate) snapshot_id: String,
    pub(crate) build_info: BuildInfo,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FileFacts {
    pub(crate) length: u64,
    pub(crate) sha256: String,
    pub(crate) display_id: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Toolchain {
    cargo: Cargo,
    rustc: Rustc,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Cargo {
    release: String,
    commit_hash: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Rustc {
    release: String,
    commit_hash: String,
    host: String,
    llvm_version: String,
}

#[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BuildInfo {
    pub(crate) export: Export,
    pub(crate) flavor: String,
    pub(crate) gateway: Versions,
    pub(crate) host_upgrade: Upgrade,
    pub(crate) omitted_features: Vec<String>,
    pub(crate) product_version: String,
    pub(crate) revision: String,
    pub(crate) runner: Runner,
    pub(crate) state_key: String,
    pub(crate) test_hooks: bool,
    pub(crate) wire: Wire,
}

#[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Export {
    pub(crate) read: Vec<u32>,
    pub(crate) write: u32,
}

#[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Versions {
    pub(crate) served: Vec<u32>,
    pub(crate) spoken: Vec<u32>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Upgrade {
    pub(crate) coordinator: u32,
    pub(crate) receipt: u32,
}

#[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Runner {
    pub(crate) protocol: u32,
    retention: Versions,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Wire {
    pub(crate) number: u32,
    pub(crate) schema_pin: String,
}

impl Manifest {
    pub(crate) fn parse(bytes: &[u8]) -> CliResult<Self> {
        let manifest: Self = serde_json::from_slice(bytes)
            .map_err(|_| fail("invalid schema-1 artifact manifest"))?;
        check(
            canonical(&manifest)? == bytes,
            "noncanonical artifact manifest",
        )?;
        manifest.validate()?;
        Ok(manifest)
    }

    fn validate(&self) -> CliResult<()> {
        check(
            self.schema == 1 && self.flavor == "release",
            "expected a schema-1 release manifest",
        )?;
        check(
            revision(&self.revision) && hex(&self.lock_sha256, 64),
            "invalid release revision or lock digest",
        )?;
        check(valid_triple(&self.platform), "invalid release platform")?;
        check(version(&self.product_version), "invalid product version")?;
        self.toolchain.validate()?;
        let info = &self.build_info;
        check(
            info.flavor == self.flavor
                && info.revision == self.revision
                && info.product_version == self.product_version
                && !info.test_hooks
                && info.omitted_features.is_empty(),
            "artifact build-info differs or omits release capabilities",
        )?;
        info.validate()?;
        check(
            self.files.len() == NAMES.len()
                && NAMES.iter().all(|name| self.files.contains_key(*name)),
            "manifest must name all three shipping executables",
        )?;
        for facts in self.files.values() {
            facts.validate()?;
        }
        check(
            self.snapshot_id == snapshot_id(&self.files),
            "snapshot id differs",
        )
    }

    pub(crate) fn verify_dir(&self, root: &Path) -> CliResult<()> {
        for (name, expected) in &self.files {
            expected.verify(&root.join(name))?;
        }
        Ok(())
    }
}

impl Toolchain {
    fn validate(&self) -> CliResult<()> {
        check(
            version(&self.cargo.release)
                && version(&self.rustc.release)
                && version(&self.rustc.llvm_version)
                && hex(&self.cargo.commit_hash, 9)
                && hex(&self.rustc.commit_hash, 40)
                && valid_triple(&self.rustc.host),
            "invalid manifest toolchain",
        )
    }
}

impl BuildInfo {
    pub(crate) fn validate(&self) -> CliResult<()> {
        check(
            crate::carry::upgrade::state_key_valid(&self.state_key),
            "invalid state key",
        )?;
        check(
            self.export.write > 0
                && sorted_positive(&self.export.read)
                && self.export.read.contains(&self.export.write)
                && self.host_upgrade.coordinator > 0
                && self.host_upgrade.receipt > 0
                && self.runner.protocol > 0
                && self.wire.number > 0
                && hex(&self.wire.schema_pin, 64)
                && [&self.gateway, &self.runner.retention]
                    .into_iter()
                    .all(|history| {
                        sorted_positive(&history.served) && sorted_positive(&history.spoken)
                    }),
            "invalid build capability versions",
        )
    }
}

impl FileFacts {
    pub(crate) fn validate(&self) -> CliResult<()> {
        check(
            self.length > 0
                && self.length <= BLOB_LIMIT
                && hex(&self.sha256, 64)
                && self.sha256.get(..12) == Some(self.display_id.as_str()),
            "invalid artifact file facts",
        )
    }

    pub(crate) fn read(path: &Path) -> CliResult<Self> {
        let (length, digest) = hash_file(path, BLOB_LIMIT, &mut std::io::sink())?;
        let result = Self {
            length,
            display_id: digest.chars().take(12).collect(),
            sha256: digest,
        };
        result.validate()?;
        Ok(result)
    }

    pub(crate) fn verify(&self, path: &Path) -> CliResult<()> {
        let actual = Self::read(path)?;
        check(actual.length == self.length, "artifact length mismatch")?;
        check(actual.sha256 == self.sha256, "artifact digest mismatch")
    }
}

pub(crate) fn open(path: &Path) -> CliResult<File> {
    use rustix::fs::{Mode, OFlags};
    let fd = rustix::fs::open(
        path,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|_| fail("artifact input is not an accessible regular file"))?;
    let file = File::from(fd);
    check(
        file.metadata().map_err(io)?.is_file(),
        "artifact input is not regular",
    )?;
    Ok(file)
}

pub(crate) fn read(path: &Path, limit: u64) -> CliResult<Vec<u8>> {
    let mut bytes = Vec::new();
    open(path)?
        .take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(io)?;
    check(
        u64::try_from(bytes.len()).map_err(|_| fail("artifact is oversized"))? <= limit,
        "artifact is oversized",
    )?;
    Ok(bytes)
}

pub(crate) fn copy(path: &Path, output: &mut impl Write, expected: &FileFacts) -> CliResult<()> {
    let (length, digest) = hash_file(path, expected.length, output)?;
    check(length == expected.length, "artifact length mismatch")?;
    check(digest == expected.sha256, "artifact digest mismatch")
}

fn hash_file(path: &Path, limit: u64, output: &mut impl Write) -> CliResult<(u64, String)> {
    let mut source = open(path)?;
    let mut sha = Sha256::new();
    let mut buffer = vec![0; 64 * 1024];
    let mut length = 0;
    loop {
        let n = source.read(&mut buffer).map_err(io)?;
        if n == 0 {
            break;
        }
        length += n as u64;
        check(length <= limit, "artifact length mismatch")?;
        sha.update(&buffer[..n]);
        output.write_all(&buffer[..n]).map_err(io)?;
    }
    Ok((length, hex_digest(&sha.finalize())))
}

pub(crate) fn snapshot_id(files: &BTreeMap<String, FileFacts>) -> String {
    let mut sha = Sha256::new();
    for name in NAMES {
        if let Some(file) = files.get(name) {
            sha.update(format!("{name} {}\n", file.sha256));
        }
    }
    hex_digest(&sha.finalize())
}

pub(crate) fn collect(root: &Path) -> CliResult<BTreeMap<String, FileFacts>> {
    NAMES
        .into_iter()
        .map(|name| Ok((name.to_owned(), FileFacts::read(&root.join(name))?)))
        .collect()
}

pub(crate) fn canonical(input: &impl Serialize) -> CliResult<Vec<u8>> {
    let mut data = serde_json::to_value(input).map_err(|_| fail("cannot encode artifact facts"))?;
    data.sort_all_objects();
    let mut raw = serde_json::to_vec(&data).map_err(|_| fail("cannot encode artifact facts"))?;
    raw.push(b'\n');
    Ok(raw)
}

pub(crate) fn digest(bytes: &[u8]) -> String {
    hex_digest(&Sha256::digest(bytes))
}

fn hex_digest(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 16] = b"0123456789abcdef";
    bytes
        .iter()
        .flat_map(|byte| {
            [
                ALPHABET[(byte >> 4) as usize],
                ALPHABET[(byte & 15) as usize],
            ]
        })
        .map(char::from)
        .collect()
}

pub(crate) fn hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

pub(crate) fn revision(value: &str) -> bool {
    hex(value, 40) || hex(value, 64)
}

fn sorted_positive(values: &[u32]) -> bool {
    values.first().is_some_and(|first| *first > 0)
        && values.windows(2).all(|pair| pair[0] < pair[1])
}

fn version(value: &str) -> bool {
    let base = value.split(['-', '+']).next().unwrap_or_default();
    base.split('.').count() == 3
        && base
            .split('.')
            .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b".-+".contains(&b))
}

fn valid_triple(value: &str) -> bool {
    (3..=4).contains(&value.split('-').count())
        && value.split('-').all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        })
}

pub(crate) fn check(condition: bool, message: &str) -> CliResult<()> {
    if condition {
        Ok(())
    } else {
        Err(fail(message))
    }
}

pub(crate) fn fail(message: &str) -> CliFailure {
    CliFailure::new(message)
}

pub(crate) fn io(error: std::io::Error) -> CliFailure {
    CliFailure::new(error.to_string())
}

pub(crate) fn absolute(path: &Path) -> CliResult<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_owned())
    } else {
        std::path::absolute(path).map_err(io)
    }
}
