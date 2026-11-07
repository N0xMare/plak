//! Client-local build probes and the host-only reload boundary.

use super::*;

fn installed_wrapper(fixture: &Fixture, body: &str) -> PathBuf {
    let installed = fixture.root.join("installed");
    std::fs::create_dir_all(&installed).unwrap();
    write_script_file(
        &installed.join("plak"),
        &format!(
            "#!/bin/bash\n{body}\nexec -a plak '{}' \"$@\"\n",
            fixture.root.join("real-bin/plak").display(),
        ),
    );
    installed
}

fn wait_for_file(path: &Path) {
    let until = Instant::now() + WAIT;
    while !path.exists() {
        assert!(
            Instant::now() < until,
            "file never appeared: {}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn review_a_slow_installed_probe_does_not_delay_the_host_tui() {
    let fixture = Fixture::with_mode(&tool_then_answer("ready before probe"), Mode::Host);
    fixture.ensure_host();
    let probe_started = fixture.root.join("probe-started");
    let installed = installed_wrapper(
        &fixture,
        &format!(
            "if [ \"$1\" = version ] && [ \"$2\" = --reload-probe ]; then touch '{}'; sleep 4; fi",
            probe_started.display(),
        ),
    );
    let started = Instant::now();
    let mut terminal = Terminal::spawn(&fixture, Launch::OnPath(&installed));
    terminal.wait_ready();
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(2),
        "slow probe blocked launch: {elapsed:?}"
    );
    wait_for_file(&probe_started);
    terminal.type_draft("usable before probe finishes");
    eprintln!("host client ready with a four-second probe: {elapsed:?}");
    terminal.quit();
}

fn stage_installed_image(root: &Path, tag: char) -> (PathBuf, String) {
    let directory = root
        .join(".local/lib/plak/maintainer")
        .join(tag.to_string().repeat(64));
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
    let image = directory.join("plak");
    // A hard link changes the shared executable's ctime (invalidating other
    // tests' identity caches), and Darwin may report its original vnode path.
    std::fs::copy(env!("CARGO_BIN_EXE_plak"), &image).unwrap();
    let output = Command::new(&image)
        .env_clear()
        .args([
            "version",
            "--write-binary-manifest",
            directory.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let manifest = std::fs::read_to_string(directory.join("binary-id")).unwrap();
    let binary = manifest.split_whitespace().next().unwrap().to_owned();
    (image, binary)
}

pub(super) fn installed_client(fixture: &Fixture) -> (PathBuf, String) {
    let installed = stage_installed_image(&fixture.root, 'b');
    let real = fixture.root.join("real-bin");
    std::fs::create_dir_all(&real).unwrap();
    std::os::unix::fs::symlink(&installed.0, real.join("plak")).unwrap();
    installed
}

pub(super) fn image_command(fixture: &Fixture, image: &Path) -> Command {
    let base = fixture.command();
    let mut command = Command::new(image);
    command.env_clear().current_dir(fixture.workspace());
    command.envs(
        base.get_envs()
            .filter_map(|(key, value)| value.map(|value| (key, value))),
    );
    command
}

pub(super) fn start_image_host(fixture: &Fixture, image: &Path) {
    fixture.ensure_host_with_command(image_command(fixture, image));
}

fn start_old_build(fixture: &Fixture) {
    let (image, _) = stage_installed_image(&fixture.root, 'a');
    // Installer identities are trusted facts. Give this fixture snapshot
    // another identity without changing the shared executable's bytes.
    let manifest = image.parent().unwrap().join("binary-id");
    let contents = std::fs::read_to_string(&manifest).unwrap();
    std::fs::set_permissions(&manifest, std::fs::Permissions::from_mode(0o600)).unwrap();
    std::fs::write(
        &manifest,
        format!("000000000000 {}", contents.split_once(' ').unwrap().1),
    )
    .unwrap();
    start_image_host(fixture, &image);
    assert_eq!(Directory::connect(fixture).binary, "000000000000");
}

#[test]
fn reconnecting_to_the_installed_host_clears_the_old_build_notice() {
    reconnect_notice(false);
}

#[test]
fn a_late_probe_result_cannot_restore_the_previous_hosts_notice() {
    reconnect_notice(true);
}

fn reconnect_notice(late: bool) {
    let fixture = Fixture::with_mode(&tool_then_answer("same resident session"), Mode::Host);
    let (installed_image, installed_id) = installed_client(&fixture);
    start_old_build(&fixture);
    let probe_started = fixture.root.join("old-probe-started");
    let release = fixture.root.join("old-probe-release");
    let finished = fixture.root.join("old-probe-finished");
    let body = if late {
        format!(
            "if [ \"$1\" = version ] && [ \"$2\" = --reload-probe ] && [ ! -e '{started}' ]; then\ntouch '{started}'\nn=0\nwhile [ ! -e '{release}' ] && [ $((n += 1)) -le 200 ]; do sleep 0.02; done\n[ -e '{release}' ] || exit 1\n'{real}' \"$@\"\ntouch '{finished}'\nexit 0\nfi",
            started = probe_started.display(),
            release = release.display(),
            finished = finished.display(),
            real = fixture.root.join("real-bin/plak").display(),
        )
    } else {
        String::new()
    };
    let installed = installed_wrapper(&fixture, &body);
    let mut terminal = Terminal::spawn(&fixture, Launch::OnPath(&installed));
    terminal.wait_ready();
    if late {
        wait_for_file(&probe_started);
    } else {
        terminal.wait_screen("host build 000000000000 differs from installed build");
    }
    terminal.send(b"work before restart\r");
    terminal.wait_screen("same resident session");
    terminal.wait_absent("Esc stop");
    let session = Directory::connect(&fixture).rows()[0]["session"]
        .as_str()
        .unwrap()
        .to_owned();
    fixture.kill_host();
    terminal.wait_screen("product host disconnected");
    start_image_host(&fixture, &installed_image);
    assert_eq!(Directory::connect(&fixture).binary, installed_id);
    terminal.send(b"/attach\r");
    terminal.wait_ready();
    terminal.wait_absent("differs from installed build");
    if late {
        std::fs::write(release, "complete old probe").unwrap();
        wait_for_file(&finished);
    }
    terminal.send(b"work after restart\r");
    let screen = terminal.wait_screen("work after restart");
    assert!(!screen.contains("host build 000000000000"), "{screen}");
    assert!(
        Directory::connect(&fixture)
            .rows()
            .iter()
            .any(|row| row["session"] == session)
    );
    terminal.quit();
}

#[test]
fn an_installed_identity_fixture_never_links_the_shared_executable() {
    use std::os::unix::fs::MetadataExt as _;
    let source = Path::new(env!("CARGO_BIN_EXE_plak"));
    // Force the same filesystem so a hard-link regression cannot hide behind
    // EXDEV, as it did when test scratch and build artifacts used separate disks.
    let root = tempfile::tempdir_in(source.parent().unwrap()).unwrap();
    let before = std::fs::metadata(source).unwrap();
    let (image, _) = stage_installed_image(root.path(), 'c');
    let after = std::fs::metadata(source).unwrap();
    let installed = std::fs::metadata(image).unwrap();
    assert_ne!(
        (installed.dev(), installed.ino()),
        (after.dev(), after.ino())
    );
    assert_eq!(
        (before.ctime(), before.ctime_nsec()),
        (after.ctime(), after.ctime_nsec())
    );
}

#[test]
fn a_genuinely_unknown_host_identity_shows_no_build_notice() {
    let mut script = tool_then_answer("unknown host still serves work");
    script["identity_hash_delay_ms"] = json!(60_000);
    let fixture = Fixture::with_mode(&script, Mode::Host);
    let (image, installed_id) = installed_client(&fixture);
    assert!(!fixture.home().join("binary-ids").exists());
    // The source-checkout host has neither a manifest nor a completed hash;
    // the installed client does have a real installer manifest.
    fixture.ensure_host();
    assert_eq!(Directory::connect(&fixture).binary, "unknown");
    let finished = fixture.root.join("installed-probe-finished");
    let installed = installed_wrapper(
        &fixture,
        &format!(
            "if [ \"$1\" = version ] && [ \"$2\" = --reload-probe ]; then\n'{real}' \"$@\"\ntouch '{finished}'\nexit 0\nfi",
            real = fixture.root.join("real-bin/plak").display(),
            finished = finished.display(),
        ),
    );
    let mut terminal = Terminal::spawn(&fixture, Launch::OnPath(&installed));
    terminal.wait_ready();
    wait_for_file(&finished);
    terminal.send(b"work without a host identity\r");
    let screen = terminal.wait_screen("unknown host still serves work");
    assert!(!screen.contains("differs from installed build"), "{screen}");
    assert_eq!(Directory::connect(&fixture).binary, "unknown");
    let output = image_command(&fixture, &image)
        .args(["version", "--reload-probe"])
        .output()
        .unwrap();
    let facts: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(facts["binary_id"], installed_id, "{output:?}");
    terminal.quit();
}

pub(super) fn make_first_installed_probe_unknown(
    fixture: &Fixture,
    image: &Path,
    installed: &Path,
) {
    let output = image_command(fixture, image)
        .args(["version", "--reload-probe"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let mut facts: Value = serde_json::from_slice(&output.stdout).unwrap();
    facts["binary_id"] = Value::Null;
    let marker = fixture.root.join("first-probe-completed");
    write_script_file(
        &installed.join("plak"),
        &format!(
            "#!/bin/bash\nif [ \"$1\" = version ] && [ \"$2\" = --reload-probe ] && [ ! -e '{marker}' ]; then\ntouch '{marker}'\nprintf '%s\\n' '{facts}'\nexit 0\nfi\nexec -a plak '{image}' \"$@\"\n",
            marker = marker.display(),
            image = image.display(),
        ),
    );
}

fn runtime_fences(fixture: &Fixture) -> Vec<(String, String, String)> {
    let paths = plak::home::paths::PlakPaths::explicit(fixture.home());
    let database = rusqlite::Connection::open_with_flags(
        paths.runtime_database(),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    database.prepare("SELECT hex(last_fence), coalesce(active_owner,''), coalesce(released_owner,'') FROM branches ORDER BY session_id,branch_id")
        .unwrap().query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .unwrap().collect::<Result<_, _>>().unwrap()
}

#[test]
fn a_host_reload_refuses_if_the_host_stops_before_client_exec() {
    let fixture = Fixture::with_mode(&tool_then_answer("host stops before exec"), Mode::Host);
    fixture.ensure_host();
    let pid = fixture.host.lock().unwrap().as_ref().unwrap().child.id();
    let started = fixture.root.join("client-started");
    let stopped = fixture.root.join("host-stopped");
    let release = fixture.root.join("exec-released");
    let args = fixture.root.join("reload-args");
    let installed = installed_wrapper(
        &fixture,
        &format!(
            "if [ \"$1\" != version ]; then\nif [ -e '{started}' ]; then\nkill -KILL {pid}\nprintf '%s\\n' \"$@\" > '{args}'\ntouch '{stopped}'\nn=0\nwhile [ ! -e '{release}' ] && [ $((n += 1)) -le 200 ]; do sleep 0.02; done\n[ -e '{release}' ] || exit 1\nfi\ntouch '{started}'\nfi",
            started = started.display(),
            args = args.display(),
            stopped = stopped.display(),
            release = release.display(),
        ),
    );
    let mut terminal = Terminal::spawn(&fixture, Launch::OnPath(&installed));
    terminal.wait_ready();
    terminal.send(b"keep this session in host mode\r");
    terminal.wait_screen("host stops before exec");
    terminal.wait_absent("Esc stop");
    let session = Directory::connect(&fixture).rows()[0]["session"]
        .as_str()
        .unwrap()
        .to_owned();
    let dispatches = provider_dispatches(&fixture);
    terminal.send(b"/reload\r");
    wait_for_file(&stopped);
    fixture
        .host
        .lock()
        .unwrap()
        .take()
        .unwrap()
        .child
        .wait()
        .unwrap();
    let fences = runtime_fences(&fixture);
    std::fs::write(release, "continue").unwrap();
    let screen = terminal.wait_where("a refusal, never an embedded shell", |frame| {
        frame.contains("the host stopped; start it, then plak resume")
            || frame.contains("/ commands")
    });
    assert!(
        screen.contains("the host stopped; start it, then plak resume"),
        "{screen}"
    );
    let (status, screen) = terminal.finish_with_screen();
    assert!(!status.success(), "{screen}");
    assert!(
        screen.contains("the host stopped; start it, then plak resume"),
        "{screen}"
    );
    assert!(screen.contains(&session), "{screen}");
    assert_eq!(
        std::fs::read_to_string(args).unwrap(),
        format!("--host-resume\n{session}\n")
    );
    assert_eq!(
        runtime_fences(&fixture),
        fences,
        "the replacement client took custody"
    );
    assert_eq!(provider_dispatches(&fixture), dispatches);
}
