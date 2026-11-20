use std::collections::BTreeSet;
use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::os::unix::fs::{PermissionsExt as _, symlink};
use std::os::unix::net::UnixListener;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

use serde_json::json;

use super::super::tests::{KEY, Socket, image, program};
use super::*;
use crate::deploy::inventory::{Host, Node};
use crate::deploy::protocol::Candidate;

const OLD: &str = "00000000-0000-0000-0000-000000000001";
const RESTORED: &str = "00000000-0000-0000-0000-000000000002";
const SUCCESSOR: &str = "00000000-0000-0000-0000-000000000003";

struct Fixture {
    _host: Socket,
    host: Host,
    link: PathBuf,
    candidate: Candidate,
    new: PathBuf,
    gateway: DrainSocket,
    _sandbox: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let sandbox = tempfile::Builder::new()
            .prefix("dg")
            .tempdir_in("/tmp")
            .unwrap();
        let home = sandbox.path().canonicalize().unwrap();
        let product = home.join("p");
        fs::create_dir(&product).unwrap();
        let source = image(&home, "source", KEY, "generated");
        let source_facts = FileFacts::read(&source).unwrap();
        let gateway = home.join(".local/lib/plak/release/gateway/plakd");
        fs::create_dir_all(gateway.parent().unwrap()).unwrap();
        fs::write(&gateway, b"source gateway image").unwrap();
        fs::set_permissions(&gateway, fs::Permissions::from_mode(0o555)).unwrap();
        let (snapshot_dir, snapshot) =
            snapshot_install::tests::publish_fixture(&home, &fs::read(&source).unwrap());
        let new = snapshot_dir.join("plakd");
        let candidate = Candidate {
            snapshot,
            sha256: FileFacts::read(&new).unwrap().sha256,
            flavor: snapshot_install::Flavor::Release,
        };
        let unit = home.join(".config/systemd/user/gateway.service");
        fs::create_dir_all(unit.parent().unwrap()).unwrap();
        fs::write(unit, "generated gateway\n").unwrap();
        let bin = home.join(".local/bin");
        fs::create_dir_all(&bin).unwrap();
        let link = bin.join("plakd-gateway");
        symlink(&source, bin.join("plakd")).unwrap();
        symlink(&gateway, &link).unwrap();
        for (pid, image) in [
            ("100", &source),
            ("101", &gateway),
            ("102", &gateway),
            ("103", &new),
        ] {
            let proc = home.join("proc").join(pid);
            fs::create_dir_all(&proc).unwrap();
            symlink(image, proc.join("exe")).unwrap();
        }
        let lock = home.join("state/plak/subscriptions/.custody-holder.lock");
        fs::create_dir_all(lock.parent().unwrap()).unwrap();
        fs::File::create(lock).unwrap();
        fs::write(product.join("id"), &source_facts.display_id).unwrap();
        fs::write(
            product.join("build-info"),
            json!({
                "revision":"b".repeat(40), "state_key":KEY, "flavor":"release", "test_hooks":false,
                "product_version":"0.0.4", "omitted_features":[],
                "wire":{"number":11,"schema_pin":shipping::digest(br#"{"wire_version":11}"#)},
                "export":{"read":[8],"write":8}, "host_upgrade":{"coordinator":3,"receipt":3},
                "gateway":{"served":[1,2],"spoken":[1,2]},
                "runner":{"protocol":9,"retention":{"served":[1,2],"spoken":[1,2]}}
            })
            .to_string(),
        )
        .unwrap();
        fs::create_dir(product.join("host")).unwrap();
        let host_socket = Socket::start(
            &product.join("host/upgrade.sock"),
            json!({
                "protocol":3, "key":KEY, "instance":"100-0000000000000001",
                "binary":source_facts.display_id, "deploy_idle":true
            }),
            false,
        );
        let gateway_socket = product.join("gateway.sock");
        let drain = DrainSocket::start(&gateway_socket, product.clone());
        let host = Host {
            node: Node {
                ssh: "example".into(),
                login: "example".into(),
                home,
                uid: rustix::process::geteuid().as_raw(),
                platform: Platform::Linux,
            },
            web_socket: product.join("web.sock"),
            gateway_socket,
            gateway_unit: "gateway.service".into(),
            gateway_sha256: FileFacts::read(&gateway).unwrap().sha256,
            product_home: product,
            source_sha256: source_facts.sha256,
            ui: bin.join("ui"),
        };
        program(&bin.join("systemctl"), MANAGER);
        let fixture = Self {
            _sandbox: sandbox,
            _host: host_socket,
            host,
            link,
            candidate,
            new,
            gateway: drain,
        };
        fixture.state("running");
        fixture
    }

    fn target(&self) -> Target<'_> {
        Target {
            host: &self.host,
            machine: "0".repeat(64),
            proc_root: self.host.node.home.join("proc"),
            launchd_tools: None,
        }
    }

