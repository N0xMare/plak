//! Private semantic TUI control channel for repository qualification.
//!
//! This module exists only under the `test-hooks` feature. A controller writes
//! monotonically numbered requests into an owner-only directory and receives
//! the redacted semantic snapshot plus the current closed legal-action table.
//! Selected actions still pass through the real TUI router and host mailbox.

use std::fs::{self, File, OpenOptions};
use std::io::Write as _;
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use super::pilot::{LegalAction, PilotAction, TuiSnapshot};
use super::{App, UserAction};

const SCHEMA: &str = "plak.tui-pilot.v2";
const MAX_REQUEST_BYTES: u64 = 16 * 1024;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PilotRequest {
    pub(super) schema: String,
    pub(super) sequence: u64,
    #[serde(default)]
    pub(super) if_revision: Option<u64>,
    #[serde(default)]
    pub(super) action: Option<String>,
    #[serde(default)]
    pub(super) text: Option<String>,
}

#[derive(Serialize)]
struct PilotResponse {
    schema: &'static str,
    sequence: u64,
    revision: u64,
    observation_digest: String,
    status: &'static str,
    host_action_pending: bool,
    snapshot: TuiSnapshot,
    legal_actions: Vec<LegalAction>,
    error: Option<String>,
}

pub(super) struct PilotSidecar {
    root: PathBuf,
    next_sequence: u64,
}

impl PilotSidecar {
    pub(super) fn from_root(root: Option<std::ffi::OsString>) -> Result<Option<Self>, String> {
        let Some(root) = root else {
            return Ok(None);
        };
        let root = PathBuf::from(root);
        if !root.is_absolute() {
            return Err("PLAK_TEST_TUI_PILOT must name an absolute directory".to_owned());
        }
        let canonical = root
            .canonicalize()
            .map_err(|error| format!("cannot open TUI pilot directory: {error}"))?;
        if canonical != root {
            return Err("PLAK_TEST_TUI_PILOT must be canonical and symlink-free".to_owned());
        }
        let metadata = fs::symlink_metadata(&root)
            .map_err(|error| format!("cannot inspect TUI pilot directory: {error}"))?;
        if !metadata.is_dir() || metadata.mode() & 0o077 != 0 {
            return Err("PLAK_TEST_TUI_PILOT must be an owner-only directory".to_owned());
        }
        Ok(Some(Self {
            root,
            next_sequence: 1,
        }))
    }

    pub(super) fn service(&mut self, app: &mut App) -> Result<Option<UserAction>, String> {
        let request_path = self.request_path(self.next_sequence);
        let Some(request) = read_request(&request_path, self.next_sequence)? else {
            return Ok(None);
        };
        let outcome = handle_request(app, &request)?;
        let response = PilotResponse {
            schema: SCHEMA,
            sequence: self.next_sequence,
            revision: outcome.revision,
            observation_digest: outcome.observation_digest,
            status: outcome.status,
            host_action_pending: outcome.action.is_some(),
            snapshot: outcome.snapshot,
            legal_actions: outcome.legal_actions,
            error: outcome.error,
        };
        write_response(&self.response_path(self.next_sequence), &response)?;
        self.next_sequence = self
            .next_sequence
            .checked_add(1)
            .ok_or_else(|| "TUI pilot sequence exhausted".to_owned())?;
        Ok(outcome.action)
    }

    fn request_path(&self, sequence: u64) -> PathBuf {
        self.root.join(format!("request-{sequence:08}.json"))
    }

    fn response_path(&self, sequence: u64) -> PathBuf {
        self.root.join(format!("response-{sequence:08}.json"))
    }
}

fn read_request(path: &Path, expected_sequence: u64) -> Result<Option<PilotRequest>, String> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("cannot inspect TUI pilot request: {error}")),
    };
    if !metadata.is_file()
        || metadata.file_type().is_symlink()
        || metadata.nlink() != 1
        || metadata.mode() & 0o177 != 0
        || metadata.len() > MAX_REQUEST_BYTES
    {
        return Err("TUI pilot request must be one owner-only bounded regular file".to_owned());
    }
    let bytes =
        fs::read(path).map_err(|error| format!("cannot read TUI pilot request: {error}"))?;
    let request: PilotRequest = serde_json::from_slice(&bytes)
        .map_err(|error| format!("invalid TUI pilot request: {error}"))?;
    if request.schema != SCHEMA || request.sequence != expected_sequence {
        return Err("TUI pilot request schema or sequence mismatch".to_owned());
    }
    Ok(Some(request))
}

/// The decided outcome of one validated request: status, optional host
/// intent, and the post-decision observation.
pub(super) struct PilotOutcome {
    pub(super) action: Option<UserAction>,
    pub(super) status: &'static str,
    pub(super) error: Option<String>,
    pub(super) revision: u64,
    pub(super) observation_digest: String,
    pub(super) snapshot: TuiSnapshot,
    pub(super) legal_actions: Vec<LegalAction>,
}

