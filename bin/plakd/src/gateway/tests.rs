use std::collections::BTreeMap;
use std::net::IpAddr;
use std::os::unix::fs::PermissionsExt as _;
use std::sync::{Arc, Mutex};

use plakous::gateway::protocol::UuidText;
use plakous::gateway::protocol::admin::{AdminRequest, AdminResponse, DisableTarget, LoginPhase};
use plakous::gateway::protocol::call;
use plakous::gateway::protocol::facts::{FactsReply, FactsRequest};
use plakous::gateway::protocol::refusal::{GatewayRefusal, RefusalBody};
use plakous::gateway::protocol::scope::{CallerId, CallerKey, ScopeSet, SourceNet};
use plakous::gateway::protocol::{Endpoint, GatewayAccount, KEY_HEADER};

use super::auth::Peer;
use super::bridge::MemoryBridge;
use super::callers::Callers;
use super::dispatch::{self, GatewayState};
use super::flags::Flags;
use super::http::Request;
use super::server_config::ServerConfig;
use crate::home::paths::PlakPaths;

fn home() -> (tempfile::TempDir, PlakPaths) {
    let dir = tempfile::tempdir().unwrap();
    let paths = PlakPaths::explicit(dir.path().join("plak"));
    std::fs::create_dir_all(paths.home()).unwrap();
    (dir, paths)
}

pub(super) fn state_for(paths: &PlakPaths) -> Arc<GatewayState> {
    let socket = paths.home().join("gateway.sock");
    let state_root = paths.home().join("state");
    std::fs::create_dir_all(&state_root).unwrap();
    let text = format!(
        "socket = \"{}\"\nstate_root = \"{}\"\n",
        socket.display(),
        state_root.display(),
    );
    let config = ServerConfig::parse(paths, &text).unwrap();
    let instance = UuidText::new(uuid::Uuid::new_v4().to_string()).unwrap();
    Arc::new(GatewayState {
        instance: instance.clone(),
        config,
        callers: Mutex::new(Callers::load(&super::server_config::callers_path(paths)).unwrap()),
        flags: Mutex::new(Flags::load(&super::server_config::flags_path(paths)).unwrap()),
        audit: super::audit::Audit::open(super::server_config::audit_dir(paths), 30).unwrap(),
        bridge: Mutex::new(Box::new(MemoryBridge::new(instance))),
        inflight: std::sync::atomic::AtomicU32::new(0),
        caller_inflight: Mutex::new(std::collections::HashMap::new()),
        draining: std::sync::atomic::AtomicBool::new(false),
        listeners: Mutex::new(Vec::new()),
        shutdown: tokio::sync::Notify::new(),
        relays: Mutex::new(std::collections::HashMap::new()),
    })
}

fn request(method: &str, path: &str, key: Option<&str>, body: Vec<u8>) -> Request {
    let mut headers = BTreeMap::new();
    if let Some(key) = key {
        headers.insert(KEY_HEADER.to_owned(), key.to_owned());
    }
    Request {
        method: method.to_owned(),
        path: path.to_owned(),
        headers,
        body,
    }
}

fn local() -> Peer {
    Peer {
        uid_matches: true,
        address: None,
    }
}

fn tcp(ip: &str) -> Peer {
    Peer {
        uid_matches: false,
        address: Some(ip.parse::<IpAddr>().unwrap()),
    }
}

fn refusal(response: &super::http::Response) -> GatewayRefusal {
    serde_json::from_slice::<RefusalBody>(&response.body)
        .unwrap()
        .gateway_error
}

fn admin(
    state: &Arc<GatewayState>,
    peer: &Peer,
    key: Option<&str>,
    request_body: &AdminRequest,
) -> AdminResponse {
    let body = call::encode(request_body).unwrap();
    let response = dispatch::handle(
        state,
        peer,
        &request("POST", &Endpoint::Admin.path(1), key, body),
    );
    assert_eq!(
        response.status,
        200,
        "{}",
        String::from_utf8_lossy(&response.body)
    );
    call::decode(&response.body).unwrap()
}

#[test]
fn tcp_without_a_key_is_a_uniform_401() {
    let (_dir, paths) = home();
    let state = state_for(&paths);
    let response = dispatch::handle(
        &state,
        &tcp("127.0.0.1"),
        &request("GET", "/versions", None, Vec::new()),
    );
    assert_eq!(response.status, 401);
    assert!(matches!(
        refusal(&response),
        GatewayRefusal::Unauthenticated {}
    ));
    let text = String::from_utf8(response.body).unwrap();
    assert!(!text.contains("secret"));
}

