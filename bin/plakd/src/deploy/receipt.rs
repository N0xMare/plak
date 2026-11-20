use std::collections::BTreeSet;
use std::fs;
use std::io::{Read, Write as _};
use std::path::Path;

use serde::Deserialize;

use crate::CliResult;
use crate::shipping_artifact::{self as shipping, check, fail, io};

use super::artifacts::Pins;
use super::evidence::{self, SCHEMA};
use super::inventory::{Route, Stage, kind};
use super::protocol::{AccountIdentity, Health, SourceTransition, StageIdentity};

const REQUIRED: &str =
    "production requires a fresh green staging receipt for these exact manifest digests and routes";
pub(super) type Models = Vec<(String, String, String)>;

#[derive(Deserialize)]
struct Receipt {
    schema: u32,
    stage: Stage,
    result: String,
    host_transition: bool,
    completed_at: u64,
    manifests: Pins,
    stage_identity: StageIdentity,
    target_accounts: BTreeSet<AccountIdentity>,
    source_transition: SourceTransition,
    canary_models: Models,
    baseline: Health,
    verification: Health,
}

pub(super) fn models(routes: &[Route]) -> Models {
    let mut models: Vec<_> = routes
        .iter()
        .map(|route| {
            (
                route.kind().to_owned(),
                route.model.clone(),
                route.effort.clone(),
            )
        })
        .collect();
    models.sort();
    models
}

pub(super) fn verify(
    path: &Path,
    pins: &Pins,
    targets: &BTreeSet<AccountIdentity>,
    transition: &SourceTransition,
    models: &Models,
) -> CliResult<()> {
    let file = shipping::open(path).map_err(|_| fail(REQUIRED))?;
    evidence::private(&file)?;
    let mut raw = Vec::new();
    file.take(shipping::LIMIT + 1)
        .read_to_end(&mut raw)
        .map_err(io)?;
    check(raw.len() as u64 <= shipping::LIMIT, REQUIRED)?;
    let receipt: Receipt = serde_json::from_slice(&raw).map_err(|_| fail(REQUIRED))?;
    check(
        receipt.schema == SCHEMA
            && receipt.stage == Stage::Staging
            && receipt.result == "green"
            && receipt.host_transition
            && receipt.manifests.same_digests(pins)
            && evidence::now()
                .checked_sub(receipt.completed_at)
                .is_some_and(|age| age <= 86_400)
            && receipt
                .target_accounts
                .contains(&receipt.stage_identity.account())
            && receipt.target_accounts.iter().all(|staging| {
                targets
                    .iter()
                    .all(|production| staging.isolated_from(production))
            })
            && receipt.source_transition == *transition
            && receipt.canary_models == *models
            && coverage(&receipt),
        REQUIRED,
    )
}

fn coverage(receipt: &Receipt) -> bool {
    let mut wanted: Vec<_> = receipt
        .canary_models
        .iter()
        .map(|model| model.0.as_str())
        .collect();
    wanted.sort_unstable();
    [&receipt.baseline, &receipt.verification]
        .into_iter()
        .all(|health| {
            health.canaries.as_ref().is_some_and(|passed| {
                let mut actual: Vec<_> = passed.keys().map(|account| kind(account)).collect();
                actual.sort_unstable();
                actual == wanted
                    && passed
                        .iter()
                        .all(|(account, adapter)| adapter == super::canary::class(kind(account)))
            })
        })
}

pub(super) fn publish(path: &Path, report: &impl serde::Serialize) -> CliResult<()> {
    if fs::symlink_metadata(path).is_ok() {
        evidence::private(&shipping::open(path)?)?;
    }
    let parent = crate::snapshot_install::parent(path)?;
    let mut output = tempfile::NamedTempFile::new_in(parent).map_err(io)?;
    output
        .write_all(&shipping::canonical(report)?)
        .map_err(io)?;
    output.as_file().sync_all().map_err(io)?;
    output.persist(path).map_err(|error| io(error.error))?;
    fs::File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(io)
}
