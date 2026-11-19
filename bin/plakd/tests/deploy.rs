//! The compiled operator and its stdin protocol, against a local SSH substitute.
#![allow(clippy::unwrap_used, reason = "local disposable integration fixtures")]

use std::fmt::Write as _;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Write as _};
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};

const NAMES: [&str; 3] = ["plakd", "plakous-js-host", "plakous-py-host"];
const PLATFORM: &str = "x86_64-unknown-linux-musl";
const KEY: &str = "r28.e39.p39.t11";
const NO_EVENTS: [String; 0] = [];

fn hash(data: impl AsRef<[u8]>) -> String {
    let mut hex = String::with_capacity(64);
    for byte in Sha256::digest(data) {
        write!(hex, "{byte:02x}").unwrap();
    }
    hex
}

fn encoded(data: &Value) -> Vec<u8> {
    let mut data = data.clone();
    data.sort_all_objects();
    let mut bytes = serde_json::to_vec(&data).unwrap();
    bytes.push(b'\n');
    bytes
}

fn read(path: &Path) -> Value {
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

fn write(path: &Path, data: &Value) {
    fs::write(path, encoded(data)).unwrap();
}

fn wire() -> Value {
    json!({"number": 39, "schema_pin": "d".repeat(64)})
}

fn upgrade() -> Value {
    json!({"coordinator":8, "receipt":5})
}

fn file_facts(bytes: &[u8]) -> Value {
    let digest = hash(bytes);
    json!({"length":bytes.len(), "sha256":digest, "display_id":&digest[..12]})
}

fn artifacts(root: &Path) {
    let root = root.join("host");
    fs::create_dir(&root).unwrap();
    let mut files = serde_json::Map::new();
    let mut facts = String::new();
    for name in NAMES {
        fs::write(root.join(name), name).unwrap();
        let file = file_facts(name.as_bytes());
        writeln!(facts, "{name} {}", file["sha256"].as_str().unwrap()).unwrap();
        files.insert(name.into(), file);
    }
    let revision = "a".repeat(40);
    write(
        &root.join("manifest.json"),
        &json!({
            "schema":1, "revision":revision, "platform":PLATFORM, "flavor":"release",
            "product_version":"0.0.4", "lock_sha256":"e".repeat(64),
            "toolchain":{
                "cargo":{"release":"1.100.0","commit_hash":"b".repeat(9)},
                "rustc":{"release":"1.100.0","commit_hash":"c".repeat(40),"host":PLATFORM,"llvm_version":"22.1.0"}
            },
            "files":files, "snapshot_id":hash(facts),
            "build_info":{
                "revision":revision, "state_key":KEY, "flavor":"release", "test_hooks":false,
                "product_version":"0.0.4", "omitted_features":[], "wire":wire(),
                "export":{"read":[7,8],"write":8}, "host_upgrade":upgrade(),
                "gateway":{"served":[1,2],"spoken":[1,2]},
                "runner":{"protocol":9,"retention":{"served":[1,2],"spoken":[1,2]}}
            }
        }),
    );
}

struct Fleet {
    temp: tempfile::TempDir,
}

impl Fleet {
    fn new() -> Self {
        let fleet = Self {
            temp: tempfile::tempdir().unwrap(),
        };
        artifacts(fleet.temp.path());
        let data = b"exact-match client fixture\n";
        fs::write(fleet.path("client"), data).unwrap();
        write(
            &fleet.path("client.json"),
            &json!({
                "schema":1, "platform":PLATFORM, "wire":wire(),
                "file":{"length":data.len(), "sha256":hash(data)}
            }),
        );
        for role in ["git", "ssh"] {
            let path = fleet.path(role);
            fs::write(
                &path,
                format!(
                    r#"#!/bin/sh
set -eu
printf '%s\n' "$@" >"$DEPLOY_FIXTURE_DIR/argv"
DEPLOY_FIXTURE_ROLE={role} "$DEPLOY_FIXTURE_EXE" --exact deploy_fixture_process --nocapture >&2
cat "$DEPLOY_FIXTURE_DIR/reply"
exit "$(cat "$DEPLOY_FIXTURE_DIR/exit")"
"#
                ),
            )
            .unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
        }
        fleet.configure(&"a".repeat(40), &["grok"]);
        fleet.reset();
        fleet
    }

    fn path(&self, name: &str) -> PathBuf {
        self.temp.path().join(name)
    }

    fn configure(&self, revision: &str, accounts: &[&str]) {
        let root = self.temp.path().display();
        let host_pin = hash(fs::read(self.path("host/manifest.json")).unwrap());
        let client_pin = hash(fs::read(self.path("client.json")).unwrap());
        let source = "1".repeat(64);
        let gateway = "2".repeat(64);
        let mut text = format!(
            r#"
schema = 1
repo = "{root}"
revision = "{revision}"
artifact = "{root}/host"
manifest_sha256 = "{host_pin}"
"#
        );
        for stage in ["staging", "production"] {
            write!(
                text,
                r#"
[{stage}]
drain_seconds = 60
evidence = "{root}/{stage}.jsonl"
receipt = "{root}/receipt.json"
[{stage}.host]
product_home = "/srv/example/.plak"
source_sha256 = "{source}"
web_socket = "/srv/example/host.sock"
ui = "/srv/example/ui"
gateway_unit = "example-gateway.service"
gateway_socket = "/srv/example/gateway.sock"
gateway_sha256 = "{gateway}"
[{stage}.host.node]
ssh = "example-{stage}"
login = "example"
home = "/srv/example"
uid = 1000
[[{stage}.clients]]
id = "example"
link = "/srv/example/.local/bin/example-client"
blob = "{root}/client"
manifest = "{root}/client.json"
manifest_sha256 = "{client_pin}"
source_sha256 = "{source}"
[{stage}.clients.node]
ssh = "example-client"
login = "example"
home = "/srv/example"
uid = 1000
"#
            )
            .unwrap();
            for account in accounts {
                write!(
                    text,
                    "\n[[{stage}.routes]]\naccount = {account:?}\nmodel = \"example-model\"\neffort = \"low\"\n"
                )
                .unwrap();
            }
        }
        fs::write(self.path("inventory.toml"), text).unwrap();
    }

    fn reset(&self) {
        let source = "1".repeat(64);
        write(
            &self.path("fleet.json"),
            &json!({
                "client":"source", "events":[],
                "host":{
                    "sha256":source, "link_sha256":source, "instance":"100-0000000000000000",
                    "link_sidecars":{(NAMES[1]):source,(NAMES[2]):source},
                    "unit_sha256":source, "custody_sha256":"2".repeat(64),
                    "gateway_instance":"unchanged", "gateway_versions":[1,2],
                    "stage_identity":{"machine_sha256":"0".repeat(64),"product_home":"/srv/example/.plak"},
                    "paired":false, "canary":["claude","grok"],
                    "accounts":{"grok":"ready","openai":"not_enrolled"}, "upgrade_idle":true,
                    "production_history":false,
                    "facts":{
                        "source":"build_info", "binary_id":&source[..12], "state_key":KEY,
                        "host_upgrade":upgrade(), "wire":wire(), "export":{"read":[7,8],"write":8},
                        "runner_triple":{"protocol":9,"source_revision":"b".repeat(40),"flavor":"release"}
                    }
                }
            }),
        );
    }

    fn change(&self, mutate: impl FnOnce(&mut Value)) {
        let path = self.path("fleet.json");
        let mut data = read(&path);
        mutate(&mut data);
        write(&path, &data);
    }

    fn events(&self) -> Vec<String> {
        serde_json::from_value(read(&self.path("fleet.json"))["events"].clone()).unwrap()
    }

    fn deploy(&self, verb: &str, stage: &str, result: &str, reason: &str) -> Value {
        let mut search = vec![self.temp.path().to_path_buf()];
        search.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap()));
        let output = Command::new(env!("CARGO_BIN_EXE_plakd"))
            .args(["deploy", verb, "--stage", stage, "--inventory"])
            .arg(self.path("inventory.toml"))
            .env("DEPLOY_FIXTURE_DIR", self.temp.path())
            .env("DEPLOY_FIXTURE_EXE", std::env::current_exe().unwrap())
            .env("PATH", std::env::join_paths(search).unwrap())
            .output()
            .unwrap();
        let report: Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|_| {
            panic!(
                "{}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            )
        });
        assert_eq!(report["result"], result, "{report}");
        assert_eq!(
            output.status.success(),
            ["ready", "green", "no_op", "withdrawn"].contains(&result),
            "{report}"
        );
        if !reason.is_empty() {
            assert!(
                report["reason"].as_str().unwrap().contains(reason),
                "{report}"
            );
        }
        report
    }

    fn staging(&self, verb: &str, result: &str, reason: &str) -> Value {
        self.deploy(verb, "staging", result, reason)
    }
}