#[test]
fn a_key_from_outside_its_source_is_unauthenticated() {
    let (_dir, paths) = home();
    let state = state_for(&paths);
    let issued = admin(
        &state,
        &local(),
        None,
        &AdminRequest::CallerAdd {
            caller: CallerId::new("host").unwrap(),
            scopes: ScopeSet::parse("relay:*").unwrap(),
            sources: vec![SourceNet::parse("10.0.0.0/8").unwrap()],
        },
    );
    let AdminResponse::CallerKeyIssued { key, .. } = issued else {
        panic!("expected a key");
    };
    let response = dispatch::handle(
        &state,
        &tcp("127.0.0.1"),
        &request("GET", "/versions", Some(key.expose()), Vec::new()),
    );
    assert_eq!(response.status, 401);
    assert!(matches!(
        refusal(&response),
        GatewayRefusal::Unauthenticated {}
    ));
}

#[test]
fn relay_scope_cannot_disable_and_admin_scope_can() {
    let (_dir, paths) = home();
    let state = state_for(&paths);
    let relay = mint(&state, "relay-only", "relay:*", "127.0.0.1");
    let admin_key = mint(&state, "admin-one", "relay:*,admin:*", "127.0.0.1");
    let denied = dispatch::handle(
        &state,
        &tcp("127.0.0.1"),
        &request(
            "POST",
            &Endpoint::Admin.path(1),
            Some(&relay),
            call::encode(&AdminRequest::Disable {
                target: DisableTarget::All {},
                cut: false,
            })
            .unwrap(),
        ),
    );
    assert_eq!(denied.status, 403);
    assert!(matches!(refusal(&denied), GatewayRefusal::ScopeDenied {}));
    let disabled = admin(
        &state,
        &tcp("127.0.0.1"),
        Some(&admin_key),
        &AdminRequest::Disable {
            target: DisableTarget::All {},
            cut: true,
        },
    );
    assert!(matches!(disabled, AdminResponse::Disabled {}));
    let blocked = dispatch::handle(
        &state,
        &tcp("127.0.0.1"),
        &request("GET", "/versions", Some(&admin_key), Vec::new()),
    );
    assert_eq!(blocked.status, 503);
    assert!(matches!(
        refusal(&blocked),
        GatewayRefusal::GatewayDisabled {}
    ));
}

#[test]
fn enable_drain_and_caller_management_are_local_admin_only() {
    let (_dir, paths) = home();
    let state = state_for(&paths);
    let admin_key = mint(&state, "admin-two", "relay:*,admin:*", "127.0.0.1");
    let enable = admin(
        &state,
        &tcp("127.0.0.1"),
        Some(&admin_key),
        &AdminRequest::Enable {
            target: DisableTarget::All {},
        },
    );
    assert!(matches!(
        enable,
        AdminResponse::Refused {
            refusal: plakous::gateway::protocol::admin::AdminRefusal::LocalAdminRequired {}
        }
    ));
    let enabled = admin(
        &state,
        &local(),
        None,
        &AdminRequest::Enable {
            target: DisableTarget::All {},
        },
    );
    assert!(matches!(enabled, AdminResponse::Enabled {}));
}

