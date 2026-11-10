//! Documented config keys and the parsers that admit their values.

use super::load::ProductConfig;
use super::policy::{CEILING_MAX_MODEL_ROUNDS, MAX_SCROLL_LINES, MIN_SCROLL_LINES};

/// A value on its way into one documented key: the TOML value a file carried,
/// or the single unquoted argument `plak config set` takes.
///
/// The two spellings differ for exactly three keys — `[root] agent` reads an
/// empty argument as *clear the pointer*, `[roster] working_set` also accepts
/// a shell shorthand, and a command-line argument is never quoted — so the
/// distinction is the value's own, not a flag beside it.
#[derive(Clone, Copy, Debug)]
pub(super) enum FieldValue<'a> {
    /// A value read out of `config.toml`, with its TOML type.
    Toml(&'a toml::Value),
    /// One `plak config set` argument, as the shell handed it over.
    Cli(&'a str),
}

/// The working set's array, and the one sentence that names it.
const WORKING_SET_MUST_BE_AN_ARRAY: &str =
    "[roster] working_set must be an array like [\"a\", \"b\"]";

impl<'a> FieldValue<'a> {
    /// The value as a string. A file must spell it as a TOML string.
    pub(super) fn string(self, key: &ConfigKey) -> Result<&'a str, String> {
        match self {
            Self::Cli(value) => Ok(value),
            Self::Toml(toml::Value::String(value)) => Ok(value.as_str()),
            Self::Toml(_) => Err(format!(
                "[{}] {} must be a string",
                key.section(),
                key.key()
            )),
        }
    }

    /// A real TOML boolean. `1`, `yes`, `0` and `no` are not booleans and the
    /// file grammar no longer pretends they are.
    pub(super) fn boolean(self) -> Result<bool, String> {
        match self {
            Self::Cli("true") => Ok(true),
            Self::Cli("false") => Ok(false),
            Self::Toml(toml::Value::Boolean(value)) => Ok(*value),
            _ => Err("boolean config values must be true or false".to_owned()),
        }
    }

    /// The integer the value carries, or `None` when it is not one. The key's
    /// own bound then says what a number out of range is called.
    pub(super) fn integer(self) -> Option<i64> {
        match self {
            Self::Cli(value) => value.trim().parse().ok(),
            Self::Toml(toml::Value::Integer(value)) => Some(*value),
            Self::Toml(_) => None,
        }
    }

    /// The working set: the file's own array of strings, or, from the command
    /// line, that same array or the comma- or space-separated shorthand a
    /// shell is easier to type. One key, one meaning, so a copy-paste out of
    /// `config.toml` cannot be the wrong grammar.
    pub(super) fn names(self) -> Result<Vec<String>, String> {
        self.strings(WORKING_SET_MUST_BE_AN_ARRAY)
    }

    /// An array of strings, from the file or the command line, with the
    /// command line's comma- or space-separated shorthand; `must_be` is the
    /// key's sentence for anything else.
    pub(super) fn strings(self, must_be: &str) -> Result<Vec<String>, String> {
        let array = |values: &[toml::Value]| {
            values
                .iter()
                .map(|value| match value {
                    toml::Value::String(name) => Ok(name.clone()),
                    _ => Err(must_be.to_owned()),
                })
                .collect()
        };
        match self {
            Self::Toml(toml::Value::Array(values)) => array(values),
            Self::Toml(_) => Err(must_be.to_owned()),
            Self::Cli(value) if value.trim().starts_with('[') => {
                let parsed: toml::Value = toml::from_str(&format!("v = {}", value.trim()))
                    .map(|table: toml::Table| table["v"].clone())
                    .map_err(|_| must_be.to_owned())?;
                match parsed {
                    toml::Value::Array(values) => array(&values),
                    _ => Err(must_be.to_owned()),
                }
            }
            Self::Cli(value) => Ok(value
                .split([',', ' '])
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .map(str::to_owned)
                .collect()),
        }
    }
}

