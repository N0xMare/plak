use super::*;
use crate::gateway::{HostCustody, gateway_seed};
use plakd_wire_types::console::{Credential, KeySource, NativeIdentity};
use plakous::api_key::{ApiKey, CredentialProviderTag};
use plakous::gateway::custody::{
    CustodyError, CustodySlot, KeyHeaderForm, KeyInjection, LocalKey, LocalKeyCustodian,
    LocalKeyStore, UpstreamPin,
};
use plakous::gateway::pins::{
    anthropic::{CLAUDE_CREDENTIAL_HEADERS, CLAUDE_UPSTREAM_PINS},
    openai::{CODEX_CREDENTIAL_HEADERS, CODEX_UPSTREAM_PINS},
    openrouter::{OPENROUTER_CREDENTIAL_HEADERS, OPENROUTER_UPSTREAM_PINS},
    xai::{SUPERGROK_CREDENTIAL_HEADERS, SUPERGROK_UPSTREAM_PINS},
};
use plakous::gateway::server::{InProcessGateway, LocalCustody, http1_upstream, new_instance};
use plakous::gateway::{GatewayCustody, GatewayEndpoint};
use plakous::model::FixedClock;
use plakous::subscriptions::custody::SubscriptionCustodian;
use plakous::subscriptions::test_support::TestDialOrigin;
use plakous::subscriptions::{NativeProvider, NativeSubscriptions, SubscriptionPaths};
use plakous_host_protocol::views::presentation::{
    UsageMeterReading, UsageMeterView, VerticalUsageState, VerticalUsageView,
};
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinSet;

const ACCOUNTS: [(&str, NativeProvider, &[UpstreamPin]); 3] = [
    ("claude", NativeProvider::Claude, CLAUDE_UPSTREAM_PINS),
    ("codex", NativeProvider::Codex, CODEX_UPSTREAM_PINS),
    ("grok", NativeProvider::Xai, SUPERGROK_UPSTREAM_PINS),
];
const CLAUDE_USAGE: &str = "GET /api/oauth/usage HTTP/1.1";
const CLAUDE_PROFILE: &str = "GET /api/oauth/profile HTTP/1.1";
const CODEX_USAGE: &str = "GET /backend-api/wham/usage HTTP/1.1";
const GROK_USAGE: &str = "GET /v1/billing?format=credits HTTP/1.1";
const OPENROUTER_USAGE: &str = "GET /api/v1/key HTTP/1.1";
const ROUTER_ACCOUNT: &str = "openrouter/work";
const ROUTER_FIXTURE_KEY: &str = "synthetic-openrouter-fixture";

#[derive(Clone, Copy, Eq, PartialEq)]
enum Profile {
    Available,
    Failed,
    Stalled,
    Unpinned,
}

struct Keys;

impl LocalKeyStore for Keys {
    fn key(&self, slot: &CustodySlot) -> Result<Option<LocalKey>, CustodyError> {
        if slot.as_str() != ROUTER_ACCOUNT {
            return Ok(None);
        }
        Ok(Some(LocalKey {
            key: ApiKey::new(ROUTER_FIXTURE_KEY).map_err(|_| CustodyError::Store)?,
            source: plakous::gateway::custody::KeySource::File,
            env_var: None,
        }))
    }
}

async fn upstream(listener: TcpListener, requests: Arc<Mutex<Vec<String>>>, profile: Profile) {
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let Ok((stream, _)) = accepted else { break };
                let requests = Arc::clone(&requests);
                connections.spawn(async move { upstream_reply(stream, &requests, profile).await });
            }
            _ = connections.join_next(), if !connections.is_empty() => {}
        }
    }
}

