use std::fs;
use std::io::{self, BufRead as _, BufReader, Write as _};
use std::os::unix::fs::{PermissionsExt as _, symlink};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::thread;
use std::time::Duration;

use serde_json::{Value, json};

use super::{FileFacts, Host, NORMALIZE, Node, Target};
use crate::deploy::{inventory::Route, target};

pub(super) const KEY: &str = "r28.e39.p39.t11";
const GATEWAY: &str = "00000000-0000-0000-0000-000000000001";

pub(super) struct Socket {
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl Socket {
    pub(super) fn start(path: &Path, verdict: Value, hello: bool) -> Self {
        let listener = UnixListener::bind(path).unwrap();
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let cancelled = Arc::clone(&stop);
        let thread = thread::spawn(move || {
            while !cancelled.load(Ordering::Relaxed) {
                let mut peer = match listener.accept() {
                    Ok((peer, _)) => peer,
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    Err(error) => panic!("{error}"),
                };
                if hello {
                    peer.set_read_timeout(Some(Duration::from_secs(4))).unwrap();
                    let mut raw = String::new();
                    BufReader::new(&peer).read_line(&mut raw).unwrap();
                    let message: Value = serde_json::from_str(&raw).unwrap();
                    assert_eq!(message["frame"], "hello");
                    assert_eq!(message["proto"], 11);
                }
                writeln!(peer, "{verdict}").unwrap();
            }
        });
        Self {
            stop,
            thread: Some(thread),
        }
    }
}

impl Drop for Socket {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.thread.take().unwrap().join().unwrap();
    }
}

pub(super) fn program(path: &Path, code: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, format!("#!/bin/sh\nset -eu\n{code}")).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

pub(super) fn image(home: &Path, name: &str, key: &str, unit: &str) -> PathBuf {
    let image = home
        .join(".local/lib/plak/release")
        .join(name)
        .join("plakd");
    program(
        &image,
        &format!(
            r#"
case "$*" in
  'host install --dry-run '*) printf '%s\n' '{unit}' ;;
  'version --host-upgrade-probe')
    id=$(cat "$PLAK_HOME/id")
    printf '{{"host_available":true,"config_error":null,"protocol":3,"receipt_version":3,"binary_id":"%s","state_key":"{key}","export_versions":[8],"runner_protocol":9,"runner_version":{{"protocol":9,"source_revision":"{revision}","flavor":"release"}}}}\n' "$id" ;;
  'version --state-key') printf '%s\n' '{key}' ;;
  'version --export-version') printf '8\n' ;;
  'version --build-info')
    test -f "$PLAK_HOME/build-info" || exit 2
    cat "$PLAK_HOME/build-info" ;;
  'wire schema')
    id=$(cat "$PLAK_HOME/id")
    printf '{{"binary_id":"%s","wire_version":11}}\n' "$id" ;;
  'deploy-canary --capabilities') printf 'plakd-deploy-canary-v1\nclaude\ngrok\n' ;;
  'gateway install --dry-run') printf '# %s/.config/systemd/user/gateway.service\ngenerated gateway\n' "$HOME" ;;
  'gateway status --dry-run') printf 'state_root %s/state\nsocket %s/gateway.sock\n' "$HOME" "$PLAK_HOME" ;;
  'gateway status')
    instance=$(cat "$PLAK_HOME/gateway-instance" 2>/dev/null || printf '{GATEWAY}')
    printf 'via gateway unix:%s/gateway.sock\ninstance %s\n' "$PLAK_HOME" "$instance" ;;
  'gateway check')
    instance=$(cat "$PLAK_HOME/gateway-instance" 2>/dev/null || printf '{GATEWAY}')
    printf 'via gateway unix:%s/gateway.sock\ngateway %s protocol 1,2\naccount claude/work\naccount claude/spare\n' "$PLAK_HOME" "$instance" ;;
  'status') printf 'via gateway unix:%s/gateway.sock\nclaude/work: credential=ready\nclaude/spare: credential=signed-out\n' "$PLAK_HOME" ;;
  'status claude/work') printf 'claude/work: credential=ready\n' ;;
  'deploy-credential-generation claude/work') printf '18\n' ;;
  'doctor') printf 'layout\tOK\n' ;;
  'session check-binding --all --summary') printf '{{"compatible":true,"count":200000}}\n' ;;
  'host upgrade '*) printf '%s\n' "$@" > "$PLAK_HOME/surrendered" ;;
  *) exit 8 ;;
esac
"#,
            revision = "b".repeat(40)
        ),
    );
    image
}

