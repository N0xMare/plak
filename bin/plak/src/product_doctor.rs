//! Read-only product health diagnostics.
//!
//! Probes in this module inspect existing state only. Database and composition
//! host owners can supply statuses where opening the owned resource would
//! create, migrate, publish, or extract state.

use std::fs;
use std::io::IsTerminal as _;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::commands::application::{AccountCommand, CredentialVerdict};
use crate::presentation::{CliStatus, CompatibilityState, CredentialState};
use crate::{telemetry_sink::TelemetryHealth, telemetry_store::CredentialRefreshRow};
use plakous::subscriptions::NativeProvider;
use serde::Deserialize;

/// Stable identity of a doctor check, independent of its presentation.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum CheckKind {
    Version,
    Home,
    Config,
    ProductDatabase,
    RuntimeDatabase,
    RuntimeStore,
    SubagentsDatabase,
    TelemetryDatabase,
    SecretsDirectory,
    SessionsDirectory,
    CarryFiles,
    WorkingDirectory,
    StdinTerminal,
    StdoutTerminal,
    CompositionHost,
    OpenRouterCatalog,
    RootTemplate,
    SpawnTemplates,
    OauthCodex,
    OauthXai,
    OauthClaude,
}

impl CheckKind {
    pub const fn key(self) -> &'static str {
        match self {
            Self::Version => "version",
            Self::Home => "home",
            Self::Config => "config",
            Self::ProductDatabase => "product_db",
            Self::SubagentsDatabase => "subagents_db",
            Self::RuntimeDatabase => "runtime_db",
            Self::RuntimeStore => "runtime_store",
            Self::TelemetryDatabase => "telemetry_db",
            Self::SecretsDirectory => "secrets_dir",
            Self::SessionsDirectory => "sessions_dir",
            Self::CarryFiles => "carry_files",
            Self::WorkingDirectory => "cwd",
            Self::StdinTerminal => "stdin_tty",
            Self::StdoutTerminal => "stdout_tty",
            Self::CompositionHost => "composition_host",
            Self::OpenRouterCatalog => "openrouter_catalog",
            Self::RootTemplate => "root_template",
            Self::SpawnTemplates => "spawn_templates",
            Self::OauthCodex => "oauth_codex",
            Self::OauthXai => "oauth_xai",
            Self::OauthClaude => "oauth_claude",
        }
    }
}

/// Diagnostic importance and summary ordering.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum Severity {
    Ok,
    Warning,
    Error,
}

impl Severity {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Warning => "warning",
            Self::Error => "error",
        }
    }
}

/// One presentation-neutral, table-ready diagnostic row.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DoctorCheck {
    pub kind: CheckKind,
    pub group: &'static str,
    pub severity: Severity,
    pub result: String,
    pub detail: String,
    pub remediation: Option<String>,
}

impl DoctorCheck {
    fn new(
        kind: CheckKind,
        group: &'static str,
        severity: Severity,
        result: impl Into<String>,
        detail: impl Into<String>,
        remediation: Option<&'static str>,
    ) -> Self {
        Self {
            kind,
            group,
            severity,
            result: result.into(),
            detail: detail.into(),
            remediation: remediation.map(str::to_owned),
        }
    }
}

/// Aggregate counts suitable for human or structured output.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DoctorSummary {
    pub ok: usize,
    pub warnings: usize,
    pub errors: usize,
}

/// Complete diagnostic result. It carries no credentials or store handles.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DoctorReport {
    pub checks: Vec<DoctorCheck>,
    pub summary: DoctorSummary,
}

impl DoctorReport {
    pub fn from_checks(checks: Vec<DoctorCheck>) -> Self {
        let mut summary = DoctorSummary::default();
        for check in &checks {
            match check.severity {
                Severity::Ok => summary.ok += 1,
                Severity::Warning => summary.warnings += 1,
                Severity::Error => summary.errors += 1,
            }
        }
        Self { checks, summary }
    }
}

/// Read-only parse outcome supplied by the product config owner.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ConfigStatus {
    Missing,
    Parsed,
    Unreadable(String),
    Invalid(String),
}

/// Read-only availability supplied by an owning database abstraction.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DatabaseStatus {
    Missing,
    Available,
    /// Written under a schema this build does not read.
    Incompatible,
    Corrupt(String),
    Unavailable(String),
}

/// Aggregate retention usage of the runtime store, read without opening it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RuntimeStoreStatus {
    Missing,
    Usage(plakous::sessions::SessionStoreUsage),
    Unavailable(String),
}

/// Whether an optional composition dependency is needed by current config.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Requirement {
    Optional,
    Required,
}

/// Status supplied by the bundled-host owner; probing/extracting it here would mutate state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CompositionHostStatus {
    Included { identity: String },
    Unavailable(String),
}

/// Owner-supplied catalog allowlist probe (discovery GETs only).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OpenRouterCatalogStatus {
    NotConfigured,
    KeyPresentNoModel,
    Ok,
    Failed(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DoctorPaths {
    pub home: PathBuf,
    pub config: PathBuf,
    pub product_database: PathBuf,
    pub runtime_database: PathBuf,
    pub subagents_database: PathBuf,
    pub telemetry_database: PathBuf,
    pub secrets_directory: PathBuf,
    pub sessions_directory: PathBuf,
}

impl DoctorPaths {
    pub fn under(home: PathBuf) -> Self {
        let paths = crate::product_paths::PlakPaths::explicit(home.clone());
        Self {
            config: paths.config_file(),
            product_database: paths.product_database(),
            runtime_database: paths.runtime_database(),
            subagents_database: paths.subagents_database(),
            telemetry_database: paths.telemetry_database(),
            secrets_directory: paths.secrets_dir(),
            sessions_directory: paths.sessions_dir(),
            home,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TerminalFacts {
    pub stdin: bool,
    pub stdout: bool,
}

impl TerminalFacts {
    pub fn capture() -> Self {
        Self {
            stdin: std::io::stdin().is_terminal(),
            stdout: std::io::stdout().is_terminal(),
        }
    }
}

/// Inputs whose owners must decide availability without giving doctor mutation authority.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DoctorInputs {
    pub version: String,
    pub paths: DoctorPaths,
    pub config: ConfigStatus,
    pub product_database: DatabaseStatus,
    pub runtime_database: DatabaseStatus,
    pub runtime_store: RuntimeStoreStatus,
    /// When the latest automatic history prune failed, with no successful one
    /// since (unix nanoseconds), from telemetry.
    pub retention_failure_at_ns: Option<i64>,
    pub subagents_database: DatabaseStatus,
    pub telemetry: TelemetryHealth,
    pub working_directory: Result<PathBuf, String>,
    pub terminals: TerminalFacts,
    pub composition_requirement: Requirement,
    pub composition_host: CompositionHostStatus,
    pub python_containment_required: bool,
    pub runtime_directory: Option<PathBuf>,
    pub openrouter_catalog: OpenRouterCatalogStatus,
    pub root_template: RootTemplateStatus,
    pub spawn_templates: SpawnTemplatesStatus,
    pub oauth: OauthCredentialProbes,
}

/// Attach readiness of the template new sessions bind at genesis.
///
/// `config OK` only says `config.toml` parses. A root template naming a lapsed
/// account parses perfectly and then fails at attach with nothing said before
/// launch. Readiness is computed from the persisted catalog and the
/// offline credential posture — the same inputs the Agents tab uses — so this
/// check performs no network I/O.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RootTemplateStatus {
    /// No `[root] agent`; the session binds the default selection.
    Unset,
    /// `[root] agent` names a template with no `<name>.toml` file.
    Undefined { name: String },
    /// The named template is attachable now.
    Ready { name: String, binding: String },
    /// The named template parses but cannot attach as written.
    NotReady {
        name: String,
        binding: String,
        reason: &'static str,
    },
    /// The catalog could not be read, so readiness is unknown.
    Unknown { detail: String },
}

/// Attach readiness of every template the root can spawn: the working set
/// and each template their `spawns` lists reach. A config loads with a
/// template whose account is not ready; spawning it is refused until it is.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SpawnTemplatesStatus {
    /// The working set is empty: nothing is spawnable.
    None,
    /// Every spawnable template is attachable now.
    Ready { count: usize },
    /// These templates' spawns are refused until their account is ready.
    NotReady { templates: Vec<NotReadyTemplate> },
    /// The catalog could not be read, so readiness is unknown.
    Unknown { detail: String },
}

/// One spawnable template that cannot attach as written.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NotReadyTemplate {
    pub name: String,
    pub account: String,
    pub reason: &'static str,
}