async fn upstream_reply(stream: TcpStream, requests: &Mutex<Vec<String>>, profile: Profile) {
    let mut stream = BufReader::new(stream);
    let mut line = String::new();
    if !matches!(stream.read_line(&mut line).await, Ok(size) if size > 0) {
        return;
    }
    let request = line.trim_end();
    requests.lock().unwrap().push(request.to_owned());
    if request == CLAUDE_PROFILE && profile == Profile::Stalled {
        std::future::pending::<()>().await;
        return;
    }
    let (status, body) = match request {
        CLAUDE_USAGE => (
            "200 OK",
            r#"{
                "five_hour":{"utilization":19,"resets_at":"2099-01-01T00:00:00Z"},
                "seven_day":{"utilization":34,"resets_at":"2099-01-02T00:00:00Z"},
                "limits":[{"kind":"weekly_scoped","percent":57,"resets_at":"2099-01-02T00:00:00Z",
                    "scope":{"model":{"display_name":"Fable"}}}]
            }"#,
        ),
        CLAUDE_PROFILE if profile != Profile::Failed => (
            "200 OK",
            r#"{
                "account":{"email_address":"claude@example.test"},
                "organization":{"organization_type":"claude_max"}
            }"#,
        ),
        CLAUDE_PROFILE => ("503 Service Unavailable", "{}"),
        CODEX_USAGE => (
            "200 OK",
            r#"{
                "email":"codex@example.test","plan_type":"pro",
                "rate_limit":{"primary_window":{"used_percent":12,"limit_window_seconds":18000,"reset_at":4000000000}}
            }"#,
        ),
        GROK_USAGE => (
            "200 OK",
            r#"{"config":{
                "creditUsagePercent":12.25,
                "currentPeriod":{"type":"USAGE_PERIOD_TYPE_WEEKLY","end":"2099-01-01T00:00:00Z"},
                "prepaidBalance":{"val":-430},"onDemandUsed":{"val":220},"onDemandCap":{"val":1200}
            }}"#,
        ),
        OPENROUTER_USAGE => (
            "200 OK",
            r#"{"data":{
                "limit":20,"limit_remaining":8,"limit_reset":"weekly",
                "usage":18,"usage_daily":2,"usage_weekly":12,"usage_monthly":16
            }}"#,
        ),
        _ => ("404 Not Found", "{}"),
    };
    loop {
        line.clear();
        if !matches!(stream.read_line(&mut line).await, Ok(size) if size > 2) {
            break;
        }
    }
    let reply = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.get_mut().write_all(reply.as_bytes()).await;
}

struct Fixture {
    console: Arc<Console>,
    endpoint: GatewayEndpoint,
    requests: Arc<Mutex<Vec<String>>>,
    upstream: tokio::task::JoinHandle<()>,
    drain: Option<Box<dyn FnOnce()>>,
    root: tempfile::TempDir,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.upstream.abort();
        if let Some(drain) = self.drain.take() {
            drain();
        }
    }
}

fn seed_native_accounts(state: &std::path::Path) {
    std::thread::scope(|scope| {
        scope
            .spawn(|| {
                for (_, provider, _) in ACCOUNTS {
                    gateway_seed::seed(state, provider, None, 4_000_000_000);
                }
            })
            .join()
            .unwrap();
    });
}

