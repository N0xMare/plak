//! P0 binary contracts: real `plak` process under isolated `PLAK_HOME`.
//!
//! These tests catch wiring/clap/path bugs that unit tests miss. No network.

#![allow(
    missing_docs,
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "integration test crate may unwrap"
)]

use std::io::{Read as _, Write as _};
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, Barrier, Mutex};
use std::time::{Duration, Instant};

use portable_pty::{CommandBuilder, PtySize, native_pty_system};

fn plak_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_plak"))
}

fn publication_command(root: &std::path::Path, args: &[&str]) -> Command {
    let mut command = Command::new(plak_bin());
    command
        .current_dir(root)
        .env("PLAK_HOME", root.join("home"))
        .env("HOME", root)
        .env("XDG_STATE_HOME", root.join("xdg"))
        .env_remove("PLAK_LOG")
        .env_remove("PLAK_OTLP_ENDPOINT")
        .env_remove("PLAK_TEST_SCRIPT")
        .env_remove("VISUAL")
        .env_remove("EDITOR");
    for key in [
        "OPENAI_API_KEY",
        "XAI_API_KEY",
        "ANTHROPIC_API_KEY",
        "OPENROUTER_API_KEY",
    ] {
        command.env_remove(key);
    }
    command.args(args);
    command
}