/// Owner-supplied OAuth record + latest refresh telemetry (no secrets).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OauthCredentialProbes {
    pub codex: OauthCredentialProbe,
    pub xai: OauthCredentialProbe,
    pub claude: OauthCredentialProbe,
}

/// One provider's credential verdict, durable record metadata and
/// last-refresh lookup.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OauthCredentialProbe {
    pub provider: &'static str,
    /// The one offline classification `plak status` and model runs use; a
    /// read failure carries the status table's cell for it.
    pub credential: Result<CliStatus, String>,
    pub record: OauthRecordStatus,
    pub last_refresh: OauthRefreshLookup,
}

/// Latest refresh telemetry, or why it could not be read.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OauthRefreshLookup {
    None,
    Row(OauthRefreshTelemetry),
    Unavailable(String),
}

/// Content-free credential.json metadata (status/generation/mtime only).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OauthRecordStatus {
    Missing,
    Unreadable(String),
    Present {
        status: String,
        generation: u64,
        mtime_age_secs: u64,
    },
}

static CREDENTIAL_STATE_HOME: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// Records the subscription state home once per process so refresh-failure
/// display can compare telemetry rows with the current credential generation.
pub(crate) fn install_credential_state_home(state_home: PathBuf) {
    let _ = CREDENTIAL_STATE_HOME.set(state_home);
}

fn current_credential_generation(provider: &str) -> Option<u64> {
    let record = CREDENTIAL_STATE_HOME
        .get()?
        .join("plak/subscriptions")
        .join(provider)
        .join("credential.json");
    match inspect_oauth_record(&record) {
        OauthRecordStatus::Present { generation, .. } => Some(generation),
        OauthRecordStatus::Missing | OauthRecordStatus::Unreadable(_) => None,
    }
}

/// A failure row belongs to the record its claim left behind: quarantine keeps
/// the claim generation and a restore advances it once. A record newer than
/// that (a later login, logout, or refresh) makes the row history, not state.
fn refresh_row_is_current(row_generation: u64, record_generation: Option<u64>) -> bool {
    record_generation.is_none_or(|current| current <= row_generation.saturating_add(1))
}

/// The newest terminal refresh outcome is display evidence only, never admission
/// or recovery authority. A later Began row does not erase the last failure.
pub(crate) fn last_failed_refresh(
    paths: &crate::product_paths::PlakPaths,
    account: &str,
    zone: &jiff::tz::TimeZone,
) -> Option<String> {
    let provider = match account {
        "grok" | "xai" | "supergrok" => "xai",
        "codex" => "codex",
        "claude" => "claude",
        _ => return None,
    };
    let row =
        crate::telemetry_store::TelemetryStore::latest_refresh_outcome(paths, provider).ok()??;
    if !refresh_row_is_current(row.generation, current_credential_generation(provider)) {
        return None;
    }
    let reason = row.failure_reason.as_deref();
    let at = jiff::Timestamp::from_nanosecond(i128::from(row.at_ns)).ok()?;
    let local = at.to_zoned(zone.clone());
    let clock = local.strftime("%H:%M");
    let class = row.failure_class.as_deref();
    let cause = match (class, row.transport_phase.as_deref()) {
        (Some("timeout"), Some("resolution")) => {
            "network unreachable (DNS lookup timed out)".to_owned()
        }
        (Some("transport"), Some("resolution")) => {
            "network unreachable (DNS lookup failed)".to_owned()
        }
        (Some("transport" | "timeout"), Some("connect")) => {
            "network unreachable (could not connect to the provider)".to_owned()
        }
        (Some("transport" | "timeout"), Some("tls")) => {
            "TLS handshake failed (a captive portal or intercepting proxy can cause this)"
                .to_owned()
        }
        (Some("transport" | "timeout"), Some("send")) => {
            "connection dropped while sending or waiting for a response".to_owned()
        }
        (Some("transport" | "timeout"), Some("response")) => {
            "connection interrupted while receiving the response".to_owned()
        }
        (Some("transport"), _) => "network transport failed (phase not recorded)".to_owned(),
        (Some("timeout"), _) => "refresh timed out (phase not recorded)".to_owned(),
        (Some("http_4xx" | "http_5xx" | "http_other"), _) => row.http_status.map_or_else(
            || "provider rejected the refresh".to_owned(),
            |status| format!("provider returned HTTP {status}"),
        ),
        (Some("validation"), _) if reason == Some("retired_format") => {
            "the stored credential was written by an older plak build (no request was sent)"
                .to_owned()
        }
        (Some("validation"), _) if reason == Some("stored_credential") => {
            "the stored credential could not be read (no request was sent)".to_owned()
        }
        (Some("validation"), _) => reason.map_or_else(
            || "refresh response could not be validated".to_owned(),
            |reason| format!("refresh response could not be validated ({reason})"),
        ),
        (Some("store"), _) => "credential storage failure".to_owned(),
        (Some("cancelled"), _) => "refresh cancelled".to_owned(),
        (Some("not_ready"), _) => "refresh dispatch was not ready".to_owned(),
        _ => "cause not recorded".to_owned(),
    };
    let state = match row.phase.as_str() {
        "restored" => "credential unchanged",
        "quarantined" => "credential quarantined",
        _ => "refresh outcome unknown",
    };
    let remedy = if row.phase == "restored" && matches!(class, Some("transport" | "timeout")) {
        "retry when online".to_owned()
    } else if matches!(reason, Some("retired_format" | "stored_credential")) {
        format!("run plak login {account}")
    } else if provider == "xai" {
        "run plak refresh grok; run plak login grok if that fails".to_owned()
    } else if row.phase == "restored" {
        format!("run plak refresh {account}; run plak login {account} if that fails")
    } else {
        format!("run plak login {account}")
    };
    Some(format!(
        "refresh failed at {clock}: {cause}; {state}; {remedy}"
    ))
}