#[test]
fn caller_keys_are_stored_hashed_and_ids_are_not_reused() {
    let (_dir, paths) = home();
    let state = state_for(&paths);
    let issued = admin(
        &state,
        &local(),
        None,
        &AdminRequest::CallerAdd {
            caller: CallerId::new("edge").unwrap(),
            scopes: ScopeSet::parse("relay:*").unwrap(),
            sources: vec![SourceNet::parse("127.0.0.1").unwrap()],
        },
    );
    let AdminResponse::CallerKeyIssued { key, .. } = issued else {
        panic!("key");
    };
    let stored = std::fs::read_to_string(super::server_config::callers_path(&paths)).unwrap();
    assert!(!stored.contains(key.expose()));
    assert!(stored.contains("secret_sha256"));
    admin(
        &state,
        &local(),
        None,
        &AdminRequest::CallerRemove {
            caller: CallerId::new("edge").unwrap(),
        },
    );
    let again = admin(
        &state,
        &local(),
        None,
        &AdminRequest::CallerAdd {
            caller: CallerId::new("edge").unwrap(),
            scopes: ScopeSet::parse("relay:*").unwrap(),
            sources: vec![SourceNet::parse("127.0.0.1").unwrap()],
        },
    );
    assert!(matches!(
        again,
        AdminResponse::Refused {
            refusal: plakous::gateway::protocol::admin::AdminRefusal::CallerExists {}
        }
    ));
    let rotated_missing = admin(
        &state,
        &local(),
        None,
        &AdminRequest::CallerRotate {
            caller: CallerId::new("missing").unwrap(),
        },
    );
    assert!(matches!(
        rotated_missing,
        AdminResponse::Refused {
            refusal: plakous::gateway::protocol::admin::AdminRefusal::CallerUnknown {}
        }
    ));
}

#[test]
fn kill_switch_is_per_account_and_per_caller() {
    let (_dir, paths) = home();
    let state = state_for(&paths);
    let key = mint(&state, "reader", "relay:*", "127.0.0.1");
    let codex = GatewayAccount::new("codex").unwrap();
    admin(
        &state,
        &local(),
        None,
        &AdminRequest::KeySet {
            account: codex.clone(),
            secret: plakous::gateway::protocol::SecretText::new(zeroize::Zeroizing::new(
                "not-a-real-key".to_owned(),
            ))
            .unwrap(),
        },
    );
    admin(
        &state,
        &local(),
        None,
        &AdminRequest::Disable {
            target: DisableTarget::Account {
                account: codex.clone(),
            },
            cut: false,
        },
    );
    let facts = dispatch::handle(
        &state,
        &tcp("127.0.0.1"),
        &request(
            "POST",
            &Endpoint::Facts.path(1),
            Some(&key),
            call::encode(&FactsRequest {
                accounts: vec![codex],
            })
            .unwrap(),
        ),
    );
    assert_eq!(facts.status, 503);
    assert!(matches!(
        refusal(&facts),
        GatewayRefusal::AccountDisabled {}
    ));
    admin(
        &state,
        &local(),
        None,
        &AdminRequest::Disable {
            target: DisableTarget::Caller {
                caller: CallerId::new("reader").unwrap(),
            },
            cut: false,
        },
    );
    let blocked = dispatch::handle(
        &state,
        &tcp("127.0.0.1"),
        &request(
            "POST",
            &Endpoint::Admin.path(1),
            Some(&key),
            call::encode(&AdminRequest::List {}).unwrap(),
        ),
    );
    assert_eq!(blocked.status, 403);
}

#[test]
fn audit_records_metadata_and_not_the_secret() {
    let (_dir, paths) = home();
    let state = state_for(&paths);
    let secret = "super-secret-value";
    admin(
        &state,
        &local(),
        None,
        &AdminRequest::KeySet {
            account: GatewayAccount::new("openai").unwrap(),
            secret: plakous::gateway::protocol::SecretText::new(zeroize::Zeroizing::new(
                secret.to_owned(),
            ))
            .unwrap(),
        },
    );
    let lines =
        super::audit::read_lines(&super::server_config::audit_dir(&paths), None, None).unwrap();
    assert_ne!(lines.len(), 0);
    let joined = lines.join("\n");
    assert!(!joined.contains(secret));
    assert!(joined.contains("\"verb\":\"key_set\""));
    assert!(joined.contains("\"caller\":\"local-admin\""));
    assert!(!joined.contains("\"upstream_failure\""));
}