#[test]
fn deploy_fake_fleet_refuses_without_mutation() {
    let f = Fleet::new();
    f.configure(&"f".repeat(40), &["grok"]);
    f.staging("plan", "blocked", "not on origin/main");
    f.configure(&"a".repeat(40), &["grok"]);
    for (pointer, value, reason) in [
        (
            "/host/facts/state_key",
            json!("r29.e39.p39.t11"),
            "cross-key",
        ),
        ("/host/paired", json!(true), "paired-runner triple change"),
        ("/host/gateway_versions", json!([1]), "gateway protocol v2"),
        (
            "/host/production_history",
            json!(true),
            "production history",
        ),
        (
            "/host/accounts",
            json!({"grok":"ready","claude/work":"ready"}),
            "missing from inventory",
        ),
        (
            "/host/facts/runner_triple/source_revision",
            json!("c".repeat(40)),
            "downgrade",
        ),
    ] {
        f.reset();
        f.change(|d| *d.pointer_mut(pointer).unwrap() = value);
        f.staging("plan", "blocked", reason);
        assert!(!f.path("staging.jsonl").exists());
        assert_eq!(f.events(), NO_EVENTS);
    }
    for error in [
        "gateway image change: use S3",
        "custody change: use the custody runbook",
    ] {
        f.reset();
        f.change(|d| d["refusal"] = json!(error));
        f.staging("plan", "blocked", error);
    }
    f.reset();
    f.change(|d| d["host"]["canary"] = Value::Null);
    for verb in ["plan", "apply"] {
        f.staging(
            verb,
            "bootstrap_required",
            "existing manual same-key procedure",
        );
    }
    f.reset();
    f.change(|d| d["legacy"] = json!(true));
    let plan = f.staging(
        "plan",
        "bootstrap_required",
        "existing manual same-key procedure",
    );
    assert_eq!(plan["legacy_facts"]["source"], "legacy facts");
    assert_eq!(plan["legacy_facts"]["state_key"], KEY);
    assert_eq!(f.events(), NO_EVENTS);
    f.reset();
    let plan = f.staging("plan", "ready", "");
    assert_eq!(plan["observed"]["accounts"]["openai"], "not_enrolled");
    assert_eq!(
        plan["plan"],
        json!([
            "baseline",
            "stage_host",
            "stage_clients",
            "host_upgrade",
            "host_link",
            "client_links",
            "verify"
        ])
    );
    assert_eq!(f.events(), NO_EVENTS);
    fs::write(f.path("host/plakd"), b"PLAKD").unwrap();
    f.staging("plan", "blocked", "artifact digest mismatch");
    fs::write(f.path("host/plakd"), b"plakd").unwrap();
    let client_path = f.path("client.json");
    let mut client = read(&client_path);
    client["wire"]["number"] = json!(40);
    write(&client_path, &client);
    f.configure(&"a".repeat(40), &["grok"]);
    f.staging("plan", "blocked", "client wire/schema pin differs");
    assert_eq!(f.events(), NO_EVENTS);
}

