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
    std::fs::set_permissions(paths.home(), std::fs::Permissions::from_mode(0o700)).unwrap();
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
        next_relay_id: std::sync::atomic::AtomicU64::new(1),
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
            "error_class": "not_found",
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
        let id = state
            .next_relay_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        state.relays.lock().unwrap().insert(
            id,
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

#[tokio::test]
async fn concurrent_identical_operation_and_attempt_relays_preserve_cut_inflight_and_audit() {
    use plakous::gateway::protocol::envelope::{ExpectedFence, RelayEnvelope, RelayLimits};
    use plakous::gateway::protocol::header::RequestHeader;
    use plakous::gateway::server::{
        GatewayService as _, Peer as ServerPeer, RelayOutcome, RelayRecord,
    };

    let (_dir, paths) = home();
    let state = state_for(&paths);
    let account = GatewayAccount::new("openai/h1-test").unwrap();
    crate::home::secrets::set_api_key(
        &paths,
        account.as_str(),
        zeroize::Zeroizing::new("sk-test-h1-synthetic-key".to_owned()),
    )
    .unwrap();
    let key1 = mint(&state, "caller-one", "relay:*", "127.0.0.1");
    let key2 = mint(&state, "caller-two", "relay:*", "127.0.0.1");
    let gateway = super::product::ProductGateway::new(&paths, Arc::clone(&state));
    let peer = ServerPeer::Tcp("127.0.0.1:4000".parse().unwrap());
    let authed1 = gateway.authenticate(&peer, Some(key1.as_bytes())).unwrap();
    let authed2 = gateway.authenticate(&peer, Some(key2.as_bytes())).unwrap();

    // Fetch exact fence facts for the custodied account.
    let facts_body = call::encode(&FactsRequest {
        accounts: vec![account.clone()],
    })
    .unwrap();
    let facts_raw = gateway
        .call(&authed1, 2, Endpoint::Facts, &facts_body)
        .await
        .unwrap();
    let facts_reply: FactsReply = call::decode(&facts_raw).unwrap();
    let fact = &facts_reply.facts[0];
    let binding = fact.identity.as_ref().unwrap().account_binding.clone();
    let generation = fact.credential.generation.unwrap();

    let shared_op = UuidText::new("11111111-1111-4111-8111-111111111111".to_owned()).unwrap();
    let make_envelope = |url: &str| RelayEnvelope {
        account: account.clone(),
        operation: shared_op.clone(),
        attempt: 1,
        method: "POST".to_owned(),
        url: url.to_owned(),
        headers: vec![RequestHeader::custodied("authorization").unwrap()],
        expected: ExpectedFence {
            account_binding: binding.clone(),
            generation,
        },
        limits: RelayLimits {
            head_timeout_ms: 5000,
            body_idle_timeout_ms: 5000,
            max_response_bytes: 1024 * 1024,
            max_chunk_bytes: 64 * 1024,
        },
        deadline_unix_ms: None,
        body_len: 0,
    };

    // Relay 1 starts and stays in flight.
    let admission1 = gateway.admit_relay(&authed1).unwrap();
    let env1 = make_envelope("https://api.openai.com/v1/responses");
    let injection1 = gateway.inject(&authed1, 2, &env1).await.unwrap();
    let token1 = {
        let relays = state.relays.lock().unwrap();
        assert_eq!(relays.len(), 1);
        relays.values().next().unwrap().token.clone()
    };

    // Relay 2a uses the IDENTICAL (operation, attempt) and fails fast (unpinned path).
    let fast_fail_admission = gateway.admit_relay(&authed2).unwrap();
    let fast_fail_env = make_envelope("https://api.openai.com/v1/unpinned-fast-fail");
    let err = gateway
        .inject(&authed2, 2, &fast_fail_env)
        .await
        .unwrap_err();
    assert_eq!(err, GatewayRefusal::UpstreamNotPinned {});
    drop(fast_fail_admission);
    gateway.audit_for(
        &authed2,
        RelayRecord {
            account: Some(account.clone()),
            operation: Some((shared_op.clone(), 1)),
            upstream: None,
            method: Some("POST".to_owned()),
            upstream_status: None,
            bytes_down: 0,
            upstream_failure: None,
            duration: std::time::Duration::from_millis(1),
            outcome: RelayOutcome::Refused(err.code()),
        },
    );

    // Relay 2b also uses the IDENTICAL (operation, attempt), succeeds inject, and finishes fast.
    let fast_ok_admission = gateway.admit_relay(&authed2).unwrap();
    let fast_ok_injection = gateway.inject(&authed2, 2, &env1).await.unwrap();
    assert_eq!(state.relays.lock().unwrap().len(), 2);
    drop(fast_ok_injection);
    drop(fast_ok_admission);
    gateway.audit_for(
        &authed2,
        RelayRecord {
            account: Some(account.clone()),
            operation: Some((shared_op.clone(), 1)),
            upstream: Some((
                "https://api.openai.com".to_owned(),
                "/v1/responses".to_owned(),
            )),
            method: Some("POST".to_owned()),
            upstream_status: Some(200),
            bytes_down: 10,
            upstream_failure: None,
            duration: std::time::Duration::from_millis(2),
            outcome: RelayOutcome::Complete,
        },
    );

    // Relay 1 MUST still be registered, uncut so far, and counted in flight.
    assert_eq!(state.relays.lock().unwrap().len(), 1);
    assert_eq!(state.inflight.load(std::sync::atomic::Ordering::Acquire), 1);
    assert!(!token1.is_cancelled());

    // `disable --cut` MUST still cancel Relay 1!
    admin(
        &state,
        &local(),
        None,
        &AdminRequest::Disable {
            target: DisableTarget::Account {
                account: account.clone(),
            },
            cut: true,
        },
    );
    assert!(
        token1.is_cancelled(),
        "Relay 1 must still be cut after colliding Relay 2 finished"
    );

    // Relay 1 terminates: inflight, caller_inflight, and relays all return to 0.
    drop(injection1);
    drop(admission1);
    gateway.audit_for(
        &authed1,
        RelayRecord {
            account: Some(account.clone()),
            operation: Some((shared_op, 1)),
            upstream: Some((
                "https://api.openai.com".to_owned(),
                "/v1/responses".to_owned(),
            )),
            method: Some("POST".to_owned()),
            upstream_status: None,
            bytes_down: 0,
            upstream_failure: None,
            duration: std::time::Duration::from_millis(5),
            outcome: RelayOutcome::Cut,
        },
    );
    assert_eq!(state.inflight.load(std::sync::atomic::Ordering::Acquire), 0);
    assert_eq!(
        state
            .caller_inflight
            .lock()
            .unwrap()
            .get("caller-one")
            .copied()
            .unwrap_or(0),
        0
    );
    assert_eq!(
        state
            .caller_inflight
            .lock()
            .unwrap()
            .get("caller-two")
            .copied()
            .unwrap_or(0),
        0
    );
    assert!(state.relays.lock().unwrap().is_empty());

    // Audit lines must record real callers, never "unknown".
    let lines =
        super::audit::read_lines(&super::server_config::audit_dir(&paths), None, None).unwrap();
    let relay_lines: Vec<_> = lines
        .iter()
        .filter(|line| line.contains("\"verb\":\"relay\""))
        .collect();
    assert_eq!(relay_lines.len(), 3, "{relay_lines:?}");
    assert!(
        relay_lines
            .iter()
            .all(|line| !line.contains("\"caller\":\"unknown\"")),
        "{relay_lines:?}"
    );
    assert!(
        relay_lines
            .iter()
            .any(|line| line.contains("\"caller\":\"caller-one\"") && line.contains("\"cut\"")),
        "{relay_lines:?}"
    );
    assert_eq!(
        relay_lines
            .iter()
            .filter(|line| line.contains("\"caller\":\"caller-two\""))
            .count(),
        2
    );
}

#[test]
fn authorization_scope_bypasses_are_blocked_across_list_status_disable_login_and_missing_scopes() {
    let (_dir, paths) = home();
    let state = state_for(&paths);
    let sandbox = GatewayAccount::new("openai/sandbox").unwrap();
    let prod = GatewayAccount::new("anthropic/prod").unwrap();
    for account in [&sandbox, &prod] {
        admin(
            &state,
            &local(),
            None,
            &AdminRequest::KeySet {
                account: account.clone(),
                secret: plakous::gateway::protocol::SecretText::new(zeroize::Zeroizing::new(
                    "synthetic-secret".to_owned(),
                ))
                .unwrap(),
            },
        );
    }

    let narrow = mint(
        &state,
        "narrow",
        "relay:openai/sandbox,admin:openai/sandbox",
        "127.0.0.1",
    );
    let admin_only = mint(&state, "admin-only", "admin:openai/sandbox", "127.0.0.1");
    let relay_only = mint(&state, "relay-only", "relay:*", "127.0.0.1");
    let broad_admin = mint(&state, "broad-admin", "relay:*,admin:*", "127.0.0.1");

    // 1. AdminRequest::List filters returned accounts to caller-authorized scopes.
    let AdminResponse::Accounts { accounts } = admin(
        &state,
        &tcp("127.0.0.1"),
        Some(&narrow),
        &AdminRequest::List {},
    ) else {
        panic!("expected Accounts");
    };
    assert_eq!(accounts, vec![sandbox.clone()]);

    // 2. AdminRequest::Status { accounts: [] } filters to authorized scopes,
    //    and refuses callers without any relay scope rather than vacuously passing.
    let AdminResponse::Status { rows } = admin(
        &state,
        &tcp("127.0.0.1"),
        Some(&narrow),
        &AdminRequest::Status {
            accounts: Vec::new(),
        },
    ) else {
        panic!("expected Status");
    };
    let row_accounts: Vec<_> = rows.into_iter().map(|row| row.facts.account).collect();
    assert_eq!(row_accounts, vec![sandbox.clone()]);

    let denied_status = dispatch::handle(
        &state,
        &tcp("127.0.0.1"),
        &request(
            "POST",
            &Endpoint::Admin.path(1),
            Some(&admin_only),
            call::encode(&AdminRequest::Status {
                accounts: Vec::new(),
            })
            .unwrap(),
        ),
    );
    assert_eq!(denied_status.status, 403);
    assert_eq!(refusal(&denied_status), GatewayRefusal::ScopeDenied {});

    // 3. DisableTarget::Caller: narrow admin cannot disable another caller,
    //    can disable self; relay-only cannot disable anyone; broad admin:* can disable others.
    let narrow_other = dispatch::handle(
        &state,
        &tcp("127.0.0.1"),
        &request(
            "POST",
            &Endpoint::Admin.path(1),
            Some(&narrow),
            call::encode(&AdminRequest::Disable {
                target: DisableTarget::Caller {
                    caller: CallerId::new("broad-admin").unwrap(),
                },
                cut: true,
            })
            .unwrap(),
        ),
    );
    assert_eq!(narrow_other.status, 403);
    assert_eq!(refusal(&narrow_other), GatewayRefusal::ScopeDenied {});

    let relay_self = dispatch::handle(
        &state,
        &tcp("127.0.0.1"),
        &request(
            "POST",
            &Endpoint::Admin.path(1),
            Some(&relay_only),
            call::encode(&AdminRequest::Disable {
                target: DisableTarget::Caller {
                    caller: CallerId::new("relay-only").unwrap(),
                },
                cut: false,
            })
            .unwrap(),
        ),
    );
    assert_eq!(relay_self.status, 403);
    assert_eq!(refusal(&relay_self), GatewayRefusal::ScopeDenied {});

    let narrow_self = admin(
        &state,
        &tcp("127.0.0.1"),
        Some(&narrow),
        &AdminRequest::Disable {
            target: DisableTarget::Caller {
                caller: CallerId::new("narrow").unwrap(),
            },
            cut: false,
        },
    );
    assert_eq!(narrow_self, AdminResponse::Disabled {});
    let broad_disable_other = admin(
        &state,
        &tcp("127.0.0.1"),
        Some(&broad_admin),
        &AdminRequest::Disable {
            target: DisableTarget::Caller {
                caller: CallerId::new("relay-only").unwrap(),
            },
            cut: false,
        },
    );
    assert_eq!(broad_disable_other, AdminResponse::Disabled {});

    // 4. Pending OAuth logins: bound to (Principal, GatewayAccount); cross-caller
    //    complete/poll/cancel and post-start account/caller disables are refused.
    let claude_admin = mint(
        &state,
        "claude-admin",
        "relay:claude,admin:claude",
        "127.0.0.1",
    );
    let codex_admin = mint(
        &state,
        "codex-admin",
        "relay:codex,admin:codex",
        "127.0.0.1",
    );
    let claude = GatewayAccount::new("claude").unwrap();
    let AdminResponse::Challenge { attempt, .. } = admin(
        &state,
        &tcp("127.0.0.1"),
        Some(&claude_admin),
        &AdminRequest::LoginStart {
            account: claude.clone(),
            notice_revision: plakous::gateway::protocol::BoundedText::new("notice").unwrap(),
            replace_newer: false,
        },
    ) else {
        panic!("expected login challenge");
    };

    for attacker_key in [&codex_admin, &broad_admin] {
        for req in [
            AdminRequest::LoginComplete {
                attempt: attempt.clone(),
                pasted: plakous::gateway::protocol::SecretText::new(zeroize::Zeroizing::new(
                    "fixture-ok".to_owned(),
                ))
                .unwrap(),
            },
            AdminRequest::LoginPoll {
                attempt: attempt.clone(),
            },
            AdminRequest::LoginCancel {
                attempt: attempt.clone(),
            },
        ] {
            let denied = dispatch::handle(
                &state,
                &tcp("127.0.0.1"),
                &request(
                    "POST",
                    &Endpoint::Admin.path(1),
                    Some(attacker_key),
                    call::encode(&req).unwrap(),
                ),
            );
            assert_eq!(
                denied.status, 403,
                "cross-caller login hijack must fail 403"
            );
            assert_eq!(refusal(&denied), GatewayRefusal::ScopeDenied {});
        }
    }

    // Account disabled AFTER LoginStart blocks LoginComplete / LoginPoll / LoginCancel.
    admin(
        &state,
        &local(),
        None,
        &AdminRequest::Disable {
            target: DisableTarget::Account {
                account: claude.clone(),
            },
            cut: false,
        },
    );
    for req in [
        AdminRequest::LoginComplete {
            attempt: attempt.clone(),
            pasted: plakous::gateway::protocol::SecretText::new(zeroize::Zeroizing::new(
                "fixture-ok".to_owned(),
            ))
            .unwrap(),
        },
        AdminRequest::LoginPoll {
            attempt: attempt.clone(),
        },
        AdminRequest::LoginCancel {
            attempt: attempt.clone(),
        },
    ] {
        let blocked = dispatch::handle(
            &state,
            &tcp("127.0.0.1"),
            &request(
                "POST",
                &Endpoint::Admin.path(1),
                Some(&claude_admin),
                call::encode(&req).unwrap(),
            ),
        );
        assert_eq!(blocked.status, 503);
        assert_eq!(refusal(&blocked), GatewayRefusal::AccountDisabled {});
    }
    admin(
        &state,
        &local(),
        None,
        &AdminRequest::Enable {
            target: DisableTarget::Account {
                account: claude.clone(),
            },
        },
    );
    // Owner can still complete cleanly after re-enable.
    let completed = admin(
        &state,
        &tcp("127.0.0.1"),
        Some(&claude_admin),
        &AdminRequest::LoginComplete {
            attempt,
            pasted: plakous::gateway::protocol::SecretText::new(zeroize::Zeroizing::new(
                "fixture-ok".to_owned(),
            ))
            .unwrap(),
        },
    );
    assert!(matches!(
        completed,
        AdminResponse::Progress {
            phase: LoginPhase::Succeeded,
            ..
        }
    ));

    // 5. Fail closed when scopes == None for a non-LocalAdmin principal.
    let caller_principal = super::auth::Principal::Caller(CallerId::new("missing-scopes").unwrap());
    let admin_denied = dispatch::serve_authed(
        &state,
        &caller_principal,
        None,
        &request(
            "POST",
            &Endpoint::Admin.path(1),
            None,
            call::encode(&AdminRequest::List {}).unwrap(),
        ),
        std::time::Instant::now(),
    );
    assert_eq!(admin_denied.status, 403);
    assert_eq!(refusal(&admin_denied), GatewayRefusal::ScopeDenied {});
    assert_eq!(
        dispatch::gate_relay(&state, &caller_principal, None, &sandbox),
        Err(GatewayRefusal::ScopeDenied {})
    );
    assert_eq!(
        dispatch::gate_admin_allowance(&state, &caller_principal, None, &sandbox),
        Err(GatewayRefusal::ScopeDenied {})
    );
    assert_eq!(
        dispatch::gate_admin_refresh(&state, &caller_principal, None, &sandbox),
        Err(GatewayRefusal::ScopeDenied {})
    );
}

#[test]
fn flags_and_callers_rollback_in_memory_state_when_persist_fails() {
    let dir = tempfile::tempdir().unwrap();
    let gw_dir = dir.path().join("gw");
    std::fs::create_dir_all(&gw_dir).unwrap();
    std::fs::set_permissions(&gw_dir, std::fs::Permissions::from_mode(0o700)).unwrap();

    // Flags persist failure (path is a symlink -> write_private refuses):
    let flags_path = gw_dir.join("flags.json");
    let mut flags = Flags::load(&flags_path).unwrap();
    std::os::unix::fs::symlink(dir.path().join("elsewhere"), &flags_path).unwrap();
    let account = GatewayAccount::new("openai").unwrap();
    let caller = CallerId::new("worker").unwrap();
    assert!(flags.disable_all().is_err());
    assert!(!flags.disabled_all());
    assert!(flags.disable_account(account.clone()).is_err());
    assert!(!flags.account_disabled(&account));
    assert!(flags.disable_caller(caller.clone()).is_err());
    assert!(!flags.caller_disabled(&caller));

    // Callers persist failure rollback:
    let callers_path = gw_dir.join("callers.json");
    let mut callers = Callers::load(&callers_path).unwrap();
    let initial_key = callers
        .add(
            caller.clone(),
            ScopeSet::parse("relay:*").unwrap(),
            vec![SourceNet::parse("127.0.0.1").unwrap()],
        )
        .unwrap();
    assert!(callers.authenticate(&initial_key).is_some());

    // Replace callers.json with a symlink so write_private fails.
    std::fs::remove_file(&callers_path).unwrap();
    std::os::unix::fs::symlink(dir.path().join("target"), &callers_path).unwrap();
    assert!(
        callers
            .add(
                CallerId::new("second").unwrap(),
                ScopeSet::parse("relay:*").unwrap(),
                vec![SourceNet::parse("127.0.0.1").unwrap()],
            )
            .is_err()
    );
    assert_eq!(callers.rows().len(), 1);
    assert!(callers.rotate(&caller).is_err());
    assert!(
        callers.authenticate(&initial_key).is_some(),
        "failed rotate must keep previous secret hash in memory"
    );
    assert!(callers.remove(&caller).is_err());
    assert!(
        callers.authenticate(&initial_key).is_some(),
        "failed remove must not revoke caller in memory"
    );
}

#[test]
fn gateway_client_bounds_responses_and_rejects_chunk_size_overflow() {
    // Chunk size usize::MAX overflows `size + 2` without checked arithmetic.
    let overflow_hex = format!("{:x}", usize::MAX);
    let raw =
        format!("HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n{overflow_hex}\r\nab\r\n");
    assert!(super::client::parse_response(raw.as_bytes()).is_err());

    let overflow_minus_one = format!("{:x}", usize::MAX - 1);
    let raw2 = format!(
        "HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n{overflow_minus_one}\r\nab\r\n"
    );
    assert!(super::client::parse_response(raw2.as_bytes()).is_err());

    // Oversized content-length and oversized unframed body are refused.
    let too_large = format!(
        "HTTP/1.1 200 OK\r\ncontent-length: {}\r\n\r\n",
        call::MAX_CALL_BYTES + 1
    );
    assert!(super::client::parse_response(too_large.as_bytes()).is_err());
    let mut unframed = b"HTTP/1.1 200 OK\r\n\r\n".to_vec();
    unframed.resize(unframed.len() + call::MAX_CALL_BYTES + 1, b'x');
    assert!(super::client::parse_response(&unframed).is_err());

    // Real socket streaming unbounded data fails cleanly at the response bound.
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("big.sock");
    let listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
    let server = std::thread::spawn(move || {
        use std::io::Write as _;
        if let Ok((mut conn, _)) = listener.accept() {
            let _ = conn.write_all(b"HTTP/1.1 200 OK\r\n\r\n");
            let chunk = vec![b'a'; 32 * 1024];
            for _ in 0..16 {
                if conn.write_all(&chunk).is_err() {
                    break;
                }
            }
        }
    });
    let err = super::client::exchange(
        &super::GatewayAddress::Unix(sock),
        None,
        "GET",
        "/versions",
        &[],
    )
    .err()
    .expect("oversized stream must be rejected");
    assert!(err.to_string().contains("exceeds size limit"), "{err}");
    server.join().unwrap();
}

#[test]
fn gateway_key_file_io_file_and_api_key_writes_refuse_symlinks_and_aliases() {
    let dir = tempfile::tempdir().unwrap();
    let priv_dir = dir.path().join("priv");
    super::io_file::ensure_private_dir(&priv_dir).unwrap();

    // Symlink target that must never be read or overwritten.
    let target = priv_dir.join("target.txt");
    super::io_file::write_private(&target, b"original-secret\n").unwrap();
    let link = priv_dir.join("link.key");
    std::os::unix::fs::symlink(&target, &link).unwrap();

    // read_key_file refuses symlinks and hard-link aliases.
    assert!(super::read_key_file(&link).is_err());
    let alias = priv_dir.join("alias.key");
    std::fs::hard_link(&target, &alias).unwrap();
    assert!(super::read_key_file(&target).is_err());
    std::fs::remove_file(&alias).unwrap();
    assert_eq!(
        super::read_key_file(&target).unwrap().as_str(),
        "original-secret"
    );

    // io_file::private_file and io_file::write_private refuse symlinks.
    assert!(super::io_file::private_file(&link).is_err());
    assert!(super::io_file::write_private(&link, b"overwritten").is_err());
    assert_eq!(std::fs::read(&target).unwrap(), b"original-secret\n");

    // home::secrets::set_api_key refuses writing through a symlink.
    let home_paths = PlakPaths::explicit(dir.path().join("plak-home"));
    home_paths.ensure_layout().unwrap();
    let named_dir = home_paths.secret_shelf_dir(crate::home::paths::SecretShelf::Api(
        plakous_hosted::account::Vertical::OpenAi,
    ));
    home_paths
        .ensure_secret_shelf_dir(crate::home::paths::SecretShelf::Api(
            plakous_hosted::account::Vertical::OpenAi,
        ))
        .unwrap();
    let key_link = named_dir.join("sym.secret");
    std::os::unix::fs::symlink(&target, &key_link).unwrap();
    assert!(
        crate::home::secrets::set_api_key(
            &home_paths,
            "openai/sym",
            zeroize::Zeroizing::new("sk-attempt".to_owned()),
        )
        .is_err()
    );
    assert_eq!(std::fs::read(&target).unwrap(), b"original-secret\n");
}
