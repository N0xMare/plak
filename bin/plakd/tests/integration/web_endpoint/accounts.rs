use std::net::TcpListener;
use std::time::{Duration, Instant};

use plakd_wire_types::console::{
    AccountStatus, Credential, HostRequest, HostResponse, KeySource, LoginPhase, OperatorAction,
    SecretInput, UsageWindow,
};
use plakous::subscriptions::{NativeProvider, NativeSubscriptions};

use super::*;
use crate::gateway_admin::{loopback_listener, oauth_reply, oauth_request};

const CLAUDE: &str = "claude/second";
const ROUTER: &str = "openrouter/second";

struct ConsoleWire {
    socket: WebSocket<UnixStream>,
    id: u64,
}

impl ConsoleWire {
    fn request(&mut self, request: HostRequest) -> HostResponse {
        self.id += 1;
        let frame = ClientFrame::<plakd_wire_types::PlakExtension>::Request {
            id: self.id,
            op: Op::Host { request },
        };
        self.socket
            .send(Message::text(encode_line(&frame).unwrap()))
            .unwrap();
        let outcome = response(&mut self.socket, self.id);
        serde_json::from_value(outcome["ok"]["response"].clone())
            .unwrap_or_else(|_| panic!("console refused the request: {outcome}"))
    }

    fn confirmation(&mut self, action: OperatorAction) -> String {
        let HostResponse::OperatorPrepared { consent } =
            self.request(HostRequest::OperatorPrepare { action })
        else {
            panic!("expected consent preparation");
        };
        let HostResponse::OperatorConfirmed { confirmation, .. } =
            self.request(HostRequest::OperatorConfirm {
                nonce: consent.nonce,
            })
        else {
            panic!("expected an operation confirmation");
        };
        confirmation
    }

    fn status(&mut self) -> Vec<AccountStatus> {
        let HostResponse::AccountsStatus { rows, .. } = self.request(HostRequest::AccountsStatus {
            account_id: None,
            cursor: None,
            limit: None,
        }) else {
            panic!("expected accounts status");
        };
        rows
    }

    fn wait_for_login(&mut self, attempt: &str) {
        let deadline = Instant::now() + WAIT;
        loop {
            let HostResponse::LoginProgress { phase, .. } =
                self.request(HostRequest::AccountLoginPoll {
                    attempt: attempt.to_owned(),
                })
            else {
                panic!("expected login progress");
            };
            if phase == LoginPhase::Succeeded {
                return;
            }
            assert_eq!(phase, LoginPhase::Exchanging);
            assert!(
                Instant::now() < deadline,
                "the fixture login did not settle"
            );
            std::thread::sleep(Duration::from_millis(1_010));
        }
    }
}

fn input(value: String) -> SecretInput {
    SecretInput::new(zeroize::Zeroizing::new(value)).unwrap()
}

fn complete_claude_login(console: &mut ConsoleWire, oauth: &TcpListener) {
    let notice = NativeSubscriptions::notice(NativeProvider::Claude).unwrap();
    let notice_revision = notice.notice_revision().to_owned();
    let confirmation = console.confirmation(OperatorAction::AccountLoginStart {
        account_id: CLAUDE.to_owned(),
        notice_revision: notice_revision.clone(),
        replace_newer: false,
    });
    let HostResponse::LoginChallenge {
        attempt,
        authorize_url,
        ..
    } = console.request(HostRequest::AccountLoginStart {
        account_id: CLAUDE.to_owned(),
        notice_revision,
        replace_newer: false,
        confirmation,
    })
    else {
        panic!("expected a named Claude paste challenge");
    };
    let state = authorize_url
        .expose()
        .split(['?', '&'])
        .find_map(|part| part.strip_prefix("state="))
        .unwrap();
    assert!(matches!(
        console.request(HostRequest::AccountLoginComplete {
            attempt: attempt.clone(),
            pasted: input(format!("{}#{state}", "fixture-code".repeat(3))),
        }),
        HostResponse::LoginProgress {
            phase: LoginPhase::Exchanging,
            ..
        }
    ));
    oauth_reply(
        oauth_request(oauth, "/v1/oauth/token"),
        "200 OK",
        r#"{"access_token":"fixture-access","refresh_token":"fixture-refresh","expires_in":3600,"scope":"user:inference","account":{"uuid":"22345678-1234-4234-8234-123456789abc"}}"#,
    );
    console.wait_for_login(&attempt);
}

#[test]
fn confirmed_named_account_writes_appear_in_both_web_console_rosters() {
    let oauth = loopback_listener();
    let fixture = Fixture::new(&json!({"version":1,"steps":[]}));
    fixture.write_config(&format!(
        "[web]\nallow = [\"{LOGIN}\"]\norigin = \"{ORIGIN}\"\naccounts_read = true\ncredentials_write = true\n"
    ));
    fixture.start_gateway_with(&[
        (
            "PLAK_TEST_GATEWAY_OAUTH_PORT",
            &oauth.local_addr().unwrap().port().to_string(),
        ),
        ("PLAK_TEST_GATEWAY_REVIEW_CLOCK", "1"),
    ]);
    let (mut web, ready) = launch_on_with_env(
        &fixture,
        &socket_dir(&fixture).join("plak.sock"),
        &[],
        &[],
        CONSOLE_CSP,
    );
    assert!(ready, "{:?}", web.lines.try_iter().collect::<Vec<_>>());
    let mut socket = upgrade(&web, &cookie(&web));
    send(
        &mut socket,
        &ClientFrame::Hello(Hello {
            proto: WIRE_VERSION,
            binary_id: "web-test".into(),
            caps: Vec::new(),
        }),
    );
    assert_eq!(next_frame(&mut socket)["frame"], "welcome");
    let mut console = ConsoleWire { socket, id: 0 };
    assert!(
        console
            .status()
            .iter()
            .all(|row| !matches!(row.account_id.as_str(), CLAUDE | ROUTER))
    );
    complete_claude_login(&mut console, &oauth);
    let confirmation = console.confirmation(OperatorAction::AccountKeySet {
        account_id: ROUTER.to_owned(),
    });
    assert!(matches!(
        console.request(HostRequest::AccountKeySet {
            account_id: ROUTER.to_owned(),
            secret: input("synthetic-openrouter-fixture".to_owned()),
            confirmation,
        }),
        HostResponse::AccountChanged { account_id, .. } if account_id == ROUTER
    ));
    let rows = console.status();
    let claude = rows.iter().find(|row| row.account_id == CLAUDE).unwrap();
    assert_eq!(
        claude.native.as_ref().unwrap().credential,
        Credential::Ready
    );
    let router = rows.iter().find(|row| row.account_id == ROUTER).unwrap();
    assert_eq!(router.key.as_ref().unwrap().source, KeySource::File);
    let HostResponse::AccountsUsage { rows, .. } = console.request(HostRequest::AccountsUsage {
        account_id: None,
        window: UsageWindow::Hours24,
        cursor: None,
        limit: None,
    }) else {
        panic!("expected accounts usage");
    };
    for account in [CLAUDE, ROUTER] {
        assert!(
            rows.iter().any(|row| row.account_id == account),
            "{account}"
        );
    }
    drop(console);
    interrupt_and_wait(&mut web);
}
