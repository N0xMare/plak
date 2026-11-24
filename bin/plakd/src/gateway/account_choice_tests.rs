//! Session account choice on a gateway host (spec 02 §4.4.1): readiness is
//! the gateway's, so an API account whose key only the gateway holds is
//! Ready for the fill, `ChooseAccount` and a spawn, and a key file in the
//! host's own home is never read.

use std::collections::BTreeSet;

use plakous_hosted::agent::RosterSource as _;
use plakous_hosted::runtime::{
    SurfaceAccountChoice, SurfaceFacts, SurfaceHandler as _, SurfacePhase, SurfaceSession,
    UsageReads,
};

use super::test_service::GatewayFixture;
use crate::account_choice::{self, FillMode, ReadyAccount};
use crate::home::paths::PlakPaths;
use crate::surface_protocol::SurfaceIntent;

fn ready(catalog: &[ReadyAccount], account: &str) -> Option<bool> {
    catalog
        .iter()
        .find(|row| row.account == account)
        .map(|row| row.ready)
}

/// `ChooseAccount` of the `openai` root on an idle session: the host reads
/// readiness off its loop, then the dispatch admits or refuses the account.
fn choose_openai(
    surface: &crate::shell::settings::ProductSurface,
    workspace: &std::path::Path,
) -> Option<SurfaceAccountChoice> {
    let intent = SurfaceIntent::ChooseAccount {
        kind: plakd_wire_types::console::Vertical::Openai,
        account: "openai".into(),
        continue_failed: false,
    };
    let (tx, refusals) = crate::surface_protocol::host_event_channel();
    let mut facts = SurfaceFacts {
        phase: SurfacePhase::Idle,
        attached_one_shot: false,
        catalog: &[],
        queue_len: 0,
        usage_reads: UsageReads::default(),
        accounts: None,
        session: Some(SurfaceSession {
            runner: None,
            id: "gateway-session".into(),
            account: Some("openai".into()),
            model: None,
            effort: None,
            service_tier: None,
            workspace: workspace.to_path_buf(),
            roster: None,
            live_children: false,
            mcp: None,
            account_choices: std::collections::BTreeMap::new(),
        }),
    };
    facts.phase = SurfacePhase::Busy;
    assert!(surface.accounts_read(&intent, &facts).is_none());
    facts.phase = SurfacePhase::Idle;
    let read = surface
        .accounts_read(&intent, &facts)
        .expect("ChooseAccount reads readiness");
    let readiness = read();
    facts.accounts = Some(&readiness);
    let effect = surface.dispatch(intent, facts, &plakous_hosted::runtime::Sender::over(tx));
    let choice = effect.account.map(|effect| effect.choice);
    if choice.is_none() {
        eprintln!("refused: {:?}", refusals.try_iter().collect::<Vec<_>>());
    }
    choice
}

