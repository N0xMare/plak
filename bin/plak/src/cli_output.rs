//! Shared primitives for human-readable CLI output.

use std::ffi::OsStr;
use std::fmt::{self, Display, Formatter};
use std::io::{self, IsTerminal as _, Write};
use unicode_width::UnicodeWidthStr as _;

/// Whether output is going to an interactive terminal or another consumer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TerminalMode {
    Interactive,
    Piped,
}

impl TerminalMode {
    /// The process's stdout, resolved once per command: table framing and
    /// `plak run`'s assistant sanitization both follow it.
    pub(crate) fn of_stdout() -> Self {
        if io::stdout().is_terminal() {
            Self::Interactive
        } else {
            Self::Piped
        }
    }
}

/// Amount of optional human-facing detail to render.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum DetailLevel {
    #[default]
    Summary,
}

/// Resolved presentation policy for one output stream.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct HumanOutput {
    terminal: TerminalMode,
}

impl HumanOutput {
    pub(crate) fn resolve(
        terminal: TerminalMode,
        _no_color: Option<&OsStr>,
        _detail: DetailLevel,
    ) -> Self {
        Self { terminal }
    }

    pub(crate) fn write_table(self, out: &mut dyn Write, table: &Table) -> io::Result<()> {
        table.write(out, self.terminal)
    }
}

/// Table cells are one line. Anything longer is cut here, with `…`, so a
/// status repair that would be cut mid-path has to be shortened before `push`.
pub(crate) const TABLE_CELL_MAX_CHARS: usize = 512;

/// A small rectangular table with deterministic terminal and pipe rendering.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Table {
    headings: Vec<String>,
    rows: Vec<Vec<String>>,
}

impl Table {
    pub(crate) fn new(headings: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            headings: headings.into_iter().map(Into::into).collect(),
            rows: Vec::new(),
        }
    }

    pub(crate) fn push(
        &mut self,
        row: impl IntoIterator<Item = impl Into<String>>,
    ) -> Result<(), &'static str> {
        let row = row
            .into_iter()
            .map(Into::into)
            .map(|cell: String| safe_terminal_text(&cell, TABLE_CELL_MAX_CHARS))
            .collect::<Vec<_>>();
        if row.len() != self.headings.len() {
            return Err("table row does not match heading count");
        }
        self.rows.push(row);
        Ok(())
    }

    fn write(&self, out: &mut dyn Write, terminal: TerminalMode) -> io::Result<()> {
        match terminal {
            TerminalMode::Interactive => self.write_aligned(out),
            TerminalMode::Piped => self.write_delimited(out),
        }
    }

    fn write_delimited(&self, out: &mut dyn Write) -> io::Result<()> {
        write_delimited_row(out, &self.headings)?;
        for row in &self.rows {
            write_delimited_row(out, row)?;
        }
        Ok(())
    }

    fn write_aligned(&self, out: &mut dyn Write) -> io::Result<()> {
        let widths = (0..self.headings.len())
            .map(|column| {
                self.rows
                    .iter()
                    .map(|row| row[column].width())
                    .chain([self.headings[column].width()])
                    .max()
                    .unwrap_or(0)
            })
            .collect::<Vec<_>>();
        write_aligned_row(out, &self.headings, &widths)?;
        for row in &self.rows {
            write_aligned_row(out, row, &widths)?;
        }
        Ok(())
    }
}

fn write_delimited_row(out: &mut dyn Write, cells: &[String]) -> io::Result<()> {
    writeln!(out, "{}", cells.join("\t"))
}

fn write_aligned_row(out: &mut dyn Write, cells: &[String], widths: &[usize]) -> io::Result<()> {
    for (column, cell) in cells.iter().enumerate() {
        if column > 0 {
            write!(out, "  ")?;
        }
        write!(out, "{cell}")?;
        if column + 1 < cells.len() {
            let padding = widths[column].saturating_sub(cell.width());
            write!(out, "{}", " ".repeat(padding))?;
        }
    }
    writeln!(out)
}

/// Projects untrusted human text onto one bounded terminal-safe line.
pub(crate) fn safe_terminal_text(value: &str, max_chars: usize) -> String {
    let mut output = String::with_capacity(value.len().min(max_chars));
    for (retained, character) in value.chars().enumerate() {
        if retained == max_chars {
            output.push('…');
            break;
        }
        let projected = match character {
            '\n' | '\r' => '↵',
            '\t' => ' ',
            value if is_terminal_hazard(value) => '�',
            value => value,
        };
        output.push(projected);
    }
    output
}

/// Projects untrusted multi-line text onto terminal-safe text, keeping its
/// line structure: newlines and tabs survive, `\r\n` becomes `\n`, and every
/// other control or bidirectional override becomes the replacement character,
/// so no escape sequence can reach the terminal.
pub(crate) fn safe_terminal_block(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let mut characters = value.chars().peekable();
    while let Some(character) = characters.next() {
        let projected = match character {
            '\r' if characters.peek() == Some(&'\n') => continue,
            '\n' | '\t' => character,
            value if is_terminal_hazard(value) => '�',
            value => value,
        };
        output.push(projected);
    }
    output
}

/// Projects one untrusted block for its destination. Pipes retain the exact
/// bytes supplied by the caller; terminals receive inert text, bounded after
/// projection because one replacement character can occupy more bytes than
/// the control byte it replaces.
pub(crate) fn terminal_block(
    value: &str,
    terminal: TerminalMode,
    maximum_bytes: Option<usize>,
) -> String {
    let mut output = match terminal {
        TerminalMode::Interactive => safe_terminal_block(value),
        TerminalMode::Piped => value.to_owned(),
    };
    if let Some(maximum) = maximum_bytes.filter(|maximum| output.len() > *maximum) {
        let mut end = maximum;
        while end > 0 && !output.is_char_boundary(end) {
            end -= 1;
        }
        output.truncate(end);
    }
    output
}

fn is_terminal_hazard(value: char) -> bool {
    value.is_control() || is_bidi_format(value)
}

const fn is_bidi_format(value: char) -> bool {
    matches!(
        value,
        '\u{061c}'
            | '\u{200e}'
            | '\u{200f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2066}'..='\u{2069}'
    )
}

/// Stable product-facing name for an account/provider selector.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ProviderLabel(String);

impl ProviderLabel {
    pub(crate) fn new(provider: &str) -> Self {
        let label = match provider {
            "codex" => "Codex Subscription",
            "grok" | "supergrok" => "Grok Subscription",
            "claude" => "Claude Subscription",
            "xai" => "xAI API",
            "openai" => "OpenAI API",
            "anthropic" => "Anthropic API",
            "openrouter" => "OpenRouter API",
            other => other,
        };
        Self(label.to_owned())
    }
}

impl Display for ProviderLabel {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests;
