//! Credential-free catalog facts, receipts and the authenticated catalog response.

use super::{
    AccountBinding, BTreeSet, Deserialize, MAX_CATALOG_MODELS, ModelId, NonZeroU32, Serialize,
    Timestamp, XaiNativeAuthError, is_subscription_coding_model_id, valid_token_component,
};

/// Credential-free diagnostics reported for one exact xAI catalog model.
///
/// `reported_context_length` is not a capacity certificate or output
/// reservation. For the two release-enrolled literal models it may only
/// corroborate an equal-or-lower release-owned bound; it can never expand that
/// bound. Missing, zero, and values outside the nonzero `u32` domain make the
/// row ineligible for the exact authenticated context contract.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct XaiCatalogDiagnosticFacts {
    pub(super) reported_context_length: NonZeroU32,
}

impl XaiCatalogDiagnosticFacts {
    /// Returns the diagnostic `/v1/models` `context_length` value.
    ///
    /// This value cannot authorize or expand context admission by itself.
    #[must_use]
    pub const fn reported_context_length(self) -> NonZeroU32 {
        self.reported_context_length
    }
}

/// One exact coding-model row retained from authenticated xAI discovery.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct XaiNativeCatalogEntry {
    /// Catalog selection key (`id`, or the effective model when omitted).
    pub(super) model: ModelId,
    /// Exact wire model selected by pinned field precedence.
    pub(super) effective_model: ModelId,
    pub(super) diagnostics: XaiCatalogDiagnosticFacts,
}

impl XaiNativeCatalogEntry {
    /// Returns the exact, unaliased catalog model identity.
    #[must_use]
    pub const fn model(&self) -> &ModelId {
        &self.model
    }

    /// Returns the exact effective model selected by the pinned proxy grammar.
    #[must_use]
    pub const fn effective_model(&self) -> &ModelId {
        &self.effective_model
    }

    /// Returns non-authoritative diagnostics reported for this model row.
    #[must_use]
    pub const fn diagnostics(&self) -> XaiCatalogDiagnosticFacts {
        self.diagnostics
    }
}

/// Credential-free authenticated model-catalog admission receipt.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct XaiNativeCatalogReceipt {
    pub(super) entries: Vec<XaiNativeCatalogEntry>,
    pub(super) account: AccountBinding,
    pub(super) observed_at: Timestamp,
    pub(super) expires_at: Timestamp,
}

impl XaiNativeCatalogReceipt {
    /// Returns every exact coding-model row admitted from authenticated discovery.
    #[must_use]
    pub fn entries(&self) -> &[XaiNativeCatalogEntry] {
        &self.entries
    }

    /// Returns every exact coding model admitted from the authenticated catalog.
    ///
    /// This is a derived compatibility projection. [`Self::entries`] retains
    /// non-authoritative catalog diagnostics alongside each exact reviewed ID.
    #[must_use]
    pub fn models(&self) -> Vec<ModelId> {
        self.model_ids()
    }

    /// Returns the retained row for one exact model identity.
    #[must_use]
    pub fn entry(&self, model: &ModelId) -> Option<&XaiNativeCatalogEntry> {
        self.entries.iter().find(|entry| entry.model() == model)
    }

    /// Returns whether this receipt admits the exact model identity.
    #[must_use]
    pub fn admits(&self, model: &ModelId) -> bool {
        self.entry(model).is_some()
    }

    /// Returns the redacted account binding used for authenticated discovery.
    #[must_use]
    pub const fn account_binding(&self) -> &AccountBinding {
        &self.account
    }

    /// Returns when this exact account-scoped catalog was observed.
    #[must_use]
    pub const fn observed_at(&self) -> Timestamp {
        self.observed_at
    }

    /// Returns the bounded receipt expiry.
    #[must_use]
    pub const fn expires_at(&self) -> Timestamp {
        self.expires_at
    }

    pub(super) fn model_ids(&self) -> Vec<ModelId> {
        self.entries
            .iter()
            .map(|entry| entry.model.clone())
            .collect()
    }
}