impl Fixture {
    async fn start(profile: Profile) -> Result<Self, Box<dyn std::error::Error>> {
        let root = tempfile::tempdir()?;
        let gateway_home = PlakPaths::explicit(root.path().join("gateway"));
        let gateway_state = root.path().join("gateway-state");
        seed_native_accounts(&gateway_state);
        gateway_home.ensure_layout()?;
        crate::home::secrets::set_api_key(
            &gateway_home,
            ROUTER_ACCOUNT,
            zeroize::Zeroizing::new(ROUTER_FIXTURE_KEY.to_owned()),
        )?;
        let mut local = LocalCustody::new(new_instance().ok_or("gateway instance")?);
        let gateway_paths = SubscriptionPaths::from_xdg_state_home(&gateway_state)?;
        let mut reviewed_at = 0;
        for (kind, provider, pins) in ACCOUNTS {
            let pins = if kind == "claude" && profile == Profile::Unpinned {
                pins.split_last().unwrap().1
            } else {
                pins
            };
            let at = NativeSubscriptions::notice(provider)?.reviewed_at() + 60;
            reviewed_at = reviewed_at.max(at);
            let subscriptions = NativeSubscriptions::new(gateway_paths.clone())
                .with_clock(Arc::new(FixedClock::new(Timestamp::new(at, 0)?)));
            local = local.with_kind(
                kind,
                Arc::new(SubscriptionCustodian::new(subscriptions, provider)),
                pins,
            );
        }
        let router = LocalKeyCustodian::new(Arc::new(Keys), |kind| {
            (kind == "openrouter").then_some(KeyInjection {
                provider: CredentialProviderTag::OpenRouterPlatform,
                header: "authorization",
                form: KeyHeaderForm::Bearer,
            })
        });
        local = local.with_kind("openrouter", Arc::new(router), OPENROUTER_UPSTREAM_PINS);

        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let dial = TestDialOrigin::loopback_http(addr.ip(), addr.port())?;
        let mut gateway = InProcessGateway::server(local, http1_upstream()?);
        for origin in [
            "https://api.anthropic.com",
            "https://chatgpt.com",
            "https://cli-chat-proxy.grok.com",
            "https://openrouter.ai",
        ] {
            gateway = gateway.with_dial_override(origin, dial.clone());
        }
        let endpoint = InProcessGateway::endpoint_for(&gateway);
        let gateway = GatewayCustody::connect(endpoint.clone(), None, headers)?;
        let (custody, drain) =
            HostCustody::in_process_for_test(gateway, &gateway_home, &gateway_state);
        let requests = Arc::new(Mutex::new(Vec::new()));
        let upstream = tokio::spawn(upstream(listener, Arc::clone(&requests), profile));
        let console = Console::new(
            PlakPaths::explicit(root.path().join("host")),
            root.path().join("host-state"),
            custody,
            Some(tokio::runtime::Handle::current()),
            ConfiguredSwitches {
                web_accounts_read: true,
                web_config_read: false,
                web_credentials_write: false,
                web_config_write: false,
                web_files_read: false,
                web_terminal: false,
            },
        );
        let paths = SubscriptionPaths::from_xdg_state_home(&console.state)?;
        *console.native_fixture.lock().unwrap() = Some(
            console
                .custody
                .subscriptions(NativeSubscriptions::new(paths))
                .with_clock(Arc::new(FixedClock::new(Timestamp::new(reviewed_at, 0)?))),
        );
        Ok(Self {
            console,
            endpoint,
            requests,
            upstream,
            drain: Some(Box::new(drain)),
            root,
        })
    }

    fn reconnect_usage_reader(&self) -> TestResult {
        let gateway = GatewayCustody::connect(self.endpoint.clone(), None, headers)?;
        let mut reader = self.console.native_fixture.lock().unwrap();
        *reader = Some(
            reader
                .take()
                .ok_or("fixture usage reader")?
                .with_custody(&gateway.custody()),
        );
        Ok(())
    }

    fn bump_claude_generation(&self) -> TestResult {
        use plakous::auth::{
            CredentialRecord, CredentialSlotId, PrivateCredentialStore, SecretBytes,
            SubscriptionIntegrationProfileId,
        };
        let provider = NativeProvider::Claude;
        let profile = SubscriptionIntegrationProfileId::new(
            NativeSubscriptions::notice(provider)?.profile_id(),
        )?;
        let path = gateway_seed::primary_record(&self.root.path().join("gateway-state"), provider);
        let store = PrivateCredentialStore::open_slot(
            &path,
            profile.clone(),
            CredentialSlotId::new("primary")?,
        )?;
        let fence = store.load()?.ok_or("seeded credential fence")?;
        let payload = {
            let lease = store
                .try_acquire_ready(&fence)?
                .ok_or("seeded credential lease")?;
            SecretBytes::new(lease.payload().expose_secret().to_vec())
        };
        store.replace(
            fence.generation(),
            &CredentialRecord::new(
                profile,
                fence.account_binding().clone(),
                fence.generation().next()?,
                fence.status(),
                payload,
            )?,
        )?;
        Ok(())
    }

    async fn request(
        &self,
        request: HostRequest,
    ) -> Result<HostResponse, Box<dyn std::error::Error>> {
        let context = HostRequestContext {
            principal: HostPrincipal::Web("fixture".to_owned()),
            ..context()
        };
        self.console
            .request(context, request)
            .await
            .map_err(|_| "wire refusal".into())
    }

