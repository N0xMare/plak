//! Ephemeral binding qualification, not a session export or an executable request.
use super::{CliFailure, CliResult, PlakPaths, Report, Row, dispositions};
use crate::carry::host_move::Presentation;
use plakous::model::CompiledProviderConfiguration;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeSet, io::Read as _, path::Path};

const MAX_BYTES: u64 = 32 * 1024 * 1024;

pub(crate) fn run(
    path: &Path,
    output: &mut dyn std::io::Write,
) -> CliResult<crate::ProcessOutcome> {
    super::write_report(read(path)?.check()?, output)
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Inspection {
    version: u32,
    heads: Vec<Head>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Head {
    session: String,
    branch: String,
    checkpoint: String,
    archived: bool,
    superseded: bool,
    configuration: Option<CompiledProviderConfiguration>,
    presentation: Option<Presentation>,
}

pub(crate) fn capture(paths: &PlakPaths, sessions: &BTreeSet<String>) -> CliResult<Inspection> {
    let dispositions = dispositions::inspect(paths)?;
    let heads = plakous::sessions::rehearsal::inspect_binding_presentations_sqlite(
        paths.runtime_database(),
    )
    .map_err(CliFailure::from)?
    .into_iter()
    .filter(|head| sessions.contains(&head.locator.session_id().to_string()))
    .map(|head| {
        let (archived, superseded) = dispositions
            .rows
            .get(&dispositions::key(head.locator))
            .copied()
            .unwrap_or_default();
        let frozen = head.presentation.transpose().map_err(CliFailure::from)?;
        Ok(Head {
            session: head.locator.session_id().to_string(),
            branch: head.locator.branch_id().to_string(),
            checkpoint: head.locator.checkpoint_id().to_string(),
            archived: archived || head.archived,
            superseded: superseded || !head.line_head,
            configuration: frozen
                .as_ref()
                .and_then(|frozen| frozen.configuration().cloned()),
            presentation: frozen
                .as_ref()
                .map(|frozen| {
                    Presentation::capture(frozen.semantic().map_err(CliFailure::from)?)
                        .map_err(CliFailure::new)
                })
                .transpose()?,
        })
    })
    .collect::<CliResult<_>>()?;
    Ok(Inspection { version: 1, heads })
}

fn read(path: &Path) -> CliResult<Inspection> {
    let file = std::fs::File::open(path).map_err(|error| CliFailure::new(error.to_string()))?;
    let mut bytes = Vec::new();
    file.take(MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| CliFailure::new(error.to_string()))?;
    if bytes.len() as u64 > MAX_BYTES {
        return Err(CliFailure::new(
            "binding inspection exceeds its input bound",
        ));
    }
    serde_json::from_slice(&bytes).map_err(|_| CliFailure::new("invalid binding inspection"))
}

impl Inspection {
    pub(crate) fn bytes(&self) -> CliResult<Vec<u8>> {
        let bytes = serde_json::to_vec(self).map_err(|error| CliFailure::new(error.to_string()))?;
        if bytes.len() as u64 > MAX_BYTES {
            return Err(CliFailure::new(
                "binding inspection exceeds its input bound",
            ));
        }
        Ok(bytes)
    }

    pub(crate) fn check(self) -> CliResult<Report> {
        if self.version != 1 || self.heads.len() > 16384 {
            return Err(CliFailure::new(
                "unsupported binding inspection version or inventory",
            ));
        }
        let mut sessions = Vec::with_capacity(self.heads.len());
        for head in self.heads {
            let (binding_change, unavailable) = match (head.configuration, head.presentation) {
                (None, None) => (None, None),
                (Some(configuration), Some(presentation)) => {
                    let frozen = presentation.decode().map_err(CliFailure::new)?;
                    let verdict = super::qualified_change(None).map_or_else(
                        || plakous_hosted::agent::check_binding_data(&configuration, &frozen),
                        |change| Ok(Some(change)),
                    );
                    match verdict {
                        Ok(change) => (change.map(|change| change.to_string()), None),
                        Err(error) => (None, Some(error.message().to_owned())),
                    }
                }
                _ => return Err(CliFailure::new("incomplete binding inspection")),
            };
            sessions.push(Row {
                session: head.session,
                branch: head.branch,
                checkpoint: head.checkpoint,
                archived: head.archived,
                superseded: head.superseded,
                binding_change,
                unavailable,
                unaffected: Vec::new(),
            });
        }
        Ok(Report {
            compatible: sessions.iter().all(|row| {
                row.archived
                    || row.superseded
                    || row.binding_change.is_none() && row.unavailable.is_none()
            }),
            sessions,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_inspection_v1_preserves_source_configuration_and_presentation_bytes() {
        let pinned = include_bytes!("inspection/pinned/v1.json");
        let inspection: Inspection = serde_json::from_slice(pinned).unwrap();
        assert_eq!(inspection.version, 1);
        assert_eq!(inspection.heads.len(), 17);
        assert!(
            inspection
                .heads
                .iter()
                .all(|head| head.configuration.is_some() && head.presentation.is_some())
        );
        assert_eq!(inspection.bytes().unwrap(), pinned);
        let mut changed: serde_json::Value = serde_json::from_slice(pinned).unwrap();
        changed["heads"][0]["configuration"]["unreviewed_field"] = serde_json::json!(true);
        assert!(serde_json::from_value::<Inspection>(changed).is_err());
    }

    #[test]
    #[ignore = "maintainer-only source-build inspection fixture producer"]
    fn generate_live_inspection_v1_pin() -> Result<(), Box<dyn std::error::Error>> {
        use std::os::unix::fs::PermissionsExt as _;
        let (_root, paths) = PlakPaths::isolated_test_home();
        paths.ensure_layout()?;
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../crates/hosting/hosted/tests/fixtures/current-bindings");
        std::fs::copy(fixture.join("runtime.sqlite3"), paths.runtime_database())?;
        std::fs::set_permissions(
            paths.runtime_database(),
            std::fs::Permissions::from_mode(0o600),
        )?;
        let manifest: serde_json::Value =
            serde_json::from_slice(&std::fs::read(fixture.join("manifest.json"))?)?;
        let sessions = manifest["cases"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| {
                row["locator"]
                    .as_str()
                    .unwrap()
                    .parse::<plakous::sessions::SessionLocator>()
                    .unwrap()
                    .session_id()
                    .to_string()
            })
            .collect();
        let bytes = capture(&paths, &sessions)?.bytes()?;
        assert!(!bytes.windows(6).any(|bytes| bytes == b"/home/"));
        assert!(!bytes.windows(7).any(|bytes| bytes == b"/Users/"));
        std::fs::write(std::env::var("PLAK_INSPECTION_FIXTURE_OUTPUT")?, bytes)?;
        Ok(())
    }

    #[test]
    fn live_inspection_is_closed_and_cannot_be_mistaken_for_a_session_carrier() {
        let file = tempfile::NamedTempFile::new().unwrap();
        for input in [
            r#"{"version":1,"heads":[],"purpose":"resume"}"#,
            r#"{"version":13,"kind":"host_upgrade","heads":[]}"#,
        ] {
            std::fs::write(file.path(), input).unwrap();
            assert!(read(file.path()).is_err());
        }
        assert!(
            Inspection {
                version: 2,
                heads: Vec::new()
            }
            .check()
            .is_err()
        );
    }
}