fn manager(home: &Path, host: &Path, exec: &Path, dropin: &str) {
    program(
        &home.join(".local/bin/systemctl"),
        &format!(
            r#"
test "$1" = --user && test "$2" = show
case "$3" in
  plakd-host.service)
    printf 'ActiveState=active\nMainPID=100\nFragmentPath=%s\nDropInPaths=%s\nNeedDaemonReload=no\n' '{host}' '{dropin}' ;;
  gateway.service)
    printf 'LoadState=loaded\nActiveState=active\nMainPID=101\nNeedDaemonReload=no\nDropInPaths=\nFragmentPath=%s/.config/systemd/user/gateway.service\nExecStart={{ path=%s ; argv[]=%s gateway serve ; }}\n' "$HOME" '{exec}' '{exec}' ;;
  *) exit 8 ;;
esac
"#,
            host = host.display(),
            exec = exec.display()
        ),
    );
}

#[test]
fn deploy_target_qualifies_the_actual_unit_and_serving_facts() {
    let sandbox = tempfile::Builder::new()
        .prefix("dt")
        .tempdir_in("/tmp")
        .unwrap();
    let home = sandbox.path().canonicalize().unwrap();
    let product = home.join("p");
    let source = image(&home, "source", KEY, "generated");
    let drifting = image(&home, "drift", KEY, "different");
    let cross = image(&home, "cross", "r99.e39.p39.t11", "generated");
    let gateway = home.join(".local/lib/plak/release/gateway/plakd");
    fs::create_dir_all(gateway.parent().unwrap()).unwrap();
    fs::write(&gateway, "test gateway image").unwrap();
    fs::set_permissions(&gateway, fs::Permissions::from_mode(0o555)).unwrap();
    let gateway_facts = FileFacts::read(&gateway).unwrap();
    let source_facts = FileFacts::read(&source).unwrap();
    let unit = home.join(".config/systemd/user/plakd-host.service");
    fs::create_dir_all(unit.parent().unwrap()).unwrap();
    fs::write(&unit, "generated\n").unwrap();
    fs::write(
        unit.with_file_name("gateway.service"),
        "generated gateway\n",
    )
    .unwrap();
    let gateway_link = home.join(".local/bin/plakd-gateway");
    manager(&home, &unit, &gateway_link, "");
    symlink(&source, home.join(".local/bin/plakd")).unwrap();
    symlink(&gateway, &gateway_link).unwrap();
    let proc_root = home.join("proc");
    for (pid, image) in [("100", &source), ("101", &gateway)] {
        let proc = proc_root.join(pid);
        fs::create_dir_all(&proc).unwrap();
        symlink(image, proc.join("exe")).unwrap();
    }
    fs::create_dir_all(product.join("host/run")).unwrap();
    fs::write(product.join("id"), &source_facts.display_id).unwrap();
    let _identity = Socket::start(
        &product.join("host/upgrade.sock"),
        json!({
            "protocol":3, "key":KEY, "instance":"100-0000000000000001",
            "binary":source_facts.display_id, "deploy_idle":true
        }),
        false,
    );
    let _welcome = Socket::start(
        &product.join(format!("host/run/{KEY}.sock")),
        json!({
            "frame":"welcome", "proto":11,"binary_id":source_facts.display_id,"state_key":KEY
        }),
        true,
    );
    let host = Host {
        node: Node {
            ssh: "example".into(),
            login: "example".into(),
            home: home.clone(),
            uid: rustix::process::geteuid().as_raw(),
            platform: super::Platform::Linux,
        },
        web_socket: product.join("web.sock"),
        gateway_socket: product.join("gateway.sock"),
        gateway_unit: "gateway.service".into(),
        gateway_sha256: gateway_facts.sha256,
        product_home: product,
        source_sha256: source_facts.sha256,
        ui: home.join("ui"),
    };
    let _web = UnixListener::bind(&host.web_socket).unwrap();
    let _gateway = UnixListener::bind(&host.gateway_socket).unwrap();
    let target = Target {
        host: &host,
        machine: "0".repeat(64),
        proc_root,
        launchd_tools: None,
    };
    assert_health(&target);
    assert_target_guards(&target, &source);
    assert!(
        target::check_unit(&target, &drifting)
            .unwrap_err()
            .message()
            .contains("unit template differs")
    );
    assert!(
        target::check_handoff_image(&target, &cross, KEY)
            .unwrap_err()
            .message()
            .contains("cross-key")
    );
    assert!(!host.product_home.join("surrendered").exists());
    manager(&home, &unit, &source, "");
    assert!(
        target
            .observe()
            .unwrap_err()
            .message()
            .contains("generated dedicated")
    );
    manager(&home, &unit, &gateway_link, "pinned.conf");
    assert_eq!(target.observe().unwrap_err().message(), NORMALIZE);
}

