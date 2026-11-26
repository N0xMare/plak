//! Pinned build and ELF facts for a maintainer snapshot install.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::CliResult;
use crate::shipping_artifact::{self as shipping, BuildInfo, FileFacts, NAMES, check, fail};

pub(crate) const MANIFEST: &str = "deploy-manifest.json";
pub(crate) const GNU: &str = "x86_64-unknown-linux-gnu";
const MUSL: &str = "x86_64-unknown-linux-musl";
const MAC: &str = "aarch64-apple-darwin";

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Manifest {
    snapshot_schema: u32,
    pub(crate) platform: String,
    pub(crate) snapshot_id: String,
    pub(crate) files: BTreeMap<String, FileFacts>,
    pub(crate) build_info: BuildInfo,
    pub(crate) glibc: Option<[u32; 3]>,
}

impl Manifest {
    pub(super) fn from_build(
        path: &Path,
        files: &BTreeMap<String, FileFacts>,
        id: &str,
    ) -> CliResult<Option<Vec<u8>>> {
        let build_info = current()?;
        if !build_is_deployable(&build_info) {
            return Ok(None);
        }
        let platform = native()?;
        let manifest = Self {
            snapshot_schema: 1,
            platform: platform.to_owned(),
            snapshot_id: id.to_owned(),
            files: files.clone(),
            build_info,
            glibc: (platform == GNU).then(|| glibc_ceiling(path)).transpose()?,
        };
        manifest.validate()?;
        shipping::canonical(&manifest).map(Some)
    }

    fn validate(&self) -> CliResult<()> {
        check(
            self.snapshot_schema == 1
                && build_is_deployable(&self.build_info)
                && matches!(self.platform.as_str(), GNU | MUSL | MAC)
                && (self.platform == GNU) == self.glibc.is_some()
                && self.glibc.is_none_or(|ceiling| ceiling[0] >= 2),
            "invalid maintainer snapshot build or platform facts",
        )?;
        self.build_info.validate()?;
        check(
            self.files.len() == NAMES.len()
                && NAMES.iter().all(|name| self.files.contains_key(*name)),
            "maintainer manifest must name all three executables",
        )?;
        for file in self.files.values() {
            file.validate()?;
        }
        check(
            shipping::snapshot_id(&self.files) == self.snapshot_id,
            "maintainer snapshot id differs",
        )
    }
}

fn build_is_deployable(build: &BuildInfo) -> bool {
    build.flavor == "maintainer release"
        && !build.test_hooks
        && build.omitted_features.is_empty()
        && build.product_version == plakous::VERSION
        && shipping::revision(&build.revision)
}

fn native() -> CliResult<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => Ok(MAC),
        #[cfg(target_env = "gnu")]
        ("linux", "x86_64") => Ok(GNU),
        #[cfg(target_env = "musl")]
        ("linux", "x86_64") => Ok(MUSL),
        _ => Err(fail("unsupported maintainer artifact platform")),
    }
}

fn current() -> CliResult<BuildInfo> {
    let mut raw = Vec::new();
    crate::build_identity::print_build_info(&mut raw)?;
    serde_json::from_slice(&raw).map_err(|_| fail("cannot read maintainer build info"))
}

fn glibc_ceiling(root: &Path) -> CliResult<[u32; 3]> {
    let mut ceiling = [0; 3];
    for name in NAMES {
        let mut command = crate::operator_process::command("/usr/bin/readelf".as_ref());
        command
            .env_clear()
            .args(["-W", "--version-info"])
            .arg(root.join(name));
        let output = crate::operator_process::capture(
            command,
            None,
            Duration::from_secs(15),
            shipping::LIMIT,
        )
        .map_err(|_| fail("cannot read GNU ELF requirements; install readelf"))?;
        check(
            output.status.success(),
            "cannot read GNU ELF version requirements",
        )?;
        let text = std::str::from_utf8(&output.stdout)
            .map_err(|_| fail("invalid GNU ELF version requirements"))?;
        ceiling = ceiling.max(required_glibc(text)?);
    }
    Ok(ceiling)
}

fn required_glibc(text: &str) -> CliResult<[u32; 3]> {
    let mut ceiling = None;
    for version in text
        .lines()
        .filter_map(|line| line.split_once("Name: GLIBC_")?.1.split_whitespace().next())
        .filter(|version| !version.starts_with("ABI_"))
    {
        let version = glibc_version(version)?;
        ceiling = Some(ceiling.map_or(version, |lower: [u32; 3]| lower.max(version)));
    }
    ceiling.ok_or_else(|| fail("artifact has no comparable GLIBC requirements"))
}

pub(crate) fn glibc_version(text: &str) -> CliResult<[u32; 3]> {
    let invalid = || fail("invalid or unsupported GLIBC version");
    let mut parts = text.split('.');
    let parse = |part: &str| {
        check(
            part.bytes().all(|b| b.is_ascii_digit()),
            "invalid or unsupported GLIBC version",
        )?;
        part.parse::<u32>().map_err(|_| invalid())
    };
    let major = parse(parts.next().ok_or_else(invalid)?)?;
    let minor = parse(parts.next().ok_or_else(invalid)?)?;
    let patch = parts.next().map(parse).transpose()?.unwrap_or(0);
    check(
        parts.next().is_none(),
        "invalid or unsupported GLIBC version",
    )?;
    Ok([major, minor, patch])
}

#[cfg(test)]
mod tests;