    fn intent(&self) -> GatewayIntent {
        let target = self.target();
        GatewayIntent {
            ticket: uuid::Uuid::new_v4(),
            source: target.gateway_observation().unwrap(),
            candidate: self.candidate.clone(),
            source_host: self.host.source_sha256.clone(),
            source_instance: "100-0000000000000001".into(),
            target: target.stage_identity(),
            minimum: 1,
            served: vec![1, 2],
            accounts: BTreeSet::new(),
        }
    }

    fn state(&self, state: &str) {
        fs::write(self.host.product_home.join("manager-state"), state).unwrap();
    }

    fn prior_drain(&self, intent: &GatewayIntent) {
        Fence::lock(&self.host.node, "gateway-drain", intent.ticket)
            .unwrap()
            .claim_once()
            .unwrap();
    }

    fn linked(&self) -> PathBuf {
        fs::read_link(&self.link).unwrap()
    }

    fn events(&self) -> Vec<String> {
        fs::read_to_string(self.host.product_home.join("manager-events"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }
}

const MANAGER: &str = r#"
test "$1" = --user
state="$PLAK_HOME/manager-state"
events="$PLAK_HOME/manager-events"
link="$HOME/.local/bin/plakd-gateway"
case "$2 $3" in
  'show plakd-host.service') printf 'ActiveState=active\nMainPID=100\n' ;;
  'show gateway.service')
    printf 'LoadState=loaded\nNeedDaemonReload=no\nDropInPaths=\nFragmentPath=%s/.config/systemd/user/gateway.service\nExecStart={ path=%s ; argv[]=%s gateway serve ; }\n' "$HOME" "$link" "$link"
    case "$(cat "$state")" in
      running) printf 'ActiveState=active\nMainPID=101\n' ;;
      restored) printf 'ActiveState=active\nMainPID=102\n' ;;
      successor) printf 'ActiveState=active\nMainPID=103\n' ;;
      draining)
        printf 'transition\n' >> "$events"
        printf clean > "$state"
        printf 'ActiveState=deactivating\nMainPID=101\n' ;;
      clean) printf 'ActiveState=inactive\nSubState=dead\nMainPID=0\nResult=success\nExecMainStatus=0\n' ;;
      failed) printf 'ActiveState=failed\nMainPID=0\nResult=start-limit-hit\n' ;;
      *) exit 8 ;;
    esac ;;
  'reset-failed gateway.service')
    test "$#" = 3 && test "$(cat "$state")" = failed
    printf '%s\n' "$*" >> "$events"
    printf clean > "$state" ;;
  'start gateway.service')
    test "$#" = 3
    printf '%s\n' "$*" >> "$events"
    case "$(readlink "$link")" in
      */release/gateway/plakd)
        printf '00000000-0000-0000-0000-000000000002' > "$PLAK_HOME/gateway-instance"
        printf restored > "$state" ;;
      *)
        if test -f "$PLAK_HOME/start-successor"; then
          printf '00000000-0000-0000-0000-000000000003' > "$PLAK_HOME/gateway-instance"
          printf successor > "$state"
        else
          printf failed > "$state"
          test ! -f "$PLAK_HOME/start-errors"
        fi ;;
    esac ;;
  *) exit 8 ;;
esac
"#;

struct DrainSocket {
    stop: Arc<AtomicBool>,
    posts: Arc<AtomicUsize>,
    thread: Option<thread::JoinHandle<()>>,
}