/// A key the table does not carry. Removed keys get no named error: they are
/// unknown keys like any other.
pub(super) fn unknown_key(section: &str, key: &str) -> String {
    match (section, key) {
        ("roster", _) => {
            format!("[roster] only accepts working_set; move {key} into agents/<name>.toml")
        }
        // A key above the first header belongs to no section; the whole of
        // its spelling is the name the reader has to fix.
        ("", _) => format!("unknown config key: {key}"),
        _ => format!("unknown config key: {section}.{key}"),
    }
}

/// The removed sections name their replacement.
pub(super) fn removed_or_unknown_section(section: &str) -> String {
    if let Some(template) = section.strip_prefix("agent.") {
        return format!(
            "[agent.{template}] in config.toml is no longer a template; move it to ~/.plak/agents/{template}.toml"
        );
    }
    if section == "default" {
        return "unknown config section [default]; tuple fields moved to template files under ~/.plak/agents with [root] agent, and subagents moved to [orchestration]".to_owned();
    }
    if section.starts_with("roster.") {
        return format!(
            "unknown config section [{section}]; [roster] holds only working_set = [\"<template>\", ...]"
        );
    }
    if section == "tools" || section.starts_with("tools.") {
        return format!(
            "unknown config section [{section}]; [tools.<name>] takes fetch_url, web_search or x_search"
        );
    }
    format!("unknown config section [{section}]")
}

/// The key sections `config.toml` accepts, in render order, each with the one
/// sentence the reference file prints above it.
pub const CONFIG_SECTION_DOCS: &[(&str, &str)] = &[
    ("root", "The template new sessions bind at genesis."),
    ("roster", "The templates spawnable this epoch."),
    ("orchestration", "Tree shape and spawn authority."),
    (
        "turn",
        "How long one root turn may run; there is no wall clock.",
    ),
    (
        "jobs",
        "Session-owned process budgets and hard runtime caps.",
    ),
    (
        "api",
        "Direct API-key routes: the input window new sessions are admitted under.",
    ),
    (
        "tui",
        "Terminal-app preferences; nothing here reaches a provider.",
    ),
    ("compaction", "Automatic transcript compaction."),
    (
        "skills",
        "Agent Skills under .agents/skills and ~/.agents/skills.",
    ),
    (
        "telemetry",
        "Local analytics store bounds; nothing leaves the machine.",
    ),
    (
        "mcp",
        "The MCP client; servers are [mcp_servers.<id>] tables.",
    ),
    (
        "host",
        "The per-user host: how it treats work a lost host left.",
    ),
    (
        "web",
        "The maintainer web endpoint (plak host --web): who it admits and where it is served.",
    ),
];

/// Sections an earlier build refuses as unknown. A render writes one live
/// only when one of its keys is set, so a file `plak` writes stays loadable
/// by those builds until the operator opts in.
pub(crate) const FORWARD_ONLY_SECTIONS: &[&str] = &["host", "web"];

/// One documented `config.toml` key.
///
/// This table is the one source for the key list: the file parser
/// ([`ProductConfig::parse`]), the `plak config set` grammar
/// ([`ProductConfig::set_key`]), the renderer behind `plak config show`,
/// `docs/reference-config/config.toml` ([`ProductConfig::render_annotated`]) and
/// the `docs/spec/07-config-reference.md` §3 table all read it, so none of
/// them can disagree. The default column is not stored: it is what the schema
/// default renders ([`ConfigKey::default_value`]).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ConfigKey {
    /// Dotted spelling, which is also the `plak config set` key.
    pub path: &'static str,
    /// The `ProductConfig` field this key reads and writes.
    pub field: ConfigField,
    /// Value type, in the words the error messages use.
    pub kind: &'static str,
    /// Accepted range or value set; empty when the type is the whole rule.
    pub constraint: &'static str,
    /// What the key does, in one sentence.
    pub doc: &'static str,
    /// Value shown in the reference file when the key carries no value.
    pub example: &'static str,
    /// Whether a save of this key has to move through a successor epoch.
    pub scope: ConfigScope,
}