/// Latest `v_credential_refreshes` row, already stripped of secrets.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OauthRefreshTelemetry {
    pub phase: String,
    pub failure_class: Option<String>,
    pub transport_phase: Option<String>,
    pub http_status_class: Option<String>,
    pub age_secs: u64,
}

impl OauthRefreshTelemetry {
    fn from_store_row(row: CredentialRefreshRow) -> Self {
        Self {
            phase: row.phase,
            failure_class: row.failure_class,
            transport_phase: row.transport_phase,
            http_status_class: row.http_status_class,
            age_secs: age_secs_from_unix_ns(row.at_ns),
        }
    }
}

#[derive(Deserialize)]
struct OauthRecordMeta {
    status: String,
    generation: u64,
}

/// Read `status` and `generation` from a credential record. The payload field
/// is never deserialized into this type and must not be printed.
pub fn inspect_oauth_record(path: &Path) -> OauthRecordStatus {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return OauthRecordStatus::Missing;
        }
        Err(error) => return OauthRecordStatus::Unreadable(error.to_string()),
    };
    let mtime_age_secs = metadata
        .modified()
        .ok()
        .and_then(|mtime| mtime.elapsed().ok())
        .map_or(0, |elapsed| elapsed.as_secs());
    match fs::read_to_string(path) {
        Ok(text) => match serde_json::from_str::<OauthRecordMeta>(&text) {
            Ok(meta) => OauthRecordStatus::Present {
                status: meta.status,
                generation: meta.generation,
                mtime_age_secs,
            },
            Err(error) => OauthRecordStatus::Unreadable(error.to_string()),
        },
        Err(error) => OauthRecordStatus::Unreadable(error.to_string()),
    }
}

/// Join one account's credential verdict and on-disk OAuth record with the
/// latest telemetry row for doctor.
pub fn probe_oauth_credential(
    provider: &'static str,
    credential: Result<CliStatus, String>,
    record_path: &Path,
    last_refresh: Result<Option<CredentialRefreshRow>, String>,
) -> OauthCredentialProbe {
    OauthCredentialProbe {
        provider,
        credential,
        record: inspect_oauth_record(record_path),
        last_refresh: match last_refresh {
            Ok(Some(row)) => OauthRefreshLookup::Row(OauthRefreshTelemetry::from_store_row(row)),
            Ok(None) => OauthRefreshLookup::None,
            Err(detail) => OauthRefreshLookup::Unavailable(detail),
        },
    }
}

fn age_secs_from_unix_ns(at_ns: i64) -> u64 {
    let now_ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    let at_ns = u128::try_from(at_ns.max(0)).unwrap_or(0);
    u64::try_from(now_ns.saturating_sub(at_ns) / 1_000_000_000).unwrap_or(u64::MAX)
}

fn format_age_secs(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86_400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86_400)
    }
}

/// Read and parse an existing config without creating its parent or a default file.
pub fn inspect_config(path: &Path, parse: impl FnOnce(&str) -> Result<(), String>) -> ConfigStatus {
    match fs::read_to_string(path) {
        Ok(contents) => match parse(&contents) {
            Ok(()) => ConfigStatus::Parsed,
            Err(error) => ConfigStatus::Invalid(error),
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => ConfigStatus::Missing,
        Err(error) => ConfigStatus::Unreadable(error.to_string()),
    }
}

/// Checks a product or tree database's version, pages and foreign keys read-only. A missing or empty file is `Missing`.
pub fn inspect_state_database(path: &Path, current: i64) -> DatabaseStatus {
    use rusqlite::{Connection, OpenFlags};

    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return DatabaseStatus::Missing;
        }
        Err(error) => return DatabaseStatus::Unavailable(error.to_string()),
        Ok(metadata) if metadata.file_type().is_file() => {}
        Ok(_) => return DatabaseStatus::Unavailable("not a regular file".to_owned()),
    }
    let connection = match Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    ) {
        Ok(connection) => connection,
        Err(error) => return state_database_error(error),
    };
    if let Err(error) = connection.busy_timeout(std::time::Duration::from_millis(250)) {
        return state_database_error(error);
    }
    match connection.query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0)) {
        Ok(0) => return DatabaseStatus::Missing,
        Ok(version) if version == current => {}
        Ok(_) => return DatabaseStatus::Incompatible,
        Err(error) => return state_database_error(error),
    }
    let checks = (|| -> rusqlite::Result<Option<String>> {
        let integrity: String =
            connection.query_row("PRAGMA integrity_check(1)", [], |row| row.get(0))?;
        if integrity != "ok" {
            return Ok(Some(integrity));
        }
        let mut statement = connection.prepare("PRAGMA foreign_key_check")?;
        if statement.query([])?.next()?.is_some() {
            return Ok(Some("foreign key check failed".to_owned()));
        }
        Ok(None)
    })();
    match checks {
        Ok(None) => DatabaseStatus::Available,
        Ok(Some(reason)) => DatabaseStatus::Corrupt(reason),
        Err(error) => state_database_error(error),
    }
}

fn state_database_error(error: rusqlite::Error) -> DatabaseStatus {
    match error.sqlite_error_code() {
        Some(rusqlite::ErrorCode::DatabaseCorrupt | rusqlite::ErrorCode::NotADatabase) => {
            DatabaseStatus::Corrupt(error.to_string())
        }
        _ => DatabaseStatus::Unavailable(error.to_string()),
    }
}

/// Classify the runtime database with the public, non-authoritative read-only facade probe.
pub fn inspect_runtime_database(path: &Path) -> DatabaseStatus {
    use plakous::sessions::{SessionBackend, SessionPersistenceShape};

    match SessionBackend::classify_sqlite(path) {
        Ok(SessionPersistenceShape::MissingOrFresh) => DatabaseStatus::Missing,
        Ok(SessionPersistenceShape::Current) => DatabaseStatus::Available,
        Err(error) => DatabaseStatus::Unavailable(error.to_string()),
        #[allow(unreachable_patterns, reason = "public facade enum is non-exhaustive")]
        Ok(_) => DatabaseStatus::Unavailable("unknown runtime database shape".to_owned()),
    }
}

/// Read the runtime store's aggregate usage with the read-only facade probe.
pub fn inspect_runtime_store(path: &Path) -> RuntimeStoreStatus {
    use plakous::sessions::SessionBackend;

    match SessionBackend::inspect_sqlite_usage(path) {
        Ok(Some(usage)) => RuntimeStoreStatus::Usage(usage),
        Ok(None) => RuntimeStoreStatus::Missing,
        Err(error) => RuntimeStoreStatus::Unavailable(error.to_string()),
    }
}

const RUNTIME_STORE_REMEDY: &str = "reclaim superseded history with `plak session gc --all`, then free sessions with `plak session delete <id>`; /clear and compaction do not reclaim it";

