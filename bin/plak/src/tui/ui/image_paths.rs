//! Image paths an operator drops or pastes on the composer, or names after
//! `/image`, in every form terminals produce: absolute, `~/`, relative to the
//! working directory, backslash-escaped, single- or double-quoted, `file://`
//! URLs, and several at once.

use std::path::{Path, PathBuf};

/// Most words a paste may have and still be read as dropped files; a longer
/// paste is text without touching the filesystem.
const MAX_DROPPED_PATHS: usize = 32;

/// Where relative and `~` paths resolve.
pub(super) struct PathBase {
    pub(super) cwd: PathBuf,
    pub(super) home: Option<PathBuf>,
}

impl PathBase {
    /// The launch directory `cwd` and `$HOME`.
    pub(super) fn at(cwd: &Path) -> Self {
        Self {
            cwd: cwd.to_path_buf(),
            home: std::env::var_os("HOME")
                .map(PathBuf::from)
                .filter(|home| home.is_absolute()),
        }
    }

    /// The image files a paste names when every word of it is one; `None`
    /// keeps the paste as text.
    pub(super) fn dropped_images(&self, text: &str) -> Option<Vec<PathBuf>> {
        let text = text.trim_matches(|ch: char| ch.is_ascii_whitespace());
        if text.is_empty() {
            return None;
        }
        // A terminal that does not escape a drop pastes the bare path.
        if !text.contains('\n')
            && let Some(path) = self.resolve(text).filter(|path| is_image(path))
        {
            return Some(vec![path]);
        }
        let words = split_words(text)?;
        if words.is_empty() || words.len() > MAX_DROPPED_PATHS {
            return None;
        }
        words
            .iter()
            .map(|word| self.resolve(word).filter(|path| is_image(path)))
            .collect()
    }

    /// The files `/image` names: the whole argument when it is an existing
    /// file, else each word. The reader reports what is not an image.
    pub(super) fn named_files(&self, text: &str) -> Vec<PathBuf> {
        let text = text.trim_matches(|ch: char| ch.is_ascii_whitespace());
        if text.is_empty() {
            return Vec::new();
        }
        if let Some(path) = self.resolve(text).filter(|path| path.is_file()) {
            return vec![path];
        }
        match split_words(text) {
            Some(words) => words.iter().filter_map(|word| self.resolve(word)).collect(),
            None => self.resolve(text).into_iter().collect(),
        }
    }

    /// One word as an absolute path. macOS names screenshots with a narrow
    /// no-break space before `AM`/`PM`; a path typed or relayed with a plain
    /// space there finds the file too.
    fn resolve(&self, word: &str) -> Option<PathBuf> {
        let word = match word.strip_prefix("file://") {
            Some(url) => percent_decode(url.strip_prefix("localhost").unwrap_or(url))?,
            None => word.to_owned(),
        };
        let path = if word == "~" {
            self.home.clone()?
        } else if let Some(rest) = word.strip_prefix("~/") {
            self.home.as_ref()?.join(rest)
        } else {
            self.cwd.join(&word)
        };
        if path.exists() {
            return Some(path);
        }
        let name = path.file_name()?.to_str()?;
        let narrow = name
            .replace(" AM.", "\u{202f}AM.")
            .replace(" PM.", "\u{202f}PM.");
        if narrow != name {
            let candidate = path.with_file_name(narrow);
            if candidate.exists() {
                return Some(candidate);
            }
        }
        Some(path)
    }
}

/// Shell-style words: unquoted ASCII whitespace separates, a backslash takes
/// the next character literally, quotes group. `None` for an unterminated
/// quote or a trailing backslash. The narrow no-break space is not a
/// separator.
fn split_words(text: &str) -> Option<Vec<String>> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut chars = text.chars();
    while let Some(ch) = chars.next() {
        match ch {
            '\\' => {
                word.push(chars.next()?);
                in_word = true;
            }
            '\'' => {
                loop {
                    match chars.next()? {
                        '\'' => break,
                        ch => word.push(ch),
                    }
                }
                in_word = true;
            }
            '"' => {
                loop {
                    match chars.next()? {
                        '"' => break,
                        '\\' => word.push(chars.next()?),
                        ch => word.push(ch),
                    }
                }
                in_word = true;
            }
            ch if ch.is_ascii_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut word));
                    in_word = false;
                }
            }
            ch => {
                word.push(ch);
                in_word = true;
            }
        }
    }
    if in_word {
        words.push(word);
    }
    Some(words)
}

/// The path of a `file://` URL: absolute, `%XX` escapes decoded as UTF-8.
fn percent_decode(path: &str) -> Option<String> {
    if !path.starts_with('/') {
        return None;
    }
    let mut bytes = Vec::with_capacity(path.len());
    let mut rest = path.as_bytes();
    while let Some((&byte, tail)) = rest.split_first() {
        if byte == b'%' {
            let hex = tail.get(..2)?;
            bytes.push(u8::from_str_radix(std::str::from_utf8(hex).ok()?, 16).ok()?);
            rest = &tail[2..];
        } else {
            bytes.push(byte);
            rest = tail;
        }
    }
    String::from_utf8(bytes).ok()
}

fn is_image(path: &Path) -> bool {
    crate::config::image_attachments::is_image_file(path)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests may assert with unwrap")]
mod tests;