/// Whether changing a key takes a successor-epoch apply.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConfigScope {
    /// `[root]`, `[roster]` and `[orchestration]`: the tree's own rules, which
    /// only a new epoch may change.
    Epoch,
    /// Everything else: no successor-epoch publication is required. The
    /// owning read site still decides when a running host observes it.
    Immediate,
}

/// The `ProductConfig` field one key reads and writes.
///
/// Naming the field rather than matching the key spelling is what keeps the
/// parser, the setter and the renderer honest: each is one exhaustive match
/// the compiler checks, and the spellings live only in [`CONFIG_KEYS`].
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum ConfigField {
    /// `[root] agent`.
    RootAgent,
    /// `[root] composition`, independent of child template capabilities.
    RootComposition,
    /// `[roster] working_set`.
    WorkingSet,
    /// `[orchestration] subagents`.
    Subagents,
    /// `[turn] auto_continue`.
    AutoContinue,
    /// `[orchestration] max_depth`.
    MaxDepth,
    /// `[orchestration] max_children`.
    MaxChildren,
    /// `[orchestration] max_nodes`.
    MaxNodes,
    /// Dormant checkout reclamation control.
    WorkspaceBudgetMib,
    ChildRetentionDays,
    ChildRetentionCount,
    /// Dormant checkout reclamation control.
    WorkspaceIdleSeconds,
    /// `[orchestration] max_attempts`.
    MaxAttempts,
    /// `[orchestration] transient_resumes`.
    TransientResumes,
    /// `[orchestration] max_concurrent_requests_per_account`.
    RequestsPerAccount,
    /// `[orchestration] require_review_evidence`.
    RequireReviewEvidence,
    /// `[turn] max_model_rounds`.
    MaxModelRounds,
    /// `[jobs] foreground_seconds`.
    JobsForegroundSeconds,
    /// `[jobs] max_runtime_hours`.
    JobsMaxRuntimeHours,
    /// `[api] full_window`.
    ApiFullWindow,
    /// `[tui] mouse`.
    TuiMouse,
    /// `[tui] theme`.
    Theme,
    /// `[tui] scroll_lines`.
    ScrollLines,
    /// `[tui] show_thinking`.
    ShowThinking,
    /// `[tui] group_tool_calls`.
    GroupToolCalls,
    /// `[tui] reduced_motion`.
    ReducedMotion,
    /// `[tui] reload_requests`.
    ReloadRequests,
    /// `[compaction] enabled`.
    CompactionEnabled,
    /// `[skills] enabled`.
    SkillsEnabled,
    /// `[telemetry] max_bytes`.
    TelemetryMaxBytes,
    /// `[telemetry] max_age_days`.
    TelemetryMaxAgeDays,
    /// `[mcp] client_id_metadata_url`.
    McpClientIdMetadataUrl,
    /// `[host] resume_after_loss`.
    ResumeAfterLoss,
    /// `[web] allow`.
    WebAllow,
    /// `[web] origin`.
    WebOrigin,
    /// Attachment-free web console reads; home-only, captured at host start.
    WebAccountsRead,
    WebConfigRead,
}

impl ConfigField {
    /// Orchestration authority comes from the operator's product home.
    pub(crate) const fn is_home_only(self) -> bool {
        matches!(
            self,
            Self::Subagents
                | Self::AutoContinue
                | Self::MaxDepth
                | Self::MaxChildren
                | Self::MaxNodes
                | Self::MaxAttempts
                | Self::TransientResumes
                | Self::RequestsPerAccount
                | Self::RequireReviewEvidence
                | Self::McpClientIdMetadataUrl
                | Self::ReloadRequests
                | Self::ResumeAfterLoss
                | Self::WebAllow
                | Self::WebOrigin
                | Self::WebAccountsRead
                | Self::WebConfigRead
        )
    }