#[test]
fn deploy_preflight_and_evidence_fail_closed() {
    let f = Fleet::new();
    for (fault, reason) in [
        ("canary_fail", "canary failed"),
        ("generation_regression", "generation regressed"),
    ] {
        f.reset();
        f.change(|d| d[fault] = json!(true));
        f.staging("apply", "blocked", reason);
        assert_eq!(f.events(), ["canary_preflight", "health", "canary"]);
    }
    let evidence = f.path("staging.jsonl");
    let lock = File::open(&evidence).unwrap();
    lock.lock().unwrap();
    f.staging("apply", "blocked", "holds the evidence log");
    drop(lock);
    fs::OpenOptions::new()
        .append(true)
        .open(&evidence)
        .unwrap()
        .write_all(b"{")
        .unwrap();
    let truncated = fs::read(&evidence).unwrap();
    f.staging(
        "apply",
        "needs_reconcile",
        "incomplete or oversized evidence log",
    );
    assert_eq!(fs::read(&evidence).unwrap(), truncated);
}

#[test]
fn deploy_fake_fleet_plans_and_converges() {
    let f = Fleet::new();
    let green = f.staging("apply", "green", "");
    let receipt = f.path("receipt.json");
    assert_eq!(read(&receipt), green);
    assert_eq!(
        f.events(),
        [
            "canary_preflight",
            "health",
            "canary",
            "stage_host",
            "client_stage",
            "destination_unit",
            "destination_canary_preflight",
            "upgrade",
            "host_link",
            "client_link",
            "client_check",
            "health",
            "canary"
        ]
    );
    let before = fs::read(&receipt).unwrap();
    f.staging("apply", "no_op", "");
    assert!(
        f.events()
            .ends_with(&["client_check".into(), "health".into()])
    );
    assert_eq!(fs::read(&receipt).unwrap(), before);
    production(&f, &green);
}

