//! Subscription admin contracts through a real gateway listener.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "offline integration test"
)]

use std::fmt::Write as _;
use std::fs;
use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use plakous::gateway::protocol::admin::{
    AdminRefusal, AdminRequest, AdminResponse, LoginMethod, LoginPhase, LogoutMode,
};
use plakous::gateway::protocol::facts::CredentialStatus;
use plakous::gateway::protocol::refusal::GatewayRefusal;
use plakous::gateway::protocol::scope::{CallerId, ScopeSet, SourceNet};
use plakous::gateway::protocol::{
    BoundedText, Endpoint, GatewayAccount, SecretText, UuidText, call,
};
use plakous::gateway::{GatewayEndpoint, RelayCallError, RelayClient, SensitiveBytes};
use plakous::subscriptions::{NativeProvider, NativeSubscriptions};

use super::daemon_lifetime::Lifetime;
use super::offline_support::TestChild;

const SUBSCRIPTIONS: [(&str, NativeProvider); 3] = [
    ("codex", NativeProvider::Codex),
    ("claude", NativeProvider::Claude),
    ("grok", NativeProvider::Xai),
];

struct GatewayFixture {
    relay: RelayClient,
    executor: plakous::application::ApplicationExecutor,
    process: TestChild,
    _lifetime: Lifetime,
    root: tempfile::TempDir,
}

impl GatewayFixture {
    fn start() -> Self {
        Self::start_with(None, None)
    }

    fn start_with(tcp: Option<SocketAddr>, oauth: Option<&TcpListener>) -> Self {
        Self::start_with_fault(tcp, oauth, "")
    }

    fn start_with_fault(tcp: Option<SocketAddr>, oauth: Option<&TcpListener>, fault: &str) -> Self {
        Self::start_fixture(tcp, oauth, fault, false, None)
    }

    fn start_fixture(
        tcp: Option<SocketAddr>,
        oauth: Option<&TcpListener>,
        fault: &str,
        api_keys: bool,
        api: Option<&TcpListener>,
    ) -> Self {
        let root = tempfile::Builder::new().prefix("gw").tempdir().unwrap();
        let base = root.path();
        fs::set_permissions(base, fs::Permissions::from_mode(0o700)).unwrap();
        let lifetime = Lifetime::new(base);
        let home = base.join("home");
        fs::DirBuilder::new().mode(0o700).create(&home).unwrap();
        let socket = base.join("g.sock");
        let mut server = format!("socket = {:?}\n", socket.to_str().unwrap());
        if let Some(tcp) = tcp {
            writeln!(server, "listen_tcp = \"{tcp}\"").unwrap();
        }
        fs::write(home.join("gateway.toml"), server).unwrap();
        fs::write(
            home.join("config.toml"),
            format!("[gateway]\naddress = \"unix:{}\"\n", socket.display()),
        )
        .unwrap();
        for (_, provider) in SUBSCRIPTIONS {
            super::gateway_seed::seed(&state(base), provider, None, 4_000_000_000);
        }
        super::gateway_seed::seed(
            &state(base),
            NativeProvider::Claude,
            Some("work"),
            4_000_000_000,
        );
        let mut serve = command(base);
        if api_keys {
            for variable in [
                "OPENAI_API_KEY",
                "ANTHROPIC_API_KEY",
                "XAI_API_KEY",
                "OPENROUTER_API_KEY",
            ] {
                serve.env(variable, "synthetic-deploy-test-key");
            }
        }
        serve
            .args(["gateway", "serve"])
            .envs([Lifetime::environment(base)])
            .env("PLAK_TEST_GATEWAY_REVIEW_CLOCK", "1")
            .env(
                "PLAK_TEST_FAULTS",
                format!(
                    "{fault}host=api.anthropic.com,path=/api/oauth/usage,fault=429;\
                 host=api.anthropic.com,path=/api/oauth/profile,fault=503;\
                 host=platform.claude.com,path=/v1/oauth/token,fault=400;{}",
                    if api.is_some() { "" } else { "fault=connect" }
                ),
            )
            .stdin(Stdio::null())
            .stdout(Stdio::null());
        if let Some(oauth) = oauth {
            serve.env(
                "PLAK_TEST_GATEWAY_OAUTH_PORT",
                oauth.local_addr().unwrap().port().to_string(),
            );
        }
        if let Some(api) = api {
            serve.env(
                "PLAK_TEST_GATEWAY_API_PORT",
                api.local_addr().unwrap().port().to_string(),
            );
        }
        let mut process = TestChild::spawn(&mut serve);
        let deadline = Instant::now() + Duration::from_secs(10);
        // The socket file appears at bind, before the listener accepts; wait
        // until a connection succeeds rather than for the path alone.
        while std::os::unix::net::UnixStream::connect(&socket).is_err() {
            assert_eq!(process.try_wait().unwrap(), None, "gateway exited");
            assert!(Instant::now() < deadline, "gateway socket did not accept");
            std::thread::sleep(Duration::from_millis(10));
        }
        Self {
            relay: RelayClient::new(GatewayEndpoint::Unix(socket), None).unwrap(),
            executor: plakous::application::ApplicationExecutor::new().unwrap(),
            process,
            _lifetime: lifetime,
            root,
        }
    }

