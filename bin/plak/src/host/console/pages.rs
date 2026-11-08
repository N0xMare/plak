use super::limits;
use super::{HostRequestContext, ReadRefusal, accounts, unavailable, validation};
use crate::home::paths::PlakPaths;
use plakd_wire_types::console::{
    HostRequest, HostResponse, ReadScope, UnavailableCode, UsageWindow, ValidationCode,
};
use plakous_hosted::host_service::wire::HostPrincipal;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Instant;

#[derive(Clone, Eq, PartialEq)]
pub(super) struct Selection {
    pub account_id: Option<String>,
    window: Option<UsageWindow>,
    kind: RequestKind,
}
#[derive(Clone, Eq, PartialEq)]
enum RequestKind {
    Status,
    Usage,
    Profiles,
    Templates,
    Metadata,
    Single,
}
impl Selection {
    pub fn from_request(request: &HostRequest) -> Self {
        match request {
            HostRequest::AccountsStatus { account_id, .. } => Self {
                account_id: account_id.clone(),
                window: None,
                kind: RequestKind::Status,
            },
            HostRequest::AccountsUsage {
                account_id, window, ..
            } => Self {
                account_id: account_id.clone(),
                window: Some(*window),
                kind: RequestKind::Usage,
            },
            HostRequest::ProfilesList { .. } => Self {
                account_id: None,
                window: None,
                kind: RequestKind::Profiles,
            },
            HostRequest::TemplatesList { .. } => Self {
                account_id: None,
                window: None,
                kind: RequestKind::Templates,
            },
            HostRequest::ConsoleMetadataGet { account_id, .. } => Self {
                account_id: account_id.clone(),
                window: None,
                kind: RequestKind::Metadata,
            },
            _ => Self {
                account_id: None,
                window: None,
                kind: RequestKind::Single,
            },
        }
    }
}

struct Snapshot {
    response: HostResponse,
    principal: HostPrincipal,
    generation: u64,
    selection: Selection,
    revision: [u8; 32],
    at: Instant,
    bytes: usize,
}
struct Cursor {
    snapshot: Arc<Snapshot>,
    offset: usize,
}
#[derive(Default)]
pub(super) struct Pages {
    cursors: BTreeMap<String, Cursor>,
}

impl Pages {
    pub fn start(
        &mut self,
        response: HostResponse,
        context: HostRequestContext,
        selection: Selection,
        revision: [u8; 32],
        limit: usize,
    ) -> Result<HostResponse, ReadRefusal> {
        let bytes = super::wire_size::size(&response, limits::DEPENDENCY_BYTES)
            .ok_or_else(|| unavailable(ReadScope::Accounts, UnavailableCode::DependencyLimit))?;
        let snapshot = Arc::new(Snapshot {
            response,
            principal: context.principal,
            generation: context.host_generation,
            selection,
            revision,
            at: Instant::now(),
            bytes,
        });
        self.page(snapshot, 0, limit)
    }
    pub fn resume(
        &mut self,
        token: &str,
        context: &HostRequestContext,
        selection: &Selection,
        revision: &[u8; 32],
        limit: usize,
    ) -> Result<HostResponse, ReadRefusal> {
        if token.len() > 256 {
            return Err(validation("cursor", ValidationCode::InvalidCursor));
        }
        let cursor = self
            .cursors
            .get(token)
            .ok_or_else(|| validation("cursor", ValidationCode::InvalidCursor))?;
        if cursor.snapshot.at.elapsed() >= limits::CURSOR_TTL {
            return Err(validation("cursor", ValidationCode::CursorExpired));
        }
        if cursor.snapshot.principal != context.principal
            || cursor.snapshot.generation != context.host_generation
            || &cursor.snapshot.selection != selection
            || &cursor.snapshot.revision != revision
        {
            return Err(validation("cursor", ValidationCode::InvalidCursor));
        }
        self.page(Arc::clone(&cursor.snapshot), cursor.offset, limit)
    }
    fn page(
        &mut self,
        snapshot: Arc<Snapshot>,
        offset: usize,
        limit: usize,
    ) -> Result<HostResponse, ReadRefusal> {
        self.cursors
            .retain(|_, cursor| cursor.snapshot.at.elapsed() < limits::CURSOR_TTL);
        let mut response = snapshot.response.clone();
        let (length, page) = match &mut response {
            HostResponse::AccountsStatus { rows, page, .. } => {
                let length = rows.len();
                *rows = rows.iter().skip(offset).take(limit).cloned().collect();
                (length, page)
            }
            HostResponse::AccountsUsage { rows, page, .. } => {
                let length = rows.len();
                *rows = rows.iter().skip(offset).take(limit).cloned().collect();
                (length, page)
            }
            HostResponse::ProfilesList { rows, page, .. } => {
                let length = rows.len();
                *rows = rows.iter().skip(offset).take(limit).cloned().collect();
                (length, page)
            }
            HostResponse::TemplatesList { rows, page, .. } => {
                let length = rows.len();
                *rows = rows.iter().skip(offset).take(limit).cloned().collect();
                (length, page)
            }
            HostResponse::ConsoleMetadata { catalog, .. } => {
                let length = catalog.rows.len();
                catalog.rows = catalog
                    .rows
                    .iter()
                    .skip(offset)
                    .take(limit)
                    .cloned()
                    .collect();
                (length, &mut catalog.page)
            }
            _ => {
                if super::wire_size::size(&response, limits::RESPONSE_BYTES - 256).is_none() {
                    return Err(unavailable(
                        ReadScope::Config,
                        UnavailableCode::DefinitionTooLarge,
                    ));
                }
                return Ok(response);
            }
        };
        let token =
            (offset.saturating_add(limit) < length).then(|| uuid::Uuid::new_v4().to_string());
        page.next = token.clone();
        // Leave room for the generic frame, outcome, id and newline. Smaller
        // pages preserve the original snapshot rather than losing its facts.
        if super::wire_size::size(&response, limits::RESPONSE_BYTES - 256).is_none() {
            if limit > 1 {
                return self.page(snapshot, offset, limit / 2);
            }
            return Err(unavailable(
                ReadScope::Accounts,
                UnavailableCode::DependencyLimit,
            ));
        }
        if let Some(token) = token {
            if self.cursors.len() >= limits::SNAPSHOTS
                || self
                    .cursors
                    .values()
                    .map(|cursor| cursor.snapshot.bytes)
                    .sum::<usize>()
                    .saturating_add(snapshot.bytes)
                    > limits::DEPENDENCY_BYTES
            {
                return Err(ReadRefusal::Busy {
                    scope: ReadScope::Accounts,
                    retry_after_ms: 1_000,
                });
            }
            self.cursors.insert(
                token,
                Cursor {
                    snapshot,
                    offset: offset + limit,
                },
            );
        }
        Ok(response)
    }
}