fn production(f: &Fleet, green: &Value) {
    let receipt = f.path("receipt.json");
    f.reset();
    fs::remove_file(&receipt).unwrap();
    f.deploy(
        "apply",
        "production",
        "blocked",
        "fresh green staging receipt",
    );
    File::create(&receipt).unwrap();
    fs::set_permissions(&receipt, fs::Permissions::from_mode(0o600)).unwrap();
    for (pointer, value) in [
        ("/manifests/host", json!("1".repeat(64))),
        ("/completed_at", json!(1)),
        ("/verification/canaries", json!({"claude":"bounded_cli_v1"})),
        ("/source_transition/runner_protocol", json!(10)),
        ("/canary_models/0/1", json!("other-model")),
        ("/stage_identity/machine_sha256", json!("9".repeat(64))),
    ] {
        let mut invalid = green.clone();
        *invalid.pointer_mut(pointer).unwrap() = value;
        write(&receipt, &invalid);
        f.deploy(
            "apply",
            "production",
            "blocked",
            "fresh green staging receipt",
        );
    }
    assert_eq!(f.events(), NO_EVENTS);
    write(&receipt, green);
    f.deploy("apply", "production", "green", "");
    let mut expired = green.clone();
    expired["completed_at"] = json!(1);
    write(&receipt, &expired);
    assert_eq!(
        f.deploy("apply", "production", "no_op", "")["plan"],
        json!(["verify_read_only"])
    );
}

#[test]
fn deploy_canaries_are_keyed_by_exact_account() {
    let f = Fleet::new();
    f.change(|d| {
        d["host"]["accounts"] =
            json!({"claude/work":"ready","claude/other":"ready","grok":"not_enrolled"});
    });
    f.configure(&"a".repeat(40), &["claude/work", "claude/other"]);
    let report = f.staging("apply", "green", "");
    for phase in ["baseline", "verification"] {
        assert_eq!(
            report[phase]["canaries"],
            json!({"claude/work":"bounded_cli_v1","claude/other":"bounded_cli_v1"})
        );
        assert_eq!(
            report[phase]["generations"],
            json!({"claude/work":6,"claude/other":6})
        );
    }
    assert_eq!(f.events().iter().filter(|x| *x == "canary").count(), 4);
}

#[test]
fn deploy_requester_loss_does_not_repeat_surrender() {
    let f = Fleet::new();
    f.change(|d| d["lose_reply"] = json!("upgrade"));
    let report = f.staging("apply", "needs_reconcile", "use deploy status");
    assert_eq!(report["step"], "host_upgrade");
    assert!(report["retry"].as_str().unwrap().contains("deploy status"));
    assert!(!f.path("receipt.json").exists());
    assert!(
        !f.staging("plan", "ready", "")["plan"]
            .as_array()
            .unwrap()
            .contains(&json!("host_upgrade"))
    );
    f.staging("apply", "green", "");
    assert_eq!(f.events().iter().filter(|x| *x == "upgrade").count(), 1);
    assert_eq!(f.events().iter().filter(|x| *x == "canary").count(), 2);
    for verb in ["status", "apply"] {
        let f = Fleet::new();
        f.change(|d| d["lose_reply"] = json!("old"));
        f.staging("apply", "needs_reconcile", "");
        f.staging("status", "needs_reconcile", "");
        f.staging("apply", "needs_reconcile", "");
        assert_eq!(f.events().iter().filter(|x| *x == "upgrade").count(), 1);
        f.change(|d| d["host"]["upgrade_idle"] = json!(true));
        f.staging("plan", "needs_reconcile", "");
        let want = if verb == "status" {
            "ready"
        } else {
            "withdrawn"
        };
        assert_eq!(f.staging(verb, want, "")["withdrawn"], true);
        let evidence = fs::read_to_string(f.path("staging.jsonl")).unwrap();
        let last: Value = serde_json::from_str(evidence.lines().last().unwrap()).unwrap();
        assert_eq!(last["phase"], "withdrawn");
        assert_eq!(f.events().iter().filter(|x| *x == "upgrade").count(), 1);
    }
    for phase in ["host_link", "client_link"] {
        let f = Fleet::new();
        f.change(|d| d["lose_reply"] = json!(phase));
        f.staging("apply", "needs_reconcile", "link outcome");
        assert!(
            !f.staging("status", "ready", "")["plan"]
                .as_array()
                .unwrap()
                .contains(&json!("host_upgrade"))
        );
        f.staging("apply", "green", "");
        for event in ["upgrade", phase] {
            assert_eq!(f.events().iter().filter(|x| *x == event).count(), 1);
        }
    }
}