#[tokio::test]
async fn upstream_failure_audit_appends_only_closed_transport_fields() {
    use plakous::gateway::server::{GatewayService, RelayOutcome, RelayRecord};
    use plakous::gateway::{GatewayEndpoint, RelayCallError, RelayClient};

    let (_dir, paths) = home();
    let state = state_for(&paths);
    let gateway = super::product::ProductGateway::new(&paths, Arc::clone(&state));
    let client =
        RelayClient::new(GatewayEndpoint::Unix(state.config.socket.clone()), None).unwrap();
    let RelayCallError::Transport(failure) = client
        .version(&RelayClient::call_control())
        .await
        .unwrap_err()
    else {
        panic!("expected an unsent connect failure");
    };
    gateway.audit(RelayRecord {
        account: None,
        operation: None,
        upstream: None,
        method: None,
        upstream_status: None,
        bytes_down: 0,
        duration: std::time::Duration::ZERO,
        outcome: RelayOutcome::UpstreamFailed(failure.transmission()),
        upstream_failure: Some(failure),
    });
    let lines =
        super::audit::read_lines(&super::server_config::audit_dir(&paths), None, None).unwrap();
    let entry: serde_json::Value = serde_json::from_str(&lines[0]).unwrap();
    assert_eq!(entry["gateway_error"], "upstream_failed");
    assert_eq!(
        entry["upstream_failure"],
        serde_json::json!({
            "connection_use": "fresh",
            "idle_age_ms": null,
            "http_version": null,
            "request_body_bytes_written": 0,
            "error_class": "io_other",
            "tls_alert": null,
            "http2_reason": null,
        })
    );
}

#[test]
fn login_fixture_covers_device_code_paste_and_replacement() {
    let (_dir, paths) = home();
    let state = state_for(&paths);
    let started = admin(
        &state,
        &local(),
        None,
        &AdminRequest::LoginStart {
            account: GatewayAccount::new("codex").unwrap(),
            notice_revision: plakous::gateway::protocol::BoundedText::new("notice").unwrap(),
            replace_newer: false,
        },
    );
    let AdminResponse::Challenge {
        attempt,
        method,
        user_code,
        ..
    } = started
    else {
        panic!("challenge");
    };
    assert_eq!(
        method,
        plakous::gateway::protocol::admin::LoginMethod::DeviceCode
    );
    assert!(user_code.is_some());
    let polled = admin(&state, &local(), None, &AdminRequest::LoginPoll { attempt });
    assert!(matches!(
        polled,
        AdminResponse::Progress {
            phase: LoginPhase::Succeeded,
            ..
        }
    ));
    paste_login_and_remote_replacement(&state);
}

fn paste_login_and_remote_replacement(state: &Arc<GatewayState>) {
    let paste = admin(
        state,
        &local(),
        None,
        &AdminRequest::LoginStart {
            account: GatewayAccount::new("claude").unwrap(),
            notice_revision: plakous::gateway::protocol::BoundedText::new("notice").unwrap(),
            replace_newer: false,
        },
    );
    let AdminResponse::Challenge { attempt, .. } = paste else {
        panic!("paste challenge");
    };
    let bad = admin(
        state,
        &local(),
        None,
        &AdminRequest::LoginComplete {
            attempt: attempt.clone(),
            pasted: plakous::gateway::protocol::SecretText::new(zeroize::Zeroizing::new(
                "nope".to_owned(),
            ))
            .unwrap(),
        },
    );
    assert!(matches!(
        bad,
        AdminResponse::Refused {
            refusal: plakous::gateway::protocol::admin::AdminRefusal::PasteInvalid {}
        }
    ));
    let good = admin(
        state,
        &local(),
        None,
        &AdminRequest::LoginComplete {
            attempt,
            pasted: plakous::gateway::protocol::SecretText::new(zeroize::Zeroizing::new(
                "fixture-ok".to_owned(),
            ))
            .unwrap(),
        },
    );
    assert!(matches!(
        good,
        AdminResponse::Progress {
            phase: LoginPhase::Succeeded,
            ..
        }
    ));
    admin(
        state,
        &local(),
        None,
        &AdminRequest::KeySet {
            account: GatewayAccount::new("codex/replace").unwrap(),
            secret: plakous::gateway::protocol::SecretText::new(zeroize::Zeroizing::new(
                "fixture".to_owned(),
            ))
            .unwrap(),
        },
    );
    let remote = mint(state, "remote-admin", "relay:*,admin:*", "127.0.0.1");
    let replaced = admin(
        state,
        &tcp("127.0.0.1"),
        Some(&remote),
        &AdminRequest::LoginStart {
            account: GatewayAccount::new("codex/replace").unwrap(),
            notice_revision: plakous::gateway::protocol::BoundedText::new("notice").unwrap(),
            replace_newer: false,
        },
    );
    assert!(matches!(
        replaced,
        AdminResponse::Refused {
            refusal: plakous::gateway::protocol::admin::AdminRefusal::LocalAdminRequired {}
        }
    ));
}