    async fn usage(
        &self,
        account_id: Option<String>,
    ) -> Result<Vec<plakd_wire_types::console::AccountUsage>, Box<dyn std::error::Error>> {
        let response = self
            .request(HostRequest::AccountsUsage {
                account_id,
                window: UsageWindow::Hours24,
                cursor: None,
                limit: None,
            })
            .await?;
        match response {
            HostResponse::AccountsUsage { rows, .. } => Ok(rows),
            other => Err(format!("expected usage response: {other:?}").into()),
        }
    }

    fn assert_gets(&self, expected: &[&str]) {
        let mut actual = self.requests.lock().unwrap().clone();
        actual.sort_unstable();
        let mut expected = expected.to_vec();
        expected.sort_unstable();
        assert_eq!(actual, expected);
    }
}

fn headers(kind: &str) -> Option<&'static [&'static str]> {
    match kind {
        "claude" => Some(CLAUDE_CREDENTIAL_HEADERS),
        "codex" => Some(CODEX_CREDENTIAL_HEADERS),
        "grok" => Some(SUPERGROK_CREDENTIAL_HEADERS),
        "openrouter" => Some(OPENROUTER_CREDENTIAL_HEADERS),
        _ => None,
    }
}

fn meters(view: &VerticalUsageView) -> &[UsageMeterView] {
    let VerticalUsageState::Ready { meters } = &view.state else {
        panic!("expected provider meters: {:?}", view.state);
    };
    meters
}