#[test]
fn deploy_fixture_process() {
    let Some(role) = std::env::var_os("DEPLOY_FIXTURE_ROLE") else {
        return;
    };
    let root = PathBuf::from(std::env::var_os("DEPLOY_FIXTURE_DIR").unwrap());
    let raw = fs::read_to_string(root.join("argv")).unwrap();
    let args: Vec<_> = raw.lines().collect();
    let (reply, code) = if role == "git" {
        fake_git(&args)
    } else {
        fake_ssh(&root, &args, &mut BufReader::new(std::io::stdin().lock()))
    };
    fs::write(root.join("reply"), reply).unwrap();
    fs::write(root.join("exit"), code.to_string()).unwrap();
}

fn fake_git(args: &[&str]) -> (Vec<u8>, i32) {
    assert_eq!(args[0], "-C");
    let revision = "a".repeat(40);
    let (out, status) = match args[2] {
        "ls-remote" => {
            assert_eq!(args[3..], ["--exit-code", "origin", "refs/heads/main"]);
            (format!("{revision}\trefs/heads/main\n"), 0)
        }
        "rev-list" => (format!("{revision}\n{}\n", "b".repeat(40)), 0),
        "merge-base" => {
            assert_eq!(args[3], "--is-ancestor");
            (String::new(), i32::from(args[4] != "b".repeat(40)))
        }
        other => panic!("unexpected git: {other}"),
    };
    (out.into_bytes(), status)
}

fn next(input: &mut impl BufRead) -> String {
    let mut line = String::new();
    input.read_line(&mut line).unwrap();
    line.strip_suffix('\n').unwrap().to_owned()
}

fn fake_ssh(root: &Path, args: &[&str], input: &mut impl BufRead) -> (Vec<u8>, i32) {
    for setting in [
        "-oStrictHostKeyChecking=yes",
        "-oForwardAgent=no",
        "-oControlMaster=no",
        "-oBatchMode=yes",
        "/dev/null",
    ] {
        assert!(args.contains(&setting), "missing {setting}");
    }
    let script = args
        .iter()
        .skip_while(|row| !row.starts_with("sh -c"))
        .copied()
        .collect::<Vec<_>>()
        .join("\n");
    let path = root.join("fleet.json");
    let mut data = read(&path);
    if script.contains("--host-upgrade-probe") {
        assert_eq!(data["legacy"], true);
        assert_eq!(next(input), "/srv/example");
        assert_eq!(next(input), "1000");
        assert_eq!(next(input), "/srv/example/.plak");
        assert_eq!(next(input), "example");
        let mut probes = encoded(&json!({
            "protocol":8,"receipt_version":5,"binary_id":"111111111111","state_key":KEY,
            "export_versions":[7,8],"host_available":true,"runner_protocol":9,
            "runner_version":{"protocol":9,"source_revision":"b".repeat(40),"flavor":"release"},
            "config_error":null
        }));
        probes.extend_from_slice(format!("{KEY}\n8\n").as_bytes());
        probes.extend_from_slice(&encoded(
            &json!({"wire_version":39,"binary_id":"111111111111"}),
        ));
        return (probes, 0);
    }
    if script.contains("test -L") {
        let current = data["client"] == "current";
        let digest = if current {
            hash(fs::read(root.join("client")).unwrap())
        } else {
            "1".repeat(64)
        };
        let resolved = if current {
            format!("/srv/example/.local/lib/plak/clients/{digest}/client")
        } else {
            "/srv/example/old-client".into()
        };
        assert_eq!(next(input), "/srv/example");
        assert_eq!(next(input), "1000");
        assert_eq!(next(input), "/srv/example/.local/bin/example-client");
        assert_eq!(next(input), "example");
        return (format!("{digest}\n{resolved}\n").into_bytes(), 0);
    }
    let transferred = script.contains("sha256sum -c");
    if transferred {
        let mut binary = vec![0; next(input).parse().unwrap()];
        let pin = next(input);
        input.read_exact(&mut binary).unwrap();
        assert_eq!(hash(binary), pin);
    } else {
        assert!(next(input).ends_with("/plakd"));
    }
    let call: Value = serde_json::from_str(&next(input)).unwrap();
    let op = &call["call"]["request"]["op"];
    let mut name = op["op"].as_str().unwrap().to_owned();
    let mut remaining = Vec::new();
    input.read_to_end(&mut remaining).unwrap();
    if call["call"]["role"] == "client" {
        name = format!("client_{name}");
    }
    if name == "observe" {
        if data["legacy"] == true {
            return (Vec::new(), 2);
        }
        if let Some(reason) = data["refusal"].as_str() {
            return (encoded(&json!({"error":reason})), 0);
        }
        let mut observed = data["host"].clone();
        if call["stage"] == "production" {
            observed["stage_identity"]["machine_sha256"] = json!("9".repeat(64));
        }
        return (encoded(&json!({"ok":observed})), 0);
    }
    data["events"].as_array_mut().unwrap().push(json!(name));
    let response = simulate(root, &mut data, &name, op, &remaining);
    let lost = data["lose_reply"] == name || name == "upgrade" && data["lose_reply"] == "old";
    write(&path, &data);
    (encoded(&response), if lost { 255 } else { 0 })
}