#[test]
fn account_verbs_run_end_to_end_on_the_fixture() {
    let (_dir, paths) = home();
    let state = state_for(&paths);
    let account = GatewayAccount::new("openai").unwrap();
    admin(
        &state,
        &local(),
        None,
        &AdminRequest::KeySet {
            account: account.clone(),
            secret: plakous::gateway::protocol::SecretText::new(zeroize::Zeroizing::new(
                "fixture".to_owned(),
            ))
            .unwrap(),
        },
    );
    let listed = admin(&state, &local(), None, &AdminRequest::List {});
    let AdminResponse::Accounts { accounts } = listed else {
        panic!("list");
    };
    assert!(accounts.iter().any(|item| item == &account));
    let status = admin(
        &state,
        &local(),
        None,
        &AdminRequest::Status {
            accounts: vec![account.clone()],
        },
    );
    assert!(matches!(status, AdminResponse::Status { .. }));
    let allowance = admin(
        &state,
        &local(),
        None,
        &AdminRequest::Allowance {
            account: account.clone(),
        },
    );
    assert!(matches!(allowance, AdminResponse::Allowance { .. }));
    let refreshed = admin(
        &state,
        &local(),
        None,
        &AdminRequest::Refresh {
            account: account.clone(),
        },
    );
    assert!(matches!(refreshed, AdminResponse::Refreshed { .. }));
    admin(
        &state,
        &local(),
        None,
        &AdminRequest::Logout {
            account: account.clone(),
            mode: plakous::gateway::protocol::admin::LogoutMode::Local,
        },
    );
    let removed = admin(
        &state,
        &local(),
        None,
        &AdminRequest::Remove {
            account: account.clone(),
        },
    );
    assert!(matches!(removed, AdminResponse::Changed { .. }));
    let facts = dispatch::handle(
        &state,
        &local(),
        &request(
            "POST",
            &Endpoint::Facts.path(1),
            None,
            call::encode(&FactsRequest {
                accounts: vec![account],
            })
            .unwrap(),
        ),
    );
    assert_eq!(facts.status, 404);
    let _ = call::decode::<FactsReply>(&facts.body);
}

#[test]
fn an_authenticated_relay_is_refused_internal_without_an_upstream() {
    let (_dir, paths) = home();
    let state = state_for(&paths);
    let response = dispatch::handle(
        &state,
        &local(),
        &request("POST", &Endpoint::Relay.path(1), None, Vec::new()),
    );
    assert_eq!(response.status, 422);
    assert!(matches!(
        refusal(&response),
        GatewayRefusal::EnvelopeInvalid {}
    ));
}

#[test]
fn drain_stops_later_calls() {
    let (_dir, paths) = home();
    let state = state_for(&paths);
    let drained = admin(&state, &local(), None, &AdminRequest::Drain {});
    assert!(matches!(drained, AdminResponse::Draining {}));
    let response = dispatch::handle(
        &state,
        &local(),
        &request("GET", "/versions", None, Vec::new()),
    );
    assert_eq!(response.status, 503);
    assert!(matches!(refusal(&response), GatewayRefusal::Busy { .. }));
}

#[test]
fn a_non_tailnet_tcp_address_is_refused_unless_opted_in() {
    assert!(super::address::parse("tcp:192.0.2.10:9", false).is_err());
    assert!(super::address::parse("tcp:192.0.2.10:9", true).is_ok());
    assert!(super::address::parse("tcp:0.0.0.0:9", true).is_err());
    assert!(super::address::parse("tcp:100.64.1.2:9", false).is_ok());
}

#[test]
fn gateway_grammar_parses_the_service_verbs() {
    use crate::command::{Command, GatewayCommand};
    let parsed = crate::cli::parse(
        [
            "gateway",
            "caller",
            "add",
            "host",
            "--source",
            "127.0.0.1/32",
        ]
        .map(std::ffi::OsString::from),
    )
    .unwrap();
    assert!(matches!(
        parsed,
        Command::Gateway(GatewayCommand::CallerAdd { .. })
    ));
    let drained = crate::cli::parse(["gateway", "drain"].map(std::ffi::OsString::from)).unwrap();
    assert!(matches!(drained, Command::Gateway(GatewayCommand::Drain)));
}

