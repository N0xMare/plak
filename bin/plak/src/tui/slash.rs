//! Slash command parse, fuzzy listing, and pure resolution.

use crate::tui::SurfaceIntent;
use crate::tui::UserAction;

/// Catalog entry: name without leading `/` + a short first-time-user sentence.
pub const COMMANDS: &[(&str, &str)] = &[
    ("help", "Show all commands"),
    ("keybinds", "View Plak key bindings"),
    ("status", "View session status"),
    ("settings", "Go to interactive settings menu"),
    ("agents", "Choose an agent profile"),
    ("providers", "View connected providers"),
    ("usage", "View usage data for connected providers"),
    ("context", "View context window data"),
    ("attach", "Reconnect the agent"),
    ("recover", "Recover an interrupted turn"),
    ("clear", "Start a new conversation"),
    ("compact", "Compact this conversation"),
    ("config", "Open config.toml in your editor"),
    ("resume", "Resume a previous session"),
    ("tree", "Open Activity: Agents"),
    ("ps", "Open Activity: Processes"),
    ("stop-all", "Stop all non-root agents"),
    ("fork", "Fork this conversation"),
    ("fork-then-compact", "Fork, then compact the new session"),
    ("drop", "Drop the last queued message"),
    ("image", "Attach image files to the next message"),
    (
        "reload",
        "Restart on the installed Plak and resume this session",
    ),
    ("quit", "Exit Plak"),
];

/// Parsed slash invocation (command name lowercased + raw args).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParsedSlash {
    /// Command name without `/`, lowercased.
    pub name: String,
    /// Remainder after the command token (trimmed).
    pub args: String,
}

/// Effect of resolving a slash command in the TUI.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SlashEffect {
    /// Open the Activity overlay's Processes tab.
    OpenProcesses,
    /// Forward a control-plane action to the host (may also clear local view).
    Action(UserAction),
    /// Quit and re-exec the installed `plak` on this session (`/reload`).
    Reload,
    /// Pure UI: append a system note; no host action.
    LocalNote(String),
    /// Attach the image files these paths name to the draft (`/image <path>...`).
    AttachImage(String),
    /// Open the Menu on Settings (`/settings`).
    OpenSettings,
    /// Open the Menu on Providers (`/providers`).
    OpenProviders,
    /// Open the Menu on Usage (`/usage`).
    OpenUsage,
    /// Open the Menu on Context (`/context`).
    OpenContext,
    /// Open the Menu on Session (`/status`).
    OpenSession,
    /// Open the Agents tab (bare `/agents`).
    OpenAgents,
    /// Ask whether to use a profile now or save it for the next session. The
    /// name is resolved against the product's list, aliases included.
    ConfirmProfile(String),
    /// Open the keybinds overlay (do not dump into transcript).
    OpenKeybinds,
    /// Open a floating help/status overlay (do not dump into transcript).
    OpenInfoOverlay {
        /// Overlay title.
        title: String,
        /// Body lines.
        body: String,
    },
    /// Quit the shell.
    Quit,
}

/// Chrome fields used by `/status` and similar local notes.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SlashContext {
    /// Session id label.
    pub session_id: Option<String>,
    /// Account label.
    pub account: Option<String>,
    /// Model label.
    pub model: Option<String>,
    /// Effort label.
    pub effort: Option<String>,
    /// Phase from host status.
    pub phase: String,
    /// Product queue length.
    pub queue_len: usize,
    /// Busy routing flag.
    pub busy: bool,
}

/// Parse a full input line into a slash command, or `None` if not slash-shaped.
#[must_use]
pub fn parse_slash(line: &str) -> Option<ParsedSlash> {
    let trimmed = line.trim();
    let rest = trimmed.strip_prefix('/')?;
    if rest.is_empty() {
        return Some(ParsedSlash {
            name: String::new(),
            args: String::new(),
        });
    }
    let mut parts = rest.splitn(2, char::is_whitespace);
    let name = parts.next().unwrap_or("").to_ascii_lowercase();
    let args = parts.next().unwrap_or("").trim().to_owned();
    Some(ParsedSlash { name, args })
}

/// Filter catalog by fuzzy/prefix match against `query` (no leading `/`).
///
/// Empty query returns the full catalog order.
#[must_use]
pub fn fuzzy_list(query: &str) -> Vec<(&'static str, &'static str)> {
    let q = query.trim().trim_start_matches('/').to_ascii_lowercase();
    if q.is_empty() {
        return COMMANDS.to_vec();
    }
    let mut scored: Vec<(usize, &str, &str)> = Vec::new();
    for &(name, help) in COMMANDS {
        if let Some(score) = fuzzy_score(name, &q) {
            scored.push((score, name, help));
        }
    }
    scored.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(b.1)));
    scored.into_iter().map(|(_, n, h)| (n, h)).collect()
}