impl DrainSocket {
    fn start(path: &Path, product: PathBuf) -> Self {
        let listener = UnixListener::bind(path).unwrap();
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let posts = Arc::new(AtomicUsize::new(0));
        let cancelled = Arc::clone(&stop);
        let count = Arc::clone(&posts);
        let thread = thread::spawn(move || {
            while !cancelled.load(Ordering::Relaxed) {
                let mut peer = match listener.accept() {
                    Ok((peer, _)) => peer,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2));
                        continue;
                    }
                    Err(error) => panic!("{error}"),
                };
                peer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
                let mut reader = BufReader::new(&peer);
                let mut head = String::new();
                reader.read_line(&mut head).unwrap();
                let mut length = 0;
                loop {
                    let mut row = String::new();
                    reader.read_line(&mut row).unwrap();
                    if row == "\r\n" || row.is_empty() {
                        break;
                    }
                    if let Some(value) = row.strip_prefix("content-length: ") {
                        length = value.trim().parse().unwrap();
                    }
                }
                let mut body = vec![0; length];
                reader.read_exact(&mut body).unwrap();
                let reply = if head == "GET /versions HTTP/1.1\r\n" {
                    json!({"versions":[1,2],"instance":OLD})
                } else {
                    assert_eq!(head, "POST /v2/admin HTTP/1.1\r\n");
                    assert_eq!(
                        serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
                        json!({"request":"drain"})
                    );
                    count.fetch_add(1, Ordering::Relaxed);
                    if !product.join("hold-drain").exists() {
                        fs::write(product.join("manager-state"), "draining").unwrap();
                    }
                    json!({"response":"draining"})
                }
                .to_string();
                write!(
                    peer,
                    "HTTP/1.1 200 OK\r\ncontent-length: {}\r\n\r\n{reply}",
                    reply.len()
                )
                .unwrap();
            }
        });
        Self {
            stop,
            posts,
            thread: Some(thread),
        }
    }
}

impl Drop for DrainSocket {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.thread.take().unwrap().join().unwrap();
    }
}

#[test]
fn deploy_gateway_target_refuses_unclaimed_exit_held_lock_and_missing_protocol() {
    let fixture = Fixture::new();
    let intent = fixture.intent();
    let target = fixture.target();
    let mut incompatible = intent.clone();
    incompatible.served = vec![1];
    assert!(
        target
            .switch_gateway(&incompatible, true)
            .unwrap_err()
            .message()
            .contains("protocol")
    );
    fixture.state("clean");
    assert!(
        target
            .switch_gateway(&intent, false)
            .unwrap_err()
            .message()
            .contains("without the recorded drain")
    );
    fixture.prior_drain(&intent);
    let lockfile = fixture
        .host
        .node
        .home
        .join("state/plak/subscriptions/.custody-holder.lock");
    let holder = fs::File::open(lockfile).unwrap();
    rustix::fs::flock(
        &holder,
        rustix::fs::FlockOperation::NonBlockingLockExclusive,
    )
    .unwrap();
    assert!(
        target
            .switch_gateway(&intent, false)
            .unwrap_err()
            .message()
            .contains("released its lock")
    );
    assert_eq!(fixture.linked(), intent.source.link.path);
    snapshot_install::atomic_link(&fixture.link, &fixture.new).unwrap();
    assert!(
        target
            .switch_gateway(&intent, false)
            .unwrap_err()
            .message()
            .contains("released its lock")
    );
    assert_eq!(fixture.linked(), fixture.new);
    assert_eq!(fixture.events(), Vec::<String>::new());
    assert_eq!(fixture.gateway.posts.load(Ordering::Relaxed), 0);
}

#[test]
fn deploy_gateway_target_claimed_drain_is_not_resent_or_reopened_after_withdrawal() {
    let fixture = Fixture::new();
    let intent = fixture.intent();
    let target = fixture.target();
    let unused = fixture.intent();
    assert!(matches!(
        target.switch_gateway(&unused, false).unwrap(),
        GatewayOutcome::Withdrawn
    ));
    assert!(
        target
            .switch_gateway(&unused, true)
            .unwrap_err()
            .message()
            .contains("was withdrawn")
    );

    fixture.prior_drain(&intent);
    assert!(
        target
            .switch_gateway(&intent, true)
            .unwrap_err()
            .message()
            .contains("drain was already claimed")
    );
    assert!(
        target
            .switch_gateway(&intent, false)
            .unwrap_err()
            .message()
            .contains("drain outcome is unresolved")
    );
    assert_eq!(fixture.gateway.posts.load(Ordering::Relaxed), 0);
    assert_eq!(fixture.linked(), intent.source.link.path);
    assert_eq!(fixture.events(), Vec::<String>::new());
}

#[test]
fn deploy_gateway_target_waits_for_drain_and_restores_after_start_error_or_failed_holder() {
    for manager_reports_error in [false, true] {
        let fixture = Fixture::new();
        let intent = fixture.intent();
        let target = fixture.target();
        if manager_reports_error {
            fs::write(fixture.host.product_home.join("start-errors"), "").unwrap();
        }
        assert!(matches!(
            target.switch_gateway(&intent, true).unwrap(),
            GatewayOutcome::Withdrawn
        ));
        assert_eq!(fixture.gateway.posts.load(Ordering::Relaxed), 1);
        assert_eq!(fixture.linked(), intent.source.link.path);
        let holder = target.gateway_observation().unwrap().holder;
        assert_eq!(holder.instance, RESTORED);
        assert_eq!(holder.pid, "102");
        assert!(
            target
                .switch_gateway(&intent, true)
                .unwrap_err()
                .message()
                .contains("was withdrawn")
        );
        assert!(matches!(
            target.switch_gateway(&intent, false).unwrap(),
            GatewayOutcome::Withdrawn
        ));
        assert_eq!(fixture.gateway.posts.load(Ordering::Relaxed), 1);
        assert_eq!(
            fixture.events(),
            [
                "transition",
                "--user start gateway.service",
                "--user reset-failed gateway.service",
                "--user start gateway.service",
            ]
        );
    }
}

