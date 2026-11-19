use std::fs::File;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use crate::CliResult;
use crate::operator_process;
use crate::shipping_artifact::{self as shipping, Manifest, NAMES, check, fail, io};

const BOUND: Duration = Duration::from_secs(120);

pub(super) fn extract(source: &Path, manifest: &Manifest) -> CliResult<tempfile::TempDir> {
    shipping::open(source)?;
    let mut list = operator_process::command("/usr/bin/tar".as_ref());
    list.arg("-tf").arg(source);
    let result = operator_process::capture(list, None, BOUND, shipping::LIMIT)?;
    check(result.status.success(), "cannot list artifact archive")?;
    let listing =
        String::from_utf8(result.stdout).map_err(|_| fail("invalid archive entry encoding"))?;
    let files = tempfile::tempdir().map_err(io)?;
    let mut root = None;
    for name in NAMES {
        let mut members = listing.lines().filter_map(|line| {
            let path = line.strip_prefix("./").unwrap_or(line);
            let (parent, leaf) = path.split_once('/').unwrap_or(("", path));
            (!path.starts_with('/') && leaf == name && parent != "." && parent != "..")
                .then_some((line, parent))
        });
        let (member, parent) = members
            .next()
            .ok_or_else(|| fail("artifact archive lacks an executable"))?;
        check(members.next().is_none(), "duplicate artifact archive entry")?;
        check(
            root.is_none_or(|root| root == parent),
            "artifact executables do not share one archive root",
        )?;
        root = Some(parent);
        let path = files.path().join(name);
        let output = File::create(&path).map_err(io)?;
        let expected = &manifest.files[name];
        let mut extract = operator_process::command("/usr/bin/tar".as_ref());
        let status = operator_process::run(
            extract
                .arg("-xOf")
                .arg(source)
                .arg("--")
                .arg(member)
                .stdin(Stdio::null())
                .stdout(output.try_clone().map_err(io)?)
                .stderr(Stdio::null()),
            BOUND,
            || {
                check(
                    output.metadata().map_err(io)?.len() <= expected.length,
                    "archive member exceeds manifest length",
                )
            },
        )?;
        check(status.success(), "cannot read artifact archive member")?;
        expected.verify(&path)?;
    }
    Ok(files)
}
