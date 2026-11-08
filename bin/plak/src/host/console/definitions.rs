//! Stable home-only definitions. Parsing consumes retained, bounded bytes.

mod catalog;
mod files;
mod settings;
#[cfg(test)]
mod tests;

use super::{ReadRefusal, ReadScope, UnavailableCode, unavailable, validation};
use crate::product_config::{AgentTemplate, ProductConfig};
use plakd_wire_types::console::{
    Diagnostic, HomeScope, HostRequest, HostResponse, Page, ProfileBody, ProfileDefinition,
    ProfileKind, ProfileSummary, ProtectedEnvironment, ProtectedText, TemplateBody,
    TemplateDefinition, TemplateSource, TemplateSummary, ValidationCode,
};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fmt::Write as _;

pub(super) struct Snapshot {
    pub config: ProductConfig,
    pub profiles: crate::profiles::console::ReadCatalog,
    pub templates: BTreeMap<String, (TemplateSource, AgentTemplate)>,
    pub etag: String,
    pub diagnostics: Vec<Diagnostic>,
}

impl Snapshot {
    pub fn read(
        paths: &crate::home::paths::PlakPaths,
        nonce: &[u8; 16],
    ) -> Result<Self, ReadRefusal> {
        let first = files::Files::read(paths)?;
        let config = ProductConfig::from_home_snapshot(first.config.as_deref(), &first.templates)
            .map_err(|_| unavailable(ReadScope::Config, UnavailableCode::Configuration))?;
        let mut remaining = super::limits::DEPENDENCY_BYTES;
        let profiles = crate::profiles::console::catalog(&config, &first.profiles, |row| {
            let Some(bytes) = super::wire_size::size(row, remaining) else {
                return false;
            };
            remaining -= bytes;
            true
        })
        .map_err(|()| unavailable(ReadScope::Profiles, UnavailableCode::DependencyLimit))?;
        let mut templates: BTreeMap<_, _> = profiles
            .builtin_templates
            .iter()
            .map(|(id, template)| (id.clone(), (TemplateSource::Builtin, template.clone())))
            .collect();
        // Home definitions win only by their own exact logical names.
        for (id, template) in &config.templates {
            templates.insert(id.clone(), (TemplateSource::Home, template.clone()));
        }
        if first != files::Files::read(paths)? {
            return Err(ReadRefusal::Busy {
                scope: ReadScope::Config,
                retry_after_ms: 50,
            });
        }
        let mut hash = Sha256::new();
        hash.update(nonce);
        hash.update(
            crate::build_identity::known_binary_id()
                .as_deref()
                .unwrap_or("unidentified-build")
                .as_bytes(),
        );
        first.hash_into(&mut hash);
        crate::profiles::console::hash_builtins(&mut hash);
        let mut etag = String::with_capacity(64);
        for byte in hash.finalize() {
            let _ = write!(etag, "{byte:02x}");
        }
        let diagnostics = profiles
            .unavailable
            .iter()
            .take(32)
            .map(|id| Diagnostic {
                code: "profile_unavailable".into(),
                subject: Some(id.clone()),
                detail: "the profile could not be resolved from home definitions".into(),
            })
            .collect();
        Ok(Self {
            config,
            profiles,
            templates,
            etag,
            diagnostics,
        })
    }