    /// The home-only keys a project file names under `[orchestration]`:
    /// every one but those with their own table and warning.
    pub(crate) const fn is_home_orchestration(self) -> bool {
        self.is_home_only()
            && !matches!(
                self,
                Self::McpClientIdMetadataUrl
                    | Self::ResumeAfterLoss
                    | Self::WebAllow
                    | Self::WebOrigin
                    | Self::WebAccountsRead
                    | Self::WebConfigRead
            )
    }
}

impl ConfigKey {
    /// Section the key lives in, without brackets.
    #[must_use]
    pub fn section(&self) -> &'static str {
        self.split().0
    }

    /// Key spelling inside the section.
    #[must_use]
    pub fn key(&self) -> &'static str {
        self.split().1
    }

    fn split(&self) -> (&'static str, &'static str) {
        match self.path.split_once('.') {
            Some(parts) => parts,
            None => (self.path, self.path),
        }
    }

    /// The literal the schema default renders, or `(unset)` for an
    /// optional key. Derived, so a changed default cannot leave a stale
    /// comment behind in the reference file or the spec.
    #[must_use]
    pub fn default_value(&self) -> String {
        ProductConfig::default_product()
            .field_value(self.field)
            .map_or_else(|| "(unset)".to_owned(), |value| value.to_string())
    }

    /// The two comment lines the reference file prints above the key.
    #[must_use]
    pub fn comment_block(&self) -> String {
        let mut facts = format!("type: {} · default: {}", self.kind, self.default_value());
        if !self.constraint.is_empty() {
            facts.push_str(&format!(" · {}", self.constraint));
        }
        format!("\n# {}\n# {facts}\n", self.doc)
    }
}

/// The table row a `plak config set` key names by its dotted spelling.
#[must_use]
pub fn config_key(key: &str) -> Option<&'static ConfigKey> {
    CONFIG_KEYS.iter().find(|row| row.path == key)
}

