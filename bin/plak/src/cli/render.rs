//! The help pages and the one renderer that prints them.
//!
//! Spec 02 §3.2 mandates one-level static pages, so the pages are static: one
//! file each under `bin/plak/help/`, included at compile time. The maintainer
//! feature gates page files, not `writeln!` blocks, and this module holds the
//! only parts a page cannot state for itself — today, the version line.

use std::io::{self, Write};

/// The single substitution a page may carry.
const VERSION_TOKEN: &str = "{version}";

/// The command list, up to the last released verb.
const GLOBAL_HEAD: &str = include_str!("../../help/global.txt");
/// The maintainer verbs, printed between the head and the tail when the
/// `maintainer` feature is on.
#[cfg(feature = "maintainer")]
const GLOBAL_MAINTAINER: &str = include_str!("../../help/global-maintainer.txt");
/// The pointer to `plak <command> --help` and the agentic-tools warning.
const GLOBAL_TAIL: &str = include_str!("../../help/global-tail.txt");

/// Every command page, in the order `plak help` lists the verbs.
///
/// A verb absent from this table falls through to the no-help arm, which is
/// what a retired verb (`init`, `setup`) must do.
const COMMAND_PAGES: &[(&str, &str)] = &[
    ("version", include_str!("../../help/version.txt")),
    ("login", include_str!("../../help/login.txt")),
    ("logout", include_str!("../../help/logout.txt")),
    ("status", include_str!("../../help/status.txt")),
    ("refresh", include_str!("../../help/refresh.txt")),
    ("key", include_str!("../../help/key.txt")),
    ("secret", include_str!("../../help/secret.txt")),
    ("run", include_str!("../../help/run.txt")),
    ("agent", include_str!("../../help/agent.txt")),
    ("resume", include_str!("../../help/resume.txt")),
    #[cfg(feature = "maintainer")]
    ("fork", include_str!("../../help/fork.txt")),
    #[cfg(feature = "maintainer")]
    ("compact", include_str!("../../help/compact.txt")),
    ("config", include_str!("../../help/config.txt")),
    ("doctor", include_str!("../../help/doctor.txt")),
    ("usage", include_str!("../../help/usage.txt")),
    ("models", include_str!("../../help/models.txt")),
    ("telemetry", include_str!("../../help/telemetry.txt")),
    ("session", include_str!("../../help/session.txt")),
    ("state", include_str!("../../help/state.txt")),
    #[cfg(feature = "maintainer")]
    (
        "compatibility",
        include_str!("../../help/compatibility.txt"),
    ),
];

/// `plak help`: the head, the maintainer verbs where the build has them, then
/// the tail.
pub(crate) fn global_help(out: &mut dyn Write) -> io::Result<()> {
    write!(
        out,
        "{}",
        GLOBAL_HEAD.replace(VERSION_TOKEN, &crate::build_identity::version_label())
    )?;
    #[cfg(feature = "maintainer")]
    write!(out, "{GLOBAL_MAINTAINER}")?;
    write!(out, "{GLOBAL_TAIL}")
}

/// `plak <command> --help`, and the bare-verb page the owner rule prints.
pub(crate) fn command_help(command: &str, out: &mut dyn Write) -> io::Result<()> {
    match COMMAND_PAGES
        .iter()
        .find(|(verb, _)| *verb == command)
        .map(|(_, page)| *page)
    {
        Some(page) => write!(out, "{page}"),
        None => write!(
            out,
            "No detailed help for `{command}`.\nRun `plak help` for the command list.\n"
        ),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests {
    use super::{command_help, global_help};

    fn rendered_command(command: &str) -> String {
        let mut output = Vec::new();
        command_help(command, &mut output).unwrap();
        String::from_utf8(output).unwrap()
    }

    #[test]
    fn global_help_uses_interactive_terminal_wording_and_has_no_setup_verbs() {
        let mut output = Vec::new();
        global_help(&mut output).unwrap();
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("interactive terminal required"));
        assert!(!output.contains("TTY required"));
        assert!(!output.contains("\n  init        "));
        assert!(!output.contains("\n  setup       "));
    }

    #[test]
    fn command_groups_have_complete_local_help() {
        for (command, expected) in [
            ("key", "plak key status [account]"),
            ("secret", "plak secret list"),
            ("config", "plak config init"),
            ("session", "plak session log <session>"),
        ] {
            let output = rendered_command(command);
            assert!(
                output.contains(expected),
                "missing {expected:?} in {command}"
            );
            assert!(!output.contains("No detailed help"));
        }
    }

    #[test]
    fn retired_setup_commands_have_no_detailed_help() {
        for command in ["init", "setup"] {
            assert!(rendered_command(command).contains("No detailed help"));
        }
    }

    /// Every page ends in exactly one newline, so a page file cannot grow or
    /// lose a trailing blank line without the test saying so.
    #[test]
    fn every_page_ends_in_one_newline() {
        for (verb, page) in super::COMMAND_PAGES {
            assert!(
                page.ends_with('\n') && !page.ends_with("\n\n"),
                "{verb} page does not end in exactly one newline"
            );
        }
    }
}
