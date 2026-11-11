//! Etag-checked publication of one allowlisted home file, never an epoch edit.

mod edit;
#[cfg(test)]
mod tests;

use super::{Console, HostRequestContext, definitions, refused, validation};
use crate::product_config::{ConfigSaveError, console::DefinitionFile};
use plakd_wire_types::console::{
    HostRequest, HostResponse, OperatorAction, ReadRefusal, ReadScope, SavedField, SettingApplies,
    ValidationCode,
};
use plakous_host_protocol::wire::RequestRefusal;
use std::collections::BTreeSet;
use std::sync::Arc;

pub(super) fn action(request: &HostRequest) -> Option<OperatorAction> {
    Some(match request {
        HostRequest::ConfigPatch {
            changes, if_match, ..
        } => OperatorAction::ConfigPatch {
            if_match: if_match.clone(),
            keys: changes.iter().map(|change| change.key.clone()).collect(),
        },
        HostRequest::TemplatePut { id, if_match, .. } => OperatorAction::TemplatePut {
            id: id.clone(),
            if_match: if_match.clone(),
        },
        HostRequest::ProfilePut { id, if_match, .. } => OperatorAction::ProfilePut {
            id: id.clone(),
            if_match: if_match.clone(),
        },
        _ => return None,
    })
}

impl Console {
    pub(super) async fn config_write(
        self: Arc<Self>,
        context: HostRequestContext,
        request: HostRequest,
    ) -> Result<HostResponse, RequestRefusal> {
        let Some(action) = action(&request) else {
            return Ok(refused(invalid("request")));
        };
        self.write_capability(&context.principal, &action)?;
        let (HostRequest::ConfigPatch { confirmation, .. }
        | HostRequest::TemplatePut { confirmation, .. }
        | HostRequest::ProfilePut { confirmation, .. }) = &request
        else {
            unreachable!()
        };
        let permit = match self.mutation(&context, &action, confirmation) {
            Ok(permit) => permit,
            Err(reason) => return reason.outcome(),
        };
        Ok(tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let result = crate::product_config::console::with_write_lock(&self.paths, || {
                self.publish_config(&request)
            })
            .unwrap_or_else(|_| {
                Err(ReadRefusal::Busy {
                    scope: ReadScope::Config,
                    retry_after_ms: 1_000,
                })
            });
            if let Ok(HostResponse::Saved {
                etag,
                published_durability_uncertain,
                ..
            }) = &result
            {
                let (before, target) = match &action {
                    OperatorAction::ConfigPatch { if_match, .. } => (if_match, "home"),
                    OperatorAction::TemplatePut { if_match, id }
                    | OperatorAction::ProfilePut { if_match, id } => (if_match, id.as_str()),
                    _ => unreachable!(),
                };
                self.audit(
                    &context,
                    "config_published",
                    Some(target),
                    if *published_durability_uncertain {
                        "published_durability_uncertain"
                    } else {
                        "success"
                    },
                    Some((before, etag)),
                );
            }
            result.unwrap_or_else(refused)
        })
        .await
        .unwrap_or_else(|_| HostResponse::OperationOutcomeUnknown {}))
    }

    fn publish_config(&self, request: &HostRequest) -> Result<HostResponse, ReadRefusal> {
        self.publish_config_with_hook(request, || {})
    }

    fn publish_config_with_hook(
        &self,
        request: &HostRequest,
        before_recheck: impl FnOnce(),
    ) -> Result<HostResponse, ReadRefusal> {
        let before = definitions::Snapshot::read(&self.paths, &self.definition_nonce)?;
        let (HostRequest::ConfigPatch {
            if_match: expected, ..
        }
        | HostRequest::TemplatePut {
            if_match: expected, ..
        }
        | HostRequest::ProfilePut {
            if_match: expected, ..
        }) = request
        else {
            return Err(invalid("request"));
        };
        if &before.etag != expected {
            return Err(ReadRefusal::Conflict { etag: before.etag });
        }
        let mut files = before.files.clone();
        let (destination, bytes, create_new, fields) = candidate(request, &before, &mut files)?;
        let candidate =
            definitions::Snapshot::from_files(files, &self.definition_nonce).map_err(|reason| {
                match reason {
                    ReadRefusal::Unavailable {
                        cause: plakd_wire_types::console::UnavailableCode::Configuration,
                        ..
                    } => invalid("changes"),
                    other => other,
                }
            })?;
        validate_profiles(&before, &candidate)?;
        if let HostRequest::ConfigPatch { changes, .. } = request {
            if changes
                .iter()
                .any(|change| crate::config_key_changes_orchestration(&change.key))
                && crate::topology_product::roster_apply_refusal(&self.paths, None).is_some()
            {
                return Err(ReadRefusal::Busy {
                    scope: ReadScope::Config,
                    retry_after_ms: 1_000,
                });
            }
        }
        // The lock coordinates product writers. Recheck actual bytes for
        // editors that do not cooperate; this is not filesystem CAS.
        before_recheck();
        let current = definitions::Snapshot::read(&self.paths, &self.definition_nonce)?;
        if current.etag != before.etag {
            return Err(ReadRefusal::Conflict { etag: current.etag });
        }
        let uncertain = match crate::product_config::console::publish(
            &self.paths,
            destination,
            bytes.as_bytes(),
            create_new,
        ) {
            Ok(()) => false,
            Err(ConfigSaveError::PublishedDurabilityUncertain(_)) => true,
            Err(ConfigSaveError::BeforePublication(_)) => {
                return Err(super::unavailable(
                    ReadScope::Config,
                    plakd_wire_types::console::UnavailableCode::Storage,
                ));
            }
        };
        let after = definitions::Snapshot::read(&self.paths, &self.definition_nonce);
        let (etag, published_durability_uncertain) =
            after.map_or((candidate.etag, true), |after| (after.etag, uncertain));
        Ok(HostResponse::Saved {
            etag,
            published_durability_uncertain,
            fields,
        })
    }
}

