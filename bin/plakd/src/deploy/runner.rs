use std::fs;
use std::io::Read;
use std::os::unix::fs::MetadataExt as _;
use std::path::Path;

use crate::CliResult;
use crate::shipping_artifact::{self as shipping, BuildInfo, FileFacts, check, io};
use crate::snapshot_install;

use super::fence::Fence;
use super::inventory::{Node, Runner, Stage};
use super::probe;
use super::protocol::{Candidate, RunnerLink, RunnerObserved, RunnerOp, RunnerTriple};
use super::target;

pub(super) fn execute(
    runner: &Runner,
    op: RunnerOp,
    body: &mut dyn Read,
    stage: Stage,
    operator: Option<&str>,
) -> CliResult<serde_json::Value> {
    runner.validate()?;
    let account = probe::validate_node(&runner.node, operator)?;
    if !matches!(op, RunnerOp::Stage { .. }) {
        target::eof(body)?;
    }
    if let RunnerOp::Observe { expected } = &op {
        let link = observe(runner)?;
        check(
            link.sha256 == runner.source_sha256 || link == candidate_link(&runner.node, expected)?,
            "unknown runner command image; inspect and re-enroll",
        )?;
        let runner_triple = build_info(&runner.node, &link)?;
        check(
            observe(runner)? == link,
            "runner link changed during observation",
        )?;
        return target::data(&RunnerObserved {
            account,
            production_history: probe::production_history(&runner.node),
            link,
            runner_triple,
        });
    }
    target::guard_account(&runner.node, stage)?;
    if stage == Stage::Production {
        target::mark_production(&runner.node, &account)?;
    }
    match op {
        RunnerOp::Stage {
            manifest,
            manifest_sha256,
            flavor,
        } => target::data(&target::stage_host(
            manifest.as_bytes(),
            &manifest_sha256,
            flavor,
            body,
        )?),
        RunnerOp::Advance {
            ticket,
            candidate,
            source,
        } => {
            let fence = Fence::lock(&runner.node, &format!("runner-{}", runner.id), ticket)?;
            fence.active()?;
            switch(runner, &candidate, &source, false)?;
            target::data(&())
        }
        RunnerOp::Restore {
            ticket,
            candidate,
            source,
        } => {
            let mut fence = Fence::lock(&runner.node, &format!("runner-{}", runner.id), ticket)?;
            fence.cancel()?;
            switch(runner, &candidate, &source, true)?;
            target::data(&())
        }
        RunnerOp::Observe { .. } => unreachable!(),
    }
}

fn build_info(node: &Node, link: &RunnerLink) -> CliResult<RunnerTriple> {
    let mut command = probe::command(link.path.as_os_str(), node, &node.home.join(".plak"));
    command.args(["version", "--build-info"]);
    let info: BuildInfo = probe::decode(&probe::capture(command, super::timing::PROBE)?)?;
    check(
        shipping::revision(&info.revision)
            && info.runner.protocol > 0
            && matches!(info.flavor.as_str(), "release" | "maintainer"),
        "invalid runner build-info triple",
    )?;
    Ok(RunnerTriple {
        protocol: info.runner.protocol,
        source_revision: info.revision,
        flavor: info.flavor,
    })
}

fn snapshot_image(node: &Node, path: &Path) -> CliResult<FileFacts> {
    super::inventory::absolute(path)?;
    let snapshot = snapshot_install::parent(path)?;
    let install = node.home.join(".local/lib/plak");
    check(
        path.file_name().is_some_and(|name| name == "plakd")
            && snapshot.parent().is_some_and(|flavor| {
                flavor == install.join("release") || flavor == install.join("maintainer")
            }),
        "runner command must point directly to an immutable plakd snapshot",
    )?;
    snapshot_install::require_owned(snapshot)?;
    let meta = fs::symlink_metadata(path).map_err(io)?;
    check(
        meta.is_file() && meta.uid() == node.uid && meta.mode() & 0o7777 == 0o555,
        "runner image is not an owned immutable executable",
    )?;
    FileFacts::read(path)
}

fn observe(runner: &Runner) -> CliResult<RunnerLink> {
    snapshot_install::require_owned(snapshot_install::parent(&runner.link)?)?;
    let path = fs::read_link(&runner.link).map_err(io)?;
    let sha256 = snapshot_image(&runner.node, &path)?.sha256;
    Ok(RunnerLink { path, sha256 })
}