pub(super) async fn source_revision(
    paths: &PlakPaths,
    state: &std::path::Path,
    selected: Option<&str>,
    include_telemetry: bool,
    deadline: Instant,
    cancelled: &std::sync::atomic::AtomicBool,
) -> Result<[u8; 32], ReadRefusal> {
    let application = accounts::reader(state)?;
    let roster = accounts::roster(&application, paths, selected).await?;
    let mut hash = Sha256::new();
    for account in roster.accounts {
        accounts::check(deadline, cancelled)?;
        hash.update((account.as_str().len() as u64).to_le_bytes());
        hash.update(account.as_str().as_bytes());
        if let Some(provider) = account.vertical().capability().native_provider() {
            if let Ok(subscriptions) = application.subscriptions_for(&account) {
                match subscriptions.account_observation_revision(provider).await {
                    Ok(Some(revision)) => hash.update(revision),
                    _ => hash.update(b"unavailable"),
                }
            }
        } else if let Ok(Some(generation)) =
            crate::home::secrets::observation::revision(paths, account.as_str())
        {
            hash.update(generation);
        }
    }
    for file in [
        paths.telemetry_database(),
        paths.telemetry_database().with_extension("sqlite3-wal"),
    ]
    .into_iter()
    .filter(|_| include_telemetry)
    {
        if let Ok(metadata) = std::fs::symlink_metadata(file) {
            hash.update(metadata.len().to_le_bytes());
            if let Ok(modified) = metadata.modified().and_then(|at| {
                at.duration_since(std::time::UNIX_EPOCH)
                    .map_err(std::io::Error::other)
            }) {
                hash.update(modified.as_nanos().to_le_bytes());
            }
        }
    }
    Ok(hash.finalize().into())
}

#[cfg(test)]
#[allow(clippy::expect_used, reason = "bounded cursor regression fixtures")]
mod tests {
    use super::*;
    use plakd_wire_types::console::{AccountUsage, Allowance, CoverageReason, Page};
    use std::time::Duration;

    #[test]
    fn encoded_byte_pressure_makes_smaller_pages_without_losing_rows() {
        use plakous_host_protocol::views::presentation::{
            UsageVertical, VerticalUsageState, VerticalUsageView,
        };
        let mut response = response();
        let HostResponse::AccountsUsage { rows, .. } = &mut response else {
            panic!("usage fixture")
        };
        let template = rows[0].clone();
        *rows = (0..120)
            .map(|index| {
                let mut row = template.clone();
                row.account_id = format!("openai/copy-{index}");
                row.allowance.view = Some(VerticalUsageView {
                    vertical: UsageVertical::new(&row.account_id, &row.account_id),
                    account_email: None,
                    plan_label: None,
                    observed_at: None,
                    state: VerticalUsageState::Unavailable {
                        reason: "x".repeat(3_000),
                    },
                    balances: Vec::new(),
                    repair: None,
                });
                row
            })
            .collect();
        let context = HostRequestContext {
            principal: HostPrincipal::Socket(7),
            connection_id: 1,
            host_generation: 10,
        };
        let selection = Selection {
            account_id: None,
            window: Some(UsageWindow::Days7),
            kind: RequestKind::Usage,
        };
        let mut pages = Pages::default();
        let first = pages
            .start(response, context.clone(), selection.clone(), [3; 32], 100)
            .expect("first page");
        assert!(
            serde_json::to_vec(&first).expect("encoded response").len() < limits::RESPONSE_BYTES
        );
        let HostResponse::AccountsUsage {
            rows,
            page: Page {
                next: Some(token), ..
            },
            ..
        } = first
        else {
            panic!("partial page")
        };
        assert!(rows.len() < 100);
        let second = pages
            .resume(&token, &context, &selection, &[3; 32], 100)
            .expect("second page");
        assert!(
            serde_json::to_vec(&second).expect("encoded response").len() < limits::RESPONSE_BYTES
        );
        let HostResponse::AccountsUsage {
            rows: remainder,
            as_of,
            page: Page { next: None, .. },
            ..
        } = second
        else {
            panic!("final page")
        };
        assert_eq!(rows.len() + remainder.len(), 120);
        assert_eq!(as_of, "accepted-instant");
    }