#[test]
fn deploy_gateway_target_reconcile_finishes_recorded_restoration_without_retrying_candidate() {
    for restored_link in [false, true] {
        let fixture = Fixture::new();
        let intent = fixture.intent();
        fixture.prior_drain(&intent);
        if restored_link {
            Fence::lock(&fixture.host.node, "gateway-restore", intent.ticket)
                .unwrap()
                .claim_once()
                .unwrap();
        } else {
            snapshot_install::atomic_link(&fixture.link, &fixture.new).unwrap();
        }
        fixture.state("clean");
        assert!(matches!(
            fixture.target().switch_gateway(&intent, false).unwrap(),
            GatewayOutcome::Withdrawn
        ));
        assert_eq!(fixture.linked(), intent.source.link.path);
        assert_eq!(fixture.events(), ["--user start gateway.service"]);
        assert_eq!(fixture.gateway.posts.load(Ordering::Relaxed), 0);
    }
}

#[test]
fn deploy_gateway_target_reconciles_a_running_successor_without_restarting_it() {
    let fixture = Fixture::new();
    let intent = fixture.intent();
    let target = fixture.target();
    fs::write(fixture.host.product_home.join("start-successor"), "").unwrap();
    for begin in [true, false] {
        let GatewayOutcome::Ready { observed } = target.switch_gateway(&intent, begin).unwrap()
        else {
            panic!("expected a ready gateway");
        };
        assert_eq!(observed.holder.instance, SUCCESSOR);
    }
    assert!(
        target
            .switch_gateway(&intent, true)
            .unwrap_err()
            .message()
            .contains("drain was already claimed")
    );
    assert_eq!(fixture.linked(), fixture.new);
    assert_eq!(
        fixture.events(),
        ["transition", "--user start gateway.service"]
    );
    assert_eq!(fixture.gateway.posts.load(Ordering::Relaxed), 1);
}

#[test]
fn deploy_gateway_launchctl_checks_exit_and_starts_without_killing() {
    let fixture = Fixture::new();
    let mut host = fixture.host.clone();
    host.node.platform = Platform::Mac;
    host.gateway_unit = "com.plakd.gateway".into();
    let plist = host
        .node
        .home
        .join("Library/LaunchAgents/com.plakd.gateway.plist");
    fs::create_dir_all(plist.parent().unwrap()).unwrap();
    fs::write(&plist, "test plist").unwrap();
    let tool = host.node.home.join("fake-launchctl");
    let plutil = host.node.home.join("fake-plutil");
    let service = format!("gui/{}/{}", host.node.uid, host.gateway_unit);
    program(
        &tool,
        &format!(
            r#"
test "$#" = 2 && test "$2" = '{service}'
case "$1" in
  print)
    printf '%s = {{\n\tpath = %s\n\ttype = LaunchAgent\n\tstate = not running\n\tprogram = %s\n\targuments = {{\n\t\t%s\n\t\tgateway\n\t\tserve\n\t}}\n\tlast exit code = %s\n}}\n' "$2" '{plist}' '{link}' '{link}' "$(cat "$PLAK_HOME/exit")" ;;
  kickstart) printf '%s\n' "$*" > "$PLAK_HOME/launchctl-events" ;;
  *) exit 8 ;;
esac"#,
            plist = plist.display(),
            link = fixture.link.display()
        ),
    );
    let json = json!({"ProgramArguments":[fixture.link,"gateway","serve"]});
    program(&plutil, &format!("printf '%s\\n' '{json}'"));
    let target = Target {
        host: &host,
        launchd_tools: Some((&tool, &plutil)),
        ..fixture.target()
    };
    for (exit, expected) in [("0", State::CleanExit), ("1", State::Transition)] {
        fs::write(host.product_home.join("exit"), exit).unwrap();
        assert_eq!(target.gateway_manager(&fixture.link).unwrap().1, expected);
    }
    target.start_gateway(false).unwrap();
    assert_eq!(
        fs::read_to_string(host.product_home.join("launchctl-events")).unwrap(),
        format!("kickstart {service}\n")
    );
}
