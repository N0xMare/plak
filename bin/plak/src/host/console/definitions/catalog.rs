//! Cached or bundled catalogs only. No catalog refresh or credential repair.

use super::super::{ReadRefusal, accounts};
use crate::presentation::{CliStatus, CompatibilityState, CredentialState};
use crate::product_config::ProductCompositionSelection;
use plakd_wire_types::console::{CatalogPage, CatalogSource, Diagnostic};

pub(super) async fn page(
    console: &super::super::Console,
    selected: Option<&str>,
    composition: ProductCompositionSelection,
) -> Result<CatalogPage, ReadRefusal> {
    let mut page = CatalogPage {
        account_id: selected.map(str::to_owned),
        source: CatalogSource::Unavailable,
        observed_at: None,
        rows: Vec::new(),
        diagnostics: Vec::new(),
        page: super::empty_page(),
    };
    let Some(selected) = selected else {
        return Ok(page);
    };
    let account = accounts::exact(selected)?;
    let capability = account.vertical().capability();
    if !capability.compiled {
        return Ok(missing(page));
    }
    if let Some(provider) = capability.platform_provider() {
        page.source = CatalogSource::Bundled;
        page.rows = crate::platform_catalog::entries(provider, composition);
    } else if let Some(provider) = capability.native_provider() {
        let reader = accounts::reader(&console.state)?;
        let subscriptions = reader.subscriptions_for(&account)?;
        let before = subscriptions.account_observation_revision(provider).await;
        // Missing namespaces stay missing: the persisted-catalog lease is
        // requested only after an offline Ready observation of this account.
        let status =
            crate::commands::support::offline_subscription_status(&subscriptions, provider).await;
        if !matches!(
            status,
            Ok(CliStatus {
                compatibility: CompatibilityState::Current,
                credential: CredentialState::Ready
            })
        ) {
            return Ok(missing(page));
        }
        let Ok(catalog) = subscriptions.model_catalog_persisted(provider).await else {
            return Ok(missing(page));
        };
        let Ok(models) = crate::commands::application::catalog_summaries(provider, &catalog) else {
            return Ok(missing(page));
        };
        let after = subscriptions.account_observation_revision(provider).await;
        if !matches!((before, after), (Ok(Some(before)), Ok(Some(after))) if before == after) {
            return Ok(missing(page));
        }
        page.rows = crate::presentation::native_model_entries(provider, models, composition);
        if let Some(recommended) =
            crate::presentation::product_recommended_model(capability.account)
        {
            page.rows.sort_by_key(|row| row.id != recommended);
        }
        page.source = CatalogSource::Cached;
        // The persisted facade does not expose its capture time. Unknown is
        // explicit rather than replacing it with the console read time.
    } else {
        return Ok(missing(page));
    }
    if page.rows.len() > super::super::limits::SCAN_ENTRIES {
        page.rows.truncate(super::super::limits::SCAN_ENTRIES);
        page.page.truncated = true;
    }
    Ok(page)
}

fn missing(mut page: CatalogPage) -> CatalogPage {
    page.diagnostics.push(Diagnostic {
        code: "catalog_unavailable".into(),
        subject: page.account_id.clone(),
        detail:
            "no usable persisted or bundled catalog is available; this read does not refresh it"
                .into(),
    });
    page
}
