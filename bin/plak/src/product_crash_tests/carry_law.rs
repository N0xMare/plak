//! The product carry crash law (spec 05): the real `/rebind` carry
//! (`carry::rebind_session`) and the reload's orchestration carry
//! (`carry::apply_on_reload`), each killed before its runtime create and
//! after the create returned but before the record that binds it is saved.
//! A relaunch settles the write-ahead intent and must show exactly one
//! carried record at the line's head, with the source redirecting to it.

#![cfg(all(unix, feature = "test-hooks", feature = "tui"))]

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::product_paths::PlakPaths;
use plakous::sessions::{
    AgentSessions, CreateSessionOutcome, CreateSessionRequest, SessionBackend, SessionLocator,
    SessionOperationId,
};

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

const CHILD: &str = "product_crash_tests::carry_law::product_carry_crash_child";
const MODE: &str = "PLAK_CARRY_LAW_MODE";
const PATH: &str = "PLAK_CARRY_LAW_PATH";
const ROOT: &str = "PLAK_CARRY_LAW_ROOT";
const CAUSE: &str = "binding changed; carried into a new session";

struct Layout {
    home: PathBuf,
    workspace: PathBuf,
    script: PathBuf,
    state: PathBuf,
}

impl Layout {
    fn new(root: &Path) -> Self {
        Self {
            home: root.join("plak-home"),
            workspace: root.join("workspace"),
            script: root.join("script.json"),
            state: root.join("carry-law.json"),
        }
    }
}

#[test]
#[ignore = "process child entrypoint used only by the product carry crash law"]
fn product_carry_crash_child() -> TestResult {
    let (Some(mode), Some(path), Some(root)) = (
        std::env::var_os(MODE),
        std::env::var(PATH).ok(),
        std::env::var_os(ROOT),
    ) else {
        return Ok(());
    };
    let layout = Layout::new(Path::new(&root));
    let executor = plakous::application::ApplicationExecutor::new()?;
    match mode.to_str() {
        Some("setup") => executor.block_on(setup(&layout, &path))??,
        Some("carry") => carry(&layout, &path)?,
        Some("relaunch") => relaunch(&layout)?,
        _ => return Err("unknown carry law mode".into()),
    }
    Ok(())
}

/// One source session with a turn, bound like the offline product binding;
/// the reload path also freezes a tree whose spawn cap the config since
/// changed, so its orchestration carry applies.
async fn setup(layout: &Layout, path: &str) -> TestResult {
    std::fs::create_dir_all(&layout.workspace)?;
    let paths = PlakPaths::explicit(layout.home.clone());
    paths.ensure_layout().map_err(std::io::Error::other)?;
    let controller = plakous_offline_crash_harness::BoundaryController::quiet_from_evidence_path(
        plakous_offline_crash_harness::protocol::CrashBoundary::None,
        layout.home.join("setup-evidence.jsonl"),
    )?;
    let sessions = AgentSessions::new(SessionBackend::sqlite(paths.runtime_database())?)?;
    let CreateSessionOutcome::Created { session, .. } = sessions
        .create(CreateSessionRequest::new(
            SessionOperationId::new("carry-law:source")?,
            plakous_offline_crash_harness::runtime_binding(&layout.workspace, controller)
                .map_err(|error| -> Box<dyn std::error::Error> { error })?,
        ))
        .await?
    else {
        return Err("source was not created".into());
    };
    let _ = session
        .agent()
        .prompt("before the carry")
        .await?
        .result()
        .await?;
    let locator = session.release().await?;
    let mut journal =
        crate::test_support::ProductJournal::create(layout.home.clone(), layout.workspace.clone())?;
    journal.bind_locator(locator)?;
    journal.set_product_binding("codex", "offline-loopback-v1", "medium")?;
    if path == "reload" {
        let workspace_paths =
            PlakPaths::explicit(layout.home.clone()).for_workspace(&layout.workspace);
        let mut config = crate::product_config::ProductConfig::default_product();
        config.bind_root_tuple("codex", "offline-loopback-v1", Some("medium"));
        config.max_children = 2;
        config.save(&workspace_paths)?;
        let mut tree = plakous_hosted::topology_runtime::test_support::bootstrap(
            journal.product_session_id(),
            layout.workspace.clone(),
        );
        tree.install_snapshot(crate::roster_input::snapshot_from_config(
            &config,
            "codex",
            "offline-loopback-v1",
            Some("medium".into()),
        ));
        crate::store::tree::save_tree(&workspace_paths, &mut tree)
            .map_err(|error| error.message())?;
        config.max_children = 10;
        config.save(&workspace_paths)?;
    }
    std::fs::write(
        &layout.state,
        serde_json::to_vec(&serde_json::json!({
            "source": journal.product_session_id(),
            "locator": locator.to_string(),
        }))?,
    )?;
    Ok(())
}

fn source_id(layout: &Layout) -> TestResult<String> {
    let state: serde_json::Value = serde_json::from_slice(&std::fs::read(&layout.state)?)?;
    Ok(state["source"].as_str().ok_or("source missing")?.to_owned())
}

/// The real carry; the offline model's `carry-cut` kills the process inside it.
fn carry(layout: &Layout, path: &str) -> TestResult {
    // The child runs in the workspace, as the product's CLI edge would name it.
    let paths = PlakPaths::explicit(layout.home.clone()).for_workspace(&layout.workspace);
    let services = crate::shell::services::Services::for_home(paths);
    let mut record = services.session_records()?.load(&source_id(layout)?)?;
    if path == "reload" {
        crate::carry::apply_on_reload(&services, &mut record).ok_or("no orchestration carry")?;
    } else {
        crate::carry::rebind_session(&services, &record, CAUSE)
            .map_err(|error| error.message().to_owned())?;
    }
    Err("the carry finished past its cut".into())
}