/// Semantic validation and dispatch for one request.
///
/// The `if_revision` fence reads `app.semantic_revision` on this same thread
/// immediately before local dispatch — there is no suspension point between
/// the check and `dispatch_pilot_action`. A stale or missing precondition
/// never mutates state and never produces a host intent.
pub(super) fn handle_request(
    app: &mut App,
    request: &PilotRequest,
) -> Result<PilotOutcome, String> {
    let (action, status, error) = match request.action.as_deref() {
        None | Some("observe") => {
            if request.text.is_some() {
                (
                    None,
                    "rejected",
                    Some("observe request cannot carry text".to_owned()),
                )
            } else if request.if_revision.is_some() {
                (
                    None,
                    "rejected",
                    Some("observe request cannot fence a revision".to_owned()),
                )
            } else {
                (None, "observed", None)
            }
        }
        Some(action_id) => {
            let fenced = request
                .if_revision
                .is_some_and(|revision| revision == app.semantic_revision && revision != u64::MAX);
            if fenced {
                match pilot_action(action_id, request.text.clone()) {
                    Err(error) => (None, "rejected", Some(error)),
                    Ok(action) => match app.dispatch_pilot_action(action) {
                        Ok(host_action) => (host_action, "applied", None),
                        Err(error) => (None, "rejected", Some(error)),
                    },
                }
            } else {
                (None, "stale_observation", None)
            }
        }
    };
    let revision = app.semantic_revision;
    let snapshot = app.semantic_snapshot();
    let legal_actions = app.legal_pilot_actions();
    let observation_digest = observation_digest(revision, &snapshot, &legal_actions)?;
    Ok(PilotOutcome {
        action,
        status,
        error,
        revision,
        observation_digest,
        snapshot,
        legal_actions,
    })
}

/// Binds the observation: revision, public snapshot and legal-action table.
/// Private draft text never enters the preimage — the revision covers it.
fn observation_digest(
    revision: u64,
    snapshot: &TuiSnapshot,
    legal_actions: &[LegalAction],
) -> Result<String, String> {
    let bytes = serde_json::to_vec(&(revision, snapshot, legal_actions))
        .map_err(|_| "cannot encode pilot observation".to_owned())?;
    let digest = Sha256::digest(&bytes);
    Ok(digest.iter().fold(String::new(), |mut acc, b| {
        use std::fmt::Write as _;
        let _ = write!(acc, "{b:02x}");
        acc
    }))
}

fn pilot_action(action_id: &str, text: Option<String>) -> Result<PilotAction, String> {
    match action_id {
        "observe" => Err("observe is not an action".to_owned()),
        "open-settings" => no_text(text, PilotAction::OpenSettings),
        "close-focused-owner" => no_text(text, PilotAction::CloseFocusedOwner),
        "type-text" => text
            .map(PilotAction::TypeText)
            .ok_or_else(|| "type-text requires text".to_owned()),
        _ if action_id.starts_with("select-setting:") => no_text(
            text,
            PilotAction::SelectSetting(action_id["select-setting:".len()..].to_owned()),
        ),
        _ if action_id.starts_with("toggle-setting:") => no_text(
            text,
            PilotAction::ToggleSetting(action_id["toggle-setting:".len()..].to_owned()),
        ),
        _ => no_text(text, PilotAction::Invoke(action_id.to_owned())),
    }
}

fn no_text(text: Option<String>, action: PilotAction) -> Result<PilotAction, String> {
    if text.is_some() {
        return Err("only type-text accepts text".to_owned());
    }
    Ok(action)
}

fn write_response(path: &Path, response: &PilotResponse) -> Result<(), String> {
    if path.exists() {
        return Err("TUI pilot response already exists".to_owned());
    }
    let temporary = path.with_extension(format!("tmp-{}", std::process::id()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)
        .map_err(|error| format!("cannot create TUI pilot response: {error}"))?;
    serde_json::to_writer(&mut file, response)
        .map_err(|error| format!("cannot encode TUI pilot response: {error}"))?;
    file.write_all(b"\n")
        .and_then(|()| file.sync_all())
        .map_err(|error| format!("cannot persist TUI pilot response: {error}"))?;
    fs::rename(&temporary, path)
        .map_err(|error| format!("cannot publish TUI pilot response: {error}"))?;
    let parent = path
        .parent()
        .ok_or_else(|| "TUI pilot response has no parent directory".to_owned())?;
    sync_directory(parent)?;
    Ok(())
}

fn sync_directory(path: &Path) -> Result<(), String> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| format!("cannot sync TUI pilot directory: {error}"))
}