fn mint(state: &Arc<GatewayState>, id: &str, scopes: &str, source: &str) -> String {
    let issued = admin(
        state,
        &local(),
        None,
        &AdminRequest::CallerAdd {
            caller: CallerId::new(id).unwrap(),
            scopes: ScopeSet::parse(scopes).unwrap(),
            sources: vec![SourceNet::parse(source).unwrap()],
        },
    );
    let AdminResponse::CallerKeyIssued { key, .. } = issued else {
        panic!("mint");
    };
    let text = key.expose().to_string();
    assert!(CallerKey::parse(&text).is_ok());
    text
}

/// A gateway with one signed-in fixture account, an admin caller, and a
/// facts call for that account from `peer`.
struct KillFixture {
    _dir: tempfile::TempDir,
    state: Arc<GatewayState>,
    key: String,
    account: GatewayAccount,
}

impl KillFixture {
    fn new() -> Self {
        let (dir, paths) = home();
        let state = state_for(&paths);
        let key = mint(&state, "operator", "relay:*,admin:*", "127.0.0.1");
        let account = GatewayAccount::new("fixture").unwrap();
        admin(
            &state,
            &local(),
            None,
            &AdminRequest::KeySet {
                account: account.clone(),
                secret: plakous::gateway::protocol::SecretText::new(zeroize::Zeroizing::new(
                    "not-a-real-key".to_owned(),
                ))
                .unwrap(),
            },
        );
        Self {
            _dir: dir,
            state,
            key,
            account,
        }
    }

    fn facts(&self, peer: &Peer, key: Option<&str>) -> super::http::Response {
        dispatch::handle(
            &self.state,
            peer,
            &request(
                "POST",
                &Endpoint::Facts.path(1),
                key,
                call::encode(&FactsRequest {
                    accounts: vec![self.account.clone()],
                })
                .unwrap(),
            ),
        )
    }

    fn remote_facts(&self) -> u16 {
        self.facts(&tcp("127.0.0.1"), Some(&self.key)).status
    }

    fn local(&self, call: &AdminRequest) -> AdminResponse {
        admin(&self.state, &local(), None, call)
    }

    fn raw(&self, peer: &Peer, key: Option<&str>, call: &AdminRequest) -> u16 {
        dispatch::handle(
            &self.state,
            peer,
            &request(
                "POST",
                &Endpoint::Admin.path(1),
                key,
                call::encode(call).unwrap(),
            ),
        )
        .status
    }
}

#[test]
fn a_global_disable_keeps_the_local_administrators_enable_and_status() {
    let fixture = KillFixture::new();
    fixture.local(&AdminRequest::Disable {
        target: DisableTarget::All {},
        cut: false,
    });
    for (peer, key) in [
        (tcp("127.0.0.1"), Some(fixture.key.as_str())),
        (local(), None),
    ] {
        let blocked = fixture.facts(&peer, key);
        assert_eq!(blocked.status, 503);
        assert!(matches!(
            refusal(&blocked),
            GatewayRefusal::GatewayDisabled {}
        ));
    }
    assert!(matches!(
        fixture.local(&AdminRequest::CallerList {}),
        AdminResponse::Callers { .. }
    ));
    let status = AdminRequest::Status {
        accounts: Vec::new(),
    };
    assert!(matches!(
        fixture.local(&status),
        AdminResponse::Status { .. }
    ));
    // The local administrator's other verbs stay blocked, as does any caller.
    let key_clear = AdminRequest::KeyClear {
        account: fixture.account.clone(),
    };
    assert_eq!(fixture.raw(&local(), None, &key_clear), 503);
    assert_eq!(
        fixture.raw(&tcp("127.0.0.1"), Some(&fixture.key), &status),
        503
    );
    assert!(matches!(
        fixture.local(&AdminRequest::Enable {
            target: DisableTarget::All {}
        }),
        AdminResponse::Enabled {}
    ));
    assert_eq!(fixture.remote_facts(), 200, "relays resume after enable");
}

#[test]
fn the_local_administrator_clears_an_account_disable_and_drains_a_disabled_gateway() {
    let fixture = KillFixture::new();
    let account = || DisableTarget::Account {
        account: fixture.account.clone(),
    };
    fixture.local(&AdminRequest::Disable {
        target: account(),
        cut: false,
    });
    assert_eq!(fixture.remote_facts(), 503);
    assert!(matches!(
        fixture.local(&AdminRequest::Enable { target: account() }),
        AdminResponse::Enabled {}
    ));
    assert_eq!(fixture.remote_facts(), 200);
    fixture.local(&AdminRequest::Disable {
        target: DisableTarget::All {},
        cut: false,
    });
    assert!(matches!(
        fixture.local(&AdminRequest::Drain {}),
        AdminResponse::Draining {}
    ));
    assert!(
        fixture
            .state
            .draining
            .load(std::sync::atomic::Ordering::Relaxed)
    );
}