    fn admin(&self, request: &AdminRequest) -> AdminResponse {
        self.try_admin(request).unwrap()
    }

    fn try_admin(&self, request: &AdminRequest) -> Result<AdminResponse, RelayCallError> {
        self.admin_via(&self.relay, request)
    }

    fn admin_via(
        &self,
        relay: &RelayClient,
        request: &AdminRequest,
    ) -> Result<AdminResponse, RelayCallError> {
        let reply = self
            .executor
            .block_on(relay.call(
                Endpoint::Admin,
                call::encode(request).unwrap(),
                &RelayClient::call_control(),
            ))
            .unwrap()?;
        Ok(call::decode(&reply).unwrap())
    }

    fn add_caller(&self, scopes: &str) -> SecretText {
        let response = self.admin(&AdminRequest::CallerAdd {
            caller: CallerId::new("fixture".to_owned()).unwrap(),
            scopes: ScopeSet::parse(scopes).unwrap(),
            sources: vec![SourceNet::parse("127.0.0.1/32").unwrap()],
        });
        let AdminResponse::CallerKeyIssued { key, .. } = response else {
            panic!("could not create fixture caller: {response:?}");
        };
        key
    }

    fn credential(&self, name: &str) -> CredentialStatus {
        let response = self.admin(&AdminRequest::Status {
            accounts: vec![account(name)],
        });
        let AdminResponse::Status { rows } = response else {
            panic!("unexpected status: {response:?}");
        };
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].facts.account.as_str(), name);
        rows[0].facts.credential.status
    }

    fn finish(mut self) {
        assert_eq!(
            self.admin(&AdminRequest::Drain {}),
            AdminResponse::Draining {}
        );
        assert!(self.process.wait().unwrap().success());
    }
}

fn state(root: &Path) -> PathBuf {
    root.join("xdg")
}

fn command(root: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_plakd"));
    command
        .current_dir(root)
        .env("HOME", root)
        .env("PLAK_HOME", root.join("home"))
        .env("XDG_STATE_HOME", state(root))
        .env_remove("PLAK_TEST_GATEWAY")
        .env_remove("PLAK_TEST_GATEWAY_OAUTH_PORT")
        .env_remove("PLAK_TEST_GATEWAY_API_PORT")
        .env_remove("PLAK_TEST_FAULTS");
    for key in [
        "OPENAI_API_KEY",
        "ANTHROPIC_API_KEY",
        "XAI_API_KEY",
        "OPENROUTER_API_KEY",
    ] {
        command.env_remove(key);
    }
    command
}

fn account(name: &str) -> GatewayAccount {
    GatewayAccount::new(name.to_owned()).unwrap()
}