/// Every `config.toml` key, in render order.
pub const CONFIG_KEYS: &[ConfigKey] = &[
    ConfigKey {
        path: "root.composition",
        field: ConfigField::RootComposition,
        kind: "enum",
        constraint: "direct|plakous-default|native-ptc|js-ptc|py-ptc|py-sdk",
        doc: "Root-only composition override for the exact root account/model tuple, validated against its account/model. Changing root.agent or applying a profile clears it. Children keep their own template composition. py-sdk uses the typed Python SDK and is not valid in child templates.",
        example: "\"py-sdk\"",
        scope: ConfigScope::Epoch,
    },
    ConfigKey {
        path: "root.agent",
        field: ConfigField::RootAgent,
        kind: "string",
        constraint: "names an agents/<name>.toml file; [a-z][a-z0-9-]{0,31}, and never a section name",
        doc: "Template a new session binds at genesis; unset leaves the tuple to the CLI flags or the shell binding.",
        example: "\"root-agent\"",
        scope: ConfigScope::Epoch,
    },
    ConfigKey {
        path: "roster.working_set",
        field: ConfigField::WorkingSet,
        kind: "array of string",
        constraint: "0..=5 names, no duplicates, each a template file with a description",
        doc: "Templates the root may spawn this epoch; every other node spawns from its own template's spawns list.",
        example: "[\"worker\", \"reviewer\"]",
        scope: ConfigScope::Epoch,
    },
    ConfigKey {
        path: "orchestration.subagents",
        field: ConfigField::Subagents,
        kind: "bool",
        constraint: "true or false",
        doc: "Whether the topology tools are compiled into the root catalog at all.",
        example: "true",
        scope: ConfigScope::Epoch,
    },
    ConfigKey {
        path: "orchestration.max_depth",
        field: ConfigField::MaxDepth,
        kind: "integer",
        constraint: "1..=4",
        doc: "Deepest node the tree admits, counting the root as depth 0.",
        example: "4",
        scope: ConfigScope::Epoch,
    },
    ConfigKey {
        path: "orchestration.max_children",
        field: ConfigField::MaxChildren,
        kind: "integer",
        constraint: "1..=64",
        doc: "Active or undelivered direct children; dormant children release slots.",
        example: "8",
        scope: ConfigScope::Epoch,
    },
    ConfigKey {
        path: "orchestration.max_nodes",
        field: ConfigField::MaxNodes,
        kind: "integer",
        constraint: "1..=65",
        doc: "Root plus active, undelivered or recovery-required nodes.",
        example: "32",
        scope: ConfigScope::Epoch,
    },
    ConfigKey {
        path: "orchestration.workspace_idle_seconds",
        field: ConfigField::WorkspaceIdleSeconds,
        kind: "integer",
        constraint: "1..=65535",
        doc: "Idle seconds before lossless dormant-checkout reclamation.",
        example: "600",
        scope: ConfigScope::Epoch,
    },
    ConfigKey {
        path: "orchestration.workspace_budget_mib",
        field: ConfigField::WorkspaceBudgetMib,
        kind: "integer",
        constraint: "1..=65535",
        doc: "Soft fork-checkout disk budget in MiB; retains uncovered work.",
        example: "4096",
        scope: ConfigScope::Epoch,
    },
    ConfigKey {
        path: "orchestration.child_retention_days",
        field: ConfigField::ChildRetentionDays,
        kind: "integer",
        constraint: "1..=65535",
        doc: "Days before a sealed dormant child conversation is retired.",
        example: "7",
        scope: ConfigScope::Epoch,
    },
    ConfigKey {
        path: "orchestration.child_retention_count",
        field: ConfigField::ChildRetentionCount,
        kind: "integer",
        constraint: "1..=65535",
        doc: "Dormant conversations retained; protected work refs never expire.",
        example: "128",
        scope: ConfigScope::Epoch,
    },
    ConfigKey {
        path: "orchestration.max_attempts",
        field: ConfigField::MaxAttempts,
        kind: "integer",
        constraint: "1..=256",
        doc: "Attempts one root prompt may open across the tree.",
        example: "64",
        scope: ConfigScope::Epoch,
    },
    ConfigKey {
        path: "orchestration.transient_resumes",
        field: ConfigField::TransientResumes,
        kind: "integer",
        constraint: "0..=2",
        doc: "New continuation turns after transient failure per root user prompt or child attempt; 0 disables automatic continuation.",
        example: "1",
        scope: ConfigScope::Epoch,
    },
    ConfigKey {
        path: "orchestration.max_concurrent_requests_per_account",
        field: ConfigField::RequestsPerAccount,
        kind: "integer",
        constraint: "1..=64",
        doc: "Model requests one account may have in flight across the root and every descendant; excess requests wait in order for a slot. Takes effect on the next launch.",
        example: "8",
        scope: ConfigScope::Immediate,
    },
    ConfigKey {
        path: "orchestration.require_review_evidence",
        field: ConfigField::RequireReviewEvidence,
        kind: "bool",
        constraint: "true or false",
        doc: "When true, adopt_candidate refuses unless its evidence names a subtree attempt that referenced the candidate.",
        example: "false",
        scope: ConfigScope::Epoch,
    },
    ConfigKey {
        path: "turn.auto_continue",
        field: ConfigField::AutoContinue,
        kind: "bool",
        constraint: "true or false",
        doc: "Wake an idle interactive root when background processes finish or child results or questions arrive. Takes effect immediately; JSONL and children do not auto-continue.",
        example: "true",
        scope: ConfigScope::Immediate,
    },
    ConfigKey {
        path: "turn.max_model_rounds",
        field: ConfigField::MaxModelRounds,
        kind: "integer",
        constraint: "1..=1024",
        doc: "Model rounds one root turn may run; the next round must answer without tools. A turn has no wall clock, so this is what stops a runaway turn. Takes effect when a session next attaches; a running turn keeps the cap it was admitted under.",
        example: "512",
        scope: ConfigScope::Immediate,
    },
    ConfigKey {
        path: "jobs.foreground_seconds",
        field: ConfigField::JobsForegroundSeconds,
        kind: "integer",
        constraint: "1..=600",
        example: "30",
        doc: "Default process foreground budget before promotion. Bound at session construction and rebind; exact reopen keeps the stored value.",
        scope: ConfigScope::Immediate,
    },
    ConfigKey {
        path: "jobs.max_runtime_hours",
        field: ConfigField::JobsMaxRuntimeHours,
        kind: "integer",
        constraint: "1..=24",
        example: "10",
        doc: "Hard runtime cap per background process. Bound at session construction and rebind; exact reopen keeps the stored value.",
        scope: ConfigScope::Immediate,
    },
    ConfigKey {
        path: "api.full_window",
        field: ConfigField::ApiFullWindow,
        kind: "bool",
        constraint: "true or false",
        example: "false",
        doc: "Admit direct API sessions (openai, anthropic, xai-api) under the model's full input window instead of the provider's price threshold. Bound at session construction and rebind; exact reopen keeps the stored value.",
        scope: ConfigScope::Immediate,
    },
    ConfigKey {
        path: "tui.mouse",
        field: ConfigField::TuiMouse,
        kind: "bool",
        constraint: "",
        doc: "Mouse capture; turning it off hands text selection back to the terminal. Takes effect on the next launch.",
        example: "true",
        scope: ConfigScope::Immediate,
    },
    ConfigKey {
        path: "tui.theme",
        field: ConfigField::Theme,
        kind: "enum",
        constraint: "default-dark|default-light|catppuccin-latte|catppuccin-frappe|catppuccin-macchiato|catppuccin-mocha; dark and light are accepted aliases that normalize on the next save",
        doc: "Color palette for the whole product shell.",
        example: "\"default-dark\"",
        scope: ConfigScope::Immediate,
    },
    ConfigKey {
        path: "tui.scroll_lines",
        field: ConfigField::ScrollLines,
        kind: "integer",
        constraint: "1..=10",
        doc: "Transcript lines moved per wheel tick.",
        example: "3",
        scope: ConfigScope::Immediate,
    },
    ConfigKey {
        path: "tui.show_thinking",
        field: ConfigField::ShowThinking,
        kind: "bool",
        constraint: "subject to the provider's reasoning policy",
        doc: "Render reasoning blocks where the provider admits them.",
        example: "true",
        scope: ConfigScope::Immediate,
    },
    ConfigKey {
        path: "tui.group_tool_calls",
        field: ConfigField::GroupToolCalls,
        kind: "bool",
        constraint: "",
        doc: "Collapse consecutive tool activities to a summary when assistant prose resumes; false keeps action lines visible.",
        example: "true",
        scope: ConfigScope::Immediate,
    },
    ConfigKey {
        path: "tui.reduced_motion",
        field: ConfigField::ReducedMotion,
        kind: "bool",
        constraint: "",
        doc: "Suppress the spinner, the pulse and camera easing.",
        example: "false",
        scope: ConfigScope::Immediate,
    },
    ConfigKey {
        path: "tui.reload_requests",
        field: ConfigField::ReloadRequests,
        kind: "bool",
        constraint: "true or false; home-scoped",
        doc: "Let this user's own processes, including an agent's shell, ask a running interactive session to reload onto the installed build (plak session reload <session> --expect-binary <id>) at its next idle boundary. Takes effect when a session next attaches.",
        example: "false",
        scope: ConfigScope::Immediate,
    },
    ConfigKey {
        path: "compaction.enabled",
        field: ConfigField::CompactionEnabled,
        kind: "bool",
        constraint: "",
        doc: "Automatic transcript compaction when a session approaches its context bound.",
        example: "true",
        scope: ConfigScope::Immediate,
    },
    ConfigKey {
        path: "skills.enabled",
        field: ConfigField::SkillsEnabled,
        kind: "bool",
        constraint: "true or false",
        doc: "Discover Agent Skills (<workspace>/.agents/skills, then ~/.agents/skills; both on the selected runner for a remote session, using that runner process's home, never the host's) when a session is constructed, list them in its instructions, let its read tool read files under that user skill root on the same machine, and give Claude-vocabulary sessions the Skill tool. Bound when a new session or spawn is constructed; exact reopen keeps the listing and tools it froze, and a fork, rebind or switch keeps the source session's on or off.",
        example: "true",
        scope: ConfigScope::Immediate,
    },
    ConfigKey {
        path: "telemetry.max_bytes",
        field: ConfigField::TelemetryMaxBytes,
        kind: "integer",
        constraint: "greater than 0; zero is refused as ambiguous. 536870912 is 512 MiB",
        doc: "Best-effort byte retention target for the main database, WAL and SHM; busy readers or unfinished sessions may keep it above target.",
        example: "536870912",
        scope: ConfigScope::Immediate,
    },
    ConfigKey {
        path: "telemetry.max_age_days",
        field: ConfigField::TelemetryMaxAgeDays,
        kind: "integer",
        constraint: "greater than 0; zero is refused as ambiguous",
        doc: "Retention cutoff for local analytics rows, in days.",
        example: "90",
        scope: ConfigScope::Immediate,
    },
    ConfigKey {
        path: "mcp.client_id_metadata_url",
        field: ConfigField::McpClientIdMetadataUrl,
        kind: "string",
        constraint: "https:// URL with a path, no query or fragment; home-scoped",
        doc: "The OAuth Client ID Metadata Document that identifies Plak to an MCP authorization server that supports one (spec 01 §8.5). Unset disables that registration path; the document lists the redirect URI http://127.0.0.1/callback, which a server matches on any port. Takes effect at the next plak mcp login.",
        example: "\"https://plak.example/oauth/client.json\"",
        scope: ConfigScope::Immediate,
    },
    ConfigKey {
        path: "host.resume_after_loss",
        field: ConfigField::ResumeAfterLoss,
        kind: "enum",
        constraint: "hold|continue; unset holds; home-scoped",
        doc: "Whether the root and the sub-agents whose host was lost (a crash, a lease loss) hold for the operator's Resume or continue once by themselves with a new instruction turn, the root's restored queue draining too (chapter 03 §14.1). Read when the root attaches and opens, and by plak host's start scan, which under continue opens a session whose root's loss is held; a released 0.0.3 build (or older) refuses a config.toml that sets it.",
        example: "\"continue\"",
        scope: ConfigScope::Immediate,
    },
    ConfigKey {
        path: "web.allow",
        field: ConfigField::WebAllow,
        kind: "array of string",
        constraint: "tailnet logins, no duplicates; unset admits no one; home-scoped",
        doc: "The tailnet logins (Tailscale-User-Login) plak host --web admits through tailscale serve. Read when plak host starts with --web; a 0.0.3 build refuses a config.toml that sets it.",
        example: "[\"you@example.com\"]",
        scope: ConfigScope::Immediate,
    },
    ConfigKey {
        path: "web.origin",
        field: ConfigField::WebOrigin,
        kind: "string",
        constraint: "https://<host>[:port], nothing after it; home-scoped",
        doc: "The exact origin tailscale serve publishes plak host --web at; every request must name its host and a WebSocket upgrade its origin. Read when plak host starts with --web; a 0.0.3 build refuses a config.toml that sets it.",
        example: "\"https://host.example.ts.net\"",
        scope: ConfigScope::Immediate,
    },
    ConfigKey {
        path: "web.accounts_read",
        field: ConfigField::WebAccountsRead,
        kind: "bool",
        constraint: "true or false; home-scoped; host restart required",
        doc: "Allow admitted web principals to read home account status and usage. Same-user sockets are trusted independently. Captured at host startup; restart to change it; a 0.0.3 build refuses a config.toml that sets it.",
        example: "false",
        scope: ConfigScope::Immediate,
    },
    ConfigKey {
        path: "web.config_read",
        field: ConfigField::WebConfigRead,
        kind: "bool",
        constraint: "true or false; home-scoped; host restart required",
        doc: "Allow admitted web principals to read home profiles, templates and allowlisted configuration. Same-user sockets are trusted independently. Captured at host startup; restart to change it; a 0.0.3 build refuses a config.toml that sets it.",
        example: "false",
        scope: ConfigScope::Immediate,
    },
];