type Publication<'a> = (DefinitionFile<'a>, String, bool, Vec<SavedField>);

fn candidate<'a>(
    request: &'a HostRequest,
    before: &definitions::Snapshot,
    files: &mut definitions::files::Files,
) -> Result<Publication<'a>, ReadRefusal> {
    match request {
        HostRequest::ConfigPatch { changes, .. } => {
            let mut seen = BTreeSet::new();
            if changes.is_empty()
                || changes.len() > 64
                || changes.iter().any(|change| {
                    !definitions::editable_key(&change.key) || !seen.insert(&change.key)
                })
            {
                return Err(invalid("changes"));
            }
            let mut config = before.config.clone();
            config
                .apply_console_changes(changes)
                .map_err(|_| invalid("changes"))?;
            let bytes =
                crate::product_config::console::edit_config(files.config.as_deref(), &config)
                    .map_err(|_| invalid("changes"))?;
            if files.config.is_none() {
                files.entries += 1;
            }
            files.config = Some(bytes.clone());
            let mut fields: Vec<_> = changes
                .iter()
                .map(|change| SavedField {
                    key: change.key.clone(),
                    applies: definitions::applicability(&change.key),
                })
                .collect();
            if config.root_composition != before.config.root_composition
                && !changes
                    .iter()
                    .any(|change| change.key == "root.composition")
            {
                fields.push(SavedField {
                    key: "root.composition".into(),
                    applies: SettingApplies::NewSession,
                });
            }
            Ok((DefinitionFile::Config, bytes, false, fields))
        }
        HostRequest::TemplatePut { id, changes, .. } => {
            valid_id(id)?;
            let existing = files.templates.get(id);
            let create_new = existing.is_none();
            let bytes = edit::template(existing.map(String::as_str), changes)?;
            if create_new {
                files.entries += 1;
            }
            files.templates.insert(id.clone(), bytes.clone());
            Ok((
                DefinitionFile::Template(id),
                bytes,
                create_new,
                vec![SavedField {
                    key: format!("template:{id}"),
                    applies: SettingApplies::NewSession,
                }],
            ))
        }
        HostRequest::ProfilePut { id, body, .. } => {
            valid_id(id)?;
            if crate::profiles::console::builtin_id(id) {
                return Err(validation("id", ValidationCode::ReadOnly));
            }
            let existing = files.profiles.get(id);
            let create_new = existing.is_none();
            let bytes = edit::profile(existing.map(String::as_str), body)?;
            if create_new {
                files.entries += 1;
            }
            files.profiles.insert(id.clone(), bytes.clone());
            Ok((
                DefinitionFile::Profile(id),
                bytes,
                create_new,
                vec![SavedField {
                    key: format!("profile:{id}"),
                    applies: SettingApplies::NewSession,
                }],
            ))
        }
        _ => Err(invalid("request")),
    }
}

fn validate_profiles(
    before: &definitions::Snapshot,
    candidate: &definitions::Snapshot,
) -> Result<(), ReadRefusal> {
    for (id, text) in &candidate.files.profiles {
        if crate::profiles::console::builtin_id(id) {
            continue;
        }
        let previously_valid = before.files.profiles.get(id).is_some_and(|text| {
            crate::profiles::console::validate_user(&before.config, id, text).is_ok()
        });
        let changed = before.files.profiles.get(id) != Some(text);
        if (changed || previously_valid)
            && crate::profiles::console::validate_user(&candidate.config, id, text).is_err()
        {
            return Err(invalid("references"));
        }
    }
    Ok(())
}

fn valid_id(id: &str) -> Result<(), ReadRefusal> {
    crate::product_config::validate_template_name(id)
        .map_err(|_| validation("id", ValidationCode::InvalidId))
}
fn invalid(field: &str) -> ReadRefusal {
    validation(field, ValidationCode::InvalidValue)
}
