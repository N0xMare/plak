//! The front matter subset skills use: a leading `---` block of top-level
//! `key: value` scalars. Plain, single-quoted, double-quoted and `|`/`>` block
//! scalars are read; nested values under other keys are skipped. Anything else
//! in `name`, `description` or `disable-model-invocation` is refused rather
//! than guessed at.

use super::{MAX_SKILL_NAME_CHARS, SkillProblem};

pub(super) struct Parsed<'a> {
    pub(super) name: Option<String>,
    pub(super) description: Option<String>,
    pub(super) disable_model_invocation: bool,
    pub(super) body: &'a str,
}

/// Why `name` breaks the Agent Skills name rules, if it does.
pub(super) fn name_problem(name: &str) -> Option<&'static str> {
    if name.is_empty() {
        return Some("is empty");
    }
    if name.chars().count() > MAX_SKILL_NAME_CHARS {
        return Some("exceeds 64 characters");
    }
    if !name
        .bytes()
        .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        return Some("may use only lowercase letters, digits and hyphens");
    }
    if name.starts_with('-') || name.ends_with('-') || name.contains("--") {
        return Some("may not start or end with a hyphen or repeat one");
    }
    None
}

/// The text from the start through the closing fence line, when `text`
/// holds a complete front matter block.
pub(super) fn leading_block(text: &str) -> Option<&str> {
    let start = text.len() - strip_bom(text).len();
    let mut lines = LineCursor::new(&text[start..]);
    if lines.next().map(|(line, _)| fence(line)) != Some(Fence::Open) {
        return None;
    }
    while let Some((line, rest)) = lines.next() {
        if fence(line) != Fence::None {
            return Some(&text[..text.len() - rest.len()]);
        }
    }
    None
}