/// `[host] resume_after_loss`: what a root whose owner was lost does.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResumeAfterLoss {
    /// Hold for the operator's Resume; the default when unset.
    Hold,
    /// Continue by itself.
    Continue,
}

impl ResumeAfterLoss {
    /// The spelling `config.toml` carries.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Hold => "hold",
            Self::Continue => "continue",
        }
    }

    pub(super) fn parse(value: &str) -> Result<Self, String> {
        match value {
            "hold" => Ok(Self::Hold),
            "continue" => Ok(Self::Continue),
            _ => Err("[host] resume_after_loss must be hold or continue".to_owned()),
        }
    }
}

/// A Client ID Metadata Document URL: `https://` with a path beyond `/`,
/// no query, fragment or credentials.
pub(super) fn check_client_id_metadata_url(url: &str) -> Result<(), String> {
    let refused = || {
        "mcp.client_id_metadata_url must be an https:// URL with a path, without query or fragment"
            .to_owned()
    };
    let rest = url.strip_prefix("https://").ok_or_else(refused)?;
    let (authority, path) = rest.split_once('/').ok_or_else(refused)?;
    if authority.is_empty()
        || authority.contains('@')
        || path.is_empty()
        || url.contains(['?', '#'])
        || url
            .chars()
            .any(|character| character.is_whitespace() || character.is_control())
    {
        return Err(refused());
    }
    Ok(())
}

