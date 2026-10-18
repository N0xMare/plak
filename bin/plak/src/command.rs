//! The parsed `plak` command and the classifiers the dispatcher reads.

use std::path::PathBuf;

use plakous::agent::ReasoningEffort;
use plakous::subscriptions::{NativeLoginMode, NativeProvider};

use crate::{CliFailureKind, product_config};

pub(crate) const NATIVE_RUN_MAX_PROMPT_BYTES: usize = 64 * 1024;
#[allow(
    dead_code,
    reason = "documented input ceiling reserved for future budget checks"
)]
const NATIVE_RUN_INPUT_UPPER_BOUND_TOTAL: u64 = 1024 * 1024;

#[derive(Clone, Debug, PartialEq)]
#[allow(
    clippy::enum_variant_names,
    reason = "CommandHelp is a recursive help subcommand name"
)]
#[cfg_attr(
    not(feature = "maintainer"),
    allow(
        dead_code,
        reason = "maintainer verbs stay on Command for execute/tests; parse cannot emit them"
    )
)]
pub(crate) enum Command {
    Help,
    CommandHelp {
        command: String,
    },
    Version,
    Tui {
        session_id: Option<String>,
        resume_picker: bool,
    },
    Resume {
        session_id: Option<String>,
    },
    Fork {
        session_id: String,
        confirmed: bool,
    },
    Compact {
        session_id: String,
        confirmed: bool,
    },
    Doctor,
    StatusAll,
    ConfigShow,
    ConfigSet {
        key: String,
        value: String,
        scope: product_config::ConfigWriteScope,
    },
    ConfigEdit,
    ConfigInit,
    /// Record the discovered project `.plak` as trusted configuration.
    ConfigTrust,
    /// Remove the discovered project `.plak` from the trust registry.
    ConfigUntrust,
    KeySet {
        account: String,
    },
    KeyClear {
        account: String,
    },
    KeyStatus {
        account: Option<String>,
    },
    SecretSet {
        name: String,
    },
    SecretRm {
        name: String,
    },
    SecretList,
    Usage {
        account: Option<String>,
        session_id: Option<String>,
    },
    SessionList,
    SessionShow {
        id: String,
    },
    SessionRecover {
        id: String,
    },
    #[cfg(feature = "maintainer")]
    SessionRehearse {
        id: String,
    },
    SessionExport {
        id: String,
        out: Option<PathBuf>,
    },
    SessionImport {
        path: PathBuf,
    },
    SessionReference {
        path: PathBuf,
        search: Option<String>,
        tail: usize,
    },
    SessionDelete {
        id: String,
        yes: bool,
        abandon_children: bool,
    },
    /// `plak session gc <id>` or, with `None`, `plak session gc --all`.
    SessionGc {
        id: Option<String>,
    },
    /// Hidden `plak --import-resume <file>`: import, delete the file, resume.
    ImportResume {
        path: PathBuf,
    },
    /// Hidden `plak version --state-key`.
    StateKey,
    ReloadProbe,
    ExportVersion,
    StatePruneLegacy {
        yes: bool,
        forget_missing_launches: bool,
        prune_orphaned_references: bool,
    },
    SessionLog {
        id: String,
        kind: Option<String>,
        tail: usize,
        json: bool,
    },
    TelemetryStatus {
        views: bool,
    },
    TelemetrySql {
        query: String,
        json: bool,
    },
    TelemetryPrune {
        max_age_days: Option<u64>,
        max_bytes: Option<u64>,
    },
    CompatibilityNotice(NativeProvider),
    AuthStatus(NativeProvider),
    AuthLogin {
        provider: NativeProvider,
        mode: NativeLoginMode,
    },
    AuthRefresh(NativeProvider),
    AuthLogout {
        provider: NativeProvider,
        local_only: bool,
    },
    Models(NativeProvider),
    /// Authenticated OpenRouter catalog identifier listing.
    ModelsOpenRouter,
    ModelsPlatform(plakous_hosted::binding::PlatformProvider),
    Run {
        provider: NativeProvider,
        model: String,
        /// Reasoning effort, exactly as the operator typed `--effort` (SuperGrok/Codex
        /// portable: low|medium|high; the CLI also accepts xhigh|max|ultra and adapters
        /// fail closed when unsupported). `None` means the flag was absent; there is no
        /// silent default.
        effort: Option<ReasoningEffort>,
        composition: Option<product_config::ProductCompositionSelection>,
        prompt: plakous_host_protocol::Draft,
        print_session_id: bool,
        limits: plakous_hosted::runtime::RunBounds,
    },
    RunOpenRouter {
        model: String,
        effort: Option<ReasoningEffort>,
        composition: product_config::ProductCompositionSelection,
        wire: product_config::ProductWireBinding,
        prompt: plakous_host_protocol::Draft,
        print_session_id: bool,
        limits: plakous_hosted::runtime::RunBounds,
    },
    RunPlatform {
        provider: plakous_hosted::binding::PlatformProvider,
        model: String,
        effort: ReasoningEffort,
        composition: product_config::ProductCompositionSelection,
        prompt: plakous_host_protocol::Draft,
        print_session_id: bool,
        limits: plakous_hosted::runtime::RunBounds,
    },
    ContinueSession {
        session_id: String,
        prompt: plakous_host_protocol::Draft,
        print_session_id: bool,
        limits: plakous_hosted::runtime::RunBounds,
        resume_tree: bool,
    },
    Agent {
        session_id: Option<String>,
        provider: Option<NativeProvider>,
        platform: Option<plakous_hosted::binding::PlatformProvider>,
        wire: Option<product_config::ProductWireBinding>,
        model: Option<String>,
        effort: Option<ReasoningEffort>,
        effort_explicit: bool,
        composition: Option<product_config::ProductCompositionSelection>,
        resume_tree: bool,
    },
}

