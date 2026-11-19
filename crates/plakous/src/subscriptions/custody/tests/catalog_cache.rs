use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};

use plakous_auth::CredentialGeneration;
#[cfg(feature = "grok")]
use plakous_auth::custody::CustodyStatus;
use plakous_auth::custody::{
    AccountFacts, Consent, CredentialCustodian, CredentialFields, CustodyFuture, CustodySlot,
    ExpectedFence, IdentityDigest, RefreshRequest,
};

use super::{
    CODEX_CREDENTIAL_HEADERS, CODEX_UPSTREAM_PINS, GROK_MODEL, GatewayCustody, InProcessGateway,
    LocalCustody, MODEL, NativeProvider, NativeSubscriptionError, NativeSubscriptions,
    SubscriptionCustodian, SubscriptionPaths, TestResult, Upstream, fixture_reply, new_instance,
    seed, upstream, upstream_with,
};

#[derive(Clone, Copy)]
enum FactChange {
    Binding,
    Selection,
    Generation,
    Consent(Consent),
    #[cfg(feature = "grok")]
    Status(CustodyStatus),
}

struct CatalogCustodian {
    local: SubscriptionCustodian,
    change: Arc<Mutex<Option<FactChange>>>,
}

impl CredentialCustodian for CatalogCustodian {
    fn facts<'a>(&'a self, slot: &'a CustodySlot) -> CustodyFuture<'a, AccountFacts> {
        Box::pin(async move {
            let mut facts = self.local.facts(slot).await?;
            match *self.change.lock().unwrap_or_else(PoisonError::into_inner) {
                Some(FactChange::Binding) => {
                    facts.account_binding = Some(IdentityDigest::derive(b"fixture", &[b"account"]));
                }
                Some(FactChange::Selection) => {
                    facts.selection_epoch_digest =
                        Some(IdentityDigest::derive(b"fixture", &[b"login"]));
                }
                Some(FactChange::Generation) => {
                    facts.generation = CredentialGeneration::new(2).ok();
                }
                Some(FactChange::Consent(consent)) => {
                    if let Some(profile) = &mut facts.profile {
                        profile.consent = consent;
                    }
                }
                #[cfg(feature = "grok")]
                Some(FactChange::Status(status)) => facts.status = status,
                None => {}
            }
            Ok(facts)
        })
    }

    fn refresh<'a>(
        &'a self,
        slot: &'a CustodySlot,
        request: RefreshRequest,
    ) -> CustodyFuture<'a, AccountFacts> {
        self.local.refresh(slot, request)
    }

    fn fields<'a>(
        &'a self,
        slot: &'a CustodySlot,
        expected: &'a ExpectedFence,
    ) -> CustodyFuture<'a, CredentialFields> {
        self.local.fields(slot, expected)
    }
}

fn host(
    root: &Path,
    provider: NativeProvider,
    upstream: &Upstream,
    change: Arc<Mutex<Option<FactChange>>>,
) -> TestResult<NativeSubscriptions> {
    let paths = SubscriptionPaths::from_xdg_state_home(root.join("gateway"))?;
    let (local, kind, origin, pins) = match provider {
        NativeProvider::Codex => {
            seed(&paths, crate::subscriptions::unix_now()?)?;
            (
                NativeSubscriptions::new(paths),
                "codex",
                "https://chatgpt.com",
                CODEX_UPSTREAM_PINS,
            )
        }
        #[cfg(feature = "grok")]
        NativeProvider::Xai => {
            use crate::gateway::pins::xai::SUPERGROK_UPSTREAM_PINS;
            crate::subscriptions::state::tests::child_catalog::seed_grok(&paths, "fixture-grok")?;
            (
                super::at_review(&paths, provider)?,
                "grok",
                "https://cli-chat-proxy.grok.com",
                SUPERGROK_UPSTREAM_PINS,
            )
        }
        _ => return Err("unexpected fixture provider".into()),
    };
    let custodian = CatalogCustodian {
        local: SubscriptionCustodian::new(local, provider),
        change,
    };
    let custody = LocalCustody::new(new_instance().ok_or("gateway instance")?).with_kind(
        kind,
        Arc::new(custodian),
        pins,
    );
    let server = InProcessGateway::server(custody, upstream.client.clone())
        .with_dial_override(origin, upstream.origin.clone());
    let gateway =
        GatewayCustody::connect(
            InProcessGateway::endpoint_for(&server),
            None,
            |kind| match kind {
                "codex" => Some(CODEX_CREDENTIAL_HEADERS),
                #[cfg(feature = "grok")]
                "grok" => Some(crate::gateway::pins::xai::SUPERGROK_CREDENTIAL_HEADERS),
                _ => None,
            },
        )?;
    let subscriptions = super::at_review(
        &SubscriptionPaths::from_xdg_state_home(root.join("host-state"))?,
        provider,
    )?;
    Ok(subscriptions
        .with_custody(&gateway.custody())
        .with_gateway_catalog_cache(&root.join("host")))
}

