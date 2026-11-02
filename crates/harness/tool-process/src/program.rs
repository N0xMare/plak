//! The program a start runs, as a content-free label for measurement.
//!
//! The label is a basename in `[A-Za-z0-9._+-]`, at most 32 bytes, not all
//! digits and not shaped like a credential, or `other`. Assignments, wrappers
//! and their options, quotes and directories are skipped or stripped; anything
//! else that could carry a value, a secret, a path or an argument becomes
//! `other`.

const OTHER: &str = "other";
const MAX_PROGRAM_BYTES: usize = 32;
/// Prefixes of common issued tokens and access keys.
const SECRET_PREFIXES: [&str; 12] = [
    "ghp_",
    "gho_",
    "ghu_",
    "ghs_",
    "ghr_",
    "github_pat_",
    "glpat-",
    "sk-",
    "xox",
    "AKIA",
    "ASIA",
    "AIza",
];
/// A letter-and-digit run this long reads as a key, not a program name.
const SECRET_RUN_BYTES: usize = 16;

/// Classifies the first program a shell command line runs.
pub(crate) fn shell_program(command: &str) -> String {
    let mut words = command.split_whitespace().peekable();
    loop {
        let Some(word) = words.next() else {
            return OTHER.to_owned();
        };
        let Some(word) = unquote(word.trim_start_matches(['(', '{'])) else {
            // A quoted word with whitespace inside: no safe basename.
            return OTHER.to_owned();
        };
        if word.is_empty() || is_assignment(word) {
            continue;
        }
        let name = basename(word.trim_end_matches([')', '}', ';']));
        match name {
            "cd" | "pushd" => {
                // `cd DIR && program`: the program after the separator.
                if !skip_past_separator(&mut words) {
                    return label(name);
                }
            }
            "sudo" | "env" | "time" | "nice" | "nohup" | "command" | "exec" | "timeout" => {
                skip_wrapper_options(name, &mut words);
            }
            _ => return label(name),
        }
    }
}

/// Classifies a literal argv program.
pub(crate) fn argv_program(program: &str) -> String {
    label(basename(program))
}

fn label(name: &str) -> String {
    if !name.is_empty()
        && name.len() <= MAX_PROGRAM_BYTES
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'+' | b'-'))
        && !name.bytes().all(|byte| byte.is_ascii_digit())
        && !name.starts_with('-')
        && !secret_shaped(name)
    {
        name.to_owned()
    } else {
        OTHER.to_owned()
    }
}

/// A known token prefix, or a long run mixing letters and digits.
fn secret_shaped(name: &str) -> bool {
    SECRET_PREFIXES
        .iter()
        .any(|prefix| name.starts_with(prefix))
        || name
            .split(|character: char| !character.is_ascii_alphanumeric())
            .any(|run| {
                run.len() >= SECRET_RUN_BYTES
                    && run.bytes().any(|byte| byte.is_ascii_digit())
                    && run.bytes().any(|byte| byte.is_ascii_alphabetic())
            })
}

fn unquote(word: &str) -> Option<&str> {
    let Some(quote) = word
        .chars()
        .next()
        .filter(|first| matches!(first, '\'' | '"'))
    else {
        return Some(word);
    };
    word.strip_prefix(quote)?.strip_suffix(quote)
}

fn basename(word: &str) -> &str {
    word.rsplit('/').next().unwrap_or(word)
}

fn is_assignment(word: &str) -> bool {
    word.split_once('=').is_some_and(|(name, _)| {
        name.bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_')
            && name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    })
}

fn skip_past_separator<'a>(words: &mut impl Iterator<Item = &'a str>) -> bool {
    for word in words {
        if matches!(word, "&&" | ";" | "||") || word.ends_with(';') {
            return true;
        }
    }
    false
}