fn switch(
    runner: &Runner,
    candidate: &Candidate,
    source: &RunnerLink,
    restore: bool,
) -> CliResult<()> {
    let mut desired = candidate_link(&runner.node, candidate)?;
    if !restore {
        desired.path = target::staged_in(&runner.node, candidate)?;
    }
    check(
        source.sha256 == runner.source_sha256
            && snapshot_image(&runner.node, &source.path)?.sha256 == source.sha256,
        "recorded source runner image differs; restore the pinned immutable snapshot before retrying",
    )?;
    let found = if restore {
        snapshot_install::require_owned(snapshot_install::parent(&runner.link)?)?;
        let path = fs::read_link(&runner.link).map_err(io)?;
        if path == desired.path
            && fs::symlink_metadata(&path)
                .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
        {
            desired.clone()
        } else {
            let sha256 = snapshot_image(&runner.node, &path)?.sha256;
            RunnerLink { path, sha256 }
        }
    } else {
        observe(runner)?
    };
    check(
        &found == source || found == desired,
        "unexpected dedicated runner link; inspect and re-enroll",
    )?;
    let expected = if restore { source } else { &desired };
    if &found != expected {
        snapshot_install::atomic_link(&runner.link, &expected.path)?;
        fs::File::open(snapshot_install::parent(&runner.link)?)
            .and_then(|dir| dir.sync_all())
            .map_err(io)?;
    }
    check(
        observe(runner)? == *expected,
        "dedicated runner link did not converge",
    )
}

fn candidate_link(node: &Node, candidate: &Candidate) -> CliResult<RunnerLink> {
    check(
        shipping::hex(&candidate.snapshot, 64) && shipping::hex(&candidate.sha256, 64),
        "invalid runner candidate pin",
    )?;
    Ok(RunnerLink {
        path: node
            .home
            .join(".local/lib/plak/release")
            .join(&candidate.snapshot)
            .join("plakd"),
        sha256: candidate.sha256.clone(),
    })
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;

    use super::*;
    use crate::deploy::inventory::Platform;

    #[test]
    fn deploy_runner_restores_the_link_and_fences_a_delayed_advance() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().canonicalize().unwrap();
        let (old, _) = snapshot_install::tests::publish_fixture(&home, b"source");
        let (new, snapshot) = snapshot_install::tests::publish_fixture(&home, b"destination");
        let source = RunnerLink {
            path: old.join("plakd"),
            sha256: FileFacts::read(&old.join("plakd")).unwrap().sha256,
        };
        let candidate = Candidate {
            flavor: snapshot_install::Flavor::Release,
            snapshot,
            sha256: FileFacts::read(&new.join("plakd")).unwrap().sha256,
        };
        let runner = Runner {
            id: "desk".into(),
            node: Node {
                ssh: "example".into(),
                login: "example".into(),
                uid: rustix::process::geteuid().as_raw(),
                platform: Platform::Mac,
                home: home.clone(),
            },
            link: home.join(".local/bin/runner-command"),
            source_sha256: source.sha256.clone(),
        };
        snapshot_install::ensure(runner.link.parent().unwrap()).unwrap();
        symlink(&source.path, &runner.link).unwrap();
        snapshot_install::link_snapshot_in(&home, candidate.flavor, &candidate.snapshot).unwrap();
        assert_eq!(observe(&runner).unwrap(), source);

        let ticket = uuid::Uuid::new_v4();
        let mut fence = Fence::lock(&runner.node, "runner-desk", ticket).unwrap();
        fence.active().unwrap();
        assert!(Fence::lock(&runner.node, "runner-desk", ticket).is_err());
        switch(&runner, &candidate, &source, false).unwrap();
        assert_eq!(fs::read_link(&runner.link).unwrap(), new.join("plakd"));
        fence.cancel().unwrap();
        switch(&runner, &candidate, &source, true).unwrap();
        let inherited = fence.requester_input().unwrap();
        drop(fence);

        assert_eq!(fs::read_link(&runner.link).unwrap(), source.path);
        assert!(Fence::lock(&runner.node, "runner-desk", ticket).is_err());
        drop(inherited);
        assert!(
            Fence::lock(&runner.node, "runner-desk", ticket)
                .unwrap()
                .active()
                .is_err()
        );
        assert_eq!(
            fs::read_link(home.join(".local/bin/plakd")).unwrap(),
            new.join("plakd")
        );
        // A staged image may disappear between advance and withdrawal. The
        // exact dangling candidate link is still safe to restore.
        snapshot_install::atomic_link(&runner.link, &new.join("plakd")).unwrap();
        fs::remove_file(new.join("plakd")).unwrap();
        switch(&runner, &candidate, &source, true).unwrap();
        assert_eq!(observe(&runner).unwrap(), source);
    }
}