pub(super) fn parse_bound(key: &str, value: FieldValue<'_>, ceiling: u16) -> Result<u16, String> {
    match value.integer().and_then(|bound| u16::try_from(bound).ok()) {
        Some(bound) if (1..=ceiling).contains(&bound) => Ok(bound),
        _ => Err(format!(
            "[orchestration] {key} must be an integer between 1 and {ceiling}"
        )),
    }
}

pub(super) fn parse_max_model_rounds(value: FieldValue<'_>) -> Result<u16, String> {
    match value
        .integer()
        .and_then(|rounds| u16::try_from(rounds).ok())
    {
        Some(rounds) if (1..=CEILING_MAX_MODEL_ROUNDS).contains(&rounds) => Ok(rounds),
        _ => Err(format!(
            "[turn] max_model_rounds must be an integer between 1 and {CEILING_MAX_MODEL_ROUNDS}"
        )),
    }
}

pub(super) fn parse_scroll_lines(value: FieldValue<'_>) -> Result<u16, String> {
    match value.integer().and_then(|lines| u16::try_from(lines).ok()) {
        Some(lines) if (MIN_SCROLL_LINES..=MAX_SCROLL_LINES).contains(&lines) => Ok(lines),
        _ => Err(format!(
            "[tui] scroll_lines must be an integer between {MIN_SCROLL_LINES} and {MAX_SCROLL_LINES}"
        )),
    }
}

/// `telemetry.*` bounds: zero is ambiguous between "unlimited" and
/// "disabled", so it is refused; the pruner relies on a finite cutoff.
pub(super) fn parse_positive(key: &str, value: FieldValue<'_>) -> Result<u64, String> {
    match value
        .integer()
        .and_then(|parsed| u64::try_from(parsed).ok())
    {
        Some(parsed) if parsed > 0 => Ok(parsed),
        _ => Err(format!("{key} must be a positive integer")),
    }
}

/// A `u64` the product holds for the telemetry store as the `i64` a TOML
/// integer is. Neither bound can have come from a file above the TOML
/// maximum, so the saturation is unreachable from a config and is here so the
/// renderer has no panic in it.
pub(super) fn saturating_toml_integer(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}
