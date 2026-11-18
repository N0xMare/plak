//! Subscription admin contracts through a real gateway listener.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "offline integration test"
)]

use std::fs;
use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use plakous::gateway::protocol::admin::{
    AdminRefusal, AdminRequest, AdminResponse, LoginMethod, LoginPhase, LogoutMode,
};
use plakous::gateway::protocol::facts::CredentialStatus;
use plakous::gateway::protocol::refusal::GatewayRefusal;
use plakous::gateway::protocol::{
    BoundedText, Endpoint, GatewayAccount, SecretText, UuidText, call,
};
use plakous::gateway::{GatewayEndpoint, RelayCallError, RelayClient};
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
        let root = tempfile::Builder::new().prefix("gw").tempdir().unwrap();
        let base = root.path();
        fs::set_permissions(base, fs::Permissions::from_mode(0o700)).unwrap();
        let home = base.join("home");
        fs::DirBuilder::new().mode(0o700).create(&home).unwrap();
        let socket = base.join("g.sock");
        fs::write(
            home.join("gateway.toml"),
            format!("socket = {:?}\n", socket.to_str().unwrap()),
        )
        .unwrap();
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
        let mut process = TestChild::spawn(
            command(base)
                .args(["gateway", "serve"])
                .env("PLAK_TEST_GATEWAY_REVIEW_CLOCK", "1")
                .env(
                    "PLAK_TEST_FAULTS",
                    "host=api.anthropic.com,path=/api/oauth/usage,fault=429;\
                     host=api.anthropic.com,path=/api/oauth/profile,fault=503;\
                     host=platform.claude.com,path=/v1/oauth/token,fault=400;\
                     fault=connect",
                )
                .stdin(Stdio::null())
                .stdout(Stdio::null()),
        );
        let deadline = Instant::now() + Duration::from_secs(10);
        while !socket.exists() {
            assert_eq!(process.try_wait().unwrap(), None, "gateway exited");
            assert!(Instant::now() < deadline, "gateway socket did not appear");
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
        let reply = self
            .executor
            .block_on(self.relay.call(
                Endpoint::Admin,
                call::encode(request).unwrap(),
                &RelayClient::call_control(),
            ))
            .unwrap()?;
        Ok(call::decode(&reply).unwrap())
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
        stdout.lines().any(|line| line == "claude: rate-limited"),
        "{stdout}"
    );

    // The fixture responds to the forced refresh, and the single writer
    // quarantines this rejected grant. Do not rely on any real OAuth endpoint.
    assert!(matches!(
        gateway.try_admin(&AdminRequest::Refresh {
            account: claude.clone()
        }),
        Err(RelayCallError::Refused(GatewayRefusal::Internal {}))
    ));
    assert_eq!(gateway.credential("claude"), CredentialStatus::Quarantined);
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

fn start_login(gateway: &GatewayFixture) -> (UuidText, SecretText) {
    let notice = NativeSubscriptions::notice(NativeProvider::Claude).unwrap();
    let response = gateway.admin(&AdminRequest::LoginStart {
        account: account("claude"),
        notice_revision: BoundedText::new(notice.notice_revision().to_owned()).unwrap(),
        replace_newer: false,
    });
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

fn paste(value: String) -> SecretText {
    SecretText::new(zeroize::Zeroizing::new(value)).unwrap()
}