fn runtime_store_check(
    status: RuntimeStoreStatus,
    retention_failure_at_ns: Option<i64>,
) -> DoctorCheck {
    let kind = CheckKind::RuntimeStore;
    let usage = match status {
        RuntimeStoreStatus::Missing => {
            return DoctorCheck::new(
                kind,
                "storage",
                Severity::Ok,
                "empty",
                "no runtime store",
                None,
            );
        }
        RuntimeStoreStatus::Unavailable(reason) => {
            return DoctorCheck::new(
                kind,
                "storage",
                Severity::Error,
                "unavailable",
                reason,
                Some("restore the database and use its owning recovery workflow"),
            );
        }
        RuntimeStoreStatus::Usage(usage) => usage,
    };
    let bounds = usage.bounds();
    let detail = bounds
        .iter()
        .map(|bound| {
            format!(
                "{} {}/{} (new work stops at {})",
                bound.bound(),
                bound.used(),
                bound.limit(),
                bound.ordinary_threshold()
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    let detail = format!(
        "{detail}; file {} bytes, {} free; feed {} rows, {} bytes",
        usage.file_bytes(),
        usage.free_bytes(),
        usage.feed_records(),
        usage.feed_bytes()
    );
    let detail = match retention_failure_at_ns {
        Some(at_ns) => format!(
            "{detail}; the last automatic history prune failed {} ago",
            format_age_secs(age_secs_from_unix_ns(at_ns))
        ),
        None => detail,
    };
    let pressure = bounds
        .iter()
        .map(|bound| StorePressure::of(bound.used(), bound.reserve(), bound.ordinary_threshold()))
        .max()
        .unwrap_or(StorePressure::Available);
    match pressure {
        StorePressure::Full => DoctorCheck::new(
            kind,
            "storage",
            Severity::Error,
            "full",
            detail,
            Some(RUNTIME_STORE_REMEDY),
        ),
        StorePressure::NearlyFull => DoctorCheck::new(
            kind,
            "storage",
            Severity::Warning,
            "nearly_full",
            detail,
            Some(RUNTIME_STORE_REMEDY),
        ),
        StorePressure::Available => {
            DoctorCheck::new(kind, "storage", Severity::Ok, "available", detail, None)
        }
    }
}

/// How close one aggregate bound is to the point where new work stops.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum StorePressure {
    Available,
    /// Within one reserve of the ordinary threshold.
    NearlyFull,
    /// New work is refused; only settlement and recovery still write.
    Full,
}

impl StorePressure {
    const fn of(used: u64, reserve: u64, ordinary_threshold: u64) -> Self {
        if used >= ordinary_threshold {
            Self::Full
        } else if used.saturating_add(reserve) >= ordinary_threshold {
            Self::NearlyFull
        } else {
            Self::Available
        }
    }
}

fn telemetry_file_version(name: &str) -> Option<u64> {
    name.strip_prefix("telemetry-v")?
        .strip_suffix(".sqlite3")?
        .parse()
        .ok()
}

fn telemetry_check(kind: CheckKind, path: &Path, health: TelemetryHealth) -> DoctorCheck {
    let mut check = match health {
        TelemetryHealth::Missing => DoctorCheck::new(
            kind,
            "storage",
            Severity::Ok,
            "missing",
            path.display().to_string(),
            Some("state is created by the first stateful Plak command"),
        ),
        TelemetryHealth::Unavailable(reason) => DoctorCheck::new(
            kind,
            "storage",
            Severity::Error,
            "unavailable",
            reason,
            Some("restore telemetry storage or explicitly remove the reported file to rebuild"),
        ),
        TelemetryHealth::Ok {
            rows,
            write_errors,
            dropped,
        } => {
            let detail = if write_errors == 0 && dropped == 0 {
                format!("{rows} rows")
            } else {
                format!("{rows} rows · {write_errors} write errors · {dropped} dropped")
            };
            DoctorCheck::new(
                kind,
                "storage",
                if write_errors == 0 && dropped == 0 {
                    Severity::Ok
                } else {
                    Severity::Warning
                },
                "ok",
                detail,
                None,
            )
        }
    };
    let mut other_files: Vec<_> = path
        .parent()
        .and_then(|directory| fs::read_dir(directory).ok())
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter(|entry| entry.path() != path && entry.file_type().is_ok_and(|kind| kind.is_file()))
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            (name == "telemetry.sqlite3"
                || (name.starts_with("telemetry-v") && name.ends_with(".sqlite3")))
            .then_some(name)
        })
        .collect();
    other_files.sort();
    let current_version = path
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(telemetry_file_version);
    let (newer_files, old_files): (Vec<_>, Vec<_>) = other_files.into_iter().partition(|name| {
        telemetry_file_version(name)
            .zip(current_version)
            .is_some_and(|(version, current)| version > current)
    });
    if !old_files.is_empty() {
        check.detail.push_str(&format!(
            "; older-schema/legacy files left untouched: {}",
            old_files.join(", ")
        ));
        check.severity = check.severity.max(Severity::Warning);
        if check.remediation.is_none() {
            check.remediation = Some(
                "old telemetry files are not migrated or pruned; removal is explicit".to_owned(),
            );
        }
    }
    if !newer_files.is_empty() {
        check.detail.push_str(&format!(
            "; newer-schema files left untouched: {}",
            newer_files.join(", ")
        ));
    }
    check
}

/// Run all filesystem and environment probes. This function performs no writes or subprocesses.
#[allow(
    clippy::vec_init_then_push,
    reason = "ordered diagnostic construction stays readable beside conditional extension"
)]
pub fn diagnose(input: DoctorInputs) -> DoctorReport {
    let mut checks = Vec::new();
    checks.push(DoctorCheck::new(
        CheckKind::Version,
        "product",
        Severity::Ok,
        "available",
        input.version,
        None,
    ));
    checks.push(directory_check(
        CheckKind::Home,
        "storage",
        &input.paths.home,
        DirectoryPolicy::Private,
        false,
    ));
    checks.push(config_check(&input.paths.config, input.config));
    checks.push(database_check(
        CheckKind::ProductDatabase,
        &input.paths.product_database,
        input.product_database,
    ));
    checks.push(database_check(
        CheckKind::RuntimeDatabase,
        &input.paths.runtime_database,
        input.runtime_database,
    ));
    checks.push(runtime_store_check(
        input.runtime_store,
        input.retention_failure_at_ns,
    ));
    checks.push(database_check(
        CheckKind::SubagentsDatabase,
        &input.paths.subagents_database,
        input.subagents_database,
    ));
    checks.push(telemetry_check(
        CheckKind::TelemetryDatabase,
        &input.paths.telemetry_database,
        input.telemetry.clone(),
    ));
    checks.push(directory_check(
        CheckKind::SecretsDirectory,
        "storage",
        &input.paths.secrets_directory,
        DirectoryPolicy::Private,
        false,
    ));
    checks.push(directory_check(
        CheckKind::SessionsDirectory,
        "storage",
        &input.paths.sessions_directory,
        DirectoryPolicy::Writable,
        false,
    ));
    checks.push(carry_files_check(&input.paths.home.join("carry")));
    checks.push(cwd_check(input.working_directory));
    checks.push(terminal_check(
        CheckKind::StdinTerminal,
        input.terminals.stdin,
    ));
    checks.push(terminal_check(
        CheckKind::StdoutTerminal,
        input.terminals.stdout,
    ));
    checks.extend(composition_checks(
        input.composition_requirement,
        input.composition_host,
        input.python_containment_required,
        input.runtime_directory.as_deref(),
    ));
    if crate::capabilities::ROUTES.iter().any(|capability| {
        matches!(
            capability.doctor,
            Some(crate::capabilities::RouteDoctor::OpenRouterCatalog)
        )
    }) {
        checks.push(openrouter_catalog_check(input.openrouter_catalog));
    }
    checks.push(root_template_check(input.root_template));
    checks.push(spawn_templates_check(input.spawn_templates));
    for doctor in crate::capabilities::ROUTES
        .iter()
        .filter_map(|capability| capability.doctor.as_ref())
    {
        if let crate::capabilities::RouteDoctor::Oauth { kind, probe } = doctor {
            checks.push(oauth_check(*kind, (probe)(&input.oauth)));
        }
    }
    DoctorReport::from_checks(checks)
}