#[test]
fn gateway_reports_all_ready_subscriptions_to_admin_status_and_doctor() {
    let gateway = GatewayFixture::start();
    let selected = SUBSCRIPTIONS.map(|(name, _)| account(name));
    let response = gateway.admin(&AdminRequest::Status {
        accounts: selected.to_vec(),
    });
    let AdminResponse::Status { rows } = response else {
        panic!("unexpected status response: {response:?}");
    };
    assert_eq!(rows.len(), selected.len(), "{rows:?}");
    for (row, expected) in rows.iter().zip(selected) {
        assert_eq!(row.facts.account, expected);
        assert_eq!(row.facts.credential.status, CredentialStatus::Ready);
    }
    let response = gateway.admin(&AdminRequest::Status { accounts: vec![] });
    let AdminResponse::Status { rows } = response else {
        panic!("unexpected all-account status response: {response:?}");
    };
    for name in ["codex", "claude", "grok", "claude/work"] {
        assert!(
            rows.iter().any(|row| row.facts.account.as_str() == name
                && row.facts.credential.status == CredentialStatus::Ready),
            "missing Ready {name}: {rows:?}"
        );
    }

    assert_eq!(
        gateway.admin(&AdminRequest::Status {
            accounts: vec![account("codex"), account("claude/missing")],
        }),
        AdminResponse::Refused {
            refusal: AdminRefusal::Unavailable {}
        },
        "a facts failure must refuse the request, not return a partial roster",
    );

    // Doctor may report unrelated setup checks for this deliberately minimal home.
    let output = command(gateway.root.path()).arg("doctor").output().unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();
    for (name, _) in SUBSCRIPTIONS {
        let label = format!("OAuth {name}");
        let row = stdout
            .lines()
            .find(|line| line.contains(&label))
            .unwrap_or("");
        assert!(
            row.contains("OK")
                && row.contains("ready")
                && row.contains("record held by the gateway"),
            "{row}\n{stdout}"
        );
        let output = command(gateway.root.path())
            .args(["status", name])
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        let status = String::from_utf8(output.stdout).unwrap();
        assert!(
            status
                .lines()
                .any(|line| line == format!("{name}: credential=ready")),
            "{status}"
        );
    }
    gateway.finish();
}

#[test]
fn gateway_lists_logs_out_and_removes_the_named_slot_it_holds() {
    let gateway = GatewayFixture::start();
    let work = account("claude/work");
    let list = || {
        let response = gateway.admin(&AdminRequest::List {});
        let AdminResponse::Accounts { accounts } = response else {
            panic!("unexpected List response: {response:?}");
        };
        for (name, _) in SUBSCRIPTIONS {
            assert!(accounts.contains(&account(name)), "missing {name}");
        }
        accounts
    };
    assert!(list().contains(&work));
    let remove = || {
        gateway.admin(&AdminRequest::Remove {
            account: work.clone(),
        })
    };
    assert_eq!(
        remove(),
        AdminResponse::Refused {
            refusal: AdminRefusal::AccountSignedIn {}
        }
    );
    assert_eq!(
        gateway.admin(&AdminRequest::Logout {
            account: work.clone(),
            mode: LogoutMode::Local
        }),
        AdminResponse::Changed {
            account: work.clone(),
            remote_grant_may_remain: Some(true)
        },
    );
    assert_eq!(gateway.credential("claude/work"), CredentialStatus::Revoked);
    assert_eq!(gateway.credential("claude"), CredentialStatus::Ready);
    assert_eq!(
        remove(),
        AdminResponse::Changed {
            account: work.clone(),
            remote_grant_may_remain: None
        }
    );
    assert!(!list().contains(&work));
    gateway.finish();
}