/// Startup's settlement and the source's open, as a relaunched TUI runs
/// them; writes where the source's open landed.
fn relaunch(layout: &Layout) -> TestResult {
    // The child runs in the workspace, as the product's CLI edge would name it.
    let paths = PlakPaths::explicit(layout.home.clone()).for_workspace(&layout.workspace);
    let services = crate::shell::services::Services::for_home(paths);
    let mut notes = crate::carry::intent::settle_workspace(&services, &layout.workspace);
    let source = services.session_records()?.load(&source_id(layout)?)?;
    let (resolved, note) = crate::carry::intent::resolve_open(&services, source)
        .map_err(|error| error.message().to_owned())?;
    notes.extend(note);
    std::fs::write(
        layout.state.with_extension("relaunch.json"),
        serde_json::to_vec(&serde_json::json!({
            "resolved": resolved.id,
            "locator": resolved.plakous_locator.map(|locator| locator.to_string()),
            "notes": notes,
        }))?,
    )?;
    Ok(())
}

fn child(
    layout: &Layout,
    root: &Path,
    mode: &str,
    path: &str,
    cut: Option<&str>,
) -> TestResult<std::process::ExitStatus> {
    let mut command = Command::new(std::env::current_exe()?);
    command
        .args([
            "--ignored",
            "--exact",
            CHILD,
            "--nocapture",
            "--test-threads=1",
        ])
        .current_dir(if layout.workspace.exists() {
            layout.workspace.clone()
        } else {
            root.to_path_buf()
        })
        .env(MODE, mode)
        .env(PATH, path)
        .env(ROOT, root)
        .env("PLAK_TEST_MODEL", crate::test_hooks::SELECTOR)
        .env("PLAK_TEST_SCRIPT", &layout.script)
        .env("PLAK_TEST_EVIDENCE", root.join("evidence.jsonl"))
        .stdout(std::process::Stdio::null());
    // The offline model reads the cut beside its evidence log.
    let marker = root.join("carry-cut");
    if let Some(cut) = cut {
        std::fs::write(&marker, cut)?;
    } else if marker.exists() {
        std::fs::remove_file(&marker)?;
    }
    Ok(command.status()?)
}

/// Every cut of both real carry paths relaunches to one successor at the
/// line's head, and the source's open redirects to it.
#[test]
fn product_carry_cuts_relaunch_to_one_successor_at_the_line_head() -> TestResult {
    use std::os::unix::process::ExitStatusExt as _;

    for path in ["rebind", "reload"] {
        for cut in ["prepared", "runtime-returned"] {
            let scenario = format!("{path}/{cut}");
            let root = PlakPaths::isolated_tempdir();
            let layout = Layout::new(root.path());
            std::fs::write(
                &layout.script,
                serde_json::to_vec(&serde_json::json!({
                    "version": 1,
                    "steps": [{"output": {"text": "after the carry"}}]
                }))?,
            )?;
            assert!(
                child(&layout, root.path(), "setup", path, None)?.success(),
                "{scenario}: setup"
            );
            let killed = child(&layout, root.path(), "carry", path, Some(cut))?;
            assert_eq!(
                killed.signal(),
                Some(6),
                "{scenario}: the cut did not stop the carry"
            );
            assert!(
                child(&layout, root.path(), "relaunch", path, None)?.success(),
                "{scenario}: relaunch"
            );

            let source = source_id(&layout)?;
            let landed: serde_json::Value = serde_json::from_slice(&std::fs::read(
                layout.state.with_extension("relaunch.json"),
            )?)?;
            let paths = PlakPaths::explicit(layout.home.clone());
            let records =
                crate::store::ProductStore::open(&paths)?.list_for_workspace(&layout.workspace)?;
            let carried = records
                .iter()
                .filter(|record| {
                    record.carried_forward_from().map(|(id, _)| id) == Some(source.as_str())
                })
                .collect::<Vec<_>>();
            assert_eq!(carried.len(), 1, "{scenario}: one carried record");
            let successor = carried[0];
            assert!(
                successor.carry_intent.is_none(),
                "{scenario}: intent settled"
            );
            let target = successor.plakous_locator.ok_or("successor unbound")?;
            assert_eq!(
                landed["resolved"],
                successor.id.as_str(),
                "{scenario}: source {source}: {landed}"
            );
            assert!(
                landed["notes"]
                    .as_array()
                    .ok_or("notes")?
                    .iter()
                    .any(|note| note.as_str().is_some_and(
                        |note| note.contains(&format!("carried forward as {}", successor.id))
                    )),
                "{scenario}: {landed}"
            );
            let state: serde_json::Value = serde_json::from_slice(&std::fs::read(&layout.state)?)?;
            let source_locator: SessionLocator =
                state["locator"].as_str().ok_or("locator")?.parse()?;
            let runtime = AgentSessions::new(SessionBackend::sqlite(paths.runtime_database())?)?;
            let executor = plakous::application::ApplicationExecutor::new()?;
            let head = executor.block_on(runtime.line_head_of(source_locator))??;
            assert_eq!(
                (head.session_id(), head.branch_id(), head.generation()),
                (target.session_id(), target.branch_id(), 2),
                "{scenario}"
            );
            let sessions: i64 = rusqlite::Connection::open(paths.runtime_database())?.query_row(
                "SELECT count(*) FROM sessions",
                [],
                |row| row.get(0),
            )?;
            assert_eq!(sessions, 2, "{scenario}: one source and one successor");
        }
    }
    Ok(())
}
