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
const MAC: &str = "aarch64-apple-darwin";
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

fn artifacts(root: &Path, platform: &str) {
    fs::create_dir(root).unwrap();
    let mut files = serde_json::Map::new();
    let mut facts = String::new();
    for name in NAMES {
        let bytes = artifact_bytes(name, platform);
        fs::write(root.join(name), &bytes).unwrap();
        let file = file_facts(&bytes);
        writeln!(facts, "{name} {}", file["sha256"].as_str().unwrap()).unwrap();
        files.insert(name.into(), file);
    }
    let revision = "a".repeat(40);
    write(
        &root.join("manifest.json"),
        &json!({
            "schema":1, "revision":revision, "platform":platform, "flavor":"release",
            "product_version":"0.0.4", "lock_sha256":"e".repeat(64),
            "toolchain":{
                "cargo":{"release":"1.100.0","commit_hash":"b".repeat(9)},
                "rustc":{"release":"1.100.0","commit_hash":"c".repeat(40),"host":platform,"llvm_version":"22.1.0"}
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

fn artifact_bytes(name: &str, platform: &str) -> Vec<u8> {
    if platform == MAC {
        format!("mac-{name}").into_bytes()
    } else {
        name.as_bytes().to_vec()
    }
}

struct Fleet {
    temp: tempfile::TempDir,
}

impl Fleet {
    fn new() -> Self {
        let fleet = Self {
            temp: tempfile::tempdir().unwrap(),
        };
        artifacts(&fleet.path("host"), PLATFORM);
        artifacts(&fleet.path("mac"), MAC);
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
        self.enroll(revision, accounts, false);
    }

    fn enroll(&self, revision: &str, accounts: &[&str], paired: bool) {
        let root = self.temp.path().display();
        let host_pin = hash(fs::read(self.path("host/manifest.json")).unwrap());
        let mac_pin = hash(fs::read(self.path("mac/manifest.json")).unwrap());
        let client_pin = hash(fs::read(self.path("client.json")).unwrap());
        let source = "1".repeat(64);
        let gateway = "2".repeat(64);
        let mut text = format!(
            r#"
schema = 2
repo = "{root}"
revision = "{revision}"
[manifests.{PLATFORM}]
artifact = "{root}/host"
manifest_sha256 = "{host_pin}"
[manifests.{MAC}]
artifact = "{root}/mac"
manifest_sha256 = "{mac_pin}"
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
[[{stage}.machines]]
[{stage}.machines.host]
product_home = "/srv/example/.plak"
source_sha256 = "{source}"
web_socket = "/srv/example/host.sock"
ui = "/srv/example/ui"
[{stage}.machines.gateway]
service = "example-gateway.service"
socket = "/srv/example/gateway.sock"
sha256 = "{gateway}"
[{stage}.machines.node]
ssh = "example-{stage}"
login = "example"
home = "/srv/example"
uid = 1000
platform = "{PLATFORM}"
[[{stage}.machines]]
[{stage}.machines.node]
ssh = "example-client-{stage}"
login = "example"
home = "/srv/example"
uid = 1000
platform = "{PLATFORM}"
[[{stage}.machines.clients]]
id = "example"
link = "/srv/example/.local/bin/example-client"
blob = "{root}/client"
manifest = "{root}/client.json"
manifest_sha256 = "{client_pin}"
source_sha256 = "{source}"
"#
            )
            .unwrap();
            if paired {
                let uid = if stage == "staging" { 501 } else { 502 };
                write!(
                    text,
                    r#"
[[{stage}.machines]]
[{stage}.machines.node]
ssh = "example-mac"
login = "runner-{stage}"
home = "/Volumes/example/runner-{stage}"
uid = {uid}
platform = "{MAC}"
[[{stage}.machines.runners]]
id = "desk"
link = "/Volumes/example/runner-{stage}/.local/bin/example-runner"
source_sha256 = "{source}"
"#
                )
                .unwrap();
            }
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

    fn paired(&self) {
        self.enroll(&"a".repeat(40), &["grok"], true);
        self.change(|data| data["paired_fleet"] = json!(true));
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
                    "stage_identity":{"machine_sha256":"0".repeat(64),"platform":PLATFORM,"uid":1000,"home":"/srv/example","product_home":"/srv/example/.plak"},
                    "paired":{}, "canary":["claude","grok"],
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
        fs::write(self.path("invocation"), b"planning").unwrap();
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
        (
            "/host/paired",
            json!({"other":{"ssh":"example@other","command":"/example/link"}}),
            "paired runner placements",
        ),
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
    f.change(|d| {
        d["host"]["canary"] = Value::Null;
        d["host"]["facts"]["source"] = json!("legacy facts");
    });
    for verb in ["plan", "apply"] {
        let plan = f.staging(
            verb,
            "bootstrap_required",
            "existing manual same-key procedure",
        );
        assert_eq!(plan["legacy_facts"]["source"], "legacy facts");
        assert_eq!(plan["legacy_facts"]["state_key"], KEY);
    }
    f.reset();
    f.change(|d| d["transport_loss"] = json!(true));
    f.staging("plan", "blocked", "SSH returned no bounded verdict");
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
    f.change(|d| d["lose_reply"] = json!("upgrade"));
    f.deploy(
        "apply",
        "production",
        "needs_reconcile",
        "host upgrade outcome",
    );
    let mut expired = green.clone();
    expired["completed_at"] = json!(1);
    write(&receipt, &expired);
    f.deploy("apply", "production", "green", "");
    assert_eq!(
        f.events()
            .iter()
            .filter(|event| *event == "mark_production")
            .count(),
        1
    );
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
fn deploy_paired_macos_runner_blocks_live_jobs_and_missing_attachment_before_baseline() {
    let f = Fleet::new();
    f.paired();
    for (flag, remedy) in [
        (
            "live_runner_jobs",
            "settle and drain them using the old build",
        ),
        (
            "missing_runner_attachment",
            "read-only and keep it attached",
        ),
    ] {
        f.change(|data| data[flag] = json!(true));
        let report = f.staging("plan", "blocked", remedy);
        assert_eq!(report["runner_links"]["desk"]["account"]["platform"], MAC);
        assert!(!f.path("staging.jsonl").exists());
        assert_eq!(f.events(), NO_EVENTS);
        f.change(|data| data[flag] = json!(false));
    }
    let plan = f.staging("plan", "ready", "");
    assert_eq!(
        plan["source_transition"]["runners"],
        json!([[MAC, "changed_triple"]])
    );
    assert_eq!(
        plan["plan"],
        json!([
            "baseline",
            "stage_host",
            "stage_clients",
            "stage_runners",
            "runner_links",
            "host_upgrade",
            "host_link",
            "client_links",
            "verify"
        ])
    );
}

#[test]
fn deploy_paired_macos_transition_promotes_between_separate_users() {
    let f = Fleet::new();
    f.paired();
    let receipt = f.staging("apply", "green", "");
    let stage = &receipt["target_accounts"];
    assert!(
        stage
            .as_array()
            .unwrap()
            .iter()
            .any(|account| account["uid"] == 501 && account["machine_sha256"] == "5".repeat(64))
    );
    let events = f.events();
    let event = |name| events.iter().position(|value| value == name).unwrap();
    assert!(event("runner_stage") < event("runner_advance"));
    assert!(event("runner_advance") < event("upgrade"));
    assert!(event("host_link") < event("client_link"));
    let before = fs::read(f.path("receipt.json")).unwrap();
    f.staging("apply", "no_op", "");
    assert_eq!(fs::read(f.path("receipt.json")).unwrap(), before);
    f.reset();
    f.paired();
    let mut invalid = receipt.clone();
    invalid["source_transition"]["runners"] = json!([[MAC, "same_triple"]]);
    write(&f.path("receipt.json"), &invalid);
    f.deploy(
        "apply",
        "production",
        "blocked",
        "fresh green staging receipt",
    );
    for (field, value) in [
        ("uid", json!(502)),
        ("home", json!("/Volumes/example/runner-production")),
    ] {
        let mut invalid = receipt.clone();
        invalid["target_accounts"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .find(|account| account["platform"] == MAC)
            .unwrap()[field] = value;
        write(&f.path("receipt.json"), &invalid);
        f.deploy(
            "apply",
            "production",
            "blocked",
            "fresh green staging receipt",
        );
    }
    write(&f.path("receipt.json"), &receipt);
    f.deploy("apply", "production", "green", "");
}

#[test]
fn deploy_inventory_rejects_alias_conflicts_missing_roles_and_link_collisions() {
    let f = Fleet::new();
    f.paired();
    let path = f.path("inventory.toml");
    let enrolled = fs::read_to_string(&path).unwrap();
    for (input, remedy) in [
        (
            enrolled.replace("ssh = \"example-production\"", "ssh = \"example-staging\""),
            "distinct evidence and logins",
        ),
        (
            enrolled.replace("uid = 502", "uid = 501"),
            "distinct evidence and logins",
        ),
        (
            enrolled.replace(
                "login = \"runner-production\"",
                "login = \"runner-staging\"",
            ),
            "distinct evidence and logins",
        ),
        (
            enrolled
                .replace(
                    "home = \"/Volumes/example/runner-production\"",
                    "home = \"/Volumes/example/runner-staging\"",
                )
                .replace(
                    "link = \"/Volumes/example/runner-production/",
                    "link = \"/Volumes/example/runner-staging/",
                ),
            "distinct evidence and logins",
        ),
        (
            enrolled.replace(
                "ssh = \"example-client-staging\"",
                "ssh = \"example-staging\"",
            ),
            "duplicate machine login",
        ),
        (
            omit_table(&enrolled, "[staging.machines.gateway]"),
            "one host and its co-located gateway",
        ),
        (
            omit_table(&enrolled, &format!("[manifests.{MAC}]")),
            "missing platform manifest",
        ),
        (
            enrolled.replace(
                "link = \"/Volumes/example/runner-staging/.local/bin/example-runner\"",
                "link = \"/Volumes/example/runner-staging/.local/bin/monty\"",
            ),
            "dedicated command link",
        ),
        (
            add_colliding_client(&enrolled),
            "duplicate runner or command link",
        ),
    ] {
        fs::write(&path, input).unwrap();
        f.staging("plan", "blocked", remedy);
    }
    assert_eq!(f.events(), NO_EVENTS);
}

fn omit_table(config: &str, name: &str) -> String {
    let start = config.find(name).unwrap();
    let end = start + config[start..].find("\n[").unwrap() + 1;
    format!("{}{}", &config[..start], &config[end..])
}

fn add_colliding_client(config: &str) -> String {
    let stage_client = config
        .split_once("[[staging.machines.clients]]")
        .unwrap()
        .1
        .split_once("\n[")
        .unwrap()
        .0;
    let copied = stage_client
        .replace("id = \"example\"", "id = \"collision\"")
        .replace(
            "/srv/example/.local/bin/example-client",
            "/Volumes/example/runner-staging/.local/bin/example-runner",
        );
    config.replace(
        "\n[production]",
        &format!("\n[[staging.machines.clients]]{copied}\n[production]"),
    )
}

#[test]
fn deploy_staging_linux_alias_is_rejected_from_local_production_history() {
    let f = Fleet::new();
    let node =
        json!({"machine_sha256":"6".repeat(64),"platform":PLATFORM,"home":"/example","uid":321});
    let evidence = f.path("production.jsonl");
    let receipt = f.path("receipt.json");
    write(&evidence, &json!({"target_accounts":[node]}));
    fs::set_permissions(&evidence, fs::Permissions::from_mode(0o600)).unwrap();
    f.staging("apply", "blocked", "matches local production evidence");
    fs::remove_file(evidence).unwrap();
    write(
        &receipt,
        &json!({"stage":"production", "target_accounts":[node]}),
    );
    fs::set_permissions(&receipt, fs::Permissions::from_mode(0o600)).unwrap();
    f.staging("apply", "blocked", "matches a local production receipt");
    assert_eq!(f.events(), NO_EVENTS);
}

#[test]
fn deploy_runner_transition_uses_the_runner_images_build_triple() {
    let f = Fleet::new();
    f.paired();
    f.change(|d| {
        d["runner_triple"] =
            json!({"protocol":9,"source_revision":"a".repeat(40),"flavor":"release"});
    });
    let same = f.staging("plan", "ready", "");
    assert_eq!(
        same["source_transition"]["runners"],
        json!([[MAC, "same_triple"]])
    );
    f.change(|d| d["runner_triple"]["protocol"] = json!(10));
    let changed = f.staging("plan", "ready", "");
    assert_eq!(
        changed["source_transition"]["runners"],
        json!([[MAC, "changed_triple"]])
    );
}

#[test]
fn deploy_paired_runner_restores_exact_link_on_pre_surrender_refusal() {
    let f = Fleet::new();
    f.paired();
    f.change(|data| data["refuse_upgrade"] = json!(true));
    let report = f.staging(
        "apply",
        "blocked",
        "deploy_runner_inspection: runner desk job 17",
    );
    assert_eq!(report["withdrawn"], true);
    let events = f.events();
    assert!(events.ends_with(&[
        "upgrade".into(),
        "withdraw_upgrade".into(),
        "runner_restore".into()
    ]));
    let facts = read(&f.path("fleet.json"));
    assert_eq!(facts["runner"], "source");
    assert_eq!(facts["host"]["sha256"], "1".repeat(64));
    assert!(!f.path("receipt.json").exists());
    f.change(|data| data["refuse_upgrade"] = json!(false));
    f.staging("apply", "green", "");
    f.staging("apply", "no_op", "");
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
    let f = Fleet::new();
    f.paired();
    f.change(|data| data["lose_reply"] = json!("runner_advance"));
    let report = f.staging("apply", "blocked", "SSH returned no bounded verdict");
    assert_eq!(report["withdrawn"], true);
    assert_eq!(read(&f.path("fleet.json"))["runner"], "source");
    assert!(!f.events().iter().any(|event| event == "upgrade"));
    f.change(|data| data["lose_reply"] = Value::Null);
    f.staging("apply", "green", "");
    assert_eq!(
        f.events().iter().filter(|x| *x == "runner_advance").count(),
        2
    );
    assert_eq!(f.events().iter().filter(|x| *x == "upgrade").count(), 1);
    let evidence = fs::read_to_string(f.path("staging.jsonl")).unwrap();
    let phases: Vec<_> = evidence
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter_map(|row| row.get("cohort").cloned())
        .collect();
    assert_eq!(
        phases,
        [
            json!("runner"),
            json!("runner_withdrawal"),
            json!("runner"),
            json!("host"),
            json!("clients")
        ]
    );
    f.staging("apply", "no_op", "");

    let f = Fleet::new();
    f.paired();
    f.change(|data| {
        data["refuse_upgrade"] = json!(true);
        data["lose_reply"] = json!("runner_restore");
    });
    f.staging("apply", "needs_reconcile", "link outcome");
    assert_eq!(read(&f.path("fleet.json"))["runner"], "source");
    f.change(|data| data["lose_reply"] = Value::Null);
    assert_eq!(f.staging("status", "ready", "")["withdrawn"], true);
    assert_eq!(
        f.events().iter().filter(|x| *x == "runner_advance").count(),
        1
    );
    assert_eq!(f.events().iter().filter(|x| *x == "upgrade").count(), 1);
    paired_host_loss();
}

fn paired_host_loss() {
    for fault in ["old_idle", "upgrade", "old", "old_unknown"] {
        let f = Fleet::new();
        f.paired();
        f.change(|d| d["lose_reply"] = json!(fault));
        match fault {
            "old_idle" => {
                assert_eq!(
                    f.staging("apply", "blocked", "SSH returned no bounded verdict")["withdrawn"],
                    true
                );
                assert!(f.events().ends_with(&[
                    "upgrade".into(),
                    "withdraw_upgrade".into(),
                    "runner_restore".into()
                ]));
                assert_eq!(read(&f.path("fleet.json"))["runner"], "source");
            }
            "upgrade" => {
                f.staging("apply", "needs_reconcile", "host upgrade outcome");
                f.staging("apply", "green", "");
                assert_eq!(read(&f.path("fleet.json"))["runner"], "current");
                assert!(!f.events().iter().any(|event| event == "runner_restore"));
            }
            _ => {
                f.staging("apply", "needs_reconcile", "host upgrade outcome");
                f.staging("status", "needs_reconcile", "unresolved");
                assert_eq!(read(&f.path("fleet.json"))["runner"], "current");
                assert!(
                    !f.events()
                        .iter()
                        .any(|event| event == "withdraw_upgrade" || event == "runner_restore")
                );
            }
        }
        assert_eq!(f.events().iter().filter(|x| *x == "upgrade").count(), 1);
    }
}

#[test]
fn deploy_pending_runner_withdraws_even_if_source_restarted_and_prepare_would_fail() {
    for verb in ["status", "apply"] {
        let f = Fleet::new();
        f.paired();
        f.change(|d| d["lose_reply"] = json!("runner_source_unknown"));
        f.staging("apply", "needs_reconcile", "");
        assert_eq!(read(&f.path("fleet.json"))["runner"], "current");
        f.change(|d| {
            d["transport_loss"] = json!(false);
            d["host"]["instance"] = json!("102-0000000000000002");
            d["host"]["upgrade_idle"] = json!(false);
            d["host"]["gateway_versions"] = json!([1]);
            d["refusal"] = json!("full host observation unavailable");
            d["refuse_preparation"] = json!(true);
        });
        let before = f
            .events()
            .iter()
            .filter(|event| *event == "destination_unit")
            .count();
        assert_eq!(
            f.staging(
                verb,
                if verb == "status" {
                    "ready"
                } else {
                    "withdrawn"
                },
                ""
            )["withdrawn"],
            true
        );
        assert_eq!(
            f.events()
                .iter()
                .filter(|event| *event == "destination_unit")
                .count(),
            before
        );
        assert_eq!(read(&f.path("fleet.json"))["runner"], "source");
        assert!(!f.events().iter().any(|event| event == "upgrade"));
    }
}

#[test]
fn deploy_runner_refusal_or_failed_observation_compensates_before_host_intent() {
    for (flag, remedy) in [
        ("refuse_advance", "candidate snapshot is missing"),
        ("refuse_runner_observe", "link outcome"),
    ] {
        let f = Fleet::new();
        f.paired();
        f.change(|d| d[flag] = json!(true));
        assert_eq!(f.staging("apply", "blocked", remedy)["withdrawn"], true);
        assert_eq!(read(&f.path("fleet.json"))["runner"], "source");
        assert!(
            !f.events()
                .iter()
                .any(|event| event == "upgrade" || event == "withdraw_upgrade")
        );
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
    let transferred = script.contains("head -c");
    assert!(script.contains("/usr/bin/shasum -a 256"));
    let image = read_candidate(input, transferred);
    let call: Value = serde_json::from_str(&next(input)).unwrap();
    assert_eq!(call["deploy_protocol"], 3);
    let request = &call["call"]["request"];
    let op = &request["op"];
    let mut name = op["op"].as_str().unwrap().to_owned();
    let mut remaining = Vec::new();
    input.read_to_end(&mut remaining).unwrap();
    let role = call["call"]["role"].as_str().unwrap();
    let node = match role {
        "client" => {
            assert!(transferred);
            name = format!("client_{name}");
            &request["node"]
        }
        "runner" => {
            assert_eq!(transferred, matches!(name.as_str(), "observe" | "stage"));
            name = format!("runner_{name}");
            &request["runner"]["node"]
        }
        "host" => {
            assert_host_loader(root, &data, &name, transferred);
            &request["host"]["node"]
        }
        _ => panic!("unexpected role {role}"),
    };
    assert_candidate_bytes(root, &image, node, transferred);
    let prod = call["stage"] == "production";
    if name == "client_observe" {
        return (
            encoded(&json!({"ok":{"current":data["client"] == "current",
            "production_history": false,
            "account": {"machine_sha256":if prod {"7"} else {"6"}.repeat(64), "platform":PLATFORM,
                "uid":1000, "home":"/srv/example"}}})),
            0,
        );
    }
    if role == "runner" {
        return runner_ssh(root, path.as_path(), &mut data, &name, node, op, &remaining);
    }
    let mut observed = data["host"].clone();
    let stage = if prod { "production" } else { "staging" };
    if prod {
        observed["stage_identity"]["machine_sha256"] = json!("9".repeat(64));
    }
    if data["paired_fleet"] == true {
        observed["paired"] = paired_placements(stage);
    }
    if matches!(name.as_str(), "observe" | "runner_withdrawal_proof") {
        return observed_reply(&data, &name, &observed);
    }
    if name == "runner_preflight" {
        assert_eq!(op["expected"], observed["instance"]);
        assert_eq!(op["placements"], paired_placements(stage));
        if !root.join("staging.jsonl").exists() {
            return (encoded(&runner_preflight(&data)), 0);
        }
    }
    data["events"].as_array_mut().unwrap().push(json!(name));
    let response = if name == "withdraw_upgrade" {
        assert_eq!(op["instance"], observed["instance"]);
        assert_eq!(op["source_sha256"], observed["sha256"]);
        assert_eq!(observed["upgrade_idle"], true);
        data["withdrawn_ticket"] = op["ticket"].clone();
        json!({"ok":observed})
    } else {
        simulate(root, &mut data, &name, op, &remaining)
    };
    let lost = data["lose_reply"] == name
        || name == "upgrade"
            && matches!(
                data["lose_reply"].as_str(),
                Some("old" | "old_idle" | "old_unknown")
            );
    write(&path, &data);
    (encoded(&response), if lost { 255 } else { 0 })
}

fn observed_reply(data: &Value, name: &str, observed: &Value) -> (Vec<u8>, i32) {
    if data["transport_loss"] == true {
        return (Vec::new(), 255);
    }
    if name == "observe"
        && let Some(reason) = data["refusal"].as_str()
    {
        return (encoded(&json!({"error":reason})), 0);
    }
    if name == "runner_withdrawal_proof" {
        return (
            encoded(&json!({"ok":{
                "sha256":observed["sha256"], "link_sha256":observed["link_sha256"],
                "stage_identity":observed["stage_identity"], "production_history":observed["production_history"],
            }})),
            0,
        );
    }
    (encoded(&json!({"ok":observed})), 0)
}

fn read_candidate(input: &mut impl BufRead, transferred: bool) -> (String, Vec<u8>) {
    let header = next(input);
    let pin = next(input);
    if transferred {
        let mut binary = vec![0; header.parse().unwrap()];
        input.read_exact(&mut binary).unwrap();
        (pin, binary)
    } else {
        (pin, header.into_bytes())
    }
}

fn assert_candidate_bytes(root: &Path, image: &(String, Vec<u8>), node: &Value, transferred: bool) {
    let platform = node["platform"].as_str().unwrap();
    let expected = artifact_bytes("plakd", platform);
    assert_eq!(image.0, hash(&expected));
    if transferred {
        assert_eq!(image.1, expected);
    } else {
        assert_eq!(image.1, deployed(root, node).into_bytes());
    }
}

fn deployed(root: &Path, node: &Value) -> String {
    let directory = if node["platform"] == MAC {
        "mac"
    } else {
        "host"
    };
    let manifest = read(&root.join(directory).join("manifest.json"));
    let snapshot = manifest["snapshot_id"].as_str().unwrap();
    let home = node["home"].as_str().unwrap();
    format!("{home}/.local/lib/plak/release/{snapshot}/plakd")
}

fn paired_placements(stage: &str) -> Value {
    json!({"desk":{"ssh":format!("runner-{stage}@example-mac"),
        "command":format!("/Volumes/example/runner-{stage}/.local/bin/example-runner")}})
}

fn runner_preflight(data: &Value) -> Value {
    if data["live_runner_jobs"] == true {
        json!({"error":"foreground/background processes are live; settle and drain them using the old build"})
    } else if data["missing_runner_attachment"] == true {
        json!({"error":"reconnect a settled source root on that runner read-only and keep it attached until upgrade completes"})
    } else {
        json!({"ok":null})
    }
}

fn runner_ssh(
    root: &Path,
    path: &Path,
    data: &mut Value,
    name: &str,
    node: &Value,
    op: &Value,
    bytes: &[u8],
) -> (Vec<u8>, i32) {
    let home = node["home"].as_str().unwrap();
    let source = json!({"path":format!("{home}/.local/lib/plak/maintainer/retained/plakd"),"sha256":"1".repeat(64)});
    let desired = json!({"path":deployed(root,node),"sha256":hash(artifact_bytes("plakd", MAC))});
    if name == "runner_observe" {
        if data["refuse_runner_observe"] == true && data["runner"] == "current" {
            return (encoded(&json!({"error":"runner observation refused"})), 0);
        }
        let link = if data["runner"] == "current" {
            &desired
        } else {
            &source
        };
        let triple = if data["runner"] == "current" {
            json!({"protocol":9,"source_revision":"a".repeat(40),"flavor":"release"})
        } else {
            data["runner_triple"].as_object().map_or_else(
                || json!({"protocol":9,"source_revision":"c".repeat(40),"flavor":"maintainer"}),
                |_| data["runner_triple"].clone(),
            )
        };
        return (
            encoded(
                &json!({"ok":{"link":link,"runner_triple":triple,"production_history":false,
            "account":{"machine_sha256":"5".repeat(64),"platform":MAC,"uid":node["uid"],"home":node["home"]}}}),
            ),
            0,
        );
    }
    data["events"].as_array_mut().unwrap().push(json!(name));
    match name {
        "runner_stage" => {
            assert_eq!(
                hash(op["manifest"].as_str().unwrap()),
                op["manifest_sha256"]
            );
            assert_eq!(bytes, b"mac-plakous-js-hostmac-plakous-py-host");
            data["runner_staged"] = json!(true);
        }
        "runner_advance" => {
            assert_eq!(data["runner_staged"], true);
            assert_eq!(op["candidate"]["sha256"], desired["sha256"]);
            assert_eq!(op["source"], source);
            assert_ne!(op["ticket"], data["runner_withdrawn_ticket"]);
            if data["refuse_advance"] == true {
                write(path, data);
                return (
                    encoded(&json!({"error":"candidate snapshot is missing or unverified"})),
                    0,
                );
            }
            data["runner"] = json!("current");
            if data["lose_reply"] == "runner_source_unknown" {
                data["transport_loss"] = json!(true);
            }
        }
        "runner_restore" => {
            if data["events"]
                .as_array()
                .unwrap()
                .iter()
                .any(|event| event == "upgrade")
            {
                assert_eq!(op["ticket"], data["withdrawn_ticket"]);
            }
            let evidence = fs::read_to_string(root.join("staging.jsonl")).unwrap();
            assert!(evidence.lines().rev().any(|line| {
                let row: Value = serde_json::from_str(line).unwrap();
                row["phase"] == "runner_withdrawal" && row["ticket"] == op["ticket"]
            }));
            assert_eq!(op["source"], source);
            data["runner_withdrawn_ticket"] = op["ticket"].clone();
            data["runner"] = json!("source");
        }
        _ => panic!("unexpected runner operation {name}"),
    }
    let lost = data["lose_reply"] == name
        || name == "runner_advance" && data["lose_reply"] == "runner_source_unknown";
    write(path, data);
    (encoded(&json!({"ok":null})), if lost { 255 } else { 0 })
}

fn assert_host_loader(root: &Path, data: &Value, op: &str, transferred: bool) {
    let progress = root.join("invocation");
    let phase = fs::read_to_string(&progress).unwrap();
    let already_serving_candidate = data["host"]["sha256"] == hash(b"plakd");
    let want_transfer = match (phase.as_str(), op) {
        ("planning", "observe" | "runner_withdrawal_proof") => {
            fs::write(&progress, b"planned").unwrap();
            true
        }
        ("planned", "stage_host") => {
            fs::write(&progress, b"installed").unwrap();
            true
        }
        ("planned", _) => {
            if !transferred {
                assert!(already_serving_candidate || data["candidate"] == hash(b"plakd"));
            }
            transferred
        }
        ("installed", _) => false,
        _ => panic!("unexpected call order: {phase}/{op}"),
    };
    assert_eq!(
        transferred, want_transfer,
        "wrong candidate loader: {phase}/{op}"
    );
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
            if data["refuse_preparation"] == true {
                return json!({"error":"destination prepare refused"});
            }
            assert_eq!(op["candidate"]["sha256"], data["candidate"]);
            Value::Null
        }
        "mark_production" => {
            assert_eq!(op["expected"]["machine_sha256"], "9".repeat(64));
            data["host"]["production_history"] = json!(true);
            Value::Null
        }
        "upgrade" => return simulate_upgrade(data, op, &want),
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
        "runner_preflight" => return runner_preflight(data),
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

fn simulate_upgrade(data: &mut Value, op: &Value, want: &Value) -> Value {
    assert_eq!(op["candidate"]["sha256"], data["candidate"]);
    assert_eq!(data["client"], "staged");
    assert_eq!(op["previous"]["instance"], data["host"]["instance"]);
    if data["paired_fleet"] == true {
        assert_eq!(data["runner"], "current");
    }
    if data["refuse_upgrade"] == true {
        return json!({"error":"deploy_runner_inspection: runner desk job 17 is outcome_unknown; use jobs.ack or jobs.disown; no source surrendered"});
    }
    if data["lose_reply"] == "old" {
        data["host"]["upgrade_idle"] = json!(false);
    } else if data["lose_reply"] == "old_idle" {
        assert_eq!(data["host"]["upgrade_idle"], true);
    } else if data["lose_reply"] == "old_unknown" {
        data["transport_loss"] = json!(true);
    } else {
        let host = &mut data["host"];
        host["instance"] = json!("101-0000000000000001");
        host["sha256"] = want["sha256"].clone();
        host["facts"]["binary_id"] = want["display_id"].clone();
        host["facts"]["runner_triple"]["source_revision"] = json!("a".repeat(40));
    }
    json!({"ok":null})
}
