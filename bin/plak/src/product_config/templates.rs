//! Agent template files: render, parse, and the first-run config.

use super::atomic::atomic_file;
use super::load::{ConfigSaveError, ConfigValueOrigin, ProductConfig};
use super::policy::{AgentTemplate, validate_template_name};
use super::toml_file;
use crate::home::paths::PlakPaths;
use plakous_hosted::agent_tree_protocol::Tier;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use toml_edit::Value;

/// One documented `agents/<name>.toml` field.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TemplateFieldDoc {
    /// Field spelling.
    pub key: &'static str,
    /// Value type, in the words the error messages use.
    pub value_type: &'static str,
    /// Literal default, `(required)`, or `(unset)`.
    pub default: &'static str,
    /// Accepted range or value set; empty when the type is the whole rule.
    pub constraint: &'static str,
    /// What the field does, in one sentence.
    pub meaning: &'static str,
    /// Value shown in the reference file when the template leaves it unset.
    pub example: &'static str,
}

impl TemplateFieldDoc {
    /// The two comment lines the reference template prints above the field.
    #[must_use]
    pub fn comment_block(&self) -> String {
        let mut facts = format!("type: {} · default: {}", self.value_type, self.default);
        if !self.constraint.is_empty() {
            facts.push_str(&format!(" · {}", self.constraint));
        }
        format!("\n# {}\n# {facts}\n", self.meaning)
    }
}

/// Every `agents/<name>.toml` field, in render order. The file stem is the
/// template's name; the body carries no `name` key.
pub const TEMPLATE_FIELD_DOCS: &[TemplateFieldDoc] = &[
    TemplateFieldDoc {
        key: "account",
        value_type: "string",
        default: "(required)",
        constraint: "vertical[/name]; vertical is codex|chatgpt|grok|supergrok|xai-oauth|claude|openrouter|openai|anthropic|xai-api; name is [a-z][a-z0-9-]{0,31}, except default",
        meaning: "Credential lane the copy runs on.",
        example: "\"codex\"",
    },
    TemplateFieldDoc {
        key: "model",
        value_type: "string",
        default: "(required)",
        constraint: "non-empty; a direct API account must name an enrolled model, checked at load; every other account is checked at bind",
        meaning: "Exact model id.",
        example: "\"gpt-5.6-luna\"",
    },
    TemplateFieldDoc {
        key: "effort",
        value_type: "string",
        default: "(unset)",
        constraint: "none|low|medium|high|xhigh|max|ultra; Codex offers low through max (never ultra); none is an explicit API effort, not an omitted one; a direct API account requires a level on the model's published ladder, checked at load; every other account is checked at bind",
        meaning: "Reasoning effort; on a subscription account a typo surfaces as a bind failure, not a load error.",
        example: "\"medium\"",
    },
    TemplateFieldDoc {
        key: "fast_mode",
        value_type: "bool",
        default: "false",
        constraint: "true is only valid on the codex vertical, including named slots",
        meaning: "Fast mode: request the provider's faster processing lane for this Codex subscription template. It is not a model choice and does not grant additional tools. A child runs on its own template's setting, never its parent's.",
        example: "true",
    },
    TemplateFieldDoc {
        key: "composition",
        value_type: "enum",
        default: "\"plakous-default\"",
        constraint: "direct|plakous-default|native-ptc|js-ptc|py-ptc; native-ptc is refused for openrouter, anthropic and xai-api, and on openai is valid only for a model enrolled as CodeModeOnly",
        meaning: "How the model composes the product tools: direct calls one at a time, or a programmatic tool calling (PTC) cell. native-ptc is the provider's own PTC system (today only Codex Code Mode is enrolled); js-ptc and py-ptc are harness PTC on every account. plakous-default uses the versioned Plakous-recommended tool presentation for this provider/model; the resolved presentation is pinned for the session and does not select or substitute a model.",
        example: "\"plakous-default\"",
    },
    TemplateFieldDoc {
        key: "protocol",
        value_type: "enum",
        default: "\"auto\"",
        constraint: "auto|chat|responses|messages; anything but auto is OpenRouter-only",
        meaning: "Wire protocol pin for an OpenRouter model whose family the gateway reports wrongly.",
        example: "\"auto\"",
    },
    TemplateFieldDoc {
        key: "description",
        value_type: "string",
        default: "\"\"",
        constraint: "required and non-blank for a template named in [roster] working_set or in a spawns list",
        meaning: "The spawn card a parent routes on: what this agent is for, in the parent's words.",
        example: "\"implements a scoped change and reports the diff\"",
    },
    TemplateFieldDoc {
        key: "access",
        value_type: "enum",
        default: "\"full\"",
        constraint: "full|read-execute",
        meaning: "Edit ceiling: full includes file-edit tools; read-execute omits dedicated file-edit tools but permits shell commands. Neither is an OS sandbox. A spawn request above the ceiling is clipped, not refused.",
        example: "\"read-execute\"",
    },
    TemplateFieldDoc {
        key: "tools",
        value_type: "array of string",
        default: "[]",
        constraint: "web_search|web_fetch|x_search|fetch_url|image_gen|mcp:<id>|mcp:<id>/<tool>, none twice; each mcp:<id> names a [mcp_servers.<id>] table; a child gets exactly its own list, never its parent's",
        meaning: "Optional tools enabled by name after the core tools; options come from config.toml [tools.<name>] and [mcp_servers.<id>]. Each is resolved against the route when a session or child binds, and a tool the route cannot serve refuses the bind.",
        example: "[\"web_search\", \"mcp:github\"]",
    },
    TemplateFieldDoc {
        key: "spawns",
        value_type: "array of string",
        default: "[]",
        constraint: "0..=5 names, no duplicates, each a template file with a description",
        meaning: "The templates a copy of this template may spawn; its own name is never on its menu. Empty is a leaf with no spawn_agent. The root spawns [roster] working_set instead; this list applies where the template runs as a child.",
        example: "[\"worker\", \"reviewer\"]",
    },
    TemplateFieldDoc {
        key: "may_adopt",
        value_type: "bool",
        default: "false",
        constraint: "requires spawns naming another template",
        meaning: "Whether a copy may adopt a sealed candidate out of its own subtree.",
        example: "false",
    },
    TemplateFieldDoc {
        key: "delegation",
        value_type: "enum",
        default: "\"explicit\"",
        constraint: "explicit|proactive",
        meaning: "Whether the model is told to delegate unprompted or only when asked.",
        example: "\"explicit\"",
    },
    TemplateFieldDoc {
        key: "cost_tier",
        value_type: "integer",
        default: "(unset)",
        constraint: "0..=255",
        meaning: "Static label a parent sees on the spawn card; never a measured cost.",
        example: "2",
    },
    TemplateFieldDoc {
        key: "max_turns",
        value_type: "integer",
        default: "(unset)",
        constraint: "1 or more",
        meaning: "Turn budget for a copy of this template.",
        example: "24",
    },
    TemplateFieldDoc {
        key: "on_mint",
        value_type: "string",
        default: "(unset)",
        constraint: "not validated at parse; runs in the copy's workspace",
        meaning: "Command run once when a copy is minted, before its first turn.",
        example: "\"cargo fetch\"",
    },
    TemplateFieldDoc {
        key: "show_model",
        value_type: "bool",
        default: "false",
        constraint: "",
        meaning: "Put the model id on the spawn card next to the description.",
        example: "false",
    },
    TemplateFieldDoc {
        key: "env",
        value_type: "table of string",
        default: "{}",
        constraint: "keys are not validated; values are strings",
        meaning: "Environment for on_mint only; not passed to the copy's tool calls.",
        example: "GIT_TERMINAL_PROMPT = \"0\"",
    },
];