#[derive(Deserialize)]
pub(super) struct CatalogResponse {
    pub(super) data: Vec<serde_json::Value>,
}

impl CatalogResponse {
    pub(super) fn admitted_coding_entries(
        &self,
    ) -> Result<Vec<XaiNativeCatalogEntry>, XaiNativeAuthError> {
        if self.data.is_empty() || self.data.len() > MAX_CATALOG_MODELS {
            return Err(XaiNativeAuthError::CatalogUnavailable);
        }
        let mut requested_identities = BTreeSet::new();
        let mut effective_identities = BTreeSet::new();
        let mut admitted = Vec::new();
        for value in &self.data {
            let Some(model) = ProxyCatalogModel::decode(value) else {
                continue;
            };
            if !requested_identities.insert(model.requested.clone())
                || !effective_identities.insert(model.effective.clone())
            {
                return Err(XaiNativeAuthError::CatalogUnavailable);
            }
            // The catalog grammar may express aliases through a distinct `id`,
            // but aliases never enter the closed literal enrollment.
            if is_subscription_coding_model_id(&model.requested)
                && model.requested == model.effective
            {
                let reported_context_length = model
                    .context_window
                    .and_then(|value| u32::try_from(value).ok())
                    .and_then(NonZeroU32::new);
                let Some(reported_context_length) = reported_context_length else {
                    continue;
                };
                admitted.push(XaiNativeCatalogEntry {
                    model: ModelId::new(model.requested.as_str())?,
                    effective_model: ModelId::new(model.effective.as_str())?,
                    diagnostics: XaiCatalogDiagnosticFacts {
                        reported_context_length,
                    },
                });
            }
        }
        // Catalog presence confirms availability but never expands the closed
        // reviewed model table or turns reported context length into capacity.
        if admitted.is_empty() {
            return Err(XaiNativeAuthError::CatalogUnavailable);
        }
        admitted.sort_by(|left, right| left.model.as_str().cmp(right.model.as_str()));
        Ok(admitted)
    }
}

pub(super) fn valid_admitted_catalog_entries(entries: &[XaiNativeCatalogEntry]) -> bool {
    !entries.is_empty()
        && entries.iter().all(|entry| {
            is_subscription_coding_model_id(entry.model.as_str())
                && entry.model == entry.effective_model
        })
        && entries
            .windows(2)
            .all(|pair| pair[0].model.as_str() < pair[1].model.as_str())
}

pub(super) struct ProxyCatalogModel {
    pub(super) requested: String,
    pub(super) effective: String,
    pub(super) context_window: Option<u64>,
}

impl ProxyCatalogModel {
    pub(super) fn decode(value: &serde_json::Value) -> Option<Self> {
        let object = value.as_object()?;
        let meta = object.get("_meta").and_then(serde_json::Value::as_object);
        let id = proxy_string(object, "id");
        let effective = proxy_string(object, "model")
            .or_else(|| proxy_string(object, "modelId"))
            .or_else(|| id.clone())
            .or_else(|| meta.and_then(|meta| proxy_string(meta, "model")))
            .or_else(|| meta.and_then(|meta| proxy_string(meta, "modelId")))?;
        let requested = id.unwrap_or_else(|| effective.clone());
        if !valid_token_component(&requested, 128) || !valid_token_component(&effective, 128) {
            return None;
        }
        let context_window = proxy_u64(object, "contextWindow")
            .or_else(|| proxy_u64(object, "context_window"))
            .or_else(|| meta.and_then(|meta| proxy_u64(meta, "contextWindow")))
            .or_else(|| meta.and_then(|meta| proxy_u64(meta, "totalContextTokens")));
        Some(Self {
            requested,
            effective,
            context_window,
        })
    }
}

pub(super) fn proxy_string(
    object: &serde_json::Map<String, serde_json::Value>,
    key: &str,
) -> Option<String> {
    object.get(key)?.as_str().map(ToOwned::to_owned)
}

pub(super) fn proxy_u64(
    object: &serde_json::Map<String, serde_json::Value>,
    key: &str,
) -> Option<u64> {
    object.get(key)?.as_u64()
}