fn assert_health(target: &Target<'_>) {
    let observed = target.observe().unwrap().1;
    assert_eq!(observed.accounts["claude/work"], "ready");
    assert_eq!(observed.accounts["claude/spare"], "not_enrolled");
    assert!(observed.upgrade_idle);
    assert_eq!(observed.facts.source, "legacy facts");
    let health = target::health(
        target,
        &target.host.source_sha256,
        &[Route {
            account: "claude/work".into(),
            model: "example".into(),
            effort: "low".into(),
        }],
    )
    .unwrap();
    assert_eq!(health.binding_count, 200_000);
    assert_eq!(health.generations["claude/work"], Some(18));
}

fn assert_target_guards(target: &Target<'_>, source: &Path) {
    use crate::deploy::{
        inventory::Stage,
        protocol::{Candidate, HostOp},
    };
    let serving = target.observe().unwrap().1;
    let (_, snapshot) = crate::snapshot_install::tests::publish_fixture(
        &target.host.node.home,
        &fs::read(source).unwrap(),
    );
    let candidate = Candidate {
        flavor: crate::snapshot_install::Flavor::Release,
        snapshot,
        sha256: serving.sha256.clone(),
    };
    let mut unexpected = serving.clone();
    unexpected.instance = "100-0000000000000002".into();
    let null = || fs::File::open("/dev/null").unwrap();
    let error = target::upgrade(target, &candidate, &unexpected, 10, null()).unwrap_err();
    assert!(error.message().contains("serving facts changed"));
    let absent = Candidate {
        flavor: crate::snapshot_install::Flavor::Release,
        snapshot: "f".repeat(64),
        sha256: candidate.sha256.clone(),
    };
    let error = target::upgrade(target, &absent, &serving, 10, null()).unwrap_err();
    assert!(error.message().contains("candidate snapshot is missing"));
    assert!(!target.host.product_home.join("surrendered").exists());
    let wrong_instance = HostOp::HostLink {
        candidate: candidate.clone(),
        instance: unexpected.instance,
    };
    let error = target::execute_host(target, wrong_instance, &mut io::empty()).unwrap_err();
    assert!(error.message().contains("no longer Ready"));
    assert_host_fence(target, &serving, &absent);
    assert!(serving.paired.is_empty());
    target::upgrade(target, &candidate, &serving, 10, null()).unwrap();
    let capture = target.host.product_home.join("surrendered");
    assert!(
        fs::read_to_string(&capture).unwrap().lines().eq([
            "host",
            "upgrade",
            "--expect-binary",
            serving.facts.binary_id.as_str(),
            "--drain",
            "10s",
        ]),
        "an unpaired target must send the legacy requester argv without inspection flags",
    );
    fs::remove_file(capture).unwrap();
    assert_eq!(
        fs::read_link(target.host.node.home.join(".local/bin/plakd")).unwrap(),
        source
    );
    let marker = super::production_marker(&target.host.node);
    fs::write(&marker, b"production history").unwrap();
    let guarded = HostOp::MarkProduction {
        expected: target.stage_identity(),
    };
    assert!(
        target::guard_stage(target, Stage::Staging, &guarded)
            .unwrap_err()
            .message()
            .contains("production history")
    );
    target::guard_stage(target, Stage::Staging, &HostOp::Observe).unwrap();
    target::guard_stage(target, Stage::Production, &guarded).unwrap();
    fs::remove_file(marker).unwrap();
}

fn assert_host_fence(
    target: &Target<'_>,
    serving: &super::Observed,
    candidate: &crate::deploy::protocol::Candidate,
) {
    use crate::deploy::{fence::Fence, protocol::HostOp};
    let ticket = uuid::Uuid::new_v4();
    let fence = Fence::lock(&target.host.node, "upgrade", ticket).unwrap();
    let inherited = fence.requester_input().unwrap();
    drop(fence);
    let withdraw = || {
        target::execute_host(
            target,
            HostOp::WithdrawUpgrade {
                ticket,
                instance: serving.instance.clone(),
                source_sha256: serving.sha256.clone(),
            },
            &mut io::empty(),
        )
    };
    assert!(
        withdraw()
            .unwrap_err()
            .message()
            .contains("still holds the deploy fence")
    );
    drop(inherited);
    withdraw().unwrap();
    let late = HostOp::Upgrade {
        ticket,
        candidate: candidate.clone(),
        drain: 0,
        previous: Box::new(serving.clone()),
    };
    assert!(
        target::execute_host(target, late, &mut io::empty())
            .unwrap_err()
            .message()
            .contains("was withdrawn")
    );
    assert!(!target.host.product_home.join("surrendered").exists());
}