    pub fn response(&self, request: &HostRequest) -> Result<HostResponse, ReadRefusal> {
        match request {
            HostRequest::ProfilesList { .. } => Ok(HostResponse::ProfilesList {
                etag: self.etag.clone(),
                current: self.profiles.current.clone(),
                rows: self
                    .profiles
                    .profiles
                    .iter()
                    .map(|profile| ProfileSummary {
                        id: profile.row.id.clone(),
                        kind: profile_kind(profile.builtin),
                        summary: profile.row.clone(),
                    })
                    .collect(),
                diagnostics: self.diagnostics.clone(),
                page: empty_page(),
            }),
            HostRequest::ProfilesGet { id } => {
                validate_id(id)?;
                let profile = self
                    .profiles
                    .profiles
                    .iter()
                    .find(|profile| &profile.row.id == id)
                    .ok_or_else(|| validation("id", ValidationCode::UnknownId))?;
                Ok(HostResponse::ProfilesGet {
                    profile: ProfileDefinition {
                        id: id.clone(),
                        kind: profile_kind(profile.builtin),
                        etag: self.etag.clone(),
                        body: ProfileBody {
                            root: profile.root.clone(),
                            working_set: profile.working_set.clone(),
                        },
                        summary: profile.row.clone(),
                        diagnostics: Vec::new(),
                    },
                })
            }
            HostRequest::TemplatesList { .. } => Ok(HostResponse::TemplatesList {
                etag: self.etag.clone(),
                rows: self
                    .templates
                    .iter()
                    .map(|(id, (source, template))| TemplateSummary {
                        id: id.clone(),
                        source: *source,
                        summary: template_summary(id, template),
                    })
                    .collect(),
                diagnostics: Vec::new(),
                page: empty_page(),
            }),
            HostRequest::TemplatesGet { id } => {
                validate_id(id)?;
                let (source, template) = self
                    .templates
                    .get(id)
                    .ok_or_else(|| validation("id", ValidationCode::UnknownId))?;
                Ok(HostResponse::TemplatesGet {
                    template: Box::new(TemplateDefinition {
                        id: id.clone(),
                        source: *source,
                        etag: self.etag.clone(),
                        body: template_body(template),
                        diagnostics: Vec::new(),
                    }),
                })
            }
            HostRequest::ConfigGet {} => Ok(HostResponse::ConfigGet {
                etag: self.etag.clone(),
                scope: HomeScope::Home,
                fields: settings::fields(&self.config),
                diagnostics: Vec::new(),
            }),
            _ => Err(unavailable(
                ReadScope::Config,
                UnavailableCode::Configuration,
            )),
        }
    }
}

pub(super) fn validate_id(id: &str) -> Result<(), ReadRefusal> {
    if id.is_empty()
        || id.len() > 128
        || id.contains(['/', '\\', '\0'])
        || id.chars().any(char::is_whitespace)
    {
        return Err(validation("id", ValidationCode::InvalidId));
    }
    Ok(())
}
pub(super) fn empty_page() -> Page {
    Page {
        next: None,
        truncated: false,
    }
}
fn profile_kind(builtin: bool) -> ProfileKind {
    if builtin {
        ProfileKind::Builtin
    } else {
        ProfileKind::User
    }
}

fn template_summary(
    id: &str,
    template: &AgentTemplate,
) -> plakous_host_protocol::views::TemplateRow {
    plakous_host_protocol::views::TemplateRow {
        name: id.into(),
        account: template
            .account
            .parse::<plakous_hosted::account::AccountRef>()
            .map_or_else(|_| template.account.clone(), |account| account.to_string()),
        model: template.model.clone(),
        effort: template.effort.clone(),
        composition: template.composition_or_default().as_str().into(),
        tier: match template.processing_tier() {
            plakous::model::ProcessingTierDisposition::Standard => "standard".into(),
            plakous::model::ProcessingTierDisposition::ProviderNamed(tier) => tier.as_str().into(),
        },
        unready_reason: Some("readiness is not evaluated by this definition read".into()),
    }
}

