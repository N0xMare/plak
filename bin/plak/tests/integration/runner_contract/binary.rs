use super::*;

struct Daemon(std::process::Child);

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn install(home: &Path) -> (PathBuf, String) {
    let snapshot = home.join(".local/lib/plak/release").join("a".repeat(64));
    std::fs::create_dir_all(&snapshot).unwrap();
    let image = snapshot.join("plak");
    std::fs::copy(plak_bin(), &image).unwrap();
    std::fs::set_permissions(&image, std::fs::Permissions::from_mode(0o555)).unwrap();
    let output = Command::new(&image)
        .args(["version", "--write-binary-manifest"])
        .arg(&snapshot)
        .env("HOME", home)
        .env("PLAK_HOME", home)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let manifest = std::fs::read_to_string(snapshot.join("binary-id")).unwrap();
    (
        image,
        manifest.split_whitespace().next().unwrap().to_owned(),
    )
}

async fn ended(child: &mut tokio::process::Child) {
    let _ = child.start_kill();
    child.wait().await.unwrap();
}

#[test]
fn runner_binary_exec_preserves_exact_native_triples_and_pairing_admission() {
    let (guard, home) = isolated_home();
    let (image, binary) = install(&home);
    let work = guard.path().join("work");
    std::fs::create_dir(&work).unwrap();
    std::fs::create_dir(home.join("runner")).unwrap();
    let host = uuid::Uuid::new_v4().to_string();
    std::fs::write(
        home.join("runner/runner.toml"),
        format!("roots = [{work:?}]\npath = \"/usr/bin:/bin\"\nhosts = [\"{host}\"]\n"),
    )
    .unwrap();
    let _daemon = Daemon(
        Command::new(&image)
            .args(["runner", "serve"])
            .env("HOME", &home)
            .env("PLAK_HOME", &home)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(20);
    while !sockets_ready(&home) {
        assert!(Instant::now() < deadline, "selected daemon never listened");
        std::thread::sleep(Duration::from_millis(10));
    }
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
        let (mismatch, mut first) = selected_bridge(
            &home,
            &host,
            VersionTriple::current("other", "other"),
            Some(&binary),
        )
        .await;
        let Err(ConnectError::VersionMismatch { runner }) = mismatch else {
            panic!("selector relaxed native triple admission");
        };
        ended(&mut first).await;
        for field in ["protocol", "revision", "flavor"] {
            let mut wrong = runner.clone();
            match field {
                "protocol" => wrong.protocol += 1,
                "revision" => wrong.source_revision.push_str("-other"),
                _ => wrong.flavor.push_str("-other"),
            }
            let (refused, mut child) = selected_bridge(&home, &host, wrong, Some(&binary)).await;
            assert!(matches!(refused, Err(ConnectError::VersionMismatch { .. })));
            ended(&mut child).await;
        }
        let (paired, mut child) =
            selected_bridge(&home, &host, runner.clone(), Some(&binary)).await;
        let paired = paired.unwrap();
        assert_eq!(paired.lines().await.unwrap(), []);
        let process = Command::new("ps")
            .args([
                "-ww",
                "-o",
                "command=",
                "-p",
                &child.id().unwrap().to_string(),
            ])
            .output()
            .unwrap();
        let invocation = String::from_utf8_lossy(&process.stdout);
        assert!(invocation.contains(image.to_str().unwrap()), "{invocation}");
        assert!(
            !invocation.contains("--binary"),
            "selector recursed: {invocation}"
        );
        drop(paired);
        ended(&mut child).await;
        let stranger = uuid::Uuid::new_v4().to_string();
        let (client, mut child) = selected_bridge(&home, &stranger, runner, Some(&binary)).await;
        let client = client.unwrap();
        assert!(client.lines().await.is_err());
        assert!(matches!(client.refusal(), Some(Refusal::NotPaired { .. })));
        ended(&mut child).await;
    });
}

fn sockets_ready(home: &Path) -> bool {
    std::fs::read_dir(home.join("runner/builds")).is_ok_and(|builds| {
        builds.flatten().any(|build| {
            std::fs::read_dir(build.path().join("run")).is_ok_and(|entries| {
                entries
                    .flatten()
                    .any(|entry| entry.path().extension().is_some_and(|ext| ext == "sock"))
            })
        })
    })
}

#[test]
fn runner_binary_missing_or_invalid_refuses_without_runner_or_product_state() {
    let (_guard, home) = isolated_home();
    for binary in ["000000000000", "../plak", "ABCDEF012345"] {
        let output = plak_command(&home)
            .args(["runner", "connect", "--stdio", "--binary", binary])
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert_eq!(output.stdout, [] as [u8; 0]);
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(
            error.contains("runner_binary_unavailable") && error.contains("install"),
            "{error}"
        );
        assert!(!home.join("runner").exists());
        assert!(!home.join("state").exists());
    }
    let missing_stdio = plak_command(&home)
        .args(["runner", "connect", "--binary", "000000000000"])
        .output()
        .unwrap();
    assert!(!missing_stdio.status.success());
    assert!(!home.join("runner").exists());
}