pub(super) fn parse(text: &str) -> Result<Parsed<'_>, SkillProblem> {
    let text = strip_bom(text);
    let mut lines = LineCursor::new(text);
    if lines.next().map(|(line, _)| fence(line)) != Some(Fence::Open) {
        return Err(SkillProblem::FrontMatter("is missing"));
    }
    let mut block = Vec::new();
    let body = loop {
        let Some((line, rest)) = lines.next() else {
            return Err(SkillProblem::FrontMatter("is not closed"));
        };
        if fence(line) != Fence::None {
            break rest;
        }
        block.push(line);
    };
    let (mut name, mut description, mut disable) = (None, None, None);
    let mut index = 0;
    while index < block.len() {
        let line = block[index];
        index += 1;
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        if line.starts_with([' ', '\t']) {
            return Err(SkillProblem::FrontMatter("has an unexpected indented line"));
        }
        let (key, value) = split_key(line)?;
        let continuation_start = index;
        while index < block.len()
            && (block[index].starts_with([' ', '\t']) || block[index].trim().is_empty())
        {
            index += 1;
        }
        let continuation = &block[continuation_start..index];
        let target: &mut Option<Option<String>> = match key.as_str() {
            "name" => &mut name,
            "description" => &mut description,
            "disable-model-invocation" => &mut disable,
            _ => continue,
        };
        if target.is_some() {
            return Err(SkillProblem::FrontMatter("repeats a key"));
        }
        *target = Some(scalar(value.trim(), continuation)?);
    }
    let disable_model_invocation = match disable.flatten().as_deref() {
        None | Some("false" | "False" | "FALSE") => false,
        Some("true" | "True" | "TRUE") => true,
        Some(_) => {
            return Err(SkillProblem::FrontMatter(
                "has a disable-model-invocation that is not true or false",
            ));
        }
    };
    Ok(Parsed {
        name: name.flatten(),
        description: description.flatten(),
        disable_model_invocation,
        body,
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Fence {
    Open,
    Close,
    None,
}

fn fence(line: &str) -> Fence {
    match line.trim_end() {
        "---" => Fence::Open,
        "..." => Fence::Close,
        _ => Fence::None,
    }
}

fn strip_bom(text: &str) -> &str {
    text.strip_prefix('\u{feff}').unwrap_or(text)
}

/// A top-level `key: value` line; the key may be quoted.
fn split_key(line: &str) -> Result<(String, &str), SkillProblem> {
    let not_key = SkillProblem::FrontMatter("has a line that is not key: value");
    for quote in ['"', '\''] {
        if let Some(rest) = line.strip_prefix(quote) {
            let (key, after) = rest.split_once(quote).ok_or(not_key.clone())?;
            let value = after.trim_start().strip_prefix(':').ok_or(not_key)?;
            return Ok((key.to_owned(), value));
        }
    }
    let (key, value) = line.split_once(':').ok_or(not_key)?;
    Ok((key.trim().to_owned(), value))
}

/// One scalar; `Ok(None)` is YAML's null (empty, `~` or `null`).
fn scalar(value: &str, continuation: &[&str]) -> Result<Option<String>, SkillProblem> {
    let folded_lines = || {
        continuation
            .iter()
            .map(|line| line.trim())
            .collect::<Vec<_>>()
    };
    if let Some(style) = value.strip_prefix(['|', '>']) {
        let indicator = strip_comment(style);
        if !indicator
            .chars()
            .all(|character| matches!(character, '-' | '+' | '1'..='9'))
        {
            return Err(SkillProblem::FrontMatter("has an unsupported block scalar"));
        }
        let lines = folded_lines();
        let joined = if value.starts_with('|') {
            lines.join("\n")
        } else {
            lines.join(" ")
        };
        return Ok(Some(joined.trim().to_owned()));
    }
    for quote in ['"', '\''] {
        let Some(inner) = value.strip_prefix(quote) else {
            continue;
        };
        no_continuation(continuation)?;
        let unterminated = SkillProblem::FrontMatter("has an unterminated quoted value");
        let (inner, after) = if quote == '"' {
            split_double_quoted(inner).ok_or(unterminated)?
        } else {
            split_single_quoted(inner).ok_or(unterminated)?
        };
        if !strip_comment(after).is_empty() {
            return Err(SkillProblem::FrontMatter("has text after a quoted value"));
        }
        return if quote == '"' {
            double_quoted(inner).map(Some)
        } else {
            Ok(Some(inner.replace("''", "'")))
        };
    }
    if value.starts_with(['[', '{', '&', '*', '!']) {
        return Err(SkillProblem::FrontMatter("has an unsupported value"));
    }
    let mut parts = vec![strip_comment(value)];
    parts.extend(folded_lines().into_iter().map(strip_comment));
    let joined = parts
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    Ok(match joined.as_str() {
        "" | "~" | "null" | "Null" | "NULL" => None,
        _ => Some(joined),
    })
}

/// The inside of a double-quoted scalar and the text after its closing quote.
fn split_double_quoted(inner: &str) -> Option<(&str, &str)> {
    let mut escaped = false;
    for (index, character) in inner.char_indices() {
        match character {
            _ if escaped => escaped = false,
            '\\' => escaped = true,
            '"' => return Some((&inner[..index], &inner[index + 1..])),
            _ => {}
        }
    }
    None
}

/// The inside of a single-quoted scalar (`''` is a quote) and the text after.
fn split_single_quoted(inner: &str) -> Option<(&str, &str)> {
    let bytes = inner.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'\'' {
            if bytes.get(index + 1) == Some(&b'\'') {
                index += 2;
                continue;
            }
            return Some((&inner[..index], &inner[index + 1..]));
        }
        index += 1;
    }
    None
}

fn no_continuation(continuation: &[&str]) -> Result<(), SkillProblem> {
    if continuation.iter().all(|line| line.trim().is_empty()) {
        Ok(())
    } else {
        Err(SkillProblem::FrontMatter("has a multi-line quoted value"))
    }
}

/// A plain scalar without its trailing comment: `#` after a space or tab.
fn strip_comment(value: &str) -> &str {
    let cut = value
        .char_indices()
        .find(|&(index, character)| {
            character == '#' && (index == 0 || value[..index].ends_with([' ', '\t']))
        })
        .map_or(value.len(), |(index, _)| index);
    value[..cut].trim()
}

fn double_quoted(inner: &str) -> Result<String, SkillProblem> {
    let unsupported = || SkillProblem::FrontMatter("has an unsupported escape");
    let mut out = String::with_capacity(inner.len());
    let mut characters = inner.chars();
    while let Some(character) = characters.next() {
        if character != '\\' {
            out.push(character);
            continue;
        }
        match characters.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('0') => out.push('\0'),
            Some('"') => out.push('"'),
            Some('\\') => out.push('\\'),
            Some('/') => out.push('/'),
            Some('u') => {
                let hex: String = characters.by_ref().take(4).collect();
                let code = (hex.len() == 4)
                    .then(|| u32::from_str_radix(&hex, 16).ok())
                    .flatten()
                    .and_then(char::from_u32)
                    .ok_or_else(unsupported)?;
                out.push(code);
            }
            _ => return Err(unsupported()),
        }
    }
    Ok(out)
}

/// Yields each line without its terminator, and the text after it.
struct LineCursor<'a> {
    rest: &'a str,
}

impl<'a> LineCursor<'a> {
    const fn new(text: &'a str) -> Self {
        Self { rest: text }
    }

    fn next(&mut self) -> Option<(&'a str, &'a str)> {
        if self.rest.is_empty() {
            return None;
        }
        let (line, rest) = self.rest.split_once('\n').unwrap_or((self.rest, ""));
        self.rest = rest;
        Some((line.strip_suffix('\r').unwrap_or(line), rest))
    }
}
