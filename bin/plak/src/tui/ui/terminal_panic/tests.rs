use super::*;
use std::sync::Arc;

const MARKER: &str = "panic payload must not reach the active terminal";

#[derive(Default)]
struct Observed {
    actions: Vec<&'static str>,
    printed: Vec<String>,
    sites: Vec<(String, String, u32, u32)>,
}

#[test]
fn panic_hook_child() {
    let Ok(scenario) = std::env::var("PLAK_TERMINAL_PANIC_TEST") else {
        return;
    };
    let previous = std::panic::take_hook();
    let expected_owner = std::thread::current().id();
    let owner = Arc::new(Mutex::new(
        (scenario != "outside").then_some(expected_owner),
    ));
    let ownership = Arc::clone(&owner);
    let observed = Arc::new(Mutex::new(Observed::default()));
    let restores = Arc::clone(&observed);
    let prints = Arc::clone(&observed);
    let reports = Arc::clone(&observed);
    std::panic::set_hook(make_hook(
        move || *ownership.lock().unwrap(),
        move || restores.lock().unwrap().actions.push("restore"),
        Box::new(move |info| {
            let mut output = prints.lock().unwrap();
            output.actions.push("print");
            output
                .printed
                .push(info.payload().downcast_ref::<&str>().unwrap().to_string());
        }),
        move |site| {
            reports
                .lock()
                .unwrap()
                .sites
                .push((site.thread, site.file, site.line, site.column));
        },
    ));
    match scenario.as_str() {
        "owner" | "outside" => {
            assert!(std::panic::catch_unwind(|| std::panic::panic_any(MARKER)).is_err());
        }
        "caught" | "uncaught" => {
            let caught = scenario == "caught";
            let result = std::thread::Builder::new()
                .name("terminal-background".into())
                .spawn(move || {
                    if caught {
                        assert!(
                            std::panic::catch_unwind(|| std::panic::panic_any(MARKER)).is_err()
                        );
                    } else {
                        std::panic::panic_any(MARKER);
                    }
                })
                .unwrap()
                .join();
            assert_eq!(result.is_ok(), caught);
        }
        _ => panic!("unknown panic-hook scenario"),
    }
    std::panic::set_hook(previous);
    let output = observed.lock().unwrap();
    assert_observed(&scenario, &output, *owner.lock().unwrap(), expected_owner);
}

fn assert_observed(
    scenario: &str,
    output: &Observed,
    owner: Option<ThreadId>,
    expected_owner: ThreadId,
) {
    match scenario {
        "owner" => {
            assert_eq!(output.actions, ["restore", "print"]);
            assert_eq!(output.printed, [MARKER]);
            assert_eq!(output.sites.len(), 0);
        }
        "outside" => {
            assert_eq!(output.actions, ["print"]);
            assert_eq!(output.printed, [MARKER]);
            assert_eq!(output.sites.len(), 0);
        }
        _ => {
            assert_eq!(output.actions, [] as [&str; 0]);
            assert_eq!(output.printed, [] as [String; 0]);
            assert_eq!(output.sites.len(), 1);
            let (thread, file, line, column) = &output.sites[0];
            assert_eq!(thread, "terminal-background");
            assert!(file.ends_with("terminal_panic/tests.rs"));
            assert!(*line > 0 && *column > 0);
            assert_eq!(owner, Some(expected_owner));
        }
    }
}

fn run_child(scenario: &str) {
    let result = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "tui::ui::terminal_panic::tests::panic_hook_child",
            "--nocapture",
        ])
        .env("PLAK_TERMINAL_PANIC_TEST", scenario)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(
        result.stderr,
        [] as [u8; 0],
        "hook must not leak panic payloads or terminal escapes"
    );
}

#[test]
fn background_panics_caught_or_uncaught_neither_restore_nor_print() {
    run_child("caught");
    run_child("uncaught");
}

#[test]
fn owner_panic_restores_before_printing() {
    run_child("owner");
}

#[test]
fn panic_outside_the_tui_keeps_the_previous_hook_without_restoring() {
    run_child("outside");
}