/// Lower score is better. Prefix match beats subsequence.
fn fuzzy_score(name: &str, query: &str) -> Option<usize> {
    if name == query {
        return Some(0);
    }
    if name.starts_with(query) {
        return Some(1);
    }
    // Contiguous substring.
    if name.contains(query) {
        return Some(2 + name.find(query).unwrap_or(0));
    }
    // Subsequence match.
    let mut ni = name.chars();
    for qc in query.chars() {
        loop {
            let nc = ni.next()?;
            if nc == qc {
                break;
            }
        }
    }
    Some(100 + name.len())
}

/// Commands that act for good: the palette's `Enter` runs them only from
/// their full name (spec 02 §5.1).
const IRREVERSIBLE: &[&str] = &[
    "clear",
    "compact",
    "fork-then-compact",
    "fork",
    "drop",
    "recover",
];

/// Whether `name` acts for good, so a partial word never runs it.
#[must_use]
pub fn is_irreversible(name: &str) -> bool {
    IRREVERSIBLE.contains(&name)
}

/// Commands whose grammar is the bare word. Trailing text is refused rather
/// than ignored: `/compact now` must not start a paid compaction, and
/// `/clear all` must not clear on the strength of a word it never read.
const TAKES_NO_ARGUMENTS: &[&str] = &[
    "ps",
    "help",
    "keybinds",
    "status",
    "settings",
    "providers",
    "usage",
    "context",
    "attach",
    "recover",
    "clear",
    "compact",
    "config",
    "stop-all",
    "stop_all",
    "drop",
    "reload",
    "quit",
    "exit",
];

/// Resolve a parsed slash into a host action or local UI note.
#[must_use]
pub fn resolve(parsed: &ParsedSlash, _ctx: &SlashContext) -> SlashEffect {
    let name = parsed.name.as_str();
    if !parsed.args.is_empty() && TAKES_NO_ARGUMENTS.contains(&name) {
        return SlashEffect::LocalNote(format!("usage: /{name}  —  takes no arguments"));
    }
    match name {
        "help" | "" => SlashEffect::OpenInfoOverlay {
            title: "help".to_owned(),
            body: help_text(),
        },
        "keybinds" => SlashEffect::OpenKeybinds,
        // 2.1: `/status` opens the Session tab, never an unscrollable
        // floating text overlay.
        "status" => SlashEffect::OpenSession,
        "clear" => SlashEffect::Action(UserAction::ClearSession),
        "compact" => SlashEffect::Action(UserAction::Compact),
        "config" => SlashEffect::Action(UserAction::Extension(SurfaceIntent::OpenConfigFile)),
        "settings" => SlashEffect::OpenSettings,
        "resume" => resolve_resume(&parsed.args),
        "ps" => SlashEffect::OpenProcesses,
        "tree" => match parsed.args.as_str() {
            "" => SlashEffect::Action(UserAction::OpenTopologyNavigator),
            "resume" => SlashEffect::Action(UserAction::ResumeTopologyTree),
            _ => SlashEffect::LocalNote("usage: /tree [resume]".to_owned()),
        },
        "stop-all" | "stop_all" => SlashEffect::Action(UserAction::StopAllTopology),
        "fork" => resolve_fork(&parsed.args),
        "fork-then-compact" => resolve_fork_then_compact(&parsed.args),
        "agents" => resolve_agents(&parsed.args),
        "providers" => SlashEffect::OpenProviders,
        "usage" => SlashEffect::OpenUsage,
        "context" => SlashEffect::OpenContext,
        "attach" => SlashEffect::Action(UserAction::RetryAttach),
        "recover" => SlashEffect::Action(UserAction::Recover),
        "drop" => SlashEffect::Action(UserAction::DropQueued),
        "image" if parsed.args.is_empty() => SlashEffect::LocalNote(
            "usage: /image <path>...  —  attach png, jpeg, gif or webp files; Ctrl+V (not Cmd+V) attaches a clipboard image".to_owned(),
        ),
        "image" => SlashEffect::AttachImage(parsed.args.clone()),
        "reload" => SlashEffect::Reload,
        "quit" | "exit" => SlashEffect::Quit,
        other => SlashEffect::LocalNote(format!("unknown command /{other} — try /help")),
    }
}