#[tokio::test]
async fn gateway_catalog_cache_rejects_foreign_bindings_bad_consent_and_bad_files() -> TestResult {
    use NativeSubscriptionError::{AcceptanceRequired, ModelUnavailable, ProfileInactive};

    let root = tempfile::tempdir()?;
    let upstream = upstream().await?;
    let change = Arc::new(Mutex::new(None));
    let provider = NativeProvider::Codex;
    let host = host(root.path(), provider, &upstream, Arc::clone(&change))?;
    host.model_catalog(provider).await?;
    host.model_catalog_persisted(provider).await?;
    for (facts, error) in [
        (FactChange::Binding, ModelUnavailable),
        (FactChange::Consent(Consent::Required), AcceptanceRequired),
        (FactChange::Consent(Consent::Inactive), ProfileInactive),
    ] {
        *change.lock().unwrap_or_else(PoisonError::into_inner) = Some(facts);
        assert_eq!(
            host.model_catalog_persisted(provider).await.err(),
            Some(error)
        );
    }
    *change.lock().unwrap_or_else(PoisonError::into_inner) = None;
    let snapshot = std::fs::read_dir(root.path().join("host/cache/subscription-catalogs"))?
        .next()
        .transpose()?
        .ok_or("no snapshot")?
        .path();
    std::fs::set_permissions(&snapshot, std::fs::Permissions::from_mode(0o644))?;
    assert_eq!(
        host.model_catalog_persisted(provider).await.err(),
        Some(ModelUnavailable)
    );
    std::fs::set_permissions(&snapshot, std::fs::Permissions::from_mode(0o600))?;
    std::fs::write(snapshot, b"not a catalog")?;
    assert_eq!(
        host.model_catalog_persisted(provider).await.err(),
        Some(ModelUnavailable)
    );
    assert_eq!(
        upstream.take().len(),
        1,
        "only the initial fetch called the provider"
    );
    Ok(())
}

async fn publish_after_fact_change(
    provider: NativeProvider,
    movement: FactChange,
    published: bool,
) -> TestResult {
    let root = tempfile::tempdir()?;
    let change = Arc::new(Mutex::new(None));
    let after_fetch = Arc::clone(&change);
    let upstream = upstream_with(Arc::new(move |request| {
        *after_fetch.lock().unwrap_or_else(PoisonError::into_inner) = Some(movement);
        let (status, body) = fixture_reply(request);
        (status, "application/json", body)
    }))
    .await?;
    let host = host(root.path(), provider, &upstream, change)?;
    let live = host.model_catalog(provider).await?;
    let expected = if provider == NativeProvider::Codex {
        MODEL
    } else {
        GROK_MODEL
    };
    assert!(
        live.models()
            .iter()
            .any(|model| model.exact_id() == expected)
    );
    assert_eq!(upstream.take().len(), 1);
    assert_eq!(
        root.path()
            .join("host/cache/subscription-catalogs")
            .exists(),
        published
    );
    if published {
        assert_eq!(
            host.model_catalog_persisted(provider).await?.models(),
            live.models()
        );
    }
    Ok(())
}

#[tokio::test]
async fn gateway_catalog_publication_tolerates_refresh_but_skips_account_or_login_change()
-> TestResult {
    let provider = NativeProvider::Codex;
    publish_after_fact_change(provider, FactChange::Generation, true).await?;
    publish_after_fact_change(provider, FactChange::Binding, false).await?;
    publish_after_fact_change(provider, FactChange::Selection, false).await
}

#[tokio::test]
#[cfg(feature = "grok")]
async fn gateway_grok_listing_remains_available_without_publishing_when_quarantined() -> TestResult
{
    publish_after_fact_change(
        NativeProvider::Xai,
        FactChange::Status(CustodyStatus::Quarantined),
        false,
    )
    .await
}