/// Operator-selected reasoning effort for subscription runs.
///
/// Dialects fail closed when a model does not admit a level (e.g. SuperGrok
/// rejects xhigh/max/ultra). Preferred capability keeps runs usable if omitted.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RunEffort {
    None,
    Low,
    Medium,
    High,
    XHigh,
    Max,
    Ultra,
}

/// The effort words `--effort` accepts, spelled from the one shared ladder
/// (`plakous_hosted::effort`) that templates and this flag both read. No surface
/// carries its own list.
pub(crate) fn run_effort_levels() -> String {
    plakous_hosted::effort::levels_pipe_list()
}

/// The one `--effort` usage message, in the ladder's own words.
pub(crate) fn run_effort_usage() -> String {
    format!("effort must be {}", run_effort_levels())
}

impl RunEffort {
    pub(crate) fn as_reasoning_effort(self) -> ReasoningEffort {
        match self {
            Self::Low => ReasoningEffort::Low,
            Self::Medium => ReasoningEffort::Medium,
            Self::High => ReasoningEffort::High,
            Self::XHigh => ReasoningEffort::XHigh,
            Self::Max => ReasoningEffort::Max,
            Self::Ultra => ReasoningEffort::Ultra,
            Self::None => ReasoningEffort::None,
        }
    }

    /// Named levels come from the shared ladder, so a level cannot exist in a
    /// template and be missing from `--effort`.
    pub(crate) fn parse(value: &str) -> Option<Self> {
        match plakous_hosted::effort::parse(value)? {
            "none" => Some(Self::None),
            "low" => Some(Self::Low),
            "medium" => Some(Self::Medium),
            "high" => Some(Self::High),
            "xhigh" => Some(Self::XHigh),
            "max" => Some(Self::Max),
            "ultra" => Some(Self::Ultra),
            _ => None,
        }
    }
}