#[test]
fn publication_read_only_commands_leave_missing_home_untouched() {
    for args in [
        vec!["status"],
        vec!["doctor"],
        vec!["key", "status"],
        vec!["config", "show"],
    ] {
        let root = tempfile::tempdir().unwrap();
        let out = publication_command(root.path(), &args).output().unwrap();
        assert!(
            out.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(!root.path().join("home").exists(), "{args:?} created home");
        assert!(
            !root.path().join("xdg").exists(),
            "{args:?} created subscription state"
        );
    }
}

#[test]
fn publication_doctor_missing_optional_state_is_normal_first_run() {
    let root = tempfile::tempdir().unwrap();
    let out = publication_command(root.path(), &["doctor"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    for label in [
        "Config",
        "Product db",
        "Runtime db",
        "Subagents db",
        "Home",
        "Secrets dir",
        "Sessions dir",
    ] {
        let row = stdout
            .lines()
            .find(|line| line.starts_with(&format!("{label}\t")))
            .unwrap();
        assert_eq!(row.split('\t').nth(1), Some("OK"), "{row}");
    }
}

#[test]
fn publication_doctor_uses_account_and_acronym_labels() {
    let root = tempfile::tempdir().unwrap();
    let out = publication_command(root.path(), &["doctor"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    for label in [
        "OAuth codex",
        "OAuth grok",
        "OAuth claude",
        "OpenRouter catalog",
    ] {
        assert!(
            stdout
                .lines()
                .any(|line| line.starts_with(&format!("{label}\t"))),
            "missing {label}: {stdout}"
        );
    }
}

#[test]
fn publication_missing_session_is_usage_without_creating_state() {
    for verb in ["show", "log"] {
        let root = tempfile::tempdir().unwrap();
        let out = publication_command(root.path(), &["session", verb, "abcd"])
            .output()
            .unwrap();
        assert_eq!(
            out.status.code(),
            Some(2),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            String::from_utf8_lossy(&out.stderr)
                .contains("no session in this workspace starts with `abcd`")
        );
        assert!(!root.path().join("home").exists());
    }
}

#[test]
fn publication_reference_resolves_short_id_and_names_missing_path() {
    use plak::home::paths::PlakPaths;
    use plak::store::ProductStore;
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let paths = PlakPaths::resolve(Some(home.as_os_str()), None).unwrap();
    paths.ensure_layout().unwrap();
    let mut store = ProductStore::open(&paths).unwrap();
    let mut record = plak::store::sessions::SessionRecord::new(root.path().to_path_buf());
    store.commit(&mut record).unwrap();
    let out = publication_command(root.path(), &["session", "reference", &record.id[..8]])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("could not inspect retained reference"),
        "{stderr}"
    );
    assert!(stderr.contains(&record.id), "{stderr}");
    {
        use std::os::unix::fs::PermissionsExt as _;
        let references = home.join("references");
        std::fs::create_dir_all(&references).unwrap();
        std::fs::set_permissions(&references, std::fs::Permissions::from_mode(0o700)).unwrap();
        let archive = references.join(format!("{}.json", record.id));
        std::fs::write(
            &archive,
            r#"{"entries":[{"actor":"user","text":"retained short-id answer"}]}"#,
        )
        .unwrap();
        std::fs::set_permissions(&archive, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    let out = publication_command(root.path(), &["session", "reference", &record.id[..8]])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("retained short-id answer"));
    let out = publication_command(root.path(), &["session", "reference", "missing.json"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains(
        "`missing.json` is neither an existing export file nor a session in this workspace"
    ));
}

#[test]
fn publication_user_refusals_are_usage() {
    for args in [
        vec!["session", "show", "abc"],
        vec!["key", "status", "codex"],
        vec!["config", "set", "tui.mouse", "maybe"],
        vec!["state", "prune-legacy"],
    ] {
        let root = tempfile::tempdir().unwrap();
        let out = publication_command(root.path(), &args).output().unwrap();
        assert_eq!(
            out.status.code(),
            Some(2),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

#[test]
fn publication_config_init_pins_nothing_and_preserves_existing_file() {
    let root = tempfile::tempdir().unwrap();
    let out = publication_command(root.path(), &["config", "init"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let path = root.path().join("home/config.toml");
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("# mouse = true"));
    let parsed: toml::Value = toml::from_str(&text).unwrap();
    // Section headers stay live so an uncommented key lands in its section;
    // every section is empty.
    assert!(
        parsed
            .as_table()
            .unwrap()
            .values()
            .all(|section| section.as_table().is_some_and(toml::Table::is_empty)),
        "template pins keys: {text}"
    );
    let out = publication_command(root.path(), &["config", "init"])
        .output()
        .unwrap();
    assert!(out.status.success());
    assert_eq!(std::fs::read_to_string(path).unwrap(), text);
}

#[test]
fn publication_config_edit_accepts_visual_with_arguments() {
    use std::os::unix::fs::PermissionsExt as _;
    let root = tempfile::tempdir().unwrap();
    let editor = root.path().join("editor stub");
    std::fs::write(
        &editor,
        "#!/bin/sh\n[ \"$1\" = --wait ] || exit 9\nprintf '# edited\\n' >> \"$2\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&editor, std::fs::Permissions::from_mode(0o700)).unwrap();
    for variable in ["VISUAL", "EDITOR"] {
        let out = publication_command(root.path(), &["config", "edit"])
            .env(variable, format!("'{}' --wait", editor.display()))
            .env(
                "EDITOR",
                if variable == "VISUAL" {
                    "false".to_owned()
                } else {
                    format!("'{}' --wait", editor.display())
                },
            )
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    assert_eq!(
        std::fs::read_to_string(root.path().join("home/config.toml"))
            .unwrap()
            .matches("# edited")
            .count(),
        2
    );
}

#[test]
fn publication_config_edit_without_editor_or_terminal_is_usage() {
    let root = tempfile::tempdir().unwrap();
    let out = publication_command(root.path(), &["config", "edit"])
        .env("PATH", "")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("set VISUAL or EDITOR"));
    assert!(!root.path().join("home").exists());
}

#[test]
fn publication_single_account_status_names_repair() {
    let root = tempfile::tempdir().unwrap();
    let out = publication_command(root.path(), &["status", "codex"])
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("run plak login codex"));
}

#[test]
fn publication_run_checks_signed_out_before_compatibility() {
    let root = tempfile::tempdir().unwrap();
    let out = publication_command(
        root.path(),
        &["run", "--account", "codex", "--model", "test", "--", "task"],
    )
    .output()
    .unwrap();
    assert_eq!(out.status.code(), Some(3));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("signed out"), "{stderr}");
    assert!(!stderr.contains("compatibility notice"), "{stderr}");
}

#[test]
fn publication_removed_spellings_name_unambiguous_replacements() {
    for verb in ["login", "logout", "refresh", "models", "status"] {
        let root = tempfile::tempdir().unwrap();
        let out = publication_command(root.path(), &[verb, "xai"])
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(2));
        assert!(String::from_utf8_lossy(&out.stderr).contains("`xai` is ambiguous: use `grok`"));
    }
    for (key, dotted) in [
        ("mouse", "tui.mouse"),
        ("theme", "tui.theme"),
        ("compaction_enabled", "compaction.enabled"),
    ] {
        let root = tempfile::tempdir().unwrap();
        let out = publication_command(root.path(), &["config", "set", key, "false"])
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(2));
        assert!(String::from_utf8_lossy(&out.stderr).contains(dotted));
    }
}

fn expected_version_label() -> String {
    use sha2::{Digest as _, Sha256};
    use std::fmt::Write as _;
    let mut digest = String::new();
    for byte in Sha256::digest(std::fs::read(plak_bin()).unwrap()) {
        write!(&mut digest, "{byte:02x}").unwrap();
    }
    let maintainer = if cfg!(feature = "maintainer") {
        "maintainer "
    } else {
        ""
    };
    let profile = if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    };
    let hooks = if cfg!(feature = "test-hooks") {
        "; test-hooks"
    } else {
        ""
    };
    format!(
        "{} ({maintainer}{profile}; binary {}{hooks})",
        env!("CARGO_PKG_VERSION"),
        &digest[..12]
    )
}

fn make_private_home(home: &std::path::Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(home, std::fs::Permissions::from_mode(0o700))
            .expect("make isolated PLAK_HOME private");
    }
}

fn run_plak(home: &std::path::Path, args: &[&str]) -> std::process::Output {
    make_private_home(home);
    Command::new(plak_bin())
        .env("PLAK_HOME", home)
        .env("HOME", home) // some paths fall back to HOME
        .current_dir(home)
        .env_remove("OPENAI_API_KEY")
        .env_remove("XAI_API_KEY")
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("OPENROUTER_API_KEY")
        .args(args)
        .output()
        .expect("spawn plak")
}

fn durable_home_files(home: &std::path::Path) -> Vec<(PathBuf, Vec<u8>)> {
    fn collect(
        root: &std::path::Path,
        current: &std::path::Path,
        out: &mut Vec<(PathBuf, Vec<u8>)>,
    ) {
        if !current.exists() {
            return;
        }
        for entry in std::fs::read_dir(current).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if path.is_dir() {
                collect(root, &path, out);
            } else if !path.to_string_lossy().contains(".sqlite3") {
                out.push((
                    path.strip_prefix(root).unwrap().to_path_buf(),
                    std::fs::read(path).unwrap(),
                ));
            }
        }
    }
    let mut files = Vec::new();
    collect(home, home, &mut files);
    files.sort_by(|a, b| a.0.cmp(&b.0));
    files
}

#[test]
fn session_show_uses_read_only_entry_even_with_diagnostics_enabled() {
    let dir = isolated_home_dir();
    let home = dir.path();
    make_private_home(home);
    let paths = plak::home::paths::PlakPaths::explicit(home.to_path_buf());
    paths.ensure_layout().unwrap();
    let mut store = plak::store::ProductStore::open(&paths).unwrap();
    let mut record = plak::store::sessions::SessionRecord::new(home.to_path_buf());
    store.commit(&mut record).unwrap();
    drop(store);
    let event_path = paths.session_events_file(&record.id);
    std::fs::create_dir_all(event_path.parent().unwrap()).unwrap();
    std::fs::write(&event_path, b"").unwrap();
    let before = durable_home_files(home);
    let before_revision = record.store_revision;
    let before_schema: i64 = rusqlite::Connection::open_with_flags(
        paths.product_database(),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap()
    .query_row("PRAGMA schema_version", [], |row| row.get(0))
    .unwrap();

    let output = Command::new(plak_bin())
        .env("PLAK_HOME", home)
        .env("HOME", home)
        .current_dir(home)
        .env("PLAK_LOG", "info")
        .args(["session", "show", &record.id])
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains(&format!("Session {}", record.id)));
    assert!(text.contains("Events        0"));
    assert!(text.contains("Tree store absent"));
    assert!(!home.join("logs/plak.log").exists());
    assert_eq!(
        durable_home_files(home),
        before,
        "show changed durable home state"
    );
    assert_eq!(
        plak::store::ProductStore::inspect_session_read_only(&paths, &record.id)
            .unwrap()
            .store_revision,
        before_revision
    );
    let after_schema: i64 = rusqlite::Connection::open_with_flags(
        paths.product_database(),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap()
    .query_row("PRAGMA schema_version", [], |row| row.get(0))
    .unwrap();
    assert_eq!(after_schema, before_schema);
}

#[test]
fn session_show_and_log_accept_an_unambiguous_prefix_in_the_workspace() {
    let dir = isolated_home_dir();
    let home = dir.path();
    make_private_home(home);
    let paths = plak::home::paths::PlakPaths::explicit(home.to_path_buf());
    paths.ensure_layout().unwrap();
    let workspace = home.join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let mut store = plak::store::ProductStore::open(&paths).unwrap();
    let mut record = plak::store::sessions::SessionRecord::new(workspace.clone());
    store.commit(&mut record).unwrap();
    drop(store);
    let event_path = paths.session_events_file(&record.id);
    std::fs::create_dir_all(event_path.parent().unwrap()).unwrap();
    std::fs::write(&event_path, b"").unwrap();
    let run = |args: &[&str], cwd: &std::path::Path| {
        Command::new(plak_bin())
            .env("PLAK_HOME", home)
            .env("HOME", home)
            .current_dir(cwd)
            .args(args)
            .output()
            .unwrap()
    };
    let prefix = &record.id[..8];

    let shown = run(&["session", "show", prefix], &workspace);
    assert!(shown.status.success(), "{}", stderr(&shown));
    assert!(stdout(&shown).contains(&format!("Session {}", record.id)));
    let logged = run(&["session", "log", prefix], &workspace);
    assert!(logged.status.success(), "{}", stderr(&logged));
    assert!(stdout(&logged).contains("No observational events recorded"));

    let elsewhere = run(&["session", "show", prefix], home);
    assert!(!elsewhere.status.success());
    assert!(
        stderr(&elsewhere).contains(&format!(
            "no session in this workspace starts with `{prefix}`"
        )),
        "{}",
        stderr(&elsewhere)
    );
    let short = run(&["session", "show", &record.id[..3]], &workspace);
    assert!(!short.status.success());
    assert!(
        stderr(&short).contains("is too short"),
        "{}",
        stderr(&short)
    );
}

#[test]
fn session_show_missing_home_does_not_create_product_state() {
    let dir = isolated_home_dir();
    let missing = dir.path().join("missing");
    let output = Command::new(plak_bin())
        .env("PLAK_HOME", &missing)
        .env("HOME", dir.path())
        .current_dir(dir.path())
        .env("PLAK_LOG", "info")
        .args(["session", "show", "00000000-0000-4000-8000-000000000001"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(!missing.exists(), "show created an absent product home");
}

#[test]
fn session_show_leaves_an_incompatible_runtime_store_untouched() {
    let dir = isolated_home_dir();
    let home = dir.path();
    make_private_home(home);
    let paths = plak::home::paths::PlakPaths::explicit(home.to_path_buf());
    paths.ensure_layout().unwrap();
    let mut store = plak::store::ProductStore::open(&paths).unwrap();
    let mut record = plak::store::sessions::SessionRecord::new(home.to_path_buf());
    store.commit(&mut record).unwrap();
    drop(store);
    install_unsupported_runtime_schema_13(&paths.runtime_database());
    let before = durable_home_files(home);

    let output = Command::new(plak_bin())
        .env("PLAK_HOME", home)
        .env("HOME", home)
        .current_dir(home)
        .env("PLAK_LOG", "info")
        .args(["session", "show", &record.id])
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(stdout(&output).contains("Recorded head  unavailable"));
    assert!(paths.runtime_database().exists());
    assert_eq!(durable_home_files(home), before);
    let version: i64 = rusqlite::Connection::open_with_flags(
        paths.runtime_database(),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap()
    .query_row("PRAGMA user_version", [], |row| row.get(0))
    .unwrap();
    assert_eq!(version, 13);
}

fn install_unsupported_runtime_schema_13(path: &std::path::Path) {
    std::fs::create_dir_all(path.parent().unwrap()).expect("predecessor fixture parent");
    let connection = rusqlite::Connection::open(path).expect("predecessor fixture");
    connection
        .pragma_update(None, "application_id", 0x504c_4b35_i64)
        .expect("predecessor application identity");
    connection
        .pragma_update(None, "user_version", 13_i64)
        .expect("predecessor schema identity");
    connection
        .execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;")
        .expect("predecessor durability pragmas");
    drop(connection);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .expect("private predecessor fixture");
    }
}

struct ProductPty {
    writer: Box<dyn std::io::Write + Send>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    terminal: Arc<Mutex<TerminalCapture>>,
    reader: Option<std::thread::JoinHandle<()>>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct FrameSnapshot {
    contents: String,
    cursor: (u16, u16),
    cursor_hidden: bool,
    raw_len: usize,
}

struct TerminalCapture {
    parser: vt100::Parser,
    raw: Vec<u8>,
}

impl TerminalCapture {
    fn new(rows: u16, cols: u16) -> Self {
        Self {
            parser: vt100::Parser::new(rows, cols, 0),
            raw: Vec::new(),
        }
    }

    fn process(&mut self, bytes: &[u8]) {
        self.parser.process(bytes);
        self.raw.extend_from_slice(bytes);
    }

    fn snapshot(&self) -> FrameSnapshot {
        FrameSnapshot {
            contents: self.parser.screen().contents(),
            cursor: self.parser.screen().cursor_position(),
            cursor_hidden: self.parser.screen().hide_cursor(),
            raw_len: self.raw.len(),
        }
    }
}

impl ProductPty {
    fn spawn(home: &std::path::Path) -> Self {
        Self::spawn_with_args(home, &[])
    }

    fn spawn_with_args(home: &std::path::Path, args: &[&str]) -> Self {
        make_private_home(home);
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: 30,
                cols: 100,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("open PTY");
        let mut command = CommandBuilder::new(plak_bin());
        command.args(args);
        command.env_clear();
        command.env("HOME", home);
        command.env("PLAK_HOME", home);
        command.env("XDG_CONFIG_HOME", home.join("xdg-config"));
        command.env("XDG_DATA_HOME", home.join("xdg-data"));
        command.env("XDG_STATE_HOME", home.join("xdg-state"));
        command.env("TERM", "xterm-256color");
        command.env("LANG", "C.UTF-8");
        command.cwd(home);
        let mut source = pair.master.try_clone_reader().expect("clone PTY reader");
        let writer = pair.master.take_writer().expect("take PTY writer");
        let child = pair.slave.spawn_command(command).expect("spawn Plak PTY");
        drop(pair.slave);
        let terminal = Arc::new(Mutex::new(TerminalCapture::new(30, 100)));
        let sink = Arc::clone(&terminal);
        let reader = std::thread::spawn(move || {
            let mut bytes = [0_u8; 8192];
            while let Ok(count) = source.read(&mut bytes) {
                if count == 0 {
                    break;
                }
                sink.lock()
                    .expect("PTY terminal lock")
                    .process(&bytes[..count]);
            }
        });
        Self {
            writer,
            child,
            terminal,
            reader: Some(reader),
        }
    }

    fn frame(&self) -> FrameSnapshot {
        self.terminal.lock().expect("PTY terminal lock").snapshot()
    }

    fn send(&mut self, bytes: &[u8]) {
        self.writer.write_all(bytes).expect("write PTY");
        self.writer.flush().expect("flush PTY");
    }

    fn wait_for(&mut self, needle: &str) -> String {
        self.wait_for_timeout(needle, Duration::from_secs(15))
    }

    fn wait_gone(&mut self, needle: &str) {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let first = self.frame();
            if !first.contents.contains(needle) {
                std::thread::sleep(Duration::from_millis(30));
                let second = self.frame();
                if first == second && !second.contents.contains(needle) {
                    return;
                }
            }
            if let Some(status) = self.child.try_wait().expect("poll Plak") {
                panic!(
                    "Plak exited before {needle:?} left the screen: {status:?}\n{}",
                    first.contents
                );
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {needle:?} to leave\n{}",
                first.contents
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn wait_for_timeout(&mut self, needle: &str, timeout: Duration) -> String {
        let deadline = Instant::now() + timeout;
        loop {
            let first = self.frame();
            if first.contents.contains(needle) {
                std::thread::sleep(Duration::from_millis(30));
                let second = self.frame();
                if first == second {
                    return second.contents;
                }
            }
            if let Some(status) = self.child.try_wait().expect("poll Plak") {
                panic!(
                    "Plak exited before stable {needle:?}: {status:?}\n{}",
                    first.contents
                );
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for stable {needle:?}\n{}",
                first.contents
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn finish(mut self) -> Vec<u8> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(status) = self.child.try_wait().expect("poll Plak exit") {
                assert!(status.success(), "Plak PTY exit failed: {status:?}");
                break;
            }
            assert!(Instant::now() < deadline, "Plak did not exit");
            std::thread::sleep(Duration::from_millis(20));
        }
        drop(self.writer);
        if let Some(reader) = self.reader.take() {
            reader.join().expect("join PTY reader");
        }
        self.terminal.lock().expect("PTY terminal lock").raw.clone()
    }

    /// Wait until `needle` is on a frame that stayed still, with no wall-clock
    /// deadline. A loaded lane can leave the debug binary unscheduled for longer
    /// than the 15s liveness bound [`Self::wait_for`] uses. A stdio-lock hang
    /// still fails the test: nextest's slow-timeout is the suite bound.
    fn wait_until_stable(&mut self, needle: &str) -> String {
        loop {
            let first = self.frame();
            if first.contents.contains(needle) {
                std::thread::sleep(Duration::from_millis(30));
                let second = self.frame();
                if first == second {
                    return second.contents;
                }
            }
            if let Some(status) = self.child.try_wait().expect("poll Plak") {
                panic!(
                    "Plak exited before stable {needle:?}: {status:?}\n{}",
                    first.contents
                );
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Block until the child exits. Same hang bound as [`Self::wait_until_stable`].
    fn finish_when_exited(mut self) -> Vec<u8> {
        let status = self.child.wait().expect("wait Plak exit");
        assert!(status.success(), "Plak PTY exit failed: {status:?}");
        drop(self.writer);
        if let Some(reader) = self.reader.take() {
            reader.join().expect("join PTY reader");
        }
        self.terminal.lock().expect("PTY terminal lock").raw.clone()
    }
}

fn stdout(out: &std::process::Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &std::process::Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn isolated_home_dir() -> tempfile::TempDir {
    let base = std::env::current_dir()
        .expect("cwd")
        .join("target/plak-cli-test-homes");
    std::fs::create_dir_all(&base).expect("cli test home base");
    tempfile::tempdir_in(&base).expect("isolated cli home")
}

#[test]
fn version_prints_the_build_label_and_update_is_not_a_verb() {
    let dir = isolated_home_dir();
    let home = dir.path();

    let out = run_plak(home, &["version"]);
    assert!(out.status.success(), "stderr={}", stderr(&out));
    assert_eq!(stdout(&out), format!("plak {}\n", expected_version_label()));

    let out = run_plak(home, &["update"]);
    assert_eq!(out.status.code(), Some(2), "stderr={}", stderr(&out));
}

/// Spec 02 §3.2: the help pages are static files, and what the binary prints
/// is those files byte for byte — the global page with its version line filled
/// in and its maintainer block gated by the feature, every command page
/// verbatim. This is the oracle that lets the pages move between renderers.
#[test]
fn help_output_is_the_shipped_page_files_byte_for_byte() {
    let dir = isolated_home_dir();
    let home = dir.path();

    let mut expected_global =
        include_str!("../../help/global.txt").replace("{version}", &expected_version_label());
    if cfg!(feature = "maintainer") {
        expected_global.push_str(include_str!("../../help/global-maintainer.txt"));
    }
    expected_global.push_str(include_str!("../../help/global-tail.txt"));
    assert_eq!(
        stdout(&run_plak(home, &["help"])),
        expected_global,
        "plak help is not bin/plak/help/global.txt"
    );

    let mut pages: Vec<(&str, &str)> = vec![
        ("login", include_str!("../../help/login.txt")),
        ("logout", include_str!("../../help/logout.txt")),
        ("status", include_str!("../../help/status.txt")),
        ("refresh", include_str!("../../help/refresh.txt")),
        ("key", include_str!("../../help/key.txt")),
        ("run", include_str!("../../help/run.txt")),
        ("agent", include_str!("../../help/agent.txt")),
        ("resume", include_str!("../../help/resume.txt")),
        ("config", include_str!("../../help/config.txt")),
        ("doctor", include_str!("../../help/doctor.txt")),
        ("usage", include_str!("../../help/usage.txt")),
        ("session", include_str!("../../help/session.txt")),
        ("state", include_str!("../../help/state.txt")),
        ("telemetry", include_str!("../../help/telemetry.txt")),
    ];
    if cfg!(feature = "maintainer") {
        pages.extend([
            ("fork", include_str!("../../help/fork.txt")),
            ("compact", include_str!("../../help/compact.txt")),
            ("models", include_str!("../../help/models.txt")),
            (
                "compatibility",
                include_str!("../../help/compatibility.txt"),
            ),
        ]);
    }
    for (command, page) in pages {
        assert_eq!(
            stdout(&run_plak(home, &[command, "--help"])),
            page,
            "plak {command} --help is not bin/plak/help/{command}.txt"
        );
    }
}

/// A bare product verb prints its own help on stdout and exits 0; a bare leaf
/// prints the help of the group that owns it. Nothing here is a clap error.
#[test]
fn bare_product_commands_print_their_help_and_exit_zero() {
    let dir = isolated_home_dir();
    let home = dir.path();
    let mut cases: Vec<(&[&str], &str)> = vec![
        (&["logout"], "logout"),
        (&["refresh"], "refresh"),
        (&["run"], "run"),
        (&["login"], "login"),
        (&["key"], "key"),
        (&["key", "set"], "key"),
        (&["key", "clear"], "key"),
        (&["config"], "config"),
        (&["config", "set"], "config"),
        (&["session"], "session"),
        (&["state"], "state"),
        (&["session", "show"], "session"),
        (&["session", "log"], "session"),
        (&["telemetry"], "telemetry"),
        (&["telemetry", "sql"], "telemetry"),
        (&["models"], "models"),
    ];
    if cfg!(feature = "maintainer") {
        cases.extend::<Vec<(&[&str], &str)>>(vec![
            (&["fork"], "fork"),
            (&["compact"], "compact"),
            (&["compatibility"], "compatibility"),
        ]);
    }
    for (argv, group) in cases {
        let bare = run_plak(home, argv);
        assert!(
            bare.status.success(),
            "bare `plak {}` must exit 0: {}",
            argv.join(" "),
            stderr(&bare)
        );
        let help = run_plak(home, &[group, "--help"]);
        assert!(help.status.success(), "{}", stderr(&help));
        assert_eq!(
            stdout(&bare),
            stdout(&help),
            "bare `plak {}` must print `plak {group} --help`",
            argv.join(" ")
        );
        assert!(
            !stdout(&bare).contains("No detailed help"),
            "`plak {}` hit the no-help arm",
            argv.join(" ")
        );
    }
}

/// `--effort none` on a route that publishes it is reconstructed. With no key
/// the run stops at authentication (exit 3) after that, not at the old
/// unknown-effort refusal (exit 5).
#[test]
fn api_effort_none_is_reconstructed_before_the_missing_key_refusal() {
    let dir = isolated_home_dir();
    let home = dir.path();
    for (account, model, key_line) in [
        ("openai", "gpt-6-luna", "OpenAI API key is not set"),
        ("xai-api", "grok-4.3", "xAI API key is not set"),
    ] {
        let out = run_plak_without_provider(
            home,
            &[
                "run",
                "--account",
                account,
                "--model",
                model,
                "--effort",
                "none",
                "--print-session-id",
                "--",
                "Reply OK",
            ],
        );
        let err = stderr(&out);
        assert_eq!(out.status.code(), Some(3), "{account}: {err}");
        assert!(err.contains(key_line), "{account}: {err}");
        assert!(
            !err.to_ascii_lowercase().contains("effort is unknown"),
            "{account}: {err}"
        );
        assert!(
            stdout(&out).contains("session_id="),
            "{account}: {}",
            stdout(&out)
        );
    }
}

/// A model whose ladder omits `none` refuses it at admission, before a session
/// or any provider call.
#[test]
fn anthropic_effort_none_is_usage_before_inference() {
    let dir = isolated_home_dir();
    let home = dir.path();
    let out = run_plak_without_provider(
        home,
        &[
            "run",
            "--account",
            "anthropic",
            "--model",
            "claude-haiku-5-5",
            "--effort",
            "none",
            "--",
            "Reply OK",
        ],
    );
    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(2), "{err}");
    assert!(
        err.contains(
            "anthropic model `claude-haiku-5-5` requires --effort low|medium|high|xhigh|max"
        ),
        "{err}"
    );
    assert!(!stdout(&out).contains("session_id="), "{}", stdout(&out));
}

fn run_plak_without_provider(home: &std::path::Path, args: &[&str]) -> std::process::Output {
    make_private_home(home);
    Command::new(plak_bin())
        .env("PLAK_HOME", home)
        .env("HOME", home)
        .current_dir(home)
        .env_remove("OPENAI_API_KEY")
        .env_remove("XAI_API_KEY")
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("OPENROUTER_API_KEY")
        .env_remove("PLAK_TEST_MODEL")
        .env_remove("PLAK_TEST_SCRIPT")
        .env_remove("PLAK_TEST_FAULTS")
        .args(args)
        .output()
        .expect("spawn plak")
}

/// `telemetry prune` mutates the store: a bare run refuses (exit 2, Usage).
#[test]
fn bare_telemetry_prune_refuses_without_a_bound_or_yes() {
    let dir = isolated_home_dir();
    let home = dir.path();
    let out = run_plak(home, &["telemetry", "prune"]);
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    assert!(stderr(&out).contains("--yes"), "{}", stderr(&out));
}

/// The legacy `plak auth` tree and `plak config path` are retired.
#[test]
fn retired_verbs_are_absent() {
    let dir = isolated_home_dir();
    let home = dir.path();
    for argv in [
        &["auth"][..],
        &["auth", "status", "codex"][..],
        &["auth", "login", "codex"][..],
        &["auth", "logout-local", "codex"][..],
        &["config", "path"][..],
        &["telemetry", "views"][..],
    ] {
        let out = run_plak(home, argv);
        assert!(
            !out.status.success(),
            "retired `plak {}` still runs",
            argv.join(" ")
        );
    }
}

#[test]
fn config_init_show_set_path_round_trip() {
    let dir = isolated_home_dir();
    let home = dir.path();

    let out = run_plak(home, &["config", "init"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stdout(&out).contains("Created") || stdout(&out).contains("Already exists:"));

    // `config show` prints `config_path=<file>` on its first line (`config path` is retired).
    let out = run_plak(home, &["config", "show"]);
    assert!(out.status.success());
    let path = stdout(&out)
        .lines()
        .next()
        .and_then(|line| line.strip_prefix("config_path="))
        .unwrap_or_default()
        .trim()
        .to_owned();
    assert!(
        path.contains("config.toml"),
        "unexpected config path: {path}"
    );
    assert!(
        path.starts_with(home.to_string_lossy().as_ref()) || std::path::Path::new(&path).exists(),
        "config path should live under PLAK_HOME: {path}"
    );

    // the closed key list. Bare tuple keys are gone with `[default]`.
    for (key, value) in [
        ("tui.theme", "catppuccin-mocha"),
        ("tui.scroll_lines", "4"),
        ("orchestration.max_children", "4"),
        ("roster.working_set", ""),
        ("compaction.enabled", "false"),
    ] {
        let out = run_plak(home, &["config", "set", key, value]);
        assert!(out.status.success(), "{key}: {}", stderr(&out));
    }
    for key in [
        "account",
        "model",
        "default.account",
        "roster.review",
        "orchestration.child_may_spawn",
    ] {
        let out = run_plak(home, &["config", "set", key, "grok"]);
        assert!(!out.status.success(), "{key} must not be settable");
        assert!(
            stderr(&out).contains("unknown config key"),
            "{key}: {}",
            stderr(&out)
        );
    }
    // templates are never authored through `plak config set`.
    let out = run_plak(home, &["config", "set", "agent.review.account", "grok"]);
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("templates are file-authored"),
        "{}",
        stderr(&out)
    );
    // The working set names template files; an absent file is a load error.
    let out = run_plak(home, &["config", "set", "roster.working_set", "absent"]);
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("names no template file absent.toml"),
        "{}",
        stderr(&out)
    );

    let out = run_plak(home, &["config", "show"]);
    assert!(out.status.success());
    let text = stdout(&out);
    assert!(text.contains("[orchestration]"), "{text}");
    assert!(text.contains("max_children = 4"), "{text}");
    assert!(text.contains("scroll_lines = 4"), "{text}");
    assert!(text.contains("theme = \"catppuccin-mocha\""), "{text}");
    // Secrets must never appear in config show.
    assert!(!text.to_ascii_lowercase().contains("sk-"));
}

#[test]
fn doctor_and_session_list_and_key_status() {
    let dir = isolated_home_dir();
    let home = dir.path();
    let _ = run_plak(home, &["config", "init"]);

    let out = run_plak(home, &["doctor"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(
        text.contains("Plak doctor") || text.contains("Version"),
        "{text}"
    );
    assert!(
        text.contains("Sessions dir") || text.contains("Home"),
        "{text}"
    );

    let out = run_plak(home, &["session", "list"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(stdout(&out).trim(), "No sessions in this workspace.");

    let out = run_plak(home, &["key", "status"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(
        text.contains("openai") || text.contains("missing"),
        "{text}"
    );
}

#[test]
fn key_clear_fails_closed_when_process_env_still_wins() {
    let dir = isolated_home_dir();
    let home = dir.path();
    make_private_home(home);
    let out = Command::new(plak_bin())
        .env("PLAK_HOME", home)
        .env("HOME", home)
        .current_dir(home)
        .env(
            "OPENROUTER_API_KEY",
            concat!("sk-or-v1-", "env-shadow-test-key"),
        )
        .env_remove("OPENAI_API_KEY")
        .env_remove("XAI_API_KEY")
        .env_remove("ANTHROPIC_API_KEY")
        .args(["key", "status", "openrouter"])
        .output()
        .expect("spawn plak");
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("openrouter\tenv\t"), "{text}");
    assert!(text.contains("OPENROUTER_API_KEY"), "{text}");
    assert!(text.contains("does not load .env"), "{text}");

    let clear = Command::new(plak_bin())
        .env("PLAK_HOME", home)
        .env("HOME", home)
        .current_dir(home)
        .env(
            "OPENROUTER_API_KEY",
            concat!("sk-or-v1-", "env-shadow-test-key"),
        )
        .args(["key", "clear", "openrouter"])
        .output()
        .expect("spawn plak");
    assert!(
        !clear.status.success(),
        "env-shadowed clear must fail closed"
    );
    let err = stderr(&clear);
    assert!(err.contains("OPENROUTER_API_KEY"), "{err}");
    assert!(err.contains("unset"), "{err}");
    assert!(err.contains("never loads .env"), "{err}");
}

#[test]
fn session_mutations_require_confirmation_and_context_never_guesses() {
    let dir = isolated_home_dir();
    let home = dir.path();

    let fork = run_plak(home, &["fork", "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee"]);
    assert!(!fork.status.success());
    let fork_error = stderr(&fork);
    if cfg!(feature = "maintainer") {
        assert_eq!(fork.status.code(), Some(2), "{fork_error}");
        assert!(fork_error.contains("confirmation"), "{fork_error}");
        assert!(fork_error.contains("--yes"), "{fork_error}");
    } else {
        assert_eq!(fork.status.code(), Some(2), "{fork_error}");
        assert!(fork_error.contains("unknown command"), "{fork_error}");
    }

    let compact = run_plak(home, &["compact", "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee"]);
    assert!(!compact.status.success());
    if cfg!(feature = "maintainer") {
        assert_eq!(compact.status.code(), Some(2));
        assert!(stderr(&compact).contains("--yes"));
    } else {
        assert_eq!(compact.status.code(), Some(2), "{}", stderr(&compact));
        assert!(
            stderr(&compact).contains("unknown command"),
            "{}",
            stderr(&compact)
        );
    }

    let usage = run_plak(home, &["usage"]);
    assert!(usage.status.success(), "{}", stderr(&usage));
    let usage_text = stdout(&usage);
    assert!(usage_text.contains("Limits"), "{usage_text}");
    // All four verticals are always listed, with no data by reason.
    for vertical in ["Codex", "SuperGrok", "Claude", "OpenRouter"] {
        assert!(usage_text.contains(vertical), "{vertical}: {usage_text}");
    }
    assert!(usage_text.contains("not connected"), "{usage_text}");
    // An unknown cost is `not available`, never `$0`.
    assert!(usage_text.contains("Cost:"), "{usage_text}");
    assert!(usage_text.contains("not available"), "{usage_text}");
    assert!(!usage_text.contains("$0"), "{usage_text}");
    assert!(
        usage_text.contains("Turn tokens and context capacity"),
        "{usage_text}"
    );
    assert!(
        !usage_text.contains("local context estimate"),
        "{usage_text}"
    );
}

#[test]
fn bare_plak_non_tty_fails_with_run_hint() {
    let dir = isolated_home_dir();
    let home = dir.path();
    // No stdin TTY in CI; bare plak should not hang.
    let out = Command::new(plak_bin())
        .env("PLAK_HOME", home)
        .env("HOME", home)
        .current_dir(home)
        .env_remove("OPENAI_API_KEY")
        .env_remove("XAI_API_KEY")
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("OPENROUTER_API_KEY")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .expect("spawn");
    assert!(!out.status.success(), "bare plak on non-TTY should fail");
    let err = stderr(&out);
    assert!(
        err.contains("TTY") || err.contains("plak run") || err.contains("interactive"),
        "unexpected stderr: {err}"
    );
}

#[test]
fn fresh_real_binary_shell_keeps_controls_and_draft_usable() {
    let dir = isolated_home_dir();
    let mut plak = ProductPty::spawn(dir.path());
    let first = plak.wait_for("/ commands");
    assert!(
        !first.contains("plak · ready"),
        "static transcript header leaked:\n{first}"
    );

    plak.send(b"/settings\r");
    let settings = plak.wait_for("Scroll lines");
    assert!(!settings.contains("Loading settings"));
    assert!(
        !settings.contains("Open Models tab") && !settings.contains("Open Keybinds tab"),
        "spec 8.1 deletes the navigation-only rows:\n{settings}"
    );
    plak.send(b"\x1b");
    plak.wait_gone("Menu");

    // Spec 02 §5.5: Usage, Context and Session are peer tabs, so
    // `/usage`, `/context` and `/status` each land on their own tab and the
    // section-jump hint is gone with the page it navigated.
    plak.send(b"/usage\r");
    let usage = plak.wait_for("Limits");
    assert!(
        usage.contains("Settings") && usage.contains("Context") && usage.contains("Session"),
        "the seven-tab strip is painted:\n{usage}"
    );
    assert!(
        !usage.contains("jump section") && !usage.contains("switch sub-tab"),
        "the Usage page's section navigation is gone:\n{usage}"
    );
    assert!(
        usage.contains("Agents"),
        "the Agents tab is in the strip:\n{usage}"
    );
    assert!(
        !usage.contains("Per attempt"),
        "usage does not list child attempts:\n{usage}"
    );
    assert!(
        usage.contains("Providers"),
        "the read-only Providers tab is in the strip:\n{usage}"
    );
    plak.send(b"\x1b");
    plak.wait_gone("Menu");

    // Spec 8.2a: `/providers` is a read-only status view — account, status and
    // the CLI command per vertical, and no action anywhere.
    plak.send(b"/providers\r");
    let providers = plak.wait_for("plak login codex");
    assert!(
        providers.contains("account:") && providers.contains("status:"),
        "each vertical states its account and status:\n{providers}"
    );
    assert!(
        !providers.contains("Log in") && !providers.contains("Paste key"),
        "the tab offers no action:\n{providers}"
    );
    plak.send(b"\x1b");
    plak.wait_gone("Menu");

    plak.send(b"/context\r");
    let context = plak.wait_for("Context usage");
    assert!(context.contains("Context"), "{context}");
    plak.send(b"\x1b");
    plak.wait_gone("Menu");

    plak.send(b"/status\r");
    let session = plak.wait_for("Session ID");
    assert!(session.contains("Working directory"), "{session}");
    plak.send(b"\x1b");
    plak.wait_gone("Menu");

    plak.send(b"draft survives failed attach\r");
    let refused = plak.wait_for("draft survives failed attach");
    assert!(
        refused.contains("│❯ draft survives failed attach"),
        "the refused draft left the composer:\n{refused}"
    );
    plak.send(b"\x03");
    plak.send(b"/exit\r");
    let raw = plak.finish();
    assert!(raw.windows(7).any(|window| window == b"[?1000h"));
    assert!(raw.windows(7).any(|window| window == b"[?1000l"));
    assert!(raw.windows(7).any(|window| window == b"[?1049l"));
}

#[test]
fn repeated_fresh_tui_open_and_quit_creates_zero_resumable_sessions() {
    let dir = isolated_home_dir();
    for _ in 0..3 {
        let mut plak = ProductPty::spawn(dir.path());
        plak.wait_for("/ commands");
        plak.send(b"/exit\r");
        let _ = plak.finish();
    }

    let connection = rusqlite::Connection::open(
        plak::home::paths::PlakPaths::explicit(dir.path().to_path_buf()).product_database(),
    )
    .unwrap();
    assert_eq!(
        connection
            .query_row("SELECT COUNT(*) FROM sessions", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        0,
        "presentation-only launches must not become resume candidates"
    );
    let sessions_dir =
        plak::home::paths::PlakPaths::explicit(dir.path().to_path_buf()).sessions_dir();
    let event_logs = std::fs::read_dir(&sessions_dir)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .ends_with(".events.jsonl")
        })
        .count();
    assert_eq!(
        event_logs, 0,
        "fresh shells must not create orphan event logs"
    );
}

/// `plak resume` (and `plak --resume`) must open the TUI directly on the
/// cwd-scoped session picker. The CLI used to send the `/resume` request to
/// the host without marking it outstanding in the TUI, so the reply was
/// dropped and the operator landed on an empty "not ready · `resume_picker`"
/// shell.
#[test]
fn plak_resume_opens_the_session_picker_on_first_frame() {
    for args in [&["resume"][..], &["--resume"][..]] {
        let dir = isolated_home_dir();
        let mut plak = ProductPty::spawn_with_args(dir.path(), args);
        // First paint is a liveness bound (process start plus the picker
        // frame), not a product deadline. The offline TUI first-frame wait
        // is the same 45s; 15s has elapsed on a loaded lane before the
        // debug binary was scheduled.
        let frame = plak.wait_for_timeout("Resume session", Duration::from_secs(45));
        assert!(
            frame.contains("No sessions in this workspace yet."),
            "{args:?} must open the cwd-scoped picker over an empty home\n{frame}"
        );
        assert!(
            !frame.contains("resume_picker"),
            "{args:?} must not leak the internal shell reason\n{frame}"
        );
        // Declining must not leave the product parked on
        // `not ready · pick a session to resume` with `/attach` as the only
        // way out. This home has no configured model, so the workspace has no
        // attachable default and the launcher quits cleanly and says why.
        plak.send(b"\x1b");
        // Declining quits the process; under load the exit can land before a
        // frame without the picker is captured, so wait on exit, not on paint.
        let raw = String::from_utf8_lossy(&plak.finish()).into_owned();
        assert!(
            raw.contains("no session resumed, and this workspace has no attachable default"),
            "{args:?} must state why it stopped instead of stranding the shell\n{raw}"
        );
        // The stream keeps every frame ever painted, so the check is ordering:
        // the quit line is the last word, after the picker's own shell reason.
        assert!(
            raw.rfind("no session resumed, and this workspace has no attachable default")
                > raw.rfind("pick a session to resume"),
            "{args:?} must end on the quit line, not on the picker shell\n{raw}"
        );
    }
}

#[test]
fn real_binary_tui_reaches_frame_and_accepts_input_without_stdio_lock_hang() {
    let dir = isolated_home_dir();
    let mut plak = ProductPty::spawn(dir.path());
    plak.wait_until_stable("/ commands");
    plak.send(b"x");
    plak.wait_until_stable("│❯ x");
    plak.send(b"\x03");
    plak.send(b"/exit\r");
    let _ = plak.finish_when_exited();
}

#[test]
fn real_binary_refuses_unsupported_runtime_without_moving_state() {
    let dir = isolated_home_dir();
    let runtime =
        plak::home::paths::PlakPaths::explicit(dir.path().to_path_buf()).runtime_database();
    install_unsupported_runtime_schema_13(&runtime);
    let before = std::fs::read(&runtime).unwrap();
    let output = run_plak(dir.path(), &["session", "list"]);
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("was created by an incompatible plak build"),
        "{}",
        stderr(&output)
    );
    assert!(
        !plak::home::paths::PlakPaths::explicit(dir.path().to_path_buf())
            .product_database()
            .exists()
    );
    assert_eq!(std::fs::read(&runtime).unwrap(), before);
}

#[test]
fn acceptance_fixture_creates_exact_empty_current_runtime() {
    let Some(home) = std::env::var_os("PLAK_ACCEPTANCE_SEED_RUNTIME_HOME") else {
        return;
    };
    let home = PathBuf::from(home);
    std::fs::create_dir_all(&home).unwrap();
    make_private_home(&home);
    drop(
        plakous::sessions::SessionBackend::sqlite(
            plak::home::paths::PlakPaths::explicit(home.clone()).runtime_database(),
        )
        .expect("create exact current runtime fixture"),
    );
}

#[test]
fn concurrent_real_binaries_refuse_an_incompatible_runtime_without_moving_it() {
    let dir = isolated_home_dir();
    let runtime =
        plak::home::paths::PlakPaths::explicit(dir.path().to_path_buf()).runtime_database();
    install_unsupported_runtime_schema_13(&runtime);
    let before = std::fs::read(&runtime).unwrap();
    let barrier = Arc::new(Barrier::new(3));
    let home = dir.path();

    let outputs = std::thread::scope(|scope| {
        let mut workers = Vec::new();
        for _ in 0..2 {
            let barrier = Arc::clone(&barrier);
            workers.push(scope.spawn(move || {
                barrier.wait();
                run_plak(home, &["session", "list"])
            }));
        }
        barrier.wait();
        workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect::<Vec<_>>()
    });

    for output in &outputs {
        assert!(!output.status.success());
        assert!(
            stderr(output).contains("was created by an incompatible plak build"),
            "{}",
            stderr(output)
        );
    }
    assert!(
        !plak::home::paths::PlakPaths::explicit(dir.path().to_path_buf())
            .product_database()
            .exists()
    );
    assert_eq!(std::fs::read(&runtime).unwrap(), before);
}

#[test]
fn another_builds_state_is_named_once_left_untouched_and_its_sessions_refused() {
    let dir = isolated_home_dir();
    let home = dir.path();
    let paths = plak::home::paths::PlakPaths::explicit(home.to_path_buf());
    // A directory another build keyed, and a pre-key flat state/ member.
    let other = paths.state_root().join("r1.e1.p1.t1");
    let old_session = "0badc0de-0000-4000-8000-000000000001";
    std::fs::create_dir_all(other.join("sessions")).unwrap();
    std::fs::write(
        other.join(format!("sessions/{old_session}.events.jsonl")),
        b"{}\n",
    )
    .unwrap();
    std::fs::write(other.join("product.sqlite3"), b"another build's bytes").unwrap();
    std::fs::write(paths.state_root().join("runtime.sqlite3"), b"flat layout").unwrap();
    let kept = [
        other.join(format!("sessions/{old_session}.events.jsonl")),
        other.join("product.sqlite3"),
        paths.state_root().join("runtime.sqlite3"),
    ];
    let before = kept
        .iter()
        .map(|path| std::fs::read(path).unwrap())
        .collect::<Vec<_>>();

    let listed = run_plak(home, &["session", "list"]);
    assert!(listed.status.success(), "{}", stderr(&listed));
    let notice = stderr(&listed);
    assert_eq!(
        notice.matches("legacy state entries").count(),
        1,
        "{notice}"
    );
    assert!(notice.contains("2 legacy state entries"), "{notice}");
    // The real binary sizes legacy entries within a wall-clock budget; the
    // byte total and its exhausted form are pinned by the unit tests.
    assert!(
        notice.contains("2 legacy state entries (") && notice.contains(" bytes) remain")
            || notice.contains("2 legacy state entries (size unknown) remain"),
        "{notice}"
    );
    assert!(notice.contains("plak doctor"), "{notice}");
    assert!(paths.product_database().exists());
    let after = kept
        .iter()
        .map(|path| std::fs::read(path).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(after, before);

    let again = run_plak(home, &["session", "list"]);
    assert!(again.status.success(), "{}", stderr(&again));

    let resumed = run_plak(home, &["agent", "--session", old_session]);
    assert!(!resumed.status.success());
    let refusal = format!("{}{}", stdout(&resumed), stderr(&resumed));
    assert!(
        refusal.contains(&format!(
            "session {old_session} was created by an incompatible plak build (state r1.e1.p1.t1); \
             carry it forward with that build's `plak session export {old_session}` and then \
             `plak session import <file>`, or start a new session"
        )),
        "{refusal}"
    );
}

#[test]
fn run_without_login_has_exact_auth_exit() {
    let dir = isolated_home_dir();
    let home = dir.path();
    let out = run_plak(
        home,
        &[
            "run",
            "--account",
            "grok",
            "--model",
            "grok-4.5",
            "--",
            "hello",
        ],
    );
    assert_eq!(out.status.code(), Some(3), "stderr={}", stderr(&out));
    let err = stderr(&out);
    assert!(
        !err.is_empty() || !stdout(&out).is_empty(),
        "run failure should explain itself"
    );
}

#[test]
fn agent_without_login_has_exact_auth_exit() {
    let dir = isolated_home_dir();
    let home = dir.path();
    let out = run_plak(
        home,
        &[
            "agent",
            "--account",
            "grok",
            "--model",
            "grok-4.5",
            "--effort",
            "low",
        ],
    );
    assert_eq!(out.status.code(), Some(3), "stderr={}", stderr(&out));
    let err = stderr(&out);
    assert!(
        !err.is_empty() || !stdout(&out).is_empty(),
        "agent failure should explain itself"
    );
}

/// An API account with no key fails as authentication (exit 3) on both
/// faces, before any model is built, and names the command that fixes it.
///
/// One test used to launch all four accounts. Eight process starts crossed
/// the lane's 120s liveness kill under load; the assertions are unchanged.
fn api_account_without_a_key_fails_as_auth(
    account: &str,
    model: &str,
    key_account: &str,
    env: &str,
) {
    let dir = isolated_home_dir();
    let effort: &[&str] = if account == "openrouter" {
        &[]
    } else {
        &["--effort", "low"]
    };
    let fix = format!("run `plak key set {key_account}` or set {env}");
    let mut run_args = vec!["run", "--account", account, "--model", model];
    run_args.extend_from_slice(effort);
    run_args.extend_from_slice(&["--", "hello"]);
    let out = run_plak(dir.path(), &run_args);
    assert_eq!(
        out.status.code(),
        Some(3),
        "run {account}: {}",
        stderr(&out)
    );
    let err = stderr(&out);
    assert!(
        err.contains("API key is not set") && err.contains(&fix),
        "run {account}: {err}"
    );
    assert!(!err.contains("[redacted]"), "run {account}: {err}");

    let mut agent_args = vec!["agent", "--account", account, "--model", model];
    agent_args.extend_from_slice(effort);
    let out = run_plak(dir.path(), &agent_args);
    assert_eq!(
        out.status.code(),
        Some(3),
        "agent {account}: {}",
        stderr(&out)
    );
    let lines = stdout(&out);
    assert!(lines.contains(&fix), "agent {account}: {lines}");
    assert!(
        lines.contains(r#""kind":"bye""#),
        "agent {account}: {lines}"
    );
}

#[test]
fn openai_api_account_without_a_key_fails_as_auth_and_names_the_fix() {
    api_account_without_a_key_fails_as_auth("openai", "gpt-6-luna", "openai", "OPENAI_API_KEY");
}

#[test]
fn anthropic_api_account_without_a_key_fails_as_auth_and_names_the_fix() {
    api_account_without_a_key_fails_as_auth(
        "anthropic",
        "claude-sonnet-5",
        "anthropic",
        "ANTHROPIC_API_KEY",
    );
}

#[test]
fn xai_api_account_without_a_key_fails_as_auth_and_names_the_fix() {
    api_account_without_a_key_fails_as_auth("xai-api", "grok-4.6", "xai-api", "XAI_API_KEY");
}

#[test]
fn openrouter_api_account_without_a_key_fails_as_auth_and_names_the_fix() {
    api_account_without_a_key_fails_as_auth(
        "openrouter",
        "vendor/model",
        "openrouter",
        "OPENROUTER_API_KEY",
    );
}

/// No command repairs a non-private home with content: writers refuse with the
/// path and the fix, read-only inspection reports, and the mode is never
/// changed. An empty directory is adopted instead.
#[cfg(unix)]
#[test]
fn a_non_private_home_is_refused_with_its_fix_and_never_repaired() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = isolated_home_dir();
    // An empty directory is adopted (made private) because nothing in it can
    // have been exposed; one with content is refused.
    let fresh = dir.path().join("fresh-home");
    std::fs::create_dir(&fresh).expect("fresh home");
    std::fs::set_permissions(&fresh, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    let out = Command::new(plak_bin())
        .env("PLAK_HOME", &fresh)
        .env("HOME", dir.path())
        .current_dir(dir.path())
        .args(["session", "list"])
        .output()
        .expect("spawn plak");
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let mode = std::fs::metadata(&fresh)
        .expect("meta")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o700, "an empty home is adopted");

    let home = dir.path().join("shared-home");
    std::fs::create_dir(&home).expect("home");
    std::fs::write(home.join("config.toml"), "").expect("config");
    std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    let plak = |args: &[&str]| {
        Command::new(plak_bin())
            .env("PLAK_HOME", &home)
            .env("HOME", dir.path())
            .current_dir(dir.path())
            .args(args)
            .output()
            .expect("spawn plak")
    };
    let fix = format!("run `chmod 700 {}`", home.display());
    for args in [
        &["session", "list"][..],
        &["config", "set", "tui.theme", "dark"][..],
    ] {
        let out = plak(args);
        assert_ne!(out.status.code(), Some(0), "{args:?}");
        assert!(stderr(&out).contains(&fix), "{args:?}: {}", stderr(&out));
    }
    for args in [&["status"][..], &["config", "show"][..], &["doctor"][..]] {
        let out = plak(args);
        if args == ["doctor"] {
            assert!(stdout(&out).contains("chmod 700"), "{}", stdout(&out));
        } else {
            assert_eq!(out.status.code(), Some(0), "{args:?}: {}", stderr(&out));
        }
    }
    let mode = std::fs::metadata(&home).expect("meta").permissions().mode() & 0o777;
    assert_eq!(mode, 0o755, "no command repairs the home");
}

#[test]
fn malformed_argv_has_exact_usage_exit() {
    let dir = isolated_home_dir();
    let out = run_plak(dir.path(), &["definitely-not-a-command"]);
    assert_eq!(out.status.code(), Some(2), "stderr={}", stderr(&out));
}

#[test]
fn agent_resume_tree_without_session_is_parse_error() {
    let dir = isolated_home_dir();
    let out = run_plak(dir.path(), &["agent", "--resume-tree"]);
    assert_eq!(out.status.code(), Some(2), "stderr={}", stderr(&out));
    let err = stderr(&out);
    assert!(
        err.contains("--session"),
        "agent --resume-tree without --session must fail closed: {err}"
    );
}

#[test]
fn agent_openrouter_account_parses_in_help_and_usage() {
    let dir = isolated_home_dir();
    let help = run_plak(dir.path(), &["agent", "--help"]);
    assert!(
        help.status.success(),
        "plak agent --help failed: {}",
        stderr(&help)
    );
    let text = stdout(&help);
    assert!(
        text.contains("openrouter"),
        "agent help should list openrouter account: {text}"
    );
    let parse = run_plak(
        dir.path(),
        &[
            "agent",
            "--account",
            "openrouter",
            "--model",
            "vendor/model",
            "--help",
        ],
    );
    assert!(
        parse.status.success(),
        "openrouter agent flags must parse before runtime auth: {}",
        stderr(&parse)
    );
}

#[test]
fn state_prune_requires_confirmation_lists_sizes_and_preserves_current_state() {
    let dir = isolated_home_dir();
    let paths = plak::home::paths::PlakPaths::explicit(dir.path().to_path_buf());
    let old = paths.state_root().join("r1.e1.p1.t1");
    std::fs::create_dir_all(&old).unwrap();
    std::fs::write(old.join("retained"), b"1234567").unwrap();
    let flat = paths.state_root().join("pre-key.log");
    std::fs::write(&flat, b"flat state").unwrap();
    std::fs::create_dir_all(paths.state_dir()).unwrap();
    std::fs::write(paths.state_dir().join("current"), b"keep").unwrap();
    let doctor = run_plak(dir.path(), &["doctor"]);
    assert!(stdout(&doctor).contains("7 bytes"), "{}", stdout(&doctor));
    assert!(stdout(&doctor).contains(old.to_str().unwrap()));
    let refused = run_plak(dir.path(), &["state", "prune-legacy"]);
    assert!(!refused.status.success());
    assert!(stderr(&refused).contains("interactive confirmation or explicit --yes"));
    assert!(old.exists());
    let removed = run_plak(dir.path(), &["state", "prune-legacy", "--yes"]);
    assert!(removed.status.success(), "{}", stderr(&removed));
    assert!(!old.exists());
    assert!(!flat.exists());
    assert!(stdout(&removed).contains("pruned 2 non-current state entries"));
    assert!(paths.state_dir().join("current").exists());
    let listed = run_plak(dir.path(), &["session", "list"]);
    assert!(listed.status.success(), "{}", stderr(&listed));
    assert!(!stderr(&listed).contains("legacy state"));
}

#[test]
fn state_partial_prune_reports_kept_counts_and_succeeds() {
    let dir = isolated_home_dir();
    let paths = plak::home::paths::PlakPaths::explicit(dir.path().to_path_buf());
    let kept = paths.state_root().join("r1.e1.p1.t1");
    std::fs::create_dir_all(&kept).unwrap();
    let repo = plakous_recursive_subagents::tree::vcs_fixtures::git_repo();
    let db = rusqlite::Connection::open(kept.join("subagents.sqlite3")).unwrap();
    db.execute_batch("PRAGMA user_version=2; CREATE TABLE agent_trees(tree_id TEXT, launch_checkout TEXT); CREATE TABLE agent_workspaces(tree_id TEXT, path TEXT, mode TEXT); CREATE TABLE agent_candidates(tree_id TEXT, candidate_id TEXT, adopted_by TEXT);").unwrap();
    db.execute(
        "INSERT INTO agent_trees VALUES('old', ?1)",
        [repo.path().display().to_string()],
    )
    .unwrap();
    db.execute(
        "INSERT INTO agent_candidates VALUES('old', 'unadopted', NULL)",
        [],
    )
    .unwrap();
    // Schema 2 uses flat candidate refs.
    assert!(
        Command::new("git")
            .args(["update-ref", "refs/plak/candidates/unadopted", "HEAD"])
            .current_dir(repo.path())
            .status()
            .unwrap()
            .success()
    );
    drop(db);
    let removable = paths.state_root().join("r2.e2.p2.t2");
    std::fs::create_dir(&removable).unwrap();
    let references = paths.home().join("references");
    std::fs::create_dir(&references).unwrap();
    let orphan = references.join(format!("{}.json", uuid::Uuid::new_v4()));
    std::fs::write(&orphan, b"fixture export").unwrap();
    let removed = run_plak(dir.path(), &["state", "prune-legacy", "--yes"]);
    assert_eq!(removed.status.code(), Some(0), "{}", stderr(&removed));
    assert!(stdout(&removed).contains("pruned 1 non-current state entries"));
    assert!(stdout(&removed).contains("kept 1 state entries and 1 owned artifact groups"));
    assert!(stderr(&removed).contains("kept:"));
    assert!(stderr(&removed).contains("1 unadopted refs"));
    assert!(kept.exists());
    assert!(!removable.exists());
    assert!(orphan.exists());
    let removed = run_plak(
        dir.path(),
        &[
            "state",
            "prune-legacy",
            "--yes",
            "--prune-orphaned-references",
        ],
    );
    assert_eq!(removed.status.code(), Some(0), "{}", stderr(&removed));
    assert!(!orphan.exists());
    assert!(kept.exists());
}

#[test]
fn doctor_reports_unowned_home_worktrees_as_informational() {
    let dir = isolated_home_dir();
    let unowned = dir.path().join("worktrees/unowned");
    std::fs::create_dir_all(&unowned).unwrap();
    std::fs::write(unowned.join("work"), b"keep").unwrap();
    let doctor = run_plak(dir.path(), &["doctor"]);
    assert_eq!(doctor.status.code(), Some(0), "{}", stderr(&doctor));
    let text = stdout(&doctor);
    assert!(text.contains("unowned worktree"), "{text}");
    assert!(text.contains("4 bytes"), "{text}");
    assert!(text.contains("informational; retained"), "{text}");
    let removed = run_plak(dir.path(), &["state", "prune-legacy", "--yes"]);
    assert_eq!(removed.status.code(), Some(0), "{}", stderr(&removed));
    assert!(unowned.exists());
}

#[test]
fn prune_keeps_orphaned_references_unless_explicitly_requested() {
    let dir = isolated_home_dir();
    let paths = plak::home::paths::PlakPaths::explicit(dir.path().to_path_buf());
    let old = paths.state_root().join("r1.e1.p1.t1");
    std::fs::create_dir_all(&old).unwrap();
    let references = paths.home().join("references");
    std::fs::create_dir(&references).unwrap();
    let orphan = references.join(format!("{}.json", uuid::Uuid::new_v4()));
    std::fs::write(&orphan, b"fixture export").unwrap();
    let removed = run_plak(dir.path(), &["state", "prune-legacy", "--yes"]);
    assert!(removed.status.success(), "{}", stderr(&removed));
    assert!(!old.exists());
    assert!(
        orphan.exists(),
        "an orphaned reference can be the only surviving export"
    );
    let removed = run_plak(
        dir.path(),
        &[
            "state",
            "prune-legacy",
            "--yes",
            "--prune-orphaned-references",
        ],
    );
    assert!(removed.status.success(), "{}", stderr(&removed));
    assert!(!orphan.exists());
}

#[test]
fn rejected_exports_do_not_create_state_or_reference_files() {
    for version in [5, 6] {
        let dir = isolated_home_dir();
        let path = dir.path().join("export.json");
        let mut doc: serde_json::Value =
            serde_json::from_str(include_str!("../fixtures/session-export-v6.json")).unwrap();
        doc["version"] = serde_json::json!(version);
        if version == 6 {
            doc["checkpoint"]["user_intent"] = serde_json::json!("x".repeat(65537));
            doc["checkpoint"]["reference"] = serde_json::json!("");
            doc["checkpoint"].as_object_mut().unwrap().remove("text");
        }
        std::fs::write(&path, serde_json::to_vec(&doc).unwrap()).unwrap();
        let rejected = run_plak(dir.path(), &["session", "import", path.to_str().unwrap()]);
        assert!(!rejected.status.success());
        assert!(
            stderr(&rejected).contains(if version == 5 {
                "version 5"
            } else {
                "oversized"
            }),
            "{}",
            stderr(&rejected)
        );
        assert!(
            !plak::home::paths::PlakPaths::explicit(dir.path().to_path_buf())
                .state_root()
                .exists()
        );
        assert!(!dir.path().join("references").exists());
    }
}

#[test]
fn unreadable_legacy_entry_is_best_effort_for_session_list_and_doctor() {
    use std::os::unix::fs::PermissionsExt as _;
    if rustix::process::geteuid().is_root() {
        return;
    }
    let dir = isolated_home_dir();
    let paths = plak::home::paths::PlakPaths::explicit(dir.path().to_path_buf());
    let old = paths.state_root().join("r1.e1.p1.t1");
    std::fs::create_dir_all(&old).unwrap();
    std::fs::write(old.join("secret-size"), b"retained").unwrap();
    std::fs::set_permissions(&old, std::fs::Permissions::from_mode(0o000)).unwrap();
    let listed = run_plak(dir.path(), &["session", "list"]);
    let doctor = run_plak(dir.path(), &["doctor"]);
    std::fs::set_permissions(&old, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(listed.status.success(), "{}", stderr(&listed));
    assert!(
        stderr(&listed).contains("size unknown"),
        "{}",
        stderr(&listed)
    );
    let report = stdout(&doctor);
    assert!(report.contains(old.to_str().unwrap()), "{report}");
    assert!(report.contains("WARNING · size unavailable:"), "{report}");
    assert!(report.contains("Result:"), "{report}");
    assert!(!stderr(&doctor).contains("could not inspect"));
}

/// The repository-only MCP fixture server. Workspace test runs build every
/// workspace binary into the directory above the test binary's `deps`.
fn fake_mcp_server() -> PathBuf {
    let executable = std::env::current_exe().expect("test binary");
    let server = executable
        .parent()
        .and_then(std::path::Path::parent)
        .expect("target directory")
        .join("mcp-fake-server");
    assert!(
        server.is_file(),
        "{} is missing; run the workspace tests so the fixture binary is built",
        server.display()
    );
    server
}

/// One HTTP/1.1 GET standing in for the browser; returns the `Location` the
/// authorization server redirects to.
fn browse_to_redirect(url: &str) -> String {
    let rest = url.strip_prefix("http://").expect("loopback http url");
    let (authority, target) = rest.split_at(rest.find('/').expect("url path"));
    let mut stream = std::net::TcpStream::connect(authority).expect("connect");
    write!(
        stream,
        "GET {target} HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\n\r\n"
    )
    .expect("request");
    let mut response = String::new();
    stream.read_to_string(&mut response).expect("response");
    response
        .lines()
        .take_while(|line| !line.is_empty())
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("location")
                .then(|| value.trim().to_owned())
        })
        .unwrap_or_else(|| panic!("no redirect in:\n{response}"))
}

/// Runs `plak mcp login fake --remote`, plays the browser on the authorize
/// URL it prints and pastes back the redirect; returns the stderr after the
/// URL.
fn login_by_paste_back(home: &std::path::Path) -> String {
    use std::io::BufRead as _;

    let mut login = Command::new(plak_bin())
        .env("PLAK_HOME", home)
        .env("HOME", home)
        .env_remove("SSH_CONNECTION")
        .env_remove("SSH_CLIENT")
        .env_remove("SSH_TTY")
        .current_dir(home)
        .args(["mcp", "login", "fake", "--remote"])
        .stdin(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = login.stdin.take().unwrap();
    let mut stderr = std::io::BufReader::new(login.stderr.take().unwrap());
    let mut shown = String::new();
    let authorize = loop {
        let mut line = String::new();
        assert!(
            stderr.read_line(&mut line).unwrap() > 0,
            "no authorization URL in:\n{shown}"
        );
        shown.push_str(&line);
        if line.starts_with("http://") && line.contains("/authorize?") {
            break line.trim().to_owned();
        }
    };
    assert!(shown.contains("mcp server fake: authorization server http://127.0.0.1"));
    assert!(
        authorize.contains("code_challenge_method=S256"),
        "{authorize}"
    );
    let redirect = browse_to_redirect(&authorize);
    writeln!(stdin, "{redirect}").unwrap();
    drop(stdin);
    let mut rest = String::new();
    stderr.read_to_string(&mut rest).unwrap();
    let status = login.wait().unwrap();
    assert!(status.success(), "stderr:\n{shown}{rest}");
    rest
}

/// `plak mcp login --remote` against a scripted OAuth server: the pasted
/// callback URL completes the login, `list` reports it, and `logout`
/// revokes it. A login over SSH without `--remote`, and one for a server
/// without `auth = "oauth"`, are refused before anything is contacted.
#[test]
fn mcp_login_remote_paste_back_logs_in_and_logout_revokes() {
    use std::io::BufRead as _;

    let root = tempfile::tempdir().unwrap();
    let home = root.path();
    let mut server = Command::new(fake_mcp_server())
        .args(["--http", "--oauth"])
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("spawn the fixture server");
    let mut line = String::new();
    std::io::BufReader::new(server.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    let url = line
        .trim()
        .strip_prefix("listening on ")
        .expect("fixture url")
        .to_owned();
    let ok = |output: &std::process::Output| {
        assert!(
            output.status.success(),
            "stdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    };
    ok(&run_plak(
        home,
        &["mcp", "add", "fake", "--url", &url, "--oauth"],
    ));
    ok(&run_plak(home, &["mcp", "add", "plain", "--url", &url]));
    assert!(ok(&run_plak(home, &["mcp", "list"])).contains("not logged in"));
    assert!(
        ok(&run_plak(home, &["mcp", "logout", "fake"]))
            .contains("mcp server fake is not logged in; nothing to log out")
    );

    let refused = run_plak(home, &["mcp", "login", "plain", "--remote"]);
    assert!(!refused.status.success());
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("mcp server plain does not use oauth"),
        "{}",
        String::from_utf8_lossy(&refused.stderr)
    );
    let mut ssh = Command::new(plak_bin());
    let ssh = ssh
        .env("PLAK_HOME", home)
        .env("HOME", home)
        .env("SSH_CONNECTION", "10.0.0.1 22 10.0.0.2 22")
        .current_dir(home)
        .args(["mcp", "login", "fake"])
        .output()
        .unwrap();
    assert!(!ssh.status.success());
    assert!(
        String::from_utf8_lossy(&ssh.stderr).contains("plak mcp login fake --remote"),
        "{}",
        String::from_utf8_lossy(&ssh.stderr)
    );

    let rest = login_by_paste_back(home);
    assert!(rest.contains("mcp server fake: logged in"), "{rest}");
    assert!(
        home.join("secrets/mcp-oauth/fake/credential.json")
            .is_file()
    );
    let listed = ok(&run_plak(home, &["mcp", "list"]));
    assert!(listed.contains("login: logged in"), "{listed}");

    assert!(
        ok(&run_plak(home, &["mcp", "logout", "fake"]))
            .contains("logged out of mcp server fake; the authorization server revoked the grant")
    );
    assert!(ok(&run_plak(home, &["mcp", "list"])).contains("not logged in"));
    server.kill().unwrap();
    server.wait().unwrap();
}