/// Session export documents left in `carry/`: a carry that failed keeps its
/// file, and each one holds a whole conversation.
fn carry_files_check(directory: &Path) -> DoctorCheck {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return DoctorCheck::new(
                CheckKind::CarryFiles,
                "storage",
                Severity::Ok,
                "none",
                directory.display().to_string(),
                None,
            );
        }
        Err(error) => {
            return DoctorCheck::new(
                CheckKind::CarryFiles,
                "storage",
                Severity::Warning,
                "unreadable",
                format!("{}: {error}", directory.display()),
                None,
            );
        }
    };
    let mut kept = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "json")
        })
        .collect::<Vec<_>>();
    kept.sort();
    if kept.is_empty() {
        return DoctorCheck::new(
            CheckKind::CarryFiles,
            "storage",
            Severity::Ok,
            "none",
            directory.display().to_string(),
            None,
        );
    }
    DoctorCheck::new(
        CheckKind::CarryFiles,
        "storage",
        Severity::Warning,
        format!("{} kept", kept.len()),
        kept.iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join(", "),
        Some("import each with `plak session import <file>`, or delete it"),
    )
}

#[derive(Clone, Copy)]
enum DirectoryPolicy {
    Private,
    Writable,
}

fn directory_check(
    kind: CheckKind,
    group: &'static str,
    path: &Path,
    policy: DirectoryPolicy,
    required: bool,
) -> DoctorCheck {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return DoctorCheck::new(
                kind,
                group,
                if required {
                    Severity::Error
                } else {
                    Severity::Ok
                },
                "missing",
                path.display().to_string(),
                Some("initialize Plak state before using stateful commands"),
            );
        }
        Err(error) => {
            return DoctorCheck::new(
                kind,
                group,
                Severity::Error,
                "unavailable",
                error.to_string(),
                Some("restore access to the configured path"),
            );
        }
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return DoctorCheck::new(
            kind,
            group,
            Severity::Error,
            "unsafe type",
            path.display().to_string(),
            Some("replace the path with a directly owned directory"),
        );
    }

    let permission_error = directory_permission_error(&metadata, policy);
    match permission_error {
        Some(detail) => DoctorCheck::new(
            kind,
            group,
            Severity::Error,
            "permissions",
            format!("{} · {detail}", path.display()),
            Some("run `chmod 700` on this path; Plak refuses a non-private home"),
        ),
        None => DoctorCheck::new(
            kind,
            group,
            Severity::Ok,
            "available",
            path.display().to_string(),
            None,
        ),
    }
}

#[cfg(unix)]
fn directory_permission_error(metadata: &fs::Metadata, policy: DirectoryPolicy) -> Option<String> {
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

    let mode = metadata.permissions().mode() & 0o777;
    let owner = rustix::process::geteuid().as_raw();
    if metadata.uid() != owner {
        return Some("directory is not owned by the current user".to_owned());
    }
    match policy {
        DirectoryPolicy::Private if mode != 0o700 => {
            Some(format!("expected mode 0700, found {mode:04o}"))
        }
        DirectoryPolicy::Writable if mode & 0o300 != 0o300 => Some(format!(
            "owner lacks write/search permission (mode {mode:04o})"
        )),
        _ => None,
    }
}

#[cfg(not(unix))]
fn directory_permission_error(metadata: &fs::Metadata, policy: DirectoryPolicy) -> Option<String> {
    let _ = policy;
    metadata
        .permissions()
        .readonly()
        .then(|| "directory is read-only".to_owned())
}

fn config_check(path: &Path, status: ConfigStatus) -> DoctorCheck {
    match status {
        ConfigStatus::Parsed => DoctorCheck::new(
            CheckKind::Config,
            "config",
            Severity::Ok,
            "valid",
            path.display().to_string(),
            None,
        ),
        ConfigStatus::Missing => DoctorCheck::new(
            CheckKind::Config,
            "config",
            Severity::Ok,
            "missing",
            path.display().to_string(),
            Some("run `plak config init` if explicit preferences are needed"),
        ),
        ConfigStatus::Unreadable(error) => DoctorCheck::new(
            CheckKind::Config,
            "config",
            Severity::Error,
            "unreadable",
            error,
            Some("restore read access to config.toml"),
        ),
        ConfigStatus::Invalid(error) => DoctorCheck::new(
            CheckKind::Config,
            "config",
            Severity::Error,
            "invalid",
            error,
            Some(
                "run `plak config edit` for the user file, or edit the project/template file named in the error",
            ),
        ),
    }
}

#[cfg(test)]
#[test]
fn invalid_config_remedy_opens_the_file_without_parsing_it() {
    let check = config_check(
        Path::new("/home/test/.plak/config.toml"),
        ConfigStatus::Invalid("unknown key".into()),
    );
    assert!(
        check
            .remediation
            .is_some_and(|hint| hint.contains("plak config edit"))
    );
}

fn database_check(kind: CheckKind, path: &Path, status: DatabaseStatus) -> DoctorCheck {
    match status {
        DatabaseStatus::Available => DoctorCheck::new(
            kind,
            "storage",
            Severity::Ok,
            "available",
            path.display().to_string(),
            None,
        ),
        DatabaseStatus::Missing => DoctorCheck::new(
            kind,
            "storage",
            Severity::Ok,
            "missing",
            path.display().to_string(),
            Some("state is created by the first stateful Plak command"),
        ),
        DatabaseStatus::Incompatible => DoctorCheck::new(
            kind,
            "storage",
            Severity::Error,
            "incompatible",
            path.display().to_string(),
            Some("written by an incompatible plak build; move it aside to start fresh"),
        ),
        DatabaseStatus::Corrupt(error) => DoctorCheck::new(
            kind,
            "storage",
            Severity::Error,
            "corrupt",
            error,
            Some("restore the database from a trusted backup or move it aside to start fresh"),
        ),
        DatabaseStatus::Unavailable(error) => DoctorCheck::new(
            kind,
            "storage",
            Severity::Error,
            "unavailable",
            error,
            Some("restore the database and use its owning recovery workflow"),
        ),
    }
}