/// One template file body with the reference commentary attached.
///
/// Fields the template leaves unset print as a commented example, so every
/// field of the schema is visible in `docs/reference-config/agents/*.toml`.
#[must_use]
pub fn render_template_annotated(name: &str, template: &AgentTemplate) -> String {
    let rendered = render_template(template);
    let mut out = format!(
        "# Plak agent template (non-secret): ~/.plak/agents/{name}.toml\n\
         #\n\
         # One flat object plus an optional [env] table. The file stem is the\n\
         # template's name. A project's .plak/agents/<name>.toml wins over the\n\
         # user's file of the same name. An unknown key is a named load error.\n"
    );
    for doc in TEMPLATE_FIELD_DOCS {
        out.push_str(&doc.comment_block());
        if doc.key == "env" {
            let table = rendered
                .split_once("\n[env]\n")
                .map(|(_, table)| table.to_owned());
            match table {
                Some(table) => {
                    out.push_str("[env]\n");
                    out.push_str(&table);
                }
                None => out.push_str(&format!("# [env]\n# {}\n", doc.example)),
            }
            continue;
        }
        match rendered
            .lines()
            .find(|line| line.starts_with(&format!("{} = ", doc.key)))
        {
            Some(line) => out.push_str(&format!("{line}\n")),
            None => out.push_str(&format!("# {} = {}\n", doc.key, doc.example)),
        }
    }
    out
}

