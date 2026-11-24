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
use plakous::gateway::{
    ApiCanaryFailure, GatewayCustody, GatewayEndpoint, RelayCallError, RelayClient, SensitiveBytes,
};
use plakous::subscriptions::{NativeProvider, NativeSubscriptions};

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

#[test]
fn deploy_api_canaries_relay_exactly_once_per_refusal_without_a_runtime() {
    let cases = [
        ("openai", "api.openai.com", "/v1/responses"),
        ("anthropic", "api.anthropic.com", "/v1/messages"),
        ("xai-api", "api.x.ai", "/v1/responses"),
        ("openrouter", "openrouter.ai", "/api/v1/chat/completions"),
    ];
    let mut faults = String::new();
    for (_, host, path) in cases {
        write!(&mut faults, "host={host},path={path},nth=1,fault=429,retry-after=0;host={host},path={path},nth=2,fault=503;").unwrap();
    }
    let gateway = GatewayFixture::start_fixture(None, None, &faults, true, None);
    let root = gateway.root.path();
    for (account, _, _) in cases {
        let start = || {
            let mut cli = command(root);
            cli.args([
                "deploy-canary",
                "--account",
                account,
                "--model=example-model",
                "--effort=none",
            ]);
            cli
        };
        let preflight = start().arg("--preflight").output().unwrap();
        assert!(preflight.status.success(), "{account}: {preflight:?}");
        assert_eq!(preflight.stdout, b"");
        for status in [429, 503] {
            let output = start().output().unwrap();
            assert!(!output.status.success(), "{account}: {output:?}");
            assert_eq!(
                output.stdout,
                format!("plakd-deploy-canary-upstream-http {status}\n").as_bytes(),
                "{account}: {output:?}"
            );
        }
    }
    let posts = audit_posts(root, 8);
    assert_eq!(posts.len(), 8, "{posts:?}");
    for (account, host, path) in cases {
        let mut statuses: Vec<_> = posts
            .iter()
            .filter(|row| row["account"] == account)
            .map(|row| {
                assert_eq!(row["upstream_origin"], format!("https://{host}"));
                assert_eq!(row["upstream_path"], path);
                row["upstream_status"].as_u64().unwrap()
            })
            .collect();
        statuses.sort_unstable();
        assert_eq!(statuses, [429, 503]);
    }
    assert!(posts.iter().all(|row| row["attempt"] == 0));
    let db = root
        .join("home/state")
        .join(plakd::home::state::state_key())
        .join("product.sqlite3");
    let db = rusqlite::Connection::open_with_flags(db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .unwrap();
    let count: i64 = db
        .query_row(
            "SELECT count(*) FROM sessions WHERE title='Deployment canary' AND archived=1
            AND locator IS NULL AND account IS NULL AND model IS NULL AND effort IS NULL",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 8);
    gateway.finish();
}

#[test]
fn deploy_api_canary_success_reset_and_deadline_relay_one_physical_post_each() {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    listener.set_nonblocking(true).unwrap();
    let gateway = GatewayFixture::start_fixture(
        None,
        None,
        concat!(
            "host=api.openai.com,path=/v1/responses,nth=2,fault=reset,after-events=1;",
            "host=api.openai.com,path=/v1/responses,nth=3,fault=stall,seconds=10,after-events=1;",
        ),
        true,
        Some(&listener),
    );
    let upstream = std::thread::spawn(move || serve_api_canary_fixture(&listener));
    let root = gateway.root.path();
    let success = command(root)
        .args([
            "deploy-canary",
            "--account=openai",
            "--model=example-success",
            "--effort=none",
        ])
        .output()
        .unwrap();
    assert!(success.status.success(), "{success:?}");
    assert_eq!(
        success.stdout,
        b"plakd-deploy-canary-provider-capped-api-pass\n"
    );
    let custody =
        GatewayCustody::connect(GatewayEndpoint::Unix(root.join("g.sock")), None, |kind| {
            (kind == "openai")
                .then_some(plakous::gateway::pins::openai::OPENAI_API_CREDENTIAL_HEADERS)
        })
        .unwrap();
    gateway
        .executor
        .block_on(async {
            for (name, bound) in [
                ("example-reset", Duration::from_secs(4)),
                ("example-stall", Duration::from_secs(1)),
            ] {
                let request = custody
                    .prepare_api_canary("openai", name, false)
                    .await
                    .unwrap();
                let started = Instant::now();
                assert_eq!(
                    request.run(started + bound).await,
                    Err(ApiCanaryFailure::Transport),
                    "{name}"
                );
                if name == "example-stall" {
                    assert!(
                        started.elapsed() >= bound,
                        "the stall did not reach the deadline"
                    );
                }
            }
        })
        .unwrap();
    upstream.join().unwrap();
    let posts = audit_posts(root, 3);
    assert_eq!(posts.len(), 3, "{posts:?}");
    assert!(
        posts
            .iter()
            .all(|row| row["attempt"] == 0 && row["account"] == "openai")
    );
    gateway.finish();
}

fn serve_api_canary_fixture(listener: &TcpListener) {
    let deadline = Instant::now() + Duration::from_secs(20);
    for model in ["example-success", "example-reset", "example-stall"] {
        let mut peer = loop {
            match listener.accept() {
                Ok((peer, _)) => break peer,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(
                        Instant::now() < deadline,
                        "fixture did not receive the physical POST"
                    );
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("fixture accept failed: {error}"),
            }
        };
        peer.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        let mut reader = BufReader::new(&mut peer);
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        assert!(line.starts_with("POST /v1/responses HTTP/1."));
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
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["model"], model);
        assert_eq!(body["max_output_tokens"], 16);
        assert_eq!(body["reasoning"]["effort"], "none");
        assert!(body.get("tools").is_none());
        // The blank line counts as one body-event boundary for the stream
        // fault injector, while the whole payload is valid Responses JSON.
        let body = concat!(
            "{\"status\":\"completed\",\n\n",
            "\"output\":[{\"type\":\"message\",\"role\":\"assistant\",",
            "\"content\":[{\"type\":\"output_text\",\"text\":\"OK\"}]}],",
            "\"usage\":{\"output_tokens\":2}}"
        );
        write!(
            peer,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len(),
        ).unwrap();
    }
}

fn state(root: &Path) -> PathBuf {
    root.join("xdg")
}

/// The gateway records an exchange after it finishes relaying it, so the row
/// can trail the caller's exit; wait boundedly for the expected POST rows.
fn audit_posts(root: &Path, expected: usize) -> Vec<serde_json::Value> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let audit = command(root).args(["gateway", "audit"]).output().unwrap();
        assert!(audit.status.success(), "{audit:?}");
        let posts: Vec<serde_json::Value> = String::from_utf8(audit.stdout)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .filter(|row| row["method"] == "POST")
            .collect();
        if posts.len() >= expected || Instant::now() >= deadline {
            return posts;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
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
fn deploy_canary_429_has_one_inference_and_an_archived_ephemeral_record() {
    let gateway = GatewayFixture::start_with_fault(
        None,
        None,
        "host=api.anthropic.com,path=/v1/messages,fault=429,retry-after=0;",
    );
    let root = gateway.root.path();
    let output = command(root)
        .env("PLAK_TEST_GATEWAY_REVIEW_CLOCK", "1")
        .args([
            "deploy-canary",
            "--account=claude/work",
            "--model=claude-opus-5-5",
            "--effort=low",
        ])
        .output()
        .unwrap();
    assert!(
        !output.status.success(),
        "the injected 429 must fail the canary"
    );
    assert!(
        output.stdout.is_empty(),
        "failure must not publish a canary pass"
    );
    let audit = command(root).args(["gateway", "audit"]).output().unwrap();
    assert!(audit.status.success());
    let inference: Vec<serde_json::Value> = String::from_utf8(audit.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .filter(|row| {
            row["method"] == "POST"
                && row["upstream_path"]
                    .as_str()
                    .is_some_and(|path| path.starts_with("/v1/messages"))
        })
        .collect();
    assert_eq!(
        inference.len(),
        1,
        "expected one physical inference: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(inference[0]["upstream_status"], 429);
    assert_eq!(inference[0]["attempt"], 0);
    let db = root
        .join("home/state")
        .join(plakd::home::state::state_key())
        .join("product.sqlite3");
    let db = rusqlite::Connection::open_with_flags(db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .unwrap();
    let count: i64 = db.query_row(
        "SELECT count(*) FROM sessions WHERE title='Deployment canary' AND archived=1 AND locator IS NULL",
        [], |row| row.get(0),
    ).unwrap();
    assert_eq!(count, 1);
    gateway.finish();
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
    peer.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
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
            rows.lines()
                .any(|row| row == "codex: error=gateway refused the call (scope_denied)"),
            "{rows}"
        );
    }
    let usage = command(gateway.root.path())
        .args(["usage"])
        .output()
        .unwrap();
    assert!(usage.status.success(), "{usage:?}");
    let limits = String::from_utf8(usage.stdout).unwrap();
    assert!(
        limits.contains("rate-limited") && limits.contains("scope_denied"),
        "{limits}"
    );
    let single = command(gateway.root.path())
        .args(["status", "codex"])
        .output()
        .unwrap();
    assert!(!single.status.success(), "{single:?}");
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