fn amounts(meter: &UsageMeterView) -> Vec<(&str, &str)> {
    let UsageMeterReading::Amounts { entries } = &meter.reading else {
        panic!("expected provider amounts");
    };
    entries
        .iter()
        .map(|entry| (entry.label.as_str(), entry.value.as_str()))
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn web_accounts_usage_and_status_share_exact_gateway_observations() -> TestResult {
    let fixture = Fixture::start(Profile::Available).await?;
    let rows = fixture.usage(None).await?;
    let view = |id| {
        rows.iter()
            .find(|row| row.account_id == id)
            .unwrap()
            .allowance
            .view
            .as_ref()
            .unwrap()
    };
    let claude = view("claude");
    assert_eq!(claude.account_email.as_deref(), Some("claude@example.test"));
    assert_eq!(claude.plan_label.as_deref(), Some("Claude Max"));
    assert_eq!(meters(claude).len(), 3);
    assert!(
        meters(claude)
            .iter()
            .any(|meter| meter.group.as_deref() == Some("Fable limit") && meter.resets_at.is_some())
    );
    let grok = view("grok");
    assert_eq!(
        (grok.account_email.as_ref(), grok.plan_label.as_ref()),
        (None, None)
    );
    assert_eq!(meters(grok).len(), 1);
    assert_eq!(
        grok.balances
            .iter()
            .map(|balance| (balance.label.as_str(), balance.value.as_str()))
            .collect::<Vec<_>>(),
        [
            ("Credits", "$4.30"),
            ("Pay as you go", "$2.20 used of $12.00 cap")
        ]
    );
    let codex = view("codex");
    assert_eq!(codex.account_email.as_deref(), Some("codex@example.test"));
    assert_eq!(codex.plan_label.as_deref(), Some("Pro"));
    assert_eq!(meters(codex).len(), 1);
    assert_router_allowance(view(ROUTER_ACCOUNT));

    let gets = [
        CLAUDE_USAGE,
        CLAUDE_PROFILE,
        GROK_USAGE,
        CODEX_USAGE,
        OPENROUTER_USAGE,
    ];
    fixture.assert_gets(&gets);
    let response = fixture
        .request(HostRequest::AccountsStatus {
            account_id: None,
            cursor: None,
            limit: None,
        })
        .await?;
    let HostResponse::AccountsStatus { rows, .. } = response else {
        return Err(format!("expected status response: {response:?}").into());
    };
    let status = |id| rows.iter().find(|row| row.account_id == id).unwrap();
    for id in ["claude", "codex", "grok"] {
        let native = status(id).native.as_ref().unwrap();
        assert_eq!(native.credential, Credential::Ready);
        let usage = view(id);
        assert_eq!(
            native.identity,
            (usage.account_email.is_some() || usage.plan_label.is_some()).then(|| NativeIdentity {
                email: usage.account_email.clone(),
                plan: usage.plan_label.clone(),
            })
        );
    }
    assert_eq!(
        status(ROUTER_ACCOUNT).key.as_ref().unwrap().source,
        KeySource::File
    );
    fixture.assert_gets(&gets);
    assert!(!fixture.console.state.exists());
    assert!(!fixture.console.paths.secrets_dir().exists());
    Ok(())
}

fn assert_router_allowance(router: &VerticalUsageView) {
    assert!(router.observed_at.is_some());
    assert_eq!(router.account_email, None);
    let [limit, spend] = meters(router) else {
        panic!("expected OpenRouter limit and spend meters");
    };
    assert_eq!(
        amounts(limit),
        [
            ("cap", "$20.00"),
            ("remaining", "$8.00"),
            ("resets", "weekly")
        ]
    );
    assert_eq!(
        amounts(spend),
        [
            ("all time", "$18.00"),
            ("today", "$2.00"),
            ("this week", "$12.00"),
            ("this month", "$16.00")
        ]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gateway_claude_profile_failure_keeps_allowance_without_fabricated_identity() -> TestResult
{
    assert_claude_without_profile(Profile::Failed, &[CLAUDE_USAGE, CLAUDE_PROFILE]).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stalled_gateway_claude_profile_does_not_erase_meters() -> TestResult {
    assert_claude_without_profile(Profile::Stalled, &[CLAUDE_USAGE, CLAUDE_PROFILE]).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn old_gateway_without_claude_profile_pin_keeps_meters_but_not_identity() -> TestResult {
    assert_claude_without_profile(Profile::Unpinned, &[CLAUDE_USAGE]).await
}

async fn assert_claude_without_profile(profile: Profile, requests: &[&str]) -> TestResult {
    let fixture = Fixture::start(profile).await?;
    let view = claude_view(&fixture).await?;
    assert_eq!(
        (view.account_email.as_ref(), view.plan_label.as_ref()),
        (None, None)
    );
    assert_eq!(meters(&view).len(), 3);
    assert_eq!(claude_status_identity(&fixture).await?, None);
    fixture.assert_gets(requests);
    Ok(())
}

async fn claude_view(fixture: &Fixture) -> Result<VerticalUsageView, Box<dyn std::error::Error>> {
    let mut rows = fixture.usage(Some("claude".to_owned())).await?;
    let allowance = rows.pop().ok_or("Claude usage row")?.allowance;
    assert!(
        allowance.unavailable.is_none(),
        "{:?}",
        allowance.unavailable
    );
    allowance.view.ok_or_else(|| "Claude allowance view".into())
}

async fn claude_status_identity(
    fixture: &Fixture,
) -> Result<Option<NativeIdentity>, Box<dyn std::error::Error>> {
    let response = fixture
        .request(HostRequest::AccountsStatus {
            account_id: Some("claude".to_owned()),
            cursor: None,
            limit: None,
        })
        .await?;
    let HostResponse::AccountsStatus { mut rows, .. } = response else {
        return Err(format!("expected Claude status: {response:?}").into());
    };
    let native = rows
        .pop()
        .and_then(|row| row.native)
        .ok_or("Claude status")?;
    assert_eq!(native.credential, Credential::Ready);
    Ok(native.identity)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gateway_console_cache_misses_after_each_generation_change_and_on_new_custodian()
-> TestResult {
    let fixture = Fixture::start(Profile::Available).await?;
    for (reads, status_first) in [(1, false), (2, true), (3, false), (4, true)] {
        if reads == 4 {
            fixture.reconnect_usage_reader()?;
        } else if reads > 1 {
            fixture.bump_claude_generation()?;
        }
        let (identity, view) = if status_first {
            (
                claude_status_identity(&fixture).await?,
                claude_view(&fixture).await?,
            )
        } else {
            let view = claude_view(&fixture).await?;
            (claude_status_identity(&fixture).await?, view)
        };
        let identity = identity.ok_or("Claude identity")?;
        assert_eq!(identity.email.as_deref(), Some("claude@example.test"));
        assert_eq!(identity.plan, view.plan_label);
        assert_eq!(meters(&view).len(), 3);
        fixture.assert_gets(&[CLAUDE_USAGE, CLAUDE_PROFILE].repeat(reads));
    }
    Ok(())
}