#[test]
fn gateway_reads_allowance_and_allows_login_after_refresh_quarantines_a_grant() {
    let gateway = GatewayFixture::start();
    let claude = account("claude");
    let response = gateway.admin(&AdminRequest::Allowance {
        account: claude.clone(),
    });
    assert!(matches!(response, AdminResponse::Allowance {
            account, view: None, unavailable: Some(reason), observed_at_unix_ms: Some(_),
        } if account == claude && reason.as_str() == "rate-limited"));
    let output = command(gateway.root.path())
        .args(["usage", "--account", "claude"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout.contains("rate-limited") && stdout.contains("Limits"),
        "{stdout}"
    );

    quarantine_with_pending_login(&gateway);
    let (attempt, _) = start_login(&gateway);
    assert_eq!(gateway.credential("claude"), CredentialStatus::Revoked);
    let poll = gateway.admin(&AdminRequest::LoginPoll {
        attempt: attempt.clone(),
    });
    assert!(
        matches!(poll, AdminResponse::Progress { attempt: id, phase: LoginPhase::AwaitingInput, .. } if id == attempt)
    );
    assert_eq!(
        gateway.admin(&AdminRequest::LoginComplete {
            attempt: attempt.clone(),
            pasted: paste("not a callback".to_owned()),
        }),
        AdminResponse::Refused {
            refusal: AdminRefusal::PasteInvalid {}
        },
    );
    let cancelled = gateway.admin(&AdminRequest::LoginCancel {
        attempt: attempt.clone(),
    });
    assert!(
        matches!(cancelled, AdminResponse::Progress { attempt: id, phase: LoginPhase::Cancelled, .. } if id == attempt)
    );
    assert_eq!(
        gateway.admin(&AdminRequest::LoginPoll { attempt }),
        AdminResponse::Refused {
            refusal: AdminRefusal::AttemptUnknown {}
        },
    );

    let (attempt, authorize_url) = start_login(&gateway);
    let state = authorize_url
        .expose()
        .split(['?', '&'])
        .find_map(|part| part.strip_prefix("state="))
        .unwrap();
    let response = gateway.admin(&AdminRequest::LoginComplete {
        attempt: attempt.clone(),
        pasted: paste(format!("{}#{state}", "fixture-code".repeat(3))),
    });
    assert_eq!(
        response,
        AdminResponse::Refused {
            refusal: AdminRefusal::ProviderRejected {}
        }
    );
    assert_eq!(
        gateway.admin(&AdminRequest::LoginPoll { attempt }),
        AdminResponse::Refused {
            refusal: AdminRefusal::AttemptUnknown {}
        },
    );
    let response = gateway.admin(&AdminRequest::Logout {
        account: claude.clone(),
        mode: LogoutMode::Remote,
    });
    assert_eq!(
        response,
        AdminResponse::Changed {
            account: claude,
            remote_grant_may_remain: None,
        }
    );
    assert_eq!(gateway.credential("claude"), CredentialStatus::Revoked);
    gateway.finish();
}

fn quarantine_with_pending_login(gateway: &GatewayFixture) {
    let (attempt, _) = start_login(gateway);
    // The forced refresh is fault-scripted; its rejected grant is quarantined.
    assert!(matches!(
        gateway.try_admin(&AdminRequest::Refresh {
            account: account("claude")
        }),
        Err(RelayCallError::Refused(GatewayRefusal::Internal {}))
    ));
    assert_eq!(gateway.credential("claude"), CredentialStatus::Quarantined);
    assert_eq!(
        gateway.admin(&login_request("claude", NativeProvider::Claude)),
        AdminResponse::Refused {
            refusal: AdminRefusal::AttemptLimit {}
        },
    );
    assert_eq!(gateway.credential("claude"), CredentialStatus::Quarantined);
    assert!(matches!(
        gateway.admin(&AdminRequest::LoginCancel { attempt }),
        AdminResponse::Progress {
            phase: LoginPhase::Cancelled,
            ..
        }
    ));
}

fn start_login(gateway: &GatewayFixture) -> (UuidText, SecretText) {
    paste_challenge(gateway.admin(&login_request("claude", NativeProvider::Claude)))
}

fn paste_challenge(response: AdminResponse) -> (UuidText, SecretText) {
    let AdminResponse::Challenge {
        attempt,
        method: LoginMethod::Paste,
        authorize_url,
        ..
    } = response
    else {
        panic!("gateway administrator could not get a login challenge: {response:?}");
    };
    (attempt, authorize_url)
}

fn login_request(name: &str, provider: NativeProvider) -> AdminRequest {
    let notice = NativeSubscriptions::notice(provider).unwrap();
    AdminRequest::LoginStart {
        account: account(name),
        notice_revision: BoundedText::new(notice.notice_revision().to_owned()).unwrap(),
        replace_newer: false,
    }
}

fn paste(value: String) -> SecretText {
    SecretText::new(zeroize::Zeroizing::new(value)).unwrap()
}

pub(super) fn loopback_listener() -> TcpListener {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    listener.set_nonblocking(true).unwrap();
    listener
}

pub(super) fn oauth_request(listener: &TcpListener, path: &str) -> TcpStream {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut peer = loop {
        match listener.accept() {
            Ok((stream, _)) => break stream,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(
                    Instant::now() < deadline,
                    "OAuth fixture did not receive {path}"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("could not accept OAuth fixture request: {error}"),
        }
    };
    peer.set_nonblocking(false).unwrap();
    crate::socket_timeout::allow_closed_peer(peer.set_read_timeout(Some(Duration::from_secs(5))))
        .unwrap();
    let mut reader = BufReader::new(&mut peer);
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    assert_eq!(line, format!("POST {path} HTTP/1.1\r\n"));
    let mut length = None;
    loop {
        line.clear();
        assert_ne!(reader.read_line(&mut line).unwrap(), 0);
        if line == "\r\n" {
            break;
        }
        if let Some((name, value)) = line.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            length = Some(value.trim().parse::<usize>().unwrap());
        }
    }
    let mut body = vec![0; length.unwrap()];
    reader.read_exact(&mut body).unwrap();
    peer
}

pub(super) fn oauth_reply(mut peer: TcpStream, status: &str, body: &str) {
    write!(peer, "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len()).unwrap();
}

const CODEX_USER_CODE_PATH: &str = "/api/accounts/deviceauth/usercode";
const CODEX_DEVICE_TOKEN_PATH: &str = "/api/accounts/deviceauth/token";
const CLAUDE_TOKEN_PATH: &str = "/v1/oauth/token";

#[test]
fn gateway_device_login_polls_and_rejects_concurrent_duplicate_start() {
    let oauth = loopback_listener();
    let gateway = GatewayFixture::start_with(None, Some(&oauth));
    let start = login_request("codex", NativeProvider::Codex);
    let (challenge, duplicate) = std::thread::scope(|scope| {
        let first = scope.spawn(|| gateway.admin(&start));
        let peer = oauth_request(&oauth, CODEX_USER_CODE_PATH);
        let second = scope.spawn(|| gateway.admin(&start));
        oauth_reply(
            peer,
            "200 OK",
            r#"{"device_auth_id":"fixture-device-id","user_code":"ABCD-1234","interval":"0"}"#,
        );
        (first.join().unwrap(), second.join().unwrap())
    });
    assert_eq!(
        duplicate,
        AdminResponse::Refused {
            refusal: AdminRefusal::AttemptLimit {}
        }
    );
    let AdminResponse::Challenge {
        attempt,
        method: LoginMethod::DeviceCode,
        user_code: Some(code),
        ..
    } = challenge
    else {
        panic!("device challenge not returned: {challenge:?}");
    };
    assert_eq!(code, paste("ABCD-1234".to_owned()));
    let peer = oauth_request(&oauth, CODEX_DEVICE_TOKEN_PATH);
    let poll = AdminRequest::LoginPoll {
        attempt: attempt.clone(),
    };
    assert!(matches!(gateway.admin(&poll), AdminResponse::Progress {
        attempt: id, phase: LoginPhase::AwaitingInput, ..
    } if id == attempt));
    oauth_reply(peer, "400 Bad Request", "{}");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match gateway.admin(&poll) {
            AdminResponse::Refused {
                refusal: AdminRefusal::ProviderRejected {},
            } => break,
            AdminResponse::Refused {
                refusal: AdminRefusal::AttemptLimit {},
            }
            | AdminResponse::Progress {
                phase: LoginPhase::AwaitingInput,
                ..
            } => {
                assert!(
                    Instant::now() < deadline,
                    "device login did not report rejection"
                );
                std::thread::sleep(Duration::from_millis(100));
            }
            other => panic!("unexpected device poll: {other:?}"),
        }
    }
    assert_eq!(gateway.credential("codex"), CredentialStatus::Ready);
    gateway.finish();
}

#[test]
fn gateway_tcp_admin_cannot_replace_ready_provider_account() {
    let oauth = loopback_listener();
    let tcp = loopback_listener().local_addr().unwrap();
    let gateway = GatewayFixture::start_with(Some(tcp), Some(&oauth));
    let key = gateway.add_caller("relay:claude,admin:claude");
    let caller = RelayClient::new(
        GatewayEndpoint::Tcp(tcp),
        Some(SensitiveBytes::new(key.expose().as_bytes().to_vec())),
    )
    .unwrap();
    let path =
        super::gateway_seed::primary_record(&state(gateway.root.path()), NativeProvider::Claude);
    let before = fs::read(&path).unwrap();
    assert_eq!(gateway.credential("claude"), CredentialStatus::Ready);
    let (attempt, authorize_url) = paste_challenge(
        gateway
            .admin_via(&caller, &login_request("claude", NativeProvider::Claude))
            .unwrap(),
    );
    let state = authorize_url
        .expose()
        .split(['?', '&'])
        .find_map(|part| part.strip_prefix("state="))
        .unwrap();
    let complete = AdminRequest::LoginComplete {
        attempt,
        pasted: paste(format!("{}#{state}", "fixture-code".repeat(3))),
    };
    let reply = std::thread::scope(|scope| {
        let call = scope.spawn(|| gateway.admin_via(&caller, &complete).unwrap());
        let peer = oauth_request(&oauth, CLAUDE_TOKEN_PATH);
        oauth_reply(
            peer,
            "200 OK",
            r#"{"access_token":"fixture-access-new","refresh_token":"fixture-refresh-new","expires_in":3600,"scope":"user:inference","account":{"uuid":"22345678-1234-4234-8234-123456789abc"}}"#,
        );
        call.join().unwrap()
    });
    assert_eq!(
        reply,
        AdminResponse::Refused {
            refusal: AdminRefusal::LocalAdminRequired {}
        }
    );
    assert!(
        fs::read(path).unwrap() == before,
        "the credential record changed"
    );
    assert_eq!(gateway.credential("claude"), CredentialStatus::Ready);
    gateway.finish();
}

#[test]
fn gateway_cli_rosters_print_successful_and_refused_rows() {
    let tcp = loopback_listener().local_addr().unwrap();
    let gateway = GatewayFixture::start_with(Some(tcp), None);
    let key = gateway.add_caller("relay:claude,relay:openai");
    let home = gateway.root.path().join("home");
    let key_file = home.join("caller.key");
    fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(&key_file)
        .unwrap()
        .write_all(key.expose().as_bytes())
        .unwrap();
    fs::write(
        home.join("config.toml"),
        format!(
            "[gateway]\naddress = \"tcp:{tcp}\"\nkey_file = {:?}\n",
            key_file.to_str().unwrap(),
        ),
    )
    .unwrap();
    for (args, healthy) in [
        (&["status"][..], "claude: credential=ready"),
        (
            &["key", "status"][..],
            "openai: credential=signed-out source=missing",
        ),
    ] {
        let output = command(gateway.root.path()).args(args).output().unwrap();
        assert!(output.status.success(), "{args:?}: {output:?}");
        let rows = String::from_utf8(output.stdout).unwrap();
        assert!(rows.lines().any(|row| row == healthy), "{rows}");
        assert!(
            !rows.contains("codex") && !rows.contains("grok") && !rows.contains("claude/work"),
            "out-of-scope accounts must not be enumerated in roster: {rows}"
        );
    }
    let usage = command(gateway.root.path())
        .args(["usage"])
        .output()
        .unwrap();
    assert!(usage.status.success(), "{usage:?}");
    let limits = String::from_utf8(usage.stdout).unwrap();
    assert!(limits.contains("rate-limited"), "{limits}");
    assert!(
        !limits.contains("codex") && !limits.contains("grok"),
        "out-of-scope accounts must not appear in usage roster: {limits}"
    );
    let single = command(gateway.root.path())
        .args(["status", "codex"])
        .output()
        .unwrap();
    assert!(!single.status.success(), "{single:?}");
    assert!(
        String::from_utf8_lossy(&single.stderr).contains("scope_denied"),
        "{single:?}"
    );
    let single_usage = command(gateway.root.path())
        .args(["usage", "--account", "codex"])
        .output()
        .unwrap();
    assert!(!single_usage.status.success(), "{single_usage:?}");
    assert!(
        String::from_utf8_lossy(&single_usage.stderr).contains("scope_denied"),
        "{single_usage:?}"
    );
    gateway.finish();
}

#[test]
fn gateway_drain_keeps_holder_while_an_admin_request_is_in_flight() {
    let oauth = loopback_listener();
    let mut gateway = GatewayFixture::start_with(None, Some(&oauth));
    let holder = state(gateway.root.path()).join("plak/subscriptions/.custody-holder.lock");
    let holder = fs::File::open(holder).unwrap();
    let shared = rustix::fs::FlockOperation::NonBlockingLockShared;
    std::thread::scope(|scope| {
        let call = scope.spawn(|| gateway.admin(&login_request("codex", NativeProvider::Codex)));
        let peer = oauth_request(&oauth, CODEX_USER_CODE_PATH);
        assert!(matches!(
            gateway.admin(&AdminRequest::CallerList {}),
            AdminResponse::Callers { .. }
        ));
        assert_eq!(
            gateway.admin(&AdminRequest::Drain {}),
            AdminResponse::Draining {}
        );
        assert_eq!(
            rustix::fs::flock(&holder, shared),
            Err(rustix::io::Errno::WOULDBLOCK)
        );
        oauth_reply(peer, "400 Bad Request", "{}");
        assert_eq!(
            call.join().unwrap(),
            AdminResponse::Refused {
                refusal: AdminRefusal::ProviderRejected {}
            }
        );
    });
    assert!(gateway.process.wait().unwrap().success());
    rustix::fs::flock(&holder, shared).unwrap();
}