fn resolve_resume(args: &str) -> SlashEffect {
    let id = args
        .split_whitespace()
        .next()
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    SlashEffect::Action(UserAction::ResumeSession { id })
}

fn resolve_fork(args: &str) -> SlashEffect {
    if !args.is_empty() {
        // The in-session fork copies the source onto the binding it already
        // has. A different profile is `/agents <name>`, which asks before it
        // branches. An advertised target would only be refused one layer down.
        return SlashEffect::LocalNote("usage: /fork  —  forks on the current binding".to_owned());
    }
    SlashEffect::Action(UserAction::ForkSession {
        account: None,
        compact: false,
    })
}

fn resolve_fork_then_compact(args: &str) -> SlashEffect {
    if !args.is_empty() {
        return SlashEffect::LocalNote(
            "usage: /fork-then-compact  —  forks on the current binding, then compacts the new session"
                .to_owned(),
        );
    }
    SlashEffect::Action(UserAction::ForkSession {
        account: None,
        compact: true,
    })
}

fn resolve_agents(args: &str) -> SlashEffect {
    let mut words = args.split_whitespace();
    let Some(name) = words.next() else {
        return SlashEffect::OpenAgents;
    };
    if words.next().is_some() {
        return SlashEffect::LocalNote("usage: /agents  —  or  /agents <profile>".to_owned());
    }
    SlashEffect::ConfirmProfile(name.to_owned())
}

