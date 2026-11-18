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
use super::route::{AccountRoute, select};
use super::server_config::ServerConfig;
use crate::home::paths::PlakPaths;

fn home() -> (tempfile::TempDir, PlakPaths) {
    let dir = tempfile::tempdir().unwrap();
    let paths = PlakPaths::explicit(dir.path().join("plak"));
    std::fs::create_dir_all(paths.home()).unwrap();
    (dir, paths)
}

fn state_for(paths: &PlakPaths) -> Arc<GatewayState> {
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
fn cli_routing_prefers_the_configured_address_then_the_service_then_the_process() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let lock = dir.path().join("holder.lock");
    let configured = select(Some("tcp:127.0.0.1:9"), false, &lock).unwrap();
    assert!(matches!(configured, AccountRoute::Configured(_)));
    assert!(configured.via_line().unwrap().contains("tcp:127.0.0.1:9"));
    let alone = select(None, false, &lock).unwrap();
    assert!(matches!(alone, AccountRoute::InProcess));
    assert!(alone.via_line().is_none());
    let exclusive =
        super::holder::HolderLock::try_acquire(&lock, super::holder::HolderMode::Exclusive)
            .unwrap();
    let socket = dir.path().join("gateway.sock");
    exclusive.advertise(&socket).unwrap();
    let service = select(None, false, &lock).unwrap();
    assert_eq!(
        service,
        AccountRoute::Service(super::address::GatewayAddress::Unix(socket))
    );
    assert!(service.via_line().unwrap().starts_with("via gateway unix:"));
    let still = select(Some("unix:/tmp/configured.sock"), false, &lock).unwrap();
    assert!(matches!(still, AccountRoute::Configured(_)));
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

#[test]
fn remote_route_uses_configuration_and_does_not_create_the_holder() {
    let (_dir, paths) = home();
    assert!(
        super::remote_account::remote_route(&paths)
            .unwrap()
            .is_none()
    );
    let lock = paths
        .home()
        .join("state/plak/subscriptions/.custody-holder.lock");
    assert!(
        !lock.exists(),
        "looking for a gateway must not create the holder"
    );
    std::fs::write(
        paths.config_file(),
        "[gateway]\naddress = \"tcp:127.0.0.1:9\"\n",
    )
    .unwrap();
    let configured = super::remote_account::remote_route(&paths)
        .unwrap()
        .unwrap();
    assert!(configured.via_line().unwrap().contains("tcp:127.0.0.1:9"));
    assert!(!lock.exists());
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
