use std::sync::{Arc, Barrier};

use super::*;

const RACE_HOME: &str = "PLAK_TEST_HOST_ID_RACE_HOME";
const RACE_CHILD: &str = "commands::runner::home::tests::host_id_race_child";

/// The second process of the race: waits for the go file, then makes or
/// reads the home's host id and prints it.
#[test]
#[ignore = "process child entrypoint used only by the host-id race"]
fn host_id_race_child() {
    let Some(home) = std::env::var_os(RACE_HOME).map(PathBuf::from) else {
        return;
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !home.join("go").exists() && std::time::Instant::now() < deadline {
        std::thread::yield_now();
    }
    let id = host_id(&PlakPaths::explicit(home)).unwrap();
    println!("host-id={id}");
}

/// First uses racing on a fresh home, in threads and in a second process,
/// all succeed and all return the one id the file holds, leaving no staged
/// file behind.
#[test]
fn racing_first_uses_agree_on_one_host_id() {
    const THREADS: usize = 16;
    for _ in 0..20 {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--ignored",
                "--exact",
                RACE_CHILD,
                "--nocapture",
                "--test-threads=1",
            ])
            .env(RACE_HOME, &home)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let start = Arc::new(Barrier::new(THREADS + 1));
        let racers: Vec<_> = (0..THREADS)
            .map(|_| {
                let (start, home) = (Arc::clone(&start), home.clone());
                std::thread::spawn(move || {
                    start.wait();
                    host_id(&PlakPaths::explicit(home))
                })
            })
            .collect();
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join("go"), "").unwrap();
        start.wait();
        let mut ids: Vec<String> = racers
            .into_iter()
            .map(|racer| racer.join().unwrap().expect("no first use is refused"))
            .collect();
        let output = child.wait_with_output().unwrap();
        let printed = String::from_utf8(output.stdout).unwrap();
        let complaint = format!("{printed}{}", String::from_utf8_lossy(&output.stderr));
        assert!(output.status.success(), "{complaint}");
        ids.push(
            printed
                .split("host-id=")
                .nth(1)
                .and_then(|rest| rest.split_whitespace().next())
                .unwrap_or_else(|| panic!("the second process printed no id: {complaint}"))
                .to_owned(),
        );
        let file = std::fs::read_to_string(home.join("host-id")).unwrap();
        assert!(ids.iter().all(|id| *id == file.trim()), "{ids:?} != {file}");
        let staged: Vec<_> = std::fs::read_dir(&home)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .filter(|name| name.to_string_lossy().starts_with(".host-id"))
            .collect();
        assert!(staged.is_empty(), "{staged:?}");
    }
}