/// One template file body, canonical field order. Also the form the epoch
/// digest covers.
#[must_use]
pub(crate) fn render_template(template: &AgentTemplate) -> String {
    fn text(out: &mut String, key: &str, value: &str) {
        out.push_str(&format!("{key} = {}\n", toml_text(value)));
    }
    let mut out = String::from("# Plak agent template (non-secret)\n");
    text(&mut out, "account", &template.account);
    text(&mut out, "model", &template.model);
    if let Some(effort) = &template.effort {
        text(&mut out, "effort", effort);
    }
    out.push_str(&format!(
        "fast_mode = {}\n",
        template.extra.as_deref() == Some("priority")
    ));
    if let Some(composition) = template.composition {
        text(&mut out, "composition", composition.as_str());
    }
    if let Some(protocol) = template.protocol {
        text(&mut out, "protocol", protocol.as_str());
    }
    text(&mut out, "description", &template.description);
    text(
        &mut out,
        "access",
        match template.max_tier {
            Tier::Mutate => "full",
            Tier::ReadOnly | Tier::Execute => "read-execute",
        },
    );
    if !template.tools.is_empty() {
        let tools: Vec<String> = template
            .tools
            .entries()
            .iter()
            .map(|tool| toml_text(&tool.to_string()))
            .collect();
        out.push_str(&format!("tools = [{}]\n", tools.join(", ")));
    }
    let spawns: Vec<String> = template
        .spawns
        .iter()
        .map(|name| toml_text(name.as_str()))
        .collect();
    out.push_str(&format!("spawns = [{}]\n", spawns.join(", ")));
    out.push_str(&format!("may_adopt = {}\n", template.may_adopt));
    out.push_str(&format!(
        "delegation = {}\n",
        toml_text(template.delegation.as_str())
    ));
    if let Some(cost_tier) = template.cost_tier {
        out.push_str(&format!("cost_tier = {cost_tier}\n"));
    }
    if let Some(max_turns) = template.max_turns {
        out.push_str(&format!("max_turns = {max_turns}\n"));
    }
    if let Some(on_mint) = &template.on_mint {
        out.push_str(&format!("on_mint = {}\n", toml_text(on_mint)));
    }
    out.push_str(&format!("show_model = {}\n", template.show_model));
    if !template.env.is_empty() {
        out.push_str("\n[env]\n");
        for (key, value) in &template.env {
            out.push_str(&format!("{} = {}\n", toml_key(key), toml_text(value)));
        }
    }
    out
}

/// One string as TOML spells it, quotes and escapes included. A description
/// carrying a quote, a backslash or a newline renders as a file that reads
/// back as the same string.
pub(super) fn toml_text(value: &str) -> String {
    Value::from(value).to_string()
}

/// One `[env]` key: bare where TOML admits a bare key, quoted otherwise.
pub(super) fn toml_key(key: &str) -> String {
    let bare = !key.is_empty()
        && key
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-');
    if bare { key.to_owned() } else { toml_text(key) }
}

/// Parses one flat template file. `name` is the file stem; the body carries
/// no `name` key.
pub(crate) fn parse_template(text: &str, name: &str) -> Result<AgentTemplate, String> {
    toml_file::parse_template(text, name)
}

/// The templates and per-name origins one agents directory produced.
type TaggedTemplates = (
    BTreeMap<String, AgentTemplate>,
    BTreeMap<String, ConfigValueOrigin>,
);

/// Every `*.toml` under one agents directory, each name tagged with the file
/// it came from. The caller decides which directories participate and in
/// which order — the loader itself knows no precedence.
pub(super) fn load_templates_dir(
    dir: &Path,
    origin: ConfigValueOrigin,
) -> Result<TaggedTemplates, String> {
    let mut templates = BTreeMap::new();
    let mut origins = BTreeMap::new();
    for (name, path) in template_files(dir)? {
        let text = fs::read_to_string(&path)
            .map_err(|_| format!("could not read template {}", path.display()))?;
        let template =
            parse_template(&text, &name).map_err(|error| format!("{}: {error}", path.display()))?;
        templates.insert(name.clone(), template);
        origins.insert(name, origin);
    }
    Ok((templates, origins))
}

fn template_files(dir: &Path) -> Result<Vec<(String, PathBuf)>, String> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(_) => return Err(format!("could not read {}", dir.display())),
    };
    let mut files = Vec::new();
    for entry in entries {
        let path = entry
            .map_err(|_| format!("could not read {}", dir.display()))?
            .path();
        if path.extension().is_none_or(|extension| extension != "toml") || !path.is_file() {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
            return Err(format!(
                "template file {} has no UTF-8 name",
                path.display()
            ));
        };
        if stem.starts_with('.') {
            continue;
        }
        validate_template_name(stem).map_err(|error| format!("{}: {error}", path.display()))?;
        files.push((stem.to_owned(), path));
    }
    files.sort();
    Ok(files)
}

pub fn init_if_missing(paths: &PlakPaths) -> Result<bool, String> {
    let _lock =
        super::publication::ConfigLock::take(paths).map_err(ConfigSaveError::into_message)?;
    paths.ensure_layout()?;
    let path = paths.config_file();
    if path.is_file() {
        return Ok(false);
    }
    let rendered = ProductConfig::default_product().render_annotated();
    let mut template = String::from(
        "# Uncomment only the preferences you want to pin. Omitted keys follow defaults.\n",
    );
    // Section headers stay live (an empty known section parses), so a key
    // uncommented per the instruction above lands in its own section.
    for line in rendered.lines() {
        if line.starts_with('#') || line.starts_with('[') || line.is_empty() {
            template.push_str(line);
        } else {
            template.push_str("# ");
            template.push_str(line);
        }
        template.push('\n');
    }
    atomic_file(&path, template.as_bytes(), true, &mut |_, _| Ok(()))
        .map_err(ConfigSaveError::into_message)?;
    Ok(true)
}
