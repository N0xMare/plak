//! `config.toml` and `agents/<name>.toml` as real TOML.
//!
//! Three jobs, in the order a value moves through them:
//!
//! 1. **Document.** The `toml` crate parses the bytes. Malformed syntax, a
//!    duplicate key and a duplicate table are its errors, reported with the
//!    line and column it found them at.
//! 2. **Typed input.** The `#[serde(deny_unknown_fields)]` structs below are
//!    the schema: an unknown key or an unknown section cannot deserialize.
//!    Serde's own wording would lose the migration hints the product owes a
//!    reader, so `translate` maps every unknown-field error back through
//!    `unknown_key` and `removed_or_unknown_section`.
//! 3. **Semantics.** Every accepted value goes through
//!    `ProductConfig::assign_field`, the same rule set `plak config set`
//!    uses, so a file and a command cannot disagree about what a value means.
//!
//! Saving runs the other way. `edit_document` loads the file that is already
//! there with `toml_edit` and writes only the keys whose value actually
//! changed, so comments, key order and spacing on every other line survive a
//! `plak config set` byte for byte.

use std::collections::BTreeMap;
use std::ops::Range;

use serde::Deserialize;
use toml::Spanned;
use toml_edit::{DocumentMut, Item, Table, TableLike, Value};

use super::{
    AgentTemplate, CONFIG_KEYS, ConfigKey, Delegation, FieldValue, ProductCompositionSelection,
    ProductConfig, ProductProtocolSelection, TemplateName, Tier, removed_or_unknown_section,
    unknown_key,
};

/// `config.toml`: nine sections, each optional, nothing else.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigFile {
    root: Option<RootSection>,
    roster: Option<RosterSection>,
    orchestration: Option<OrchestrationSection>,
    turn: Option<TurnSection>,
    jobs: Option<JobsSection>,
    api: Option<ApiSection>,
    tui: Option<TuiSection>,
    compaction: Option<CompactionSection>,
    telemetry: Option<TelemetrySection>,
}