fn cwd_check(cwd: Result<PathBuf, String>) -> DoctorCheck {
    match cwd {
        Ok(path) if path.is_absolute() && path.is_dir() => DoctorCheck::new(
            CheckKind::WorkingDirectory,
            "environment",
            Severity::Ok,
            "available",
            path.display().to_string(),
            None,
        ),
        Ok(path) => DoctorCheck::new(
            CheckKind::WorkingDirectory,
            "environment",
            Severity::Error,
            "invalid",
            path.display().to_string(),
            Some("run Plak from an accessible absolute working directory"),
        ),
        Err(error) => DoctorCheck::new(
            CheckKind::WorkingDirectory,
            "environment",
            Severity::Error,
            "unavailable",
            error,
            Some("run Plak from an accessible working directory"),
        ),
    }
}

fn terminal_check(kind: CheckKind, terminal: bool) -> DoctorCheck {
    DoctorCheck::new(
        kind,
        "terminal",
        Severity::Ok,
        if terminal {
            "terminal"
        } else {
            "not a terminal"
        },
        "observed without reading or writing the stream",
        None,
    )
}

fn openrouter_catalog_check(status: OpenRouterCatalogStatus) -> DoctorCheck {
    match status {
        OpenRouterCatalogStatus::NotConfigured => DoctorCheck::new(
            CheckKind::OpenRouterCatalog,
            "provider",
            Severity::Ok,
            "not configured",
            "no OpenRouter key; catalog allowlist is checked at attach",
            None,
        ),
        OpenRouterCatalogStatus::KeyPresentNoModel => DoctorCheck::new(
            CheckKind::OpenRouterCatalog,
            "provider",
            Severity::Warning,
            "key present",
            "point the root template at OpenRouter to probe catalog keys without inference",
            Some(
                "author ~/.plak/agents/<name>.toml with `account = \"openrouter\"` and `model = \"<id>\"`, then `plak config set root.agent <name>`, or `plak run --account openrouter --model <id>`",
            ),
        ),
        OpenRouterCatalogStatus::Ok => DoctorCheck::new(
            CheckKind::OpenRouterCatalog,
            "provider",
            Severity::Ok,
            "catalog matches",
            "authenticated models and endpoints documents validated",
            None,
        ),
        OpenRouterCatalogStatus::Failed(detail) => {
            let remediation = if detail.contains("missing catalog field") {
                "required catalog field was absent; fail closed (no inference)"
            } else {
                "catalog discovery failed closed; named class is in the detail (no inference, no body dump)"
            };
            DoctorCheck::new(
                CheckKind::OpenRouterCatalog,
                "provider",
                Severity::Error,
                "catalog invalid",
                detail,
                Some(remediation),
            )
        }
    }
}

fn spawn_templates_check(status: SpawnTemplatesStatus) -> DoctorCheck {
    let check = |severity, result: &str, detail: String, remediation| {
        DoctorCheck::new(
            CheckKind::SpawnTemplates,
            "config",
            severity,
            result,
            detail,
            remediation,
        )
    };
    match status {
        SpawnTemplatesStatus::None => check(
            Severity::Ok,
            "none",
            "[roster] working_set is empty".to_owned(),
            None,
        ),
        SpawnTemplatesStatus::Ready { count } => check(
            Severity::Ok,
            "attachable",
            format!("{count} spawnable templates"),
            None,
        ),
        SpawnTemplatesStatus::NotReady { templates } => check(
            Severity::Warning,
            "not attachable",
            templates
                .iter()
                .map(|template| {
                    format!(
                        "{} · {} · {}; spawning it is refused",
                        template.name, template.account, template.reason
                    )
                })
                .collect::<Vec<_>>()
                .join("; "),
            Some("run `plak login <account>` for each account named"),
        ),
        SpawnTemplatesStatus::Unknown { detail } => check(
            Severity::Warning,
            "unknown",
            format!("catalog unavailable, readiness not checked: {detail}"),
            Some("run `plak models <account>` to see whether the catalog can be read"),
        ),
    }
}

fn root_template_check(status: RootTemplateStatus) -> DoctorCheck {
    match status {
        RootTemplateStatus::Unset => DoctorCheck::new(
            CheckKind::RootTemplate,
            "config",
            Severity::Ok,
            "unset",
            "no [root] agent; new sessions bind the default selection".to_owned(),
            None,
        ),
        RootTemplateStatus::Undefined { name } => DoctorCheck::new(
            CheckKind::RootTemplate,
            "config",
            Severity::Error,
            "undefined",
            format!(
                "[root] agent = \"{name}\" has no {name}.toml under ~/.plak/agents or .plak/agents"
            ),
            Some("author the template file or clear [root] agent in config.toml"),
        ),
        RootTemplateStatus::Ready { name, binding } => DoctorCheck::new(
            CheckKind::RootTemplate,
            "config",
            Severity::Ok,
            "attachable",
            format!("{name} · {binding}"),
            None,
        ),
        RootTemplateStatus::NotReady {
            name,
            binding,
            reason,
        } => DoctorCheck::new(
            CheckKind::RootTemplate,
            "config",
            Severity::Warning,
            "not attachable",
            format!("{name} · {binding} · {reason}"),
            Some("run `plak login <account>` or rebind the template with `plak config set`"),
        ),
        RootTemplateStatus::Unknown { detail } => DoctorCheck::new(
            CheckKind::RootTemplate,
            "config",
            Severity::Warning,
            "unknown",
            format!("catalog unavailable, readiness not checked: {detail}"),
            Some("run `plak models <account>` to see whether the catalog can be read"),
        ),
    }
}

fn oauth_check(kind: CheckKind, probe: OauthCredentialProbe) -> DoctorCheck {
    let refresh_detail = match &probe.last_refresh {
        OauthRefreshLookup::Row(row) => {
            let failure = row.failure_class.as_deref().unwrap_or("none");
            let transport = row.transport_phase.as_deref().unwrap_or("-");
            let status_class = row.http_status_class.as_deref().unwrap_or("-");
            format!(
                "last refresh {} {failure} {status_class} transport={transport} {} ago",
                row.phase,
                format_age_secs(row.age_secs)
            )
        }
        OauthRefreshLookup::None => "no refresh telemetry".to_owned(),
        OauthRefreshLookup::Unavailable(detail) => {
            format!("refresh telemetry unavailable: {detail}")
        }
    };
    let record_detail = match &probe.record {
        OauthRecordStatus::Missing => "no record".to_owned(),
        OauthRecordStatus::Unreadable(reason) => format!("record unreadable: {reason}"),
        OauthRecordStatus::Present {
            status,
            generation,
            mtime_age_secs,
        } => format!(
            "record {status} · generation {generation} · {} old",
            format_age_secs(*mtime_age_secs)
        ),
    };
    let status = match probe.credential {
        Ok(status) => status,
        Err(cell) => {
            return DoctorCheck::new(
                kind,
                "oauth",
                Severity::Error,
                "unreadable",
                format!("{cell} · {record_detail} · {refresh_detail}"),
                None,
            );
        }
    };
    let verdict = CredentialVerdict::from_status(status);
    let severity = match verdict {
        CredentialVerdict::SignedOut | CredentialVerdict::Usable(CompatibilityState::Current) => {
            Severity::Ok
        }
        CredentialVerdict::Usable(_)
        | CredentialVerdict::NeedsRefresh(_)
        | CredentialVerdict::Quarantined
        | CredentialVerdict::Unsupported => Severity::Warning,
        CredentialVerdict::NeedsLogin(_)
        | CredentialVerdict::WrittenByNewerBuild
        | CredentialVerdict::ProfileInactive => Severity::Error,
    };
    let result = match (verdict, status.credential) {
        (CredentialVerdict::ProfileInactive, _) => "profile_inactive",
        (CredentialVerdict::Usable(CompatibilityState::Required), _) => "acceptance_required",
        (_, CredentialState::SignedOut) => "signed_out",
        (_, CredentialState::Ready) => "ready",
        (_, CredentialState::RefreshRequired) => "refresh_required",
        (_, CredentialState::Quarantined) => "refresh_outcome_unknown",
        (_, CredentialState::Refreshing) => "refreshing",
        (_, CredentialState::Revoked) => "revoked",
        (_, CredentialState::LoginRequired) => "retired_format",
        (_, CredentialState::ProfileChanged) => "profile_changed",
        (_, CredentialState::WrittenByNewerBuild) => "written_by_newer_build",
        (_, CredentialState::Unsupported) => "unsupported",
    };
    let remediation = match (verdict, native_provider(probe.provider)) {
        (CredentialVerdict::SignedOut, _) | (_, None) => None,
        (CredentialVerdict::Usable(CompatibilityState::Required), Some(provider)) => Some(format!(
            "run plak login {} to review and accept the notice",
            crate::commands::support::cli_provider_name(provider)
        )),
        (verdict, Some(provider)) => verdict.remedy(provider, AccountCommand::Status),
    };
    let mut check = DoctorCheck::new(
        kind,
        "oauth",
        severity,
        result,
        format!("{record_detail} · {refresh_detail}"),
        None,
    );
    check.remediation = remediation;
    check
}

