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
    /// The process working directory and `$HOME`.
    pub(super) fn current() -> Option<Self> {
        Some(Self {
            cwd: std::env::current_dir().ok()?,
            home: std::env::var_os("HOME")
                .map(PathBuf::from)
                .filter(|home| home.is_absolute()),
        })
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
mod tests {
    use super::*;

    struct Fixture {
        _dir: tempfile::TempDir,
        base: PathBase,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let cwd = dir.path().join("work");
            let home = dir.path().join("home");
            std::fs::create_dir_all(cwd.join("shots")).unwrap();
            std::fs::create_dir_all(home.join("Desktop")).unwrap();
            let png = crate::config::image_attachments::png(4, 3, 16);
            for path in [
                cwd.join("shots/a.png"),
                cwd.join("b c.png"),
                home.join("Desktop/Screenshot 2026-10-16 at 10.23.45\u{202f}AM.png"),
                home.join("Desktop/it's \"q\".png"),
                home.join("cat.png"),
            ] {
                std::fs::write(path, &png).unwrap();
            }
            std::fs::write(cwd.join("notes.png"), "not an image").unwrap();
            Self {
                base: PathBase {
                    cwd,
                    home: Some(home),
                },
                _dir: dir,
            }
        }

        fn cwd(&self, rest: &str) -> PathBuf {
            self.base.cwd.join(rest)
        }

        fn home(&self, rest: &str) -> PathBuf {
            self.base.home.as_ref().unwrap().join(rest)
        }
    }

    #[test]
    fn every_terminal_drop_form_resolves_to_the_image() {
        let fixture = Fixture::new();
        let screenshot = fixture.home("Desktop/Screenshot 2026-10-16 at 10.23.45\u{202f}AM.png");
        let absolute = screenshot.display().to_string();
        let home = fixture.home("").display().to_string();
        let cases = [
            // macOS Terminal: backslash-escaped spaces, the narrow space literal.
            absolute.replace(' ', "\\ "),
            // A bare path with spaces.
            absolute.clone(),
            format!("'{absolute}'"),
            format!("\"{absolute}\"  \n"),
            "~/Desktop/Screenshot\\ 2026-10-16\\ at\\ 10.23.45\u{202f}AM.png".to_owned(),
            // The narrow space relayed as a plain one.
            "~/Desktop/Screenshot\\ 2026-10-16\\ at\\ 10.23.45\\ AM.png".to_owned(),
            format!(
                "file://{}",
                absolute
                    .replace(' ', "%20")
                    .replace('\u{202f}', "%E2%80%AF")
            ),
            format!(
                "file://localhost{}/Desktop/Screenshot%202026-10-16%20at%2010.23.45%E2%80%AFAM.png",
                home.trim_end_matches('/')
            ),
        ];
        for case in cases {
            assert_eq!(
                fixture.base.dropped_images(&case),
                Some(vec![screenshot.clone()]),
                "{case:?}"
            );
        }
        assert_eq!(
            fixture.base.dropped_images("shots/a.png"),
            Some(vec![fixture.cwd("shots/a.png")])
        );
        assert_eq!(
            fixture.base.dropped_images("~/cat.png"),
            Some(vec![fixture.home("cat.png")])
        );
        assert_eq!(
            fixture.base.dropped_images(r#"~/Desktop/it\'s\ \"q\".png"#),
            Some(vec![fixture.home("Desktop/it's \"q\".png")])
        );
    }

    #[test]
    fn several_dropped_paths_attach_together_only_when_all_are_images() {
        let fixture = Fixture::new();
        assert_eq!(
            fixture
                .base
                .dropped_images("shots/a.png b\\ c.png\n~/cat.png"),
            Some(vec![
                fixture.cwd("shots/a.png"),
                fixture.cwd("b c.png"),
                fixture.home("cat.png"),
            ])
        );
        // One word that is not an existing image keeps the whole paste text.
        assert_eq!(fixture.base.dropped_images("shots/a.png shots/b.png"), None);
        assert_eq!(
            fixture
                .base
                .dropped_images("shots/a.png \"b c.png\" ~/cat.png"),
            Some(vec![
                fixture.cwd("shots/a.png"),
                fixture.cwd("b c.png"),
                fixture.home("cat.png"),
            ])
        );
    }

    #[test]
    fn pastes_that_are_not_existing_images_stay_text() {
        let fixture = Fixture::new();
        for text in [
            "",
            "   ",
            "hello world",
            "look at shots/a.png",
            "notes.png",
            "shots/missing.png",
            "shots",
            "'shots/a.png",
            "shots/a.png\\",
            "file://relative/a.png",
            "file:///bad%zz.png",
        ] {
            assert_eq!(fixture.base.dropped_images(text), None, "{text:?}");
        }
        let many = vec!["shots/a.png"; MAX_DROPPED_PATHS + 1].join(" ");
        assert_eq!(fixture.base.dropped_images(&many), None);
    }

    #[test]
    fn slash_image_names_one_or_several_files() {
        let fixture = Fixture::new();
        assert_eq!(
            fixture.base.named_files("b c.png"),
            vec![fixture.cwd("b c.png")]
        );
        assert_eq!(
            fixture.base.named_files("~/cat.png shots/a.png"),
            vec![fixture.home("cat.png"), fixture.cwd("shots/a.png")]
        );
        assert_eq!(
            fixture.base.named_files("missing.png"),
            vec![fixture.cwd("missing.png")]
        );
        assert_eq!(fixture.base.named_files(" "), Vec::<PathBuf>::new());
    }
}