fn simulate(root: &Path, data: &mut Value, name: &str, op: &Value, blob: &[u8]) -> Value {
    let want = file_facts(b"plakd");
    let value = match name {
        "stage_host" => {
            assert_eq!(
                hash(op["manifest"].as_str().unwrap()),
                op["manifest_sha256"]
            );
            assert_eq!(blob, b"plakous-js-hostplakous-py-host");
            data["candidate"] = want["sha256"].clone();
            Value::Null
        }
        "client_stage" => {
            assert_eq!(blob, fs::read(root.join("client")).unwrap());
            assert_eq!(data["candidate"], want["sha256"]);
            data["client"] = json!("staged");
            Value::Null
        }
        "destination_unit" | "destination_canary_preflight" => {
            assert_eq!(op["candidate"]["sha256"], data["candidate"]);
            Value::Null
        }
        "mark_production" => {
            assert_eq!(op["expected"]["machine_sha256"], "9".repeat(64));
            data["host"]["production_history"] = json!(true);
            Value::Null
        }
        "upgrade" => {
            assert_eq!(op["candidate"]["sha256"], data["candidate"]);
            assert_eq!(data["client"], "staged");
            assert_eq!(op["previous"]["instance"], data["host"]["instance"]);
            if data["lose_reply"] == "old" {
                data["host"]["upgrade_idle"] = json!(false);
            } else {
                let host = &mut data["host"];
                host["instance"] = json!("101-0000000000000001");
                host["sha256"] = want["sha256"].clone();
                host["facts"]["binary_id"] = want["display_id"].clone();
                host["facts"]["runner_triple"]["source_revision"] = json!("a".repeat(40));
            }
            Value::Null
        }
        "host_link" => {
            assert_eq!(op["instance"], data["host"]["instance"]);
            data["host"]["link_sha256"] = data["candidate"].clone();
            for name in &NAMES[1..] {
                data["host"]["link_sidecars"][name] = json!(hash(name));
            }
            Value::Null
        }
        "client_link" | "client_check" => {
            assert_eq!(op["connection"]["wire"], wire());
            assert_eq!(data["host"]["link_sha256"], data["candidate"]);
            if name == "client_link" {
                data["client"] = json!("current");
            }
            assert_eq!(data["client"], "current");
            Value::Null
        }
        "health" => {
            assert_eq!(op["expected"], data["host"]["sha256"]);
            let generations = op["routes"]
                .as_array()
                .unwrap()
                .iter()
                .map(|route| (route["account"].as_str().unwrap().to_owned(), json!(5)))
                .collect::<serde_json::Map<_, _>>();
            json!({"doctor":"green","status":"ready","bindings":"compatible","binding_count":10_000_000,"generations":generations})
        }
        "canary_preflight" => {
            assert_eq!(op["expected"], data["host"]["sha256"]);
            Value::Null
        }
        "canary" => {
            assert_eq!(op["expected"], data["host"]["sha256"]);
            if data["canary_fail"] == true {
                return json!({"error":"canary failed"});
            }
            let generation = if data["generation_regression"] == true {
                4
            } else {
                6
            };
            json!({"adapter":"bounded_cli_v1","generation_before":5,"generation_after":generation})
        }
        other => panic!("unexpected SSH operation: {other}"),
    };
    json!({"ok":value})
}