impl Command {
    pub(crate) const fn diagnostic_verb(&self) -> &'static str {
        match self {
            Self::Help | Self::CommandHelp { .. } => "help",
            Self::Version | Self::StateKey | Self::ReloadProbe | Self::ExportVersion => "version",
            Self::Tui { .. } => "tui",
            Self::Resume { .. } | Self::ImportResume { .. } => "resume",
            Self::Fork { .. } => "fork",
            Self::Compact { .. } => "compact",
            Self::Doctor => "doctor",
            Self::StatusAll | Self::AuthStatus(_) => "status",
            Self::ConfigShow
            | Self::ConfigSet { .. }
            | Self::ConfigEdit
            | Self::ConfigInit
            | Self::ConfigTrust
            | Self::ConfigUntrust => "config",
            Self::KeySet { .. } | Self::KeyClear { .. } | Self::KeyStatus { .. } => "key",
            Self::SecretSet { .. } | Self::SecretRm { .. } | Self::SecretList => "secret",
            Self::Usage { .. } => "usage",
            Self::SessionList
            | Self::SessionShow { .. }
            | Self::SessionRecover { .. }
            | Self::SessionExport { .. }
            | Self::SessionImport { .. }
            | Self::SessionReference { .. }
            | Self::SessionDelete { .. }
            | Self::SessionGc { .. }
            | Self::SessionLog { .. } => "session",
            #[cfg(feature = "maintainer")]
            Self::SessionRehearse { .. } => "session",
            Self::StatePruneLegacy { .. } => "state",
            Self::TelemetryStatus { .. }
            | Self::TelemetrySql { .. }
            | Self::TelemetryPrune { .. } => "telemetry",
            Self::CompatibilityNotice(_) => "compatibility",
            Self::AuthLogin { .. } => "login",
            Self::AuthRefresh(_) => "refresh",
            Self::AuthLogout { .. } => "logout",
            Self::Models(_) | Self::ModelsOpenRouter | Self::ModelsPlatform(_) => "models",
            Self::Run { .. }
            | Self::RunOpenRouter { .. }
            | Self::RunPlatform { .. }
            | Self::ContinueSession { .. } => "run",
            Self::Agent { .. } => "agent",
        }
    }

    pub(crate) const fn default_failure_kind(&self) -> CliFailureKind {
        match self {
            Self::AuthStatus(_)
            | Self::AuthLogin { .. }
            | Self::AuthRefresh(_)
            | Self::AuthLogout { .. }
            | Self::Models(_)
            | Self::ModelsOpenRouter => CliFailureKind::Auth,
            _ => CliFailureKind::Internal,
        }
    }
}

pub(crate) fn command_needs_product_state(command: &Command) -> bool {
    matches!(
        command,
        Command::Run { .. }
            | Command::RunOpenRouter { .. }
            | Command::RunPlatform { .. }
            | Command::ContinueSession { .. }
            | Command::Agent { .. }
            | Command::SessionList
            | Command::SessionRecover { .. }
            | Command::SessionExport { .. }
            | Command::SessionImport { .. }
            | Command::SessionDelete { .. }
            | Command::SessionGc { .. }
            | Command::Fork { .. }
            | Command::Compact { .. }
            | Command::Usage {
                session_id: Some(_),
                ..
            }
    )
}

pub(crate) fn command_is_read_only(command: &Command) -> bool {
    matches!(
        command,
        Command::StatusAll
            | Command::AuthStatus(_)
            | Command::Doctor
            | Command::KeyStatus { .. }
            | Command::SecretList
            | Command::ConfigShow
            | Command::SessionShow { .. }
            | Command::SessionLog { .. }
            | Command::SessionReference { .. }
    )
}

/// Whether this command runs a model turn on the CLI face.
///
/// Only these arms reach `commands::run::run_config`, and only they leave a
/// resumable session behind, so only they need the watcher. The JSONL face
/// installs its own, with its own announcement, at the top of `run_agent`.
pub(crate) fn runs_a_cli_turn(command: &Command) -> bool {
    matches!(
        command,
        Command::Run { .. }
            | Command::RunOpenRouter { .. }
            | Command::RunPlatform { .. }
            | Command::ContinueSession { .. }
    )
}