fn native_provider(provider: &str) -> Option<NativeProvider> {
    plakous_hosted::capabilities::native_providers()
        .find(|candidate| candidate.as_str() == provider)
}

fn composition_checks(
    requirement: Requirement,
    host: CompositionHostStatus,
    python_containment_required: bool,
    #[cfg_attr(
        not(all(target_os = "linux", target_arch = "x86_64")),
        allow(
            unused_variables,
            reason = "XDG_RUNTIME_DIR is a Linux containment probe only"
        )
    )]
    runtime_directory: Option<&Path>,
) -> Vec<DoctorCheck> {
    let missing_severity = match requirement {
        Requirement::Optional => Severity::Warning,
        Requirement::Required => Severity::Error,
    };
    let mut checks = Vec::new();
    checks.push(match host {
        CompositionHostStatus::Included { identity } => DoctorCheck::new(
            CheckKind::CompositionHost,
            "composition",
            Severity::Ok,
            "included",
            identity,
            None,
        ),
        CompositionHostStatus::Unavailable(error) => DoctorCheck::new(
            CheckKind::CompositionHost,
            "composition",
            missing_severity,
            "unavailable",
            error,
            Some(if python_containment_required {
                composition_host_unavailable_remediation()
            } else {
                javascript_host_unavailable_remediation()
            }),
        ),
    });
    let _ = (python_containment_required, runtime_directory);
    checks
}

fn composition_host_unavailable_remediation() -> &'static str {
    "set PLAK_MONTY_WORKER to the monty 1.2.0 worker or place monty beside plak"
}