fn help_text() -> String {
    let mut lines = vec!["commands:".to_owned()];
    for (name, help) in COMMANDS {
        lines.push(format!("  /{name:<18} {help}"));
    }
    // Deferred capabilities are named in prose, never advertised as commands
    // that only print a placeholder (spec 02 §5.1).
    lines.push(String::new());
    lines.push("not implemented: skills, MCP, and workflows have no command.".to_owned());
    lines.push(String::new());
    lines.push("/providers is a read-only status view: per vertical it names the".to_owned());
    lines.push("account, the status, and the CLI command that connects it.".to_owned());
    lines.push(String::new());
    lines.push("login and API keys are CLI-only:".to_owned());
    lines.push(format!(
        "  plak login {} [--remote]",
        crate::tui::providers::OAUTH_ACCOUNT_PLACEHOLDER
    ));
    lines.push(format!(
        "  plak logout {}",
        crate::tui::providers::OAUTH_ACCOUNT_PLACEHOLDER
    ));
    lines.push(format!(
        "  plak key set|clear {}",
        crate::tui::providers::API_KEY_ACCOUNT_PLACEHOLDER
    ));
    lines.push("templates, roster and orchestration are config-only:".to_owned());
    lines.push("  plak config set …  ·  /config opens config.toml".to_owned());
    // Keys are generated from the one keymap registry, never restated here.
    // Hand-written prose left 64 of the 65 registered bindings undocumented in
    // `/help`, and the one it named could go stale without a test noticing.
    lines.push(String::new());
    lines.push("keys (? opens While typing; Enter opens All keybinds in Menu):".to_owned());
    lines.push(String::new());
    lines.extend(crate::tui::keymap::sheet_lines());
    lines.join("\n")
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests {
    use super::*;

    #[test]
    fn harness_mode_is_not_a_slash_command() {
        assert!(!COMMANDS.iter().any(|(name, _)| *name == "plan"));
        assert!(matches!(
            resolve(
                &ParsedSlash {
                    name: "plan".into(),
                    args: String::new()
                },
                &SlashContext::default()
            ),
            SlashEffect::LocalNote(_)
        ));
    }

    #[test]
    fn parse_table() {
        assert_eq!(
            parse_slash("/help"),
            Some(ParsedSlash {
                name: "help".into(),
                args: String::new()
            })
        );

        assert!(parse_slash("hello").is_none());
        assert_eq!(parse_slash("/CLEAR").unwrap().name, "clear");
    }

    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "one table asserts the whole closed slash catalog"
    )]
    fn resolve_core_actions() {
        let ctx = SlashContext {
            phase: "idle".into(),
            ..SlashContext::default()
        };
        assert_eq!(
            resolve(&parse_slash("/clear").unwrap(), &ctx),
            SlashEffect::Action(UserAction::ClearSession)
        );
        assert_eq!(
            resolve(&parse_slash("/compact").unwrap(), &ctx),
            SlashEffect::Action(UserAction::Compact)
        );
        assert_eq!(
            resolve(&parse_slash("/config").unwrap(), &ctx),
            SlashEffect::Action(UserAction::Extension(SurfaceIntent::OpenConfigFile))
        );
        assert_eq!(
            resolve(&parse_slash("/resume").unwrap(), &ctx),
            SlashEffect::Action(UserAction::ResumeSession { id: None })
        );
        assert_eq!(
            resolve(&parse_slash("/tree").unwrap(), &ctx),
            SlashEffect::Action(UserAction::OpenTopologyNavigator)
        );
        assert_eq!(
            resolve(&parse_slash("/tree resume").unwrap(), &ctx),
            SlashEffect::Action(UserAction::ResumeTopologyTree)
        );
        assert_eq!(
            resolve(&parse_slash("/tree later").unwrap(), &ctx),
            SlashEffect::LocalNote("usage: /tree [resume]".to_owned())
        );
        assert_eq!(
            resolve(&parse_slash("/stop-all").unwrap(), &ctx),
            SlashEffect::Action(UserAction::StopAllTopology)
        );
        assert_eq!(
            resolve(&parse_slash("/resume deadbeef-uuid").unwrap(), &ctx),
            SlashEffect::Action(UserAction::ResumeSession {
                id: Some("deadbeef-uuid".into())
            })
        );
        assert_eq!(
            resolve(&parse_slash("/fork").unwrap(), &ctx),
            SlashEffect::Action(UserAction::ForkSession {
                account: None,
                compact: false,
            })
        );
        // The host refuses a fork onto another binding, so the catalog must not
        // advertise a target the resolver would only refuse.
        for line in ["/fork --to codex", "/fork codex"] {
            assert!(
                matches!(
                    resolve(&parse_slash(line).unwrap(), &ctx),
                    SlashEffect::LocalNote(note) if note.starts_with("usage: /fork")
                ),
                "{line} must not reach the host"
            );
        }
        let fork_help = COMMANDS
            .iter()
            .find(|(name, _)| *name == "fork")
            .map(|(_, help)| *help)
            .unwrap();
        assert!(!fork_help.contains("--to"), "{fork_help}");
        assert_eq!(
            resolve(&parse_slash("/usage").unwrap(), &ctx),
            SlashEffect::OpenUsage
        );
        assert_eq!(
            resolve(&parse_slash("/providers").unwrap(), &ctx),
            SlashEffect::OpenProviders
        );
        assert_eq!(
            resolve(&parse_slash("/context").unwrap(), &ctx),
            SlashEffect::OpenContext
        );
        assert_eq!(
            resolve(&parse_slash("/status").unwrap(), &ctx),
            SlashEffect::OpenSession
        );
        assert_eq!(
            resolve(&parse_slash("/settings").unwrap(), &ctx),
            SlashEffect::OpenSettings
        );
        assert_eq!(
            resolve(&parse_slash("/keybinds").unwrap(), &ctx),
            SlashEffect::OpenKeybinds
        );
        assert_eq!(
            resolve(&parse_slash("/quit").unwrap(), &ctx),
            SlashEffect::Quit
        );
    }

    #[test]
    fn a_command_without_arguments_refuses_trailing_text() {
        let ctx = SlashContext::default();
        for (line, name) in [
            ("/compact now", "compact"),
            ("/clear all", "clear"),
            ("/reload please", "reload"),
            ("/quit later", "quit"),
            ("/exit now", "exit"),
            ("/drop 2", "drop"),
            ("/stop-all now", "stop-all"),
            ("/help me", "help"),
            ("/status  x", "status"),
            ("/CONFIG edit", "config"),
        ] {
            assert_eq!(
                resolve(&parse_slash(line).unwrap(), &ctx),
                SlashEffect::LocalNote(format!("usage: /{name}  —  takes no arguments")),
                "{line} must be refused, not run"
            );
        }
        // Every bare-word catalog row stays runnable without arguments.
        for name in TAKES_NO_ARGUMENTS {
            assert!(
                !matches!(
                    resolve(&parse_slash(&format!("/{name}")).unwrap(), &ctx),
                    SlashEffect::LocalNote(_)
                ),
                "/{name} must still run bare"
            );
        }
        // Commands with a grammar keep parsing their arguments.
        assert_eq!(
            resolve(&parse_slash("/resume abc").unwrap(), &ctx),
            SlashEffect::Action(UserAction::ResumeSession {
                id: Some("abc".into())
            })
        );

        // An unknown command is still named as unknown, arguments or not.
        assert!(matches!(
            resolve(&parse_slash("/nope now").unwrap(), &ctx),
            SlashEffect::LocalNote(note) if note.contains("unknown command /nope")
        ));
    }

    #[test]
    fn resolve_local_notes() {
        let ctx = SlashContext {
            session_id: Some("s1".into()),
            account: Some("grok".into()),
            model: Some("m".into()),
            effort: Some("high".into()),
            phase: "idle".into(),
            queue_len: 2,
            busy: false,
        };
        match resolve(&parse_slash("/help").unwrap(), &ctx) {
            SlashEffect::OpenInfoOverlay { title, body } => {
                assert_eq!(title, "help");
                assert!(body.contains("/clear"));
            }
            other => panic!("expected help overlay, got {other:?}"),
        }
        // §2.1: `/status` routes to the Session tab, which builds every row
        // from typed host data (§5.5).
        assert_eq!(
            resolve(&parse_slash("/status").unwrap(), &ctx),
            SlashEffect::OpenSession
        );
        match resolve(&parse_slash("/keybinds").unwrap(), &ctx) {
            SlashEffect::OpenKeybinds => {}
            other => panic!("expected keybinds overlay, got {other:?}"),
        }
        match resolve(&parse_slash("/settings").unwrap(), &ctx) {
            SlashEffect::OpenSettings => {}
            other => panic!("expected the Settings tab, got {other:?}"),
        }
    }

    #[test]
    fn the_catalog_is_closed_and_carries_no_deferred_capability_rows() {
        // Spec 02 §5.1: a deferred capability must not be advertised as a
        // command, and neither may a removed surface. Login, logout
        // and API keys are CLI-only; the roster is config-only.
        for dead in [
            "skills",
            "mcp",
            "sandbox",
            "workflows",
            "login",
            "connect",
            "load-api-key",
        ] {
            assert!(
                !COMMANDS.iter().any(|(name, _)| *name == dead),
                "/{dead} must not be in the catalog"
            );
            assert!(fuzzy_list(dead).is_empty(), "/{dead} must not complete");
        }
        // Nothing left in the catalog is a placeholder row.
        for (name, help) in COMMANDS {
            assert!(
                !help.contains("not in v1"),
                "/{name} is still a placeholder row"
            );
            assert!(
                !help.contains("Control center"),
                "/{name} must not name the Menu tab: {help}"
            );
        }
    }

    #[test]
    fn resolve_auth_and_binding() {
        let ctx = SlashContext {
            phase: "idle".into(),
            ..SlashContext::default()
        };
        assert_eq!(
            resolve(&parse_slash("/attach").unwrap(), &ctx),
            SlashEffect::Action(UserAction::RetryAttach)
        );
        for retired in ["/login", "/connect"] {
            assert!(
                matches!(
                    resolve(&parse_slash(retired).unwrap(), &ctx),
                    SlashEffect::LocalNote(note) if note.contains("unknown command")
                ),
                "{retired}"
            );
        }
        // The binding changes only through templates and profiles: the
        // retired binding commands are unknown commands, absent from the
        // palette.
        for retired in ["model", "account", "effort"] {
            assert!(!COMMANDS.iter().any(|(name, _)| *name == retired));
            assert!(fuzzy_list(retired).iter().all(|(name, _)| *name != retired));
            for line in [format!("/{retired}"), format!("/{retired} high")] {
                assert_eq!(
                    resolve(&parse_slash(&line).unwrap(), &ctx),
                    SlashEffect::LocalNote(format!("unknown command /{retired} — try /help")),
                    "{line}"
                );
            }
        }
        // The product resolves the name, aliases included, from its list.
        assert_eq!(
            resolve(&parse_slash("/agents grok").unwrap(), &ctx),
            SlashEffect::ConfirmProfile("grok".into())
        );
        assert!(matches!(
            resolve(&parse_slash("/fork-then-compact").unwrap(), &ctx),
            SlashEffect::Action(UserAction::ForkSession { compact: true, .. })
        ));
    }

    #[test]
    fn fuzzy_list_filters() {
        let all = fuzzy_list("");
        assert_eq!(all.len(), COMMANDS.len());
        let help = fuzzy_list("he");
        assert!(help.iter().any(|(n, _)| *n == "help"));
        let clr = fuzzy_list("clr"); // subsequence c-l-r in clear
        assert!(clr.iter().any(|(n, _)| *n == "clear"));
        assert_eq!(fuzzy_list("zzznotfound"), [] as [(&str, &str); 0]);
    }
}