/// Every key is read as a spanned TOML value rather than a Rust type so the
/// type complaint is the product's own sentence, at the value's own location,
/// instead of serde's.
type Entry = Option<Spanned<toml::Value>>;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RootSection {
    agent: Entry,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RosterSection {
    working_set: Entry,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct OrchestrationSection {
    subagents: Entry,
    max_depth: Entry,
    max_children: Entry,
    workspace_idle_seconds: Entry,
    workspace_budget_mib: Entry,
    child_retention_days: Entry,
    child_retention_count: Entry,
    max_nodes: Entry,
    max_attempts: Entry,
    transient_resumes: Entry,
    require_review_evidence: Entry,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TurnSection {
    auto_continue: Entry,
    max_model_rounds: Entry,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct JobsSection {
    foreground_seconds: Entry,
    max_runtime_hours: Entry,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TuiSection {
    mouse: Entry,
    theme: Entry,
    scroll_lines: Entry,
    show_thinking: Entry,
    group_tool_calls: Entry,
    reduced_motion: Entry,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ApiSection {
    full_window: Entry,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CompactionSection {
    enabled: Entry,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TelemetrySection {
    max_bytes: Entry,
    max_age_days: Entry,
}

/// One `agents/<name>.toml`: one flat object plus an optional `[env]` table.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TemplateFile {
    account: Entry,
    model: Entry,
    effort: Entry,
    fast_mode: Entry,
    composition: Entry,
    protocol: Entry,
    description: Entry,
    tools: Entry,
    spawns: Entry,
    may_adopt: Entry,
    delegation: Entry,
    cost_tier: Entry,
    max_turns: Entry,
    on_mint: Entry,
    show_model: Entry,
    env: Option<BTreeMap<String, Spanned<toml::Value>>>,
}

/// `config.toml` as the product reads it: document, then schema, then
/// semantics.
pub(super) fn parse_config(text: &str) -> Result<ProductConfig, String> {
    let file: ConfigFile = toml::from_str(text).map_err(|error| translate(text, &error))?;
    let mut config = ProductConfig::default_product();
    let mut assign = |path: &'static str, entry: &Entry| -> Result<(), String> {
        let Some(spanned) = entry else {
            return Ok(());
        };
        let key = config_key_by_path(path);
        config
            .assign_field(key, FieldValue::Toml(spanned.as_ref()))
            .map_err(|message| located(text, spanned.span(), &message))?;
        config.present.insert(key.field);
        Ok(())
    };
    if let Some(section) = &file.root {
        assign("root.agent", &section.agent)?;
    }
    if let Some(section) = &file.roster {
        assign("roster.working_set", &section.working_set)?;
    }
    if let Some(section) = &file.orchestration {
        assign("orchestration.subagents", &section.subagents)?;
        assign("orchestration.max_depth", &section.max_depth)?;
        assign("orchestration.max_children", &section.max_children)?;
        assign(
            "orchestration.workspace_idle_seconds",
            &section.workspace_idle_seconds,
        )?;
        assign(
            "orchestration.workspace_budget_mib",
            &section.workspace_budget_mib,
        )?;
        assign("orchestration.max_nodes", &section.max_nodes)?;
        assign(
            "orchestration.child_retention_days",
            &section.child_retention_days,
        )?;
        assign(
            "orchestration.child_retention_count",
            &section.child_retention_count,
        )?;
        assign("orchestration.max_attempts", &section.max_attempts)?;
        assign(
            "orchestration.transient_resumes",
            &section.transient_resumes,
        )?;
        assign(
            "orchestration.require_review_evidence",
            &section.require_review_evidence,
        )?;
    }
    if let Some(section) = &file.turn {
        assign("turn.auto_continue", &section.auto_continue)?;
        assign("turn.max_model_rounds", &section.max_model_rounds)?;
    }
    if let Some(section) = &file.jobs {
        assign("jobs.foreground_seconds", &section.foreground_seconds)?;
        assign("jobs.max_runtime_hours", &section.max_runtime_hours)?;
    }
    if let Some(section) = &file.api {
        assign("api.full_window", &section.full_window)?;
    }
    if let Some(section) = &file.tui {
        assign("tui.mouse", &section.mouse)?;
        assign("tui.theme", &section.theme)?;
        assign("tui.scroll_lines", &section.scroll_lines)?;
        assign("tui.show_thinking", &section.show_thinking)?;
        assign("tui.group_tool_calls", &section.group_tool_calls)?;
        assign("tui.reduced_motion", &section.reduced_motion)?;
    }
    if let Some(section) = &file.compaction {
        assign("compaction.enabled", &section.enabled)?;
    }
    if let Some(section) = &file.telemetry {
        assign("telemetry.max_bytes", &section.max_bytes)?;
        assign("telemetry.max_age_days", &section.max_age_days)?;
    }
    Ok(config)
}

/// One template file. The file stem is the template's name; the body carries
/// no `name` key.
pub(super) fn parse_template(text: &str, name: &str) -> Result<AgentTemplate, String> {
    let file: TemplateFile =
        toml::from_str(text).map_err(|error| template_translate(text, name, &error))?;
    let mut template = AgentTemplate::default();
    let string = |field: &'static str, entry: &Entry| -> Result<Option<String>, String> {
        let Some(spanned) = entry else {
            return Ok(None);
        };
        match spanned.as_ref() {
            toml::Value::String(value) => Ok(Some(value.clone())),
            _ => Err(located(
                text,
                spanned.span(),
                &format!("template {name}: {field} must be a string"),
            )),
        }
    };
    let boolean = |field: &'static str, entry: &Entry| -> Result<Option<bool>, String> {
        let Some(spanned) = entry else {
            return Ok(None);
        };
        match spanned.as_ref() {
            toml::Value::Boolean(value) => Ok(Some(*value)),
            _ => Err(located(
                text,
                spanned.span(),
                &format!("template {name}: {field} must be a boolean"),
            )),
        }
    };
    let integer = |field: &'static str, entry: &Entry| -> Result<Option<i64>, String> {
        let Some(spanned) = entry else {
            return Ok(None);
        };
        match spanned.as_ref() {
            toml::Value::Integer(value) => Ok(Some(*value)),
            _ => Err(located(
                text,
                spanned.span(),
                &format!("template {name}: {field} must be an integer"),
            )),
        }
    };

    if let Some(account) = string("account", &file.account)? {
        template.account = account;
    }
    if let Some(model) = string("model", &file.model)? {
        template.model = model;
    }
    template.effort = string("effort", &file.effort)?;
    template.extra = boolean("fast_mode", &file.fast_mode)?
        .unwrap_or(false)
        .then(|| "priority".to_owned());
    if let Some(composition) = string("composition", &file.composition)? {
        template.composition = Some(ProductCompositionSelection::parse_config(&composition)?);
    }
    if let Some(protocol) = string("protocol", &file.protocol)? {
        template.protocol = Some(ProductProtocolSelection::parse(&protocol)?);
    }
    if let Some(description) = string("description", &file.description)? {
        template.description = description;
    }
    template.max_tier = match string("tools", &file.tools)?.as_deref() {
        None | Some("full") => Tier::Mutate,
        Some("read-execute") => Tier::Execute,
        Some(_) => {
            return Err(format!("template {name}: tools must be full|read-execute"));
        }
    };
    if let Some(spanned) = &file.spawns {
        let not_names = || {
            located(
                text,
                spanned.span(),
                &format!("template {name}: spawns must be an array of template names"),
            )
        };
        let toml::Value::Array(items) = spanned.as_ref() else {
            return Err(not_names());
        };
        for item in items {
            let toml::Value::String(child) = item else {
                return Err(not_names());
            };
            let child = TemplateName::parse(child).map_err(|error| {
                located(
                    text,
                    spanned.span(),
                    &format!("template {name}: spawns: {error}"),
                )
            })?;
            template.spawns.push(child);
        }
    }
    if let Some(may_adopt) = boolean("may_adopt", &file.may_adopt)? {
        template.may_adopt = may_adopt;
    }
    if let Some(delegation) = string("delegation", &file.delegation)? {
        template.delegation = Delegation::parse(&delegation)?;
    }
    if let Some(cost_tier) = integer("cost_tier", &file.cost_tier)? {
        template.cost_tier = Some(
            u8::try_from(cost_tier)
                .map_err(|_| format!("template {name}: cost_tier must be 0..=255"))?,
        );
    }
    if let Some(max_turns) = integer("max_turns", &file.max_turns)? {
        template.max_turns =
            Some(u32::try_from(max_turns).map_err(|_| {
                format!("template {name}: max_turns must be an integer of 1 or more")
            })?);
    }
    template.on_mint = string("on_mint", &file.on_mint)?;
    if let Some(show_model) = boolean("show_model", &file.show_model)? {
        template.show_model = show_model;
    }
    for (key, spanned) in file.env.unwrap_or_default() {
        match spanned.as_ref() {
            toml::Value::String(value) => {
                template.env.insert(key, value.clone());
            }
            _ => {
                return Err(located(
                    text,
                    spanned.span(),
                    &format!("template {name}: env {key} must be a string"),
                ));
            }
        }
    }
    template.validate(name)?;
    Ok(template)
}

/// The config file to publish: the one that is already on disk with only the
/// changed keys rewritten, or the canonical render when there is none.
///
/// Only a key whose value genuinely differs is touched, so an operator's
/// comments, key order, alignment and any blank lines survive a save of one
/// other key. A key the file leaves out keeps its default by staying out —
/// writing every default back would turn a four-line file into the whole
/// schema on the first save. A `[root] agent` the config no longer carries is
/// removed.
pub(super) fn edit_document(
    existing: Option<&str>,
    config: &ProductConfig,
) -> Result<String, String> {
    let Some(existing) = existing else {
        return Ok(config.render());
    };
    let mut document: DocumentMut = existing
        .parse()
        .map_err(|error| format!("could not read config.toml as TOML: {error}"))?;
    let defaults = ProductConfig::default_product();
    for row in CONFIG_KEYS {
        let current = document
            .get(row.section())
            .and_then(|section| section.get(row.key()))
            .and_then(Item::as_value)
            .cloned();
        let desired = config.field_value(row.field);
        match (current, desired) {
            (Some(_), None) => {
                section_table(&mut document, row.section())?.remove(row.key());
            }
            (Some(current), Some(desired)) => {
                if !same_value(&current, &desired) {
                    section_table(&mut document, row.section())?
                        .insert(row.key(), Item::Value(desired));
                }
            }
            (None, Some(desired)) => {
                if !defaults
                    .field_value(row.field)
                    .is_some_and(|default| same_value(&default, &desired))
                {
                    section_table(&mut document, row.section())?
                        .insert(row.key(), Item::Value(desired));
                }
            }
            (None, None) => {}
        }
    }
    Ok(document.to_string())
}

/// The project override document: a key is written only where the effective
/// value differs from the global baseline, and a written key that no longer
/// differs is removed so the project inherits again. `[root] agent` cannot be
/// cleared in project scope — an override must name a template.
pub(super) fn edit_project_document(
    existing: Option<&str>,
    config: &ProductConfig,
    baseline: &ProductConfig,
) -> Result<String, String> {
    let mut document: DocumentMut = match existing {
        Some(text) => text
            .parse()
            .map_err(|error| format!("could not read project config.toml as TOML: {error}"))?,
        None => DocumentMut::new(),
    };
    for row in CONFIG_KEYS {
        let desired = config.field_value(row.field);
        let inherited = baseline.field_value(row.field);
        if same_value_opt(desired.as_ref(), inherited.as_ref()) {
            // Matches the baseline: inherit — a stale override leaves.
            if let Some(table) = document
                .get_mut(row.section())
                .and_then(Item::as_table_like_mut)
            {
                table.remove(row.key());
            }
            continue;
        }
        let Some(desired) = desired else {
            return Err(
                "a project config cannot clear root.agent; override it with a template name or clear it globally"
                    .to_owned(),
            );
        };
        section_table(&mut document, row.section())?.insert(row.key(), Item::Value(desired));
    }
    let rendered = document.to_string();
    if existing.is_none() {
        return Ok(format!(
            "# Plak project config (non-secret)\n\
             # Keys here override ~/.plak/config.toml; omitted keys inherit it.\n\n{rendered}"
        ));
    }
    Ok(rendered)
}

/// `same_value` lifted to optional values: both absent means equal.
pub(super) fn same_value_opt(left: Option<&Value>, right: Option<&Value>) -> bool {
    match (left, right) {
        (Some(left), Some(right)) => same_value(left, right),
        (None, None) => true,
        _ => false,
    }
}

/// The named section as the operator wrote it, a standard or an inline table,
/// created (and made explicit, so it prints its own header) when the file does
/// not carry it yet. A section that is neither is refused before publication.
fn section_table<'a>(
    document: &'a mut DocumentMut,
    section: &str,
) -> Result<&'a mut dyn TableLike, String> {
    let root = document.as_table_mut();
    if !root.contains_key(section) {
        let mut table = Table::new();
        table.set_implicit(false);
        root.insert(section, Item::Table(table));
    }
    root.get_mut(section)
        .and_then(Item::as_table_like_mut)
        .ok_or_else(|| format!("config.toml [{section}] is not a table"))
}

/// Whether the file already says what the config means, ignoring how it is
/// spelled. Equal values are left exactly as the operator wrote them.
fn same_value(current: &Value, desired: &Value) -> bool {
    match (current, desired) {
        (Value::String(left), Value::String(right)) => left.value() == right.value(),
        (Value::Boolean(left), Value::Boolean(right)) => left.value() == right.value(),
        (Value::Integer(left), Value::Integer(right)) => left.value() == right.value(),
        (Value::Array(left), Value::Array(right)) => {
            left.len() == right.len()
                && left
                    .iter()
                    .zip(right.iter())
                    .all(|(left, right)| match (left, right) {
                        (Value::String(left), Value::String(right)) => {
                            left.value() == right.value()
                        }
                        _ => false,
                    })
        }
        _ => false,
    }
}

/// A message with the place in the file it belongs to.
fn located(text: &str, span: Range<usize>, message: &str) -> String {
    let (line, column) = line_column(text, span.start);
    format!("{message} (line {line}, column {column})")
}

/// One-based line and column of a byte offset.
fn line_column(text: &str, offset: usize) -> (usize, usize) {
    let offset = offset.min(text.len());
    let before = &text[..offset];
    let line = before.matches('\n').count() + 1;
    let column = before
        .rfind('\n')
        .map_or(before, |index| &before[index + 1..])
        .chars()
        .count()
        + 1;
    (line, column)
}

/// A `config.toml` parse failure in the product's own words.
fn translate(text: &str, error: &toml::de::Error) -> String {
    match unknown_field(error) {
        Some(field) => {
            let message = match site(text, error.span()) {
                Site::Section(section) => removed_or_unknown_section(&section),
                Site::Key(section) => unknown_key(&section, &field),
            };
            located(text, error.span().unwrap_or(0..0), &message)
        }
        None => document_message(text, error),
    }
}

/// The same for a template file, whose messages all name the template.
fn template_translate(text: &str, name: &str, error: &toml::de::Error) -> String {
    match unknown_field(error) {
        Some(field) => match site(text, error.span()) {
            Site::Section(section) => format!(
                "template {name}: unknown section [{section}]; a template is one flat object plus an optional [env] table"
            ),
            Site::Key(_) => format!("template {name}: unknown key {field}"),
        },
        None => format!("template {name}: {}", document_message(text, error)),
    }
}

/// A syntax, duplicate-key or wrong-shape complaint, on one line, at its own
/// location.
fn document_message(text: &str, error: &toml::de::Error) -> String {
    let message = error.message().to_owned();
    match error.span() {
        Some(span) => located(text, span, &message),
        None => message,
    }
}

/// The field name out of serde's `unknown field \`x\`, expected …`.
fn unknown_field(error: &toml::de::Error) -> Option<String> {
    let message = error.message();
    let rest = message.strip_prefix("unknown field `")?;
    let (field, _) = rest.split_once('`')?;
    Some(field.to_owned())
}

/// Whether an unknown field was a table header or a key, and the section it
/// sits in. Serde reports the field alone; the file says which it was.
enum Site {
    /// A `[header]`, carrying its full dotted name.
    Section(String),
    /// A key, carrying the section it is written under (empty above the first
    /// header).
    Key(String),
}

fn site(text: &str, span: Option<Range<usize>>) -> Site {
    let Some(span) = span else {
        return Site::Key(String::new());
    };
    let offset = span.start.min(text.len());
    let start = text[..offset].rfind('\n').map_or(0, |index| index + 1);
    let end = text[start..]
        .find('\n')
        .map_or(text.len(), |index| start + index);
    if let Some(header) = table_header(&text[start..end]) {
        return Site::Section(header);
    }
    let section = text[..start]
        .lines()
        .rev()
        .find_map(table_header)
        .unwrap_or_default();
    Site::Key(section)
}

/// The dotted name of a `[header]` line, or `None` when the line is not one.
fn table_header(line: &str) -> Option<String> {
    let trimmed = line.trim();
    let inner = trimmed.strip_prefix('[')?;
    let inner = inner.strip_prefix('[').unwrap_or(inner);
    let (name, _) = inner.split_once(']')?;
    Some(name.trim().to_owned())
}

/// The table row a dotted path names. The paths below are written out in
/// [`parse_config`], so a missing row is a bug in this file, not in a config.
fn config_key_by_path(path: &'static str) -> &'static ConfigKey {
    CONFIG_KEYS
        .iter()
        .find(|row| row.path == path)
        .unwrap_or_else(|| unreachable!("{path} is not a documented config key"))
}