fn javascript_host_unavailable_remediation() -> &'static str {
    "set PLAK_JS_HOST to plakous-js-host or place plakous-js-host beside plak"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn doctor_reports_old_telemetry_files_without_modifying_them()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        let current = directory.path().join("telemetry-v10.sqlite3");
        for name in ["telemetry.sqlite3", "telemetry-v9.sqlite3"] {
            fs::write(directory.path().join(name), b"retained")?;
        }
        fs::write(&current, b"current")?;
        let check = telemetry_check(
            CheckKind::TelemetryDatabase,
            &current,
            TelemetryHealth::Ok {
                rows: 1,
                write_errors: 0,
                dropped: 0,
            },
        );
        assert_eq!(check.severity, Severity::Warning);
        assert!(check.detail.contains("telemetry-v9.sqlite3"));
        assert!(check.detail.contains("telemetry.sqlite3"));
        assert!(!check.detail.contains("telemetry-v10.sqlite3"));
        assert_eq!(
            fs::read(directory.path().join("telemetry-v9.sqlite3"))?,
            b"retained"
        );
        Ok(())
    }

    #[test]
    fn doctor_preserves_current_telemetry_failure_remedy() -> Result<(), Box<dyn std::error::Error>>
    {
        let directory = tempfile::tempdir()?;
        let current = directory.path().join("telemetry-v17.sqlite3");
        fs::write(directory.path().join("telemetry-v9.sqlite3"), b"retained")?;
        for reason in ["unavailable", "schema mismatch"] {
            let check = telemetry_check(
                CheckKind::TelemetryDatabase,
                &current,
                TelemetryHealth::Unavailable(reason.to_owned()),
            );
            assert_eq!(check.severity, Severity::Error);
            assert_eq!(
                check.remediation.as_deref(),
                Some("restore telemetry storage or explicitly remove the reported file to rebuild")
            );
            assert!(check.detail.starts_with(reason));
        }
        Ok(())
    }

    #[test]
    fn doctor_labels_newer_telemetry_files_without_warning_or_removal_advice()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        let current = directory.path().join("telemetry-v19.sqlite3");
        fs::write(directory.path().join("telemetry-v20.sqlite3"), b"retained")?;
        let check = telemetry_check(
            CheckKind::TelemetryDatabase,
            &current,
            TelemetryHealth::Ok {
                rows: 1,
                write_errors: 0,
                dropped: 0,
            },
        );
        assert_eq!(check.severity, Severity::Ok);
        assert_eq!(check.remediation, None);
        assert!(
            check
                .detail
                .contains("newer-schema files left untouched: telemetry-v20.sqlite3")
        );
        assert_eq!(
            fs::read(directory.path().join("telemetry-v20.sqlite3"))?,
            b"retained"
        );
        Ok(())
    }

    #[test]
    fn store_pressure_is_full_at_the_ordinary_threshold_and_warns_one_reserve_before() {
        // A 16-checkpoint store: reserve 2, new work stops at 14.
        assert_eq!(StorePressure::of(11, 2, 14), StorePressure::Available);
        assert_eq!(StorePressure::of(12, 2, 14), StorePressure::NearlyFull);
        assert_eq!(StorePressure::of(14, 2, 14), StorePressure::Full);
        assert_eq!(StorePressure::of(17, 2, 14), StorePressure::Full);
        assert!(StorePressure::Full > StorePressure::NearlyFull);
    }
    #[test]
    fn last_failed_refresh_fixture_ignores_began_and_clears_on_completed()
    -> Result<(), Box<dyn std::error::Error>> {
        let (_guard, paths) = crate::product_paths::PlakPaths::isolated_test_home();
        paths.ensure_layout()?;
        let zone = jiff::tz::TimeZone::fixed(jiff::tz::offset(-4));
        assert_eq!(last_failed_refresh(&paths, "claude", &zone), None);
        assert!(!paths.telemetry_database().exists());
        drop(crate::telemetry_store::TelemetryStore::open(&paths)?);
        let db = rusqlite::Connection::open(paths.telemetry_database())?;
        // 2024-07-03 09:46:40 UTC, shown in a fixed UTC-4 zone.
        db.execute_batch(
            "INSERT INTO credential_refreshes
            (at, provider, phase, failure_class, transport_phase, generation) VALUES
            (1720000000000000000, 'claude', 'completed', NULL, NULL, 1),
            (1720000100000000000, 'claude', 'restored', 'timeout', 'resolution', 2),
            (1720000200000000000, 'claude', 'began', NULL, NULL, 3),
            (1720000300000000000, 'codex', 'completed', NULL, NULL, 9);",
        )?;
        assert_eq!(
            last_failed_refresh(&paths, "claude", &zone).as_deref(),
            Some(
                "refresh failed at 05:48: network unreachable (DNS lookup timed out); credential unchanged; retry when online"
            )
        );
        assert_eq!(last_failed_refresh(&paths, "codex", &zone), None);
        // Same timestamp is resolved by insertion id, never nondeterministically.
        db.execute_batch(
            "INSERT INTO credential_refreshes
            (at, provider, phase, generation) VALUES
            (1720000200000000000, 'claude', 'completed', 3),
            (1720000300000000000, 'claude', 'began', 4);",
        )?;
        assert_eq!(last_failed_refresh(&paths, "claude", &zone), None);
        db.execute_batch(
            "INSERT INTO credential_refreshes
            (at, provider, phase, failure_class, transport_phase, generation) VALUES
            (1720000200000000000, 'claude', 'outcome_unknown', 'transport', 'send', 4);",
        )?;
        let message = last_failed_refresh(&paths, "claude", &zone).ok_or("missing failure")?;
        assert!(message.contains("while sending"), "{message}");
        assert!(message.contains("refresh outcome unknown"), "{message}");
        assert!(!message.contains("credential unchanged"), "{message}");
        Ok(())
    }

    #[test]
    fn last_failed_refresh_names_the_sub_reason_and_a_retired_record_needs_login()
    -> Result<(), Box<dyn std::error::Error>> {
        let (_guard, paths) = crate::product_paths::PlakPaths::isolated_test_home();
        paths.ensure_layout()?;
        drop(crate::telemetry_store::TelemetryStore::open(&paths)?);
        let db = rusqlite::Connection::open(paths.telemetry_database())?;
        let zone = jiff::tz::TimeZone::UTC;
        db.execute_batch(
            "INSERT INTO credential_refreshes
            (at, provider, phase, failure_class, failure_reason, generation) VALUES
            (1720000100000000000, 'xai', 'restored', 'validation', 'id_token_exp', 2);",
        )?;
        let message = last_failed_refresh(&paths, "grok", &zone).ok_or("missing row")?;
        assert!(
            message.contains("refresh response could not be validated (id_token_exp)"),
            "{message}"
        );
        db.execute_batch(
            "INSERT INTO credential_refreshes
            (at, provider, phase, failure_class, failure_reason, generation) VALUES
            (1720000200000000000, 'codex', 'restored', 'validation', 'retired_format', 5);",
        )?;
        let message = last_failed_refresh(&paths, "codex", &zone).ok_or("missing row")?;
        assert!(
            message.contains("written by an older plak build"),
            "{message}"
        );
        assert!(!message.contains("could not be validated"), "{message}");
        assert!(message.ends_with("run plak login codex"), "{message}");
        assert!(!message.contains("plak refresh"), "{message}");
        Ok(())
    }

    #[test]
    fn last_failed_refresh_hides_rows_older_than_the_current_credential()
    -> Result<(), Box<dyn std::error::Error>> {
        assert!(refresh_row_is_current(215, None));
        assert!(refresh_row_is_current(215, Some(215)));
        assert!(refresh_row_is_current(215, Some(216)));
        assert!(!refresh_row_is_current(215, Some(217)));

        let (_guard, paths) = crate::product_paths::PlakPaths::isolated_test_home();
        paths.ensure_layout()?;
        let state = tempfile::tempdir()?;
        let record = state.path().join("plak/subscriptions/xai");
        fs::create_dir_all(&record)?;
        fs::write(
            record.join("credential.json"),
            br#"{"status":"ready","generation":217}"#,
        )?;
        install_credential_state_home(state.path().to_path_buf());
        drop(crate::telemetry_store::TelemetryStore::open(&paths)?);
        let db = rusqlite::Connection::open(paths.telemetry_database())?;
        db.execute_batch(
            "INSERT INTO credential_refreshes
            (at, provider, phase, failure_class, generation) VALUES
            (1720000100000000000, 'xai', 'restored', 'validation', 215);",
        )?;
        let zone = jiff::tz::TimeZone::UTC;
        assert_eq!(last_failed_refresh(&paths, "grok", &zone), None);
        db.execute_batch(
            "INSERT INTO credential_refreshes
            (at, provider, phase, failure_class, generation) VALUES
            (1720000200000000000, 'xai', 'restored', 'validation', 216);",
        )?;
        assert!(last_failed_refresh(&paths, "grok", &zone).is_some());
        Ok(())
    }

    #[test]
    fn last_failed_refresh_fixture_non_network_and_quarantine_are_truthful()
    -> Result<(), Box<dyn std::error::Error>> {
        let (_guard, paths) = crate::product_paths::PlakPaths::isolated_test_home();
        paths.ensure_layout()?;
        drop(crate::telemetry_store::TelemetryStore::open(&paths)?);
        let db = rusqlite::Connection::open(paths.telemetry_database())?;
        let zone = jiff::tz::TimeZone::UTC;
        for (class, phase, expected) in [
            ("http_4xx", "restored", "provider returned HTTP 401"),
            (
                "validation",
                "quarantined",
                "refresh response could not be validated",
            ),
            ("store", "outcome_unknown", "credential storage failure"),
            ("not_ready", "restored", "refresh dispatch was not ready"),
        ] {
            db.execute(
                "INSERT INTO credential_refreshes
                (at, provider, phase, failure_class, http_status, generation)
                VALUES (1720000100000000000, 'xai', ?1, ?2, 401, 2)",
                rusqlite::params![phase, class],
            )?;
            let message = last_failed_refresh(&paths, "grok", &zone).ok_or("missing row")?;
            assert!(message.contains(expected), "{message}");
            assert!(!message.contains("network unreachable"), "{message}");
            assert!(!message.contains("retry when online"), "{message}");
            assert_eq!(
                message.contains("credential unchanged"),
                phase == "restored"
            );
        }
        Ok(())
    }

    #[test]
    fn last_failed_refresh_fixture_unreadable_schema_is_not_rebuilt()
    -> Result<(), Box<dyn std::error::Error>> {
        let (_guard, paths) = crate::product_paths::PlakPaths::isolated_test_home();
        paths.ensure_layout()?;
        drop(crate::telemetry_store::TelemetryStore::open(&paths)?);
        let db = rusqlite::Connection::open(paths.telemetry_database())?;
        db.pragma_update(None, "user_version", 999)?;
        assert_eq!(
            last_failed_refresh(&paths, "claude", &jiff::tz::TimeZone::UTC),
            None
        );
        assert_eq!(
            db.pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))?,
            999
        );
        Ok(())
    }
}