#[test]
fn disable_cut_ends_only_the_matching_relays_in_flight() {
    let (_dir, paths) = home();
    let state = state_for(&paths);
    let first = GatewayAccount::new("openai").unwrap();
    let second = GatewayAccount::new("anthropic").unwrap();
    let relay = |principal: super::auth::Principal, account: &GatewayAccount| {
        let token = plakous::gateway::server::CancellationToken::new();
        state.relays.lock().unwrap().insert(
            format!("{}-{}", principal.audit_name(), account),
            dispatch::RelayCut {
                principal,
                account: account.clone(),
                token: token.clone(),
            },
        );
        token
    };
    let reader = super::auth::Principal::Caller(CallerId::new("reader").unwrap());
    let streaming = relay(reader.clone(), &first);
    let other_account = relay(super::auth::Principal::LocalAdmin, &second);
    let disable = |target: DisableTarget, cut: bool| {
        admin(
            &state,
            &local(),
            None,
            &AdminRequest::Disable { target, cut },
        )
    };
    disable(
        DisableTarget::Account {
            account: first.clone(),
        },
        false,
    );
    assert!(
        !streaming.is_cancelled(),
        "without --cut a streaming relay continues"
    );
    disable(
        DisableTarget::Account {
            account: first.clone(),
        },
        true,
    );
    assert!(streaming.is_cancelled());
    assert!(!other_account.is_cancelled());
    let by_caller = relay(reader, &second);
    disable(
        DisableTarget::Caller {
            caller: CallerId::new("reader").unwrap(),
        },
        true,
    );
    assert!(by_caller.is_cancelled());
    assert!(!other_account.is_cancelled());
    disable(DisableTarget::All {}, true);
    assert!(other_account.is_cancelled());
}

