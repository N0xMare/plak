use super::*;
use std::os::unix::fs::PermissionsExt as _;

fn seed_paired(fixture: &Fixture, path: &str, id: &str) -> String {
    let mut document: serde_json::Value = serde_json::from_str(include_str!(
        "../../../fixtures/session-export-v12-default.json"
    ))
    .unwrap();
    let previous: serde_json::Value =
        serde_json::from_str(include_str!("../../../fixtures/session-export-v12.json")).unwrap();
    document["history"] = previous["history"].clone();
    document["source"]["session_id"] = json!(uuid::Uuid::new_v4().to_string());
    document["workspace_root"] = json!(fixture.workspace().canonicalize().unwrap());
    document["runner"] = json!({"name":"remote", "runner_id":id});
    document["profile"] = json!({"root_template":"lead", "working_set":["lead"]});
    document["named_state"] = json!({"paired":true});
    let file = fixture.root.join("paired-seed.json");
    write_private(&file, &document.to_string());
    let imported = output_env(
        fixture,
        &["session", "import", file.to_str().unwrap()],
        false,
        &[("PATH", path)],
    );
    assert!(imported.status.success(), "{imported:?}");
    String::from_utf8(imported.stdout)
        .unwrap()
        .lines()
        .find_map(|line| line.strip_prefix("resume with: plakd run --session "))
        .unwrap()
        .to_owned()
}

#[test]
fn offline_paired_export_uses_the_recorded_workspace_from_a_full_id() {
    let Some(fixture) = fixture() else { return };
    let (daemon, path, id) = runner::remote_fixture(&fixture);
    let session = seed_paired(&fixture, &path, &id);
    drop(daemon);
    let other = fixture.root.join("operator-workspace");
    std::fs::create_dir(&other).unwrap();
    let file = fixture.root.join("paired-export.json");
    let export = |id: &str| {
        fixture
            .command()
            .current_dir(&other)
            .args(["session", "export", id, "--out", file.to_str().unwrap()])
            .output()
            .unwrap()
    };
    let refused = export(&session[..8]);
    assert!(String::from_utf8_lossy(&refused.stderr).contains("no session in this workspace"));
    assert!(!file.exists());
    let exported = export(&session);
    assert!(exported.status.success(), "{exported:?}");
    let document: serde_json::Value =
        serde_json::from_slice(&std::fs::read(file).unwrap()).unwrap();
    assert_eq!(
        document["workspace_root"],
        json!(fixture.workspace().canonicalize().unwrap())
    );
    assert_eq!(document["runner"]["runner_id"], id);
    assert_eq!(document["named_state"], json!({"paired":true}));
}

#[test]
fn same_key_paired_runner_mismatch_refuses_before_drain() {
    let Some(fixture) = fixture() else { return };
    let (_daemon, path, id) = runner::remote_fixture(&fixture);
    fixture.write_script(&json!({"version":1,"steps":[{"output":{"text":ANSWER}}]}));
    let session = seed_paired(&fixture, &path, &id);
    let mut host = start_env(&fixture, &[], &[("PATH", path.clone())]);
    let mut client = Client::connect(&host);
    runner::remote_ready(&mut client, &id);
    let row = client
        .rows(1)
        .into_iter()
        .find(|row| row.session == session)
        .unwrap();
    let attachment = client.attach(2, &row.node);
    let stub = fixture.root.join("different-snapshot");
    let hello = json!({"kind":"hello", "version":{"protocol":plakous::runner::protocol::PROTOCOL_VERSION,"source_revision":"different-revision","flavor":"different-flavor"}, "binary_id":null,"runner_id":id,"instance":"other","boot_id":"boot","machine_id":"machine","descriptor":{},"capabilities":[],"path":null,"home":null,"job_bound":8});
    std::fs::write(&stub, format!("#!/usr/bin/env python3\nimport sys\ndata={hello:?}.encode()\nsys.stdout.buffer.write(len(data).to_bytes(4,'big')+data)\nsys.stdout.buffer.flush()\nsys.stdin.buffer.read()\n", hello=hello.to_string())).unwrap();
    std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o700)).unwrap();
    let config = fixture.home().join("config.toml");
    let original = std::fs::read_to_string(&config).unwrap();
    std::fs::write(
        &config,
        original.replace(
            &format!("command = {:?}", env!("CARGO_BIN_EXE_plakd")),
            &format!("command = {stub:?}"),
        ),
    )
    .unwrap();
    let refused = reopen::same_key_upgrade(&fixture, &path);
    let detail = String::from_utf8_lossy(&refused.stderr);
    assert!(!refused.status.success(), "{detail}");
    assert!(
        detail.contains("host_upgrade_runner_qualification"),
        "{detail}"
    );
    assert!(
        detail.contains("matching snapshot") && detail.contains("VersionMismatch"),
        "{detail}"
    );
    assert!(
        !detail.contains("resolving earlier admission verdicts"),
        "{detail}"
    );
    std::fs::write(config, original).unwrap();
    assert_eq!(
        client.request(3, Op::Detach { attachment }),
        Outcome::Ok(Answer::Detached)
    );
    stop(&mut host);
}