    #[test]
    fn snapshot_registry_refuses_capacity_without_evicting_live_cursors() {
        let context = HostRequestContext {
            principal: HostPrincipal::Socket(7),
            connection_id: 1,
            host_generation: 10,
        };
        let selection = Selection {
            account_id: None,
            window: Some(UsageWindow::Days7),
            kind: RequestKind::Usage,
        };
        let mut pages = Pages::default();
        for _ in 0..limits::SNAPSHOTS {
            pages
                .start(response(), context.clone(), selection.clone(), [3; 32], 1)
                .expect("within capacity");
        }
        assert!(matches!(
            pages.start(response(), context, selection, [3; 32], 1),
            Err(ReadRefusal::Busy { .. })
        ));
        assert_eq!(pages.cursors.len(), limits::SNAPSHOTS);
    }

    fn response() -> HostResponse {
        HostResponse::AccountsUsage {
            as_of: "accepted-instant".into(),
            window: UsageWindow::Days7,
            from: "start".into(),
            to: "end".into(),
            rows: ["openai", "codex", "grok"]
                .into_iter()
                .map(|account| AccountUsage {
                    account_id: account.into(),
                    recorded: super::super::recorded::unknown(CoverageReason::MissingMeasurements),
                    allowance: Allowance {
                        observed_at: None,
                        stale_age_ms: None,
                        view: None,
                        unavailable: None,
                        remedies: vec![],
                    },
                })
                .collect(),
            unattributed: Box::new(super::super::recorded::unknown(
                CoverageReason::UnknownAttribution,
            )),
            diagnostics: vec![],
            page: Page {
                next: None,
                truncated: false,
            },
        }
    }

    #[test]
    fn cursors_bind_principal_window_filter_generation_revision_and_original_time() {
        let context = HostRequestContext {
            principal: HostPrincipal::Socket(7),
            connection_id: 1,
            host_generation: 10,
        };
        let selection = Selection {
            account_id: None,
            window: Some(UsageWindow::Days7),
            kind: RequestKind::Usage,
        };
        let mut pages = Pages::default();
        let first = pages
            .start(response(), context.clone(), selection.clone(), [3; 32], 1)
            .expect("first page");
        let HostResponse::AccountsUsage {
            page: Page {
                next: Some(token), ..
            },
            ..
        } = first
        else {
            panic!("next page")
        };
        let second = pages
            .resume(&token, &context, &selection, &[3; 32], 1)
            .expect("resume");
        let HostResponse::AccountsUsage { as_of, rows, .. } = second else {
            panic!("usage page")
        };
        assert_eq!(as_of, "accepted-instant");
        assert_eq!(rows[0].account_id, "codex");
        for wrong in [
            HostRequestContext {
                principal: HostPrincipal::Socket(8),
                ..context.clone()
            },
            HostRequestContext {
                principal: HostPrincipal::Web("login".into()),
                ..context.clone()
            },
            HostRequestContext {
                host_generation: 11,
                ..context.clone()
            },
        ] {
            assert!(
                pages
                    .resume(&token, &wrong, &selection, &[3; 32], 1)
                    .is_err()
            );
        }
        assert!(
            pages
                .resume(
                    &token,
                    &context,
                    &Selection {
                        window: Some(UsageWindow::Days30),
                        ..selection.clone()
                    },
                    &[3; 32],
                    1
                )
                .is_err()
        );
        assert!(
            pages
                .resume(
                    &token,
                    &context,
                    &Selection {
                        account_id: Some("codex".into()),
                        ..selection.clone()
                    },
                    &[3; 32],
                    1
                )
                .is_err()
        );
        assert!(
            pages
                .resume(&token, &context, &selection, &[4; 32], 1)
                .is_err()
        );
        pages.cursors.retain(|key, _| key == &token);
        let cursor = pages.cursors.get_mut(&token).expect("cursor");
        Arc::get_mut(&mut cursor.snapshot)
            .expect("only stored snapshot")
            .at = Instant::now()
            .checked_sub(Duration::from_secs(31))
            .expect("clock range");
        assert!(matches!(
            pages.resume(&token, &context, &selection, &[3; 32], 1),
            Err(ReadRefusal::ValidationFailed { .. })
        ));
    }
}