#[test]
fn an_api_account_ready_only_at_the_gateway_fills_chooses_and_spawns() {
    let directory = tempfile::tempdir().unwrap();
    // The gateway's home holds the only `openai` key.
    let gateway_home = PlakPaths::explicit(directory.path().join("gateway-home"));
    std::fs::create_dir_all(gateway_home.home()).unwrap();
    crate::home::secrets::set_api_key(
        &gateway_home,
        "openai",
        zeroize::Zeroizing::new("synthetic-openai-key".to_owned()),
    )
    .unwrap();
    let gateway = GatewayFixture::start(&gateway_home, &directory.path().join("gateway-state"));

    // The host's home names the gateway and holds only an `anthropic` key,
    // which a host that read its own key files would report Ready.
    let host = PlakPaths::explicit(directory.path().join("host-home"));
    std::fs::create_dir_all(host.home()).unwrap();
    crate::home::secrets::set_api_key(
        &host,
        "anthropic",
        zeroize::Zeroizing::new("synthetic-local-key".to_owned()),
    )
    .unwrap();
    std::fs::write(
        host.config_file(),
        format!("[gateway]\naddress = \"{}\"\n", gateway.address().label()),
    )
    .unwrap();
    let custody = super::HostCustody::resolve(&host).unwrap();
    assert!(custody.admin().is_some(), "the host names the gateway");

    let catalog = account_choice::ready_catalog(&host, &custody).unwrap();
    assert_eq!(ready(&catalog, "openai"), Some(true), "{catalog:?}");
    assert_eq!(
        ready(&catalog, "anthropic"),
        Some(false),
        "the host's own key file is not read: {catalog:?}"
    );

    // Fill: the kind's default slot is Ready at the gateway.
    let filled = account_choice::fill(
        &BTreeSet::from(["openai".to_owned(), "anthropic".to_owned()]),
        Some("openai"),
        &[],
        &catalog,
        FillMode::BestEffort,
    )
    .unwrap();
    assert_eq!(filled.root_account.as_deref(), Some("openai"));
    assert!(
        !filled.choices.contains_key("anthropic"),
        "a locally held key fills nothing: {filled:?}"
    );

    // ChooseAccount: the host reads readiness off its loop, then the
    // dispatch admits the gateway's Ready account.
    let choice = choose_openai(
        &crate::shell::settings::ProductSurface::new(host.clone(), custody.clone()),
        directory.path(),
    );
    assert!(
        matches!(
            &choice,
            Some(SurfaceAccountChoice::Root { account, continue_failed: false }) if account == "openai"
        ),
        "{choice:?}"
    );

    // Spawn: a card of the kind with no stored choice fills from the
    // gateway's readiness.
    let source = crate::roster_input::ProductRosterSource::new(Some(host.clone()), custody);
    let fill = source
        .fill_spawn_account("openai")
        .expect("a product home fills a spawn");
    assert_eq!(fill().as_deref(), Some("openai"));
    let local_only = source
        .fill_spawn_account("anthropic")
        .expect("a product home fills a spawn");
    assert_eq!(
        local_only(),
        None,
        "a spawn never reads the host's key file"
    );
}

#[test]
fn the_gateway_reports_the_default_and_named_subscription_slots() {
    use plakous::gateway::protocol::GatewayAccount;
    use plakous::gateway::protocol::admin::{AdminRequest, AdminResponse};
    use plakous::subscriptions::{NativeProvider, NativeSubscriptions, SubscriptionPaths};
    use std::os::unix::fs::DirBuilderExt as _;

    let directory = tempfile::tempdir().unwrap();
    let gateway_home = PlakPaths::explicit(directory.path().join("gateway-home"));
    let state = directory.path().join("gateway-state");
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(gateway_home.home())
        .unwrap();
    gateway_home.ensure_layout().unwrap();
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&state)
        .unwrap();
    let subscriptions =
        NativeSubscriptions::new(SubscriptionPaths::from_xdg_state_home(&state).unwrap());
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    // Keep the login reservation alive so the named, signed-out slot remains
    // on the service's roster for the whole read.
    let _named = runtime.block_on(async {
        subscriptions
            .for_named_account(NativeProvider::Codex, "work")
            .unwrap()
            .create_account_for_login(NativeProvider::Codex)
            .await
            .unwrap()
    });
    let gateway = GatewayFixture::start(&gateway_home, &state);
    let host = PlakPaths::explicit(directory.path().join("host-home"));
    std::fs::create_dir_all(host.home()).unwrap();
    std::fs::write(
        host.config_file(),
        format!("[gateway]\naddress = \"{}\"\n", gateway.address().label()),
    )
    .unwrap();
    let custody = super::HostCustody::resolve(&host).unwrap();
    let status = custody.admin().unwrap().call(&AdminRequest::Status {
        accounts: ["codex", "codex/work"]
            .into_iter()
            .map(|account| GatewayAccount::new(account).unwrap())
            .collect(),
    });
    let catalog = account_choice::ready_catalog(&host, &custody);
    let AdminResponse::Status { rows } = status.unwrap() else {
        panic!("the gateway did not answer the subscription status request");
    };
    assert_eq!(
        rows.iter()
            .map(|row| row.facts.account.as_str())
            .collect::<Vec<_>>(),
        ["codex", "codex/work"]
    );
    let catalog = catalog.expect("the gateway reports status for both subscription slots");
    assert_eq!(ready(&catalog, "codex"), Some(false), "{catalog:?}");
    assert_eq!(ready(&catalog, "codex/work"), Some(false), "{catalog:?}");
}