/// Skips a wrapper's options, option arguments and leading operands so the
/// next word is the wrapped program.
fn skip_wrapper_options<'a>(
    wrapper: &str,
    words: &mut std::iter::Peekable<impl Iterator<Item = &'a str>>,
) {
    let takes_argument: &[&str] = match wrapper {
        "sudo" => &["-u", "-g", "-U", "-C", "-D", "-h", "-p", "-r", "-t", "-T"],
        "env" => &["-u", "-C", "-S"],
        "nice" => &["-n"],
        "exec" => &["-a"],
        "timeout" => &["-s", "-k"],
        "time" => &["-f", "-o"],
        _ => &[],
    };
    while let Some(word) = words.peek().copied() {
        if takes_argument.contains(&word) {
            words.next();
            words.next();
        } else if word.starts_with('-') || (wrapper == "env" && is_assignment(word)) {
            words.next();
        } else {
            break;
        }
    }
    if wrapper == "timeout" {
        // The duration operand precedes the program.
        words.next();
    }
}

#[cfg(test)]
mod tests {
    use super::{argv_program, shell_program};

    #[test]
    fn keeps_only_the_program_basename() {
        for (command, expected) in [
            ("cargo test -- --nocapture", "cargo"),
            ("/usr/bin/git status", "git"),
            ("  rg -n needle src", "rg"),
            ("python3.12 -m pytest", "python3.12"),
            ("g++ -O2 main.cc", "g++"),
            ("'/opt/my tools/run' --flag", "other"),
            ("\"/usr/local/bin/ssh\" host", "ssh"),
            ("./scripts/build.sh release", "build.sh"),
        ] {
            assert_eq!(shell_program(command), expected, "{command}");
        }
    }

    #[test]
    fn never_stores_an_assignment_or_its_value() {
        for (command, expected) in [
            ("PGPASSWORD=hunter2 psql -h db", "psql"),
            ("GITHUB_TOKEN=ghp_secret gh pr list", "gh"),
            ("A=1 B=2 make", "make"),
            ("env API_KEY=sk-secret curl x", "curl"),
            ("env -u HOME TOKEN=x node app.js", "node"),
            ("TOKEN=secret", "other"),
        ] {
            let program = shell_program(command);
            assert_eq!(program, expected, "{command}");
            assert!(!program.contains('='));
            assert!(!program.contains("secret") && !program.contains("hunter2"));
        }
    }

    #[test]
    fn a_token_shaped_word_is_never_a_label() {
        for command in [
            "ghp_abcdefghijklmnopqrstuvwxyz0123456789 --x",
            "gho_shortish run",
            "sk-proj-abc def",
            "xoxb-1234-abcd post",
            "AKIAIOSFODNN7EXAMPLE",
            "a1b2c3d4e5f6a7b8c9d0 --flag",
            "deploy-9f86d081884c7d659a2feaa0c55ad015 now",
        ] {
            assert_eq!(shell_program(command), "other", "{command:?}");
        }
        for (command, expected) in [
            ("x86_64-linux-gnu-gcc -c a.c", "x86_64-linux-gnu-gcc"),
            ("python3.12 -V", "python3.12"),
            ("skopeo inspect", "skopeo"),
        ] {
            assert_eq!(shell_program(command), expected, "{command}");
        }
    }

    #[test]
    fn skips_wrappers_and_their_options() {
        for (command, expected) in [
            ("sudo -u postgres psql", "psql"),
            ("sudo apt-get install x", "apt-get"),
            ("time cargo build", "cargo"),
            ("nice -n 10 make -j8", "make"),
            ("nohup ./server &", "server"),
            ("command -v git", "git"),
            ("exec python3 app.py", "python3"),
            ("timeout 60 cargo test", "cargo"),
            ("timeout -s KILL 5m just fast", "just"),
            ("cd crates/agent && cargo test", "cargo"),
            ("cd /tmp; ls -la", "ls"),
            ("(cd sub && make)", "make"),
            ("cd somewhere", "cd"),
        ] {
            assert_eq!(shell_program(command), expected, "{command}");
        }
    }

    #[test]
    fn anything_that_could_carry_a_value_is_other() {
        for command in [
            "",
            "   ",
            "$CMD --flag",
            "`which cargo` build",
            "1234 run",
            "--version",
            "prog$(id)",
            "~/bin/a:b",
            "a-very-long-program-name-past-32-bytes",
        ] {
            assert_eq!(shell_program(command), "other", "{command:?}");
        }
        assert_eq!(argv_program("/usr/bin/env"), "env");
        assert_eq!(argv_program("/opt/a b/run"), "run");
        assert_eq!(argv_program("12345"), "other");
    }
}