fn template_body(template: &AgentTemplate) -> TemplateBody {
    TemplateBody {
        account: template
            .account
            .parse::<plakous_hosted::account::AccountRef>()
            .map_or_else(|_| template.account.clone(), |account| account.to_string()),
        model: template.model.clone(),
        effort: template.effort.clone(),
        extra: template.extra.clone(),
        composition: template.composition.map(Into::into),
        protocol: template.protocol.map(Into::into),
        description: template.description.clone(),
        max_tier: template.max_tier,
        tools: template
            .tools
            .entries()
            .iter()
            .map(ToString::to_string)
            .collect(),
        spawns: template.spawns.iter().map(ToString::to_string).collect(),
        may_adopt: template.may_adopt,
        delegation: template.delegation,
        cost_tier: template.cost_tier,
        max_turns: template.max_turns,
        show_model: template.show_model,
        on_mint: if template.on_mint.is_some() {
            ProtectedText::Withheld {}
        } else {
            ProtectedText::Absent {}
        },
        env: template
            .env
            .keys()
            .map(|name| ProtectedEnvironment {
                name: name.clone(),
                value: ProtectedText::Withheld {},
            })
            .collect(),
    }
}

impl super::Console {
    pub(super) async fn read_definitions(
        &self,
        context: super::HostRequestContext,
        request: HostRequest,
    ) -> Result<HostResponse, ReadRefusal> {
        match &request {
            HostRequest::ProfilesGet { id } | HostRequest::TemplatesGet { id } => validate_id(id)?,
            HostRequest::ConsoleMetadataGet {
                account_id: Some(account),
                ..
            } => {
                super::accounts::exact(account)?;
            }
            _ => {}
        }
        let (cursor, limit) = match &request {
            HostRequest::ProfilesList { cursor, limit }
            | HostRequest::TemplatesList { cursor, limit }
            | HostRequest::ConsoleMetadataGet { cursor, limit, .. } => {
                (cursor.as_deref(), limit.unwrap_or(super::limits::PAGE_ROWS))
            }
            _ => (None, super::limits::PAGE_ROWS),
        };
        if !(1..=super::limits::PAGE_ROWS).contains(&limit) {
            return Err(validation("limit", ValidationCode::InvalidLimit));
        }
        let paths = self.paths.clone();
        let nonce = self.definition_nonce;
        let snapshot = tokio::task::spawn_blocking(move || Snapshot::read(&paths, &nonce))
            .await
            .map_err(|_| unavailable(ReadScope::Config, UnavailableCode::Storage))??;
        let response = if let HostRequest::ConsoleMetadataGet { account_id, .. } = &request {
            let catalog = catalog::page(
                self,
                account_id.as_deref(),
                snapshot.config.root_composition.unwrap_or_default(),
            )
            .await?;
            HostResponse::ConsoleMetadata {
                etag: snapshot.etag.clone(),
                needs_setup: crate::product_onboarding::needs_setup(&snapshot.config),
                settings: settings::rows(&snapshot.config),
                catalog,
                diagnostics: snapshot.diagnostics.clone(),
            }
        } else {
            snapshot.response(&request)?
        };
        // Fence cached catalog changes as well as definition changes. The
        // digest stays private; public etags are generation-salted above.
        if super::wire_size::size(&response, super::limits::DEPENDENCY_BYTES).is_none() {
            return Err(unavailable(
                ReadScope::Config,
                UnavailableCode::DependencyLimit,
            ));
        }
        let bytes = serde_json::to_vec(&response)
            .map_err(|_| unavailable(ReadScope::Config, UnavailableCode::Configuration))?;
        let revision: [u8; 32] = Sha256::digest(&bytes).into();
        let paths = self.paths.clone();
        let after = tokio::task::spawn_blocking(move || Snapshot::read(&paths, &nonce))
            .await
            .map_err(|_| unavailable(ReadScope::Config, UnavailableCode::Storage))??;
        if after.etag != snapshot.etag {
            return Err(ReadRefusal::Busy {
                scope: ReadScope::Config,
                retry_after_ms: 50,
            });
        }
        let selection = super::pages::Selection::from_request(&request);
        let mut pages = self
            .pages
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let limit = usize::try_from(limit).unwrap_or(100);
        if let Some(cursor) = cursor {
            pages.resume(cursor, &context, &selection, &revision, limit)
        } else {
            pages.start(response, context, selection, revision, limit)
        }
    }
}