/// `plakd gateway drain` ends the service under a global disable: the
/// service exits and releases the holder lock.
#[test]
fn a_globally_disabled_service_still_drains_and_exits() {
    let (_dir, paths) = home();
    std::fs::set_permissions(paths.home(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let socket = paths.home().join("gw.sock");
    let state_root = paths.home().join("state");
    std::fs::create_dir_all(&state_root).unwrap();
    let config = ServerConfig::parse(
        &paths,
        &format!(
            "socket = \"{}\"\nstate_root = \"{}\"\n",
            socket.display(),
            state_root.display()
        ),
    )
    .unwrap();
    let instance = UuidText::new(uuid::Uuid::new_v4().to_string()).unwrap();
    let gateway =
        super::service::Gateway::open_with(&paths, config, Box::new(MemoryBridge::new(instance)))
            .unwrap();
    let lock = super::holder::lock_path(&state_root);
    assert!(super::holder::is_busy(
        &super::holder::advertised_socket(&lock).unwrap_err()
    ));
    let served = std::thread::spawn(move || gateway.serve_foreground());
    // The path appears at bind, before the listener accepts.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while super::holder::advertised_socket(&lock)
        .ok()
        .flatten()
        .as_ref()
        != Some(&socket)
    {
        assert!(!served.is_finished() && std::time::Instant::now() < deadline);
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(std::os::unix::net::UnixStream::connect(&socket).is_ok());
    let address = super::GatewayAddress::Unix(socket);
    let disabled: AdminResponse = super::client::admin(
        &address,
        None,
        &AdminRequest::Disable {
            target: DisableTarget::All {},
            cut: true,
        },
    )
    .unwrap();
    assert!(matches!(disabled, AdminResponse::Disabled {}));
    let drained: AdminResponse =
        super::client::admin(&address, None, &AdminRequest::Drain {}).unwrap();
    assert!(matches!(drained, AdminResponse::Draining {}));
    served.join().unwrap().unwrap();
    super::holder::HolderLock::try_acquire(&lock, super::holder::HolderMode::Exclusive)
        .expect("a drained service releases the holder lock");
}

#[test]
fn the_service_replaces_only_its_own_stale_socket() {
    let (_dir, paths) = home();
    std::fs::set_permissions(paths.home(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let socket = paths.home().join("gw.sock");
    std::fs::write(&socket, "not a socket").unwrap();
    let state_root = paths.home().join("state");
    std::fs::create_dir_all(&state_root).unwrap();
    let config = ServerConfig::parse(
        &paths,
        &format!(
            "socket = \"{}\"\nstate_root = \"{}\"\n",
            socket.display(),
            state_root.display()
        ),
    )
    .unwrap();
    let instance = UuidText::new(uuid::Uuid::new_v4().to_string()).unwrap();
    let gateway =
        super::service::Gateway::open_with(&paths, config, Box::new(MemoryBridge::new(instance)))
            .unwrap();
    let refused = gateway.serve_foreground().unwrap_err();
    assert!(refused.contains("not this user's socket"), "{refused}");
    assert_eq!(std::fs::read_to_string(&socket).unwrap(), "not a socket");
}

#[test]
fn audit_append_failures_are_counted_and_published_for_status() {
    let (_dir, paths) = home();
    let dir = super::server_config::audit_dir(&paths);
    let published = super::server_config::audit_failures_path(&paths);
    let audit = super::audit::Audit::open(dir.clone(), 30)
        .unwrap()
        .publishing_failures(published.clone(), "instance-a");
    assert_eq!(
        super::audit::published_failures(&published, "instance-a"),
        Some(0)
    );
    std::fs::remove_dir_all(&dir).unwrap();
    std::fs::write(&dir, "not a directory").unwrap();
    audit.record(&super::audit::AuditEvent::default());
    audit.record(&super::audit::AuditEvent::default());
    assert_eq!(
        super::audit::published_failures(&published, "instance-a"),
        Some(2)
    );
    assert_eq!(
        super::audit::published_failures(&published, "instance-b"),
        None,
        "another instance's count is not this one's"
    );
}

#[test]
fn audit_since_is_an_instant_not_text() {
    let (_dir, paths) = home();
    let dir = super::server_config::audit_dir(&paths);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("2026-11-18.jsonl"),
        concat!(
            "{\"ts\":\"2026-11-18T09:00:00Z\",\"caller\":\"a\"}\n",
            "{\"ts\":\"2026-11-18T11:00:00.5Z\",\"caller\":\"b\"}\n",
        ),
    )
    .unwrap();
    let since = super::audit::parse_since("2026-11-18T10:00:00+00:00").unwrap();
    let lines = super::audit::read_lines(&dir, Some(since), None).unwrap();
    assert_eq!(lines.len(), 1);
    assert!(lines[0].contains("\"b\""));
    assert!(super::audit::parse_since("yesterday").is_err());
}

/// `run`, `continue` and `models` are not admin verbs: under a configured
/// gateway they run in this process, whose routes relay through the
/// gateway, and they neither hold nor create the subscription store.
#[test]
fn model_commands_run_in_process_under_a_configured_gateway_without_the_store() {
    let (dir, paths) = home();
    std::fs::write(
        paths.config_file(),
        "[gateway]\naddress = \"unix:/tmp/plak-model-commands.sock\"\n",
    )
    .unwrap();
    let state = dir.path().join("host-state");
    std::fs::create_dir_all(&state).unwrap();
    let services =
        crate::shell::services::Services::for_home_with_state(paths.clone(), state.clone());
    for args in [
        &["models", "codex"][..],
        &["run", "--model", "m", "--provider", "claude", "--", "hello"][..],
    ] {
        let command = crate::cli::parse(args.iter().map(std::ffi::OsString::from)).unwrap();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let forwarded = super::try_remote_account(
            &services,
            &command,
            &mut std::io::empty(),
            &mut stdout,
            &mut stderr,
        )
        .unwrap();
        assert!(forwarded.is_none(), "{args:?} must run in this process");
        assert!(stdout.is_empty(), "{args:?} is not an admin verb");
        let app = crate::commands::application::PlakousApplication::new(
            state.clone(),
            None,
            services.custody().unwrap(),
        )
        .unwrap();
        assert!(app.custody.gateway().is_some());
    }
    assert!(
        !state.join("plak/subscriptions").exists(),
        "a gateway process must not create its subscription store"
    );
}
