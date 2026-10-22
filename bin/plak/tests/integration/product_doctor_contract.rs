//! Focused contracts for the read-only Plak doctor diagnostic core.

#![allow(
    dead_code,
    reason = "path-loaded contract exercises the private product doctor module"
)]
#![allow(clippy::unwrap_used, reason = "focused diagnostic contract tests")]

use std::fs;

use plak::presentation::{CliStatus, CompatibilityState, CredentialState};
use plak::product_doctor;
use plak::product_doctor::{
    CheckKind, CompositionHostStatus, ConfigStatus, DatabaseStatus, DoctorInputs, DoctorPaths,
    DoctorReport, OauthCredentialProbe, OauthCredentialProbes, OauthRecordStatus,
    OauthRefreshLookup, OauthRefreshTelemetry, OpenRouterCatalogStatus, Requirement,
    RootTemplateStatus, RuntimeStoreStatus, Severity, SpawnTemplatesStatus, TerminalFacts,
    diagnose, inspect_config, inspect_oauth_record, inspect_runtime_database,
    inspect_runtime_store,
};
use plak::telemetry_sink::TelemetryHealth;
use tempfile::TempDir;

#[cfg(unix)]
fn private_directory(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt as _;
    fs::create_dir(path).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

#[cfg(not(unix))]
fn private_directory(path: &std::path::Path) {
    fs::create_dir(path).unwrap();
}

fn signed_out() -> CliStatus {
    CliStatus {
        compatibility: CompatibilityState::Current,
        credential: CredentialState::SignedOut,
    }
}

fn input(temp: &TempDir) -> DoctorInputs {
    let home = temp.path().join("plak-home");
    private_directory(&home);
    private_directory(&home.join("secrets"));
    let paths = plak::home::paths::PlakPaths::explicit(home.clone());
    private_directory(&paths.state_root());
    private_directory(&paths.state_dir());
    private_directory(&paths.sessions_dir());
    DoctorInputs {
        jj_cli: product_doctor::jj_cli::probe(None, temp.path()),
        version: "0.1.0-test".to_owned(),
        paths: DoctorPaths::under(home),
        config: ConfigStatus::Parsed,
        product_database: DatabaseStatus::Available,

        subagents_database: DatabaseStatus::Available,
        runtime_database: DatabaseStatus::Available,
        runtime_store: RuntimeStoreStatus::Missing,
        retention_failure_at_ns: None,
        telemetry: TelemetryHealth::Missing,
        working_directory: Ok(temp.path().to_path_buf()),
        terminals: TerminalFacts {
            stdin: false,
            stdout: true,
        },
        composition_requirement: Requirement::Optional,
        composition_host: CompositionHostStatus::Included {
            identity: "sha256:test".to_owned(),
        },
        python_containment_required: false,
        runtime_directory: Some(temp.path().to_path_buf()),
        openrouter_catalog: OpenRouterCatalogStatus::NotConfigured,
        root_template: RootTemplateStatus::Unset,
        spawn_templates: SpawnTemplatesStatus::None,
        oauth: OauthCredentialProbes {
            codex: OauthCredentialProbe {
                provider: "codex",
                credential: Ok(signed_out()),
                record: OauthRecordStatus::Missing,
                last_refresh: OauthRefreshLookup::None,
            },
            xai: OauthCredentialProbe {
                provider: "xai",
                credential: Ok(signed_out()),
                record: OauthRecordStatus::Missing,
                last_refresh: OauthRefreshLookup::None,
            },
            claude: OauthCredentialProbe {
                provider: "claude",
                credential: Ok(signed_out()),
                record: OauthRecordStatus::Missing,
                last_refresh: OauthRefreshLookup::None,
            },
        },
    }
}

fn check(report: &DoctorReport, kind: CheckKind) -> &product_doctor::DoctorCheck {
    report
        .checks
        .iter()
        .find(|check| check.kind == kind)
        .unwrap()
}

#[test]
fn config_inspection_parses_existing_file_without_creating_missing_state() {
    let temp = TempDir::new().unwrap();
    let missing = temp.path().join("absent").join("config.toml");
    assert_eq!(inspect_config(&missing, |_| Ok(())), ConfigStatus::Missing);
    assert!(!temp.path().join("absent").exists());

    let config = temp.path().join("config.toml");
    fs::write(&config, "valid = true\n").unwrap();
    assert_eq!(
        inspect_config(&config, |text| (text == "valid = true\n")
            .then_some(())
            .ok_or_else(|| "bad config".to_owned())),
        ConfigStatus::Parsed
    );
    assert_eq!(
        inspect_config(&config, |_| Err("line 1 is invalid".to_owned())),
        ConfigStatus::Invalid("line 1 is invalid".to_owned())
    );
}

#[test]
fn runtime_database_probe_does_not_create_a_missing_database_or_parent() {
    let temp = TempDir::new().unwrap();
    let parent = temp.path().join("missing-home");
    let database = parent.join("runtime.sqlite3");
    assert_eq!(inspect_runtime_database(&database), DatabaseStatus::Missing);
    assert!(!parent.exists());
}

#[test]
fn report_is_table_ready_and_summarizes_warnings() {
    let temp = TempDir::new().unwrap();
    let mut values = input(&temp);
    values.config = ConfigStatus::Missing;
    values.product_database = DatabaseStatus::Missing;
    values.runtime_database = DatabaseStatus::Incompatible;
    let report = diagnose(values);

    assert_eq!(check(&report, CheckKind::Version).result, "available");
    assert_eq!(check(&report, CheckKind::Config).severity, Severity::Ok);
    assert_eq!(
        check(&report, CheckKind::RuntimeDatabase).result,
        "incompatible"
    );
    assert_eq!(report.summary.errors, 1);
    assert!(report.summary.ok >= 2);
    assert_eq!(CheckKind::ProductDatabase.key(), "product_db");
}

/// A spawnable template whose account is not ready loads; doctor names it
/// and the login that makes it spawnable.
#[test]
fn a_spawnable_template_without_its_login_is_a_named_warning() {
    let temp = TempDir::new().unwrap();
    let mut values = input(&temp);
    values.spawn_templates = SpawnTemplatesStatus::NotReady {
        templates: vec![product_doctor::NotReadyTemplate {
            name: "multi-default-researcher".to_owned(),
            account: "grok".to_owned(),
            reason: "not logged in",
        }],
    };
    let report = diagnose(values);
    let spawn = check(&report, CheckKind::SpawnTemplates);
    assert_eq!(spawn.severity, Severity::Warning);
    assert_eq!(
        spawn.detail,
        "multi-default-researcher · grok · not logged in; spawning it is refused"
    );
    assert!(spawn.remediation.as_deref().unwrap().contains("plak login"));
    assert_eq!(CheckKind::SpawnTemplates.key(), "spawn_templates");
}

#[test]
fn unsafe_home_and_required_composition_failure_produce_failed_exit() {
    let temp = TempDir::new().unwrap();
    let mut values = input(&temp);
    fs::remove_dir_all(&values.paths.home).unwrap();
    fs::write(&values.paths.home, "not a directory").unwrap();
    values.composition_requirement = Requirement::Required;
    values.python_containment_required = true;
    values.composition_host = CompositionHostStatus::Unavailable("not bundled".to_owned());
    values.runtime_directory = None;
    let report = diagnose(values);

    assert_eq!(check(&report, CheckKind::Home).severity, Severity::Error);
    assert_eq!(
        check(&report, CheckKind::CompositionHost).severity,
        Severity::Error
    );
    assert!(report.summary.errors >= 2);
}

#[test]
fn javascript_host_unavailable_fix_names_js_host_not_python() {
    let temp = TempDir::new().unwrap();
    let mut values = input(&temp);
    values.composition_requirement = Requirement::Required;
    values.python_containment_required = false;
    values.composition_host = CompositionHostStatus::Unavailable("no JavaScript host".to_owned());
    let report = diagnose(values);
    let host = check(&report, CheckKind::CompositionHost);
    assert_eq!(host.severity, Severity::Error);
    let fix = host.remediation.as_deref().unwrap();
    assert!(fix.contains("PLAK_JS_HOST"));
    assert!(!fix.contains("PLAK_MONTY_WORKER"));
}

#[test]
fn python_host_unavailable_fix_names_python_host() {
    let temp = TempDir::new().unwrap();
    let mut values = input(&temp);
    values.composition_requirement = Requirement::Required;
    values.python_containment_required = true;
    values.composition_host = CompositionHostStatus::Unavailable("no Python host".to_owned());
    values.runtime_directory = None;
    let report = diagnose(values);
    let host = check(&report, CheckKind::CompositionHost);
    assert_eq!(host.severity, Severity::Error);
    let fix = host.remediation.as_deref().unwrap();
    assert!(fix.contains("PLAK_MONTY_WORKER"));
}

#[test]
fn non_terminal_streams_are_facts_not_health_failures() {
    let temp = TempDir::new().unwrap();
    let mut values = input(&temp);
    values.terminals = TerminalFacts {
        stdin: false,
        stdout: false,
    };
    let report = diagnose(values);

    assert_eq!(
        check(&report, CheckKind::StdinTerminal).severity,
        Severity::Ok
    );
    assert_eq!(
        check(&report, CheckKind::StdoutTerminal).severity,
        Severity::Ok
    );
    assert_eq!(
        check(&report, CheckKind::StdoutTerminal).result,
        "not a terminal"
    );
}

#[test]
fn openrouter_catalog_not_configured_is_ok() {
    let temp = TempDir::new().unwrap();
    let mut values = input(&temp);
    values.openrouter_catalog = OpenRouterCatalogStatus::NotConfigured;
    let report = diagnose(values);
    let catalog = check(&report, CheckKind::OpenRouterCatalog);
    assert_eq!(catalog.severity, Severity::Ok);
    assert_eq!(catalog.result, "not configured");
}

#[test]
fn openrouter_catalog_key_present_no_model_is_warning() {
    let temp = TempDir::new().unwrap();
    let mut values = input(&temp);
    values.openrouter_catalog = OpenRouterCatalogStatus::KeyPresentNoModel;
    let report = diagnose(values);
    let catalog = check(&report, CheckKind::OpenRouterCatalog);
    assert_eq!(catalog.severity, Severity::Warning);
    assert_eq!(catalog.result, "key present");
    let remediation = catalog.remediation.as_deref().unwrap();
    assert!(
        remediation.contains("plak config set root.agent"),
        "remediation must name a current config key: {remediation}"
    );
    assert!(
        !remediation.contains("default.account"),
        "remediation must not name the removed default.* keys: {remediation}"
    );
}

#[test]
fn openrouter_catalog_ok_reports_catalog_matches() {
    let temp = TempDir::new().unwrap();
    let mut values = input(&temp);
    values.openrouter_catalog = OpenRouterCatalogStatus::Ok;
    let report = diagnose(values);
    let catalog = check(&report, CheckKind::OpenRouterCatalog);
    assert_eq!(catalog.severity, Severity::Ok);
    assert_eq!(catalog.result, "catalog matches");
}

#[test]
fn openrouter_catalog_failed_missing_field_is_error_with_remediation() {
    let temp = TempDir::new().unwrap();
    let mut values = input(&temp);
    let path = "endpoints.data.endpoints[].tag";
    values.openrouter_catalog =
        OpenRouterCatalogStatus::Failed(format!("missing catalog field at {path}"));
    let report = diagnose(values);
    let catalog = check(&report, CheckKind::OpenRouterCatalog);
    assert_eq!(catalog.severity, Severity::Error);
    assert_eq!(catalog.result, "catalog invalid");
    assert!(catalog.detail.contains(path));
    let fix = catalog.remediation.as_deref().unwrap();
    assert!(fix.contains("fail closed"));
}

#[test]
fn openrouter_catalog_failed_json_class_names_transport_not_pin() {
    let temp = TempDir::new().unwrap();
    let mut values = input(&temp);
    values.openrouter_catalog = OpenRouterCatalogStatus::Failed(
        "OpenRouter Chat discovery response is invalid: catalog JSON".into(),
    );
    let report = diagnose(values);
    let catalog = check(&report, CheckKind::OpenRouterCatalog);
    assert_eq!(catalog.severity, Severity::Error);
    assert!(catalog.detail.contains("catalog JSON"));
    let fix = catalog.remediation.as_deref().unwrap();
    assert!(fix.contains("named class"));
    assert!(!fix.contains("missing"));
}

#[test]
fn telemetry_doctor_reports_ok_with_row_count() {
    let temp = TempDir::new().unwrap();
    let mut values = input(&temp);
    values.telemetry = TelemetryHealth::Ok {
        rows: 42,
        write_errors: 0,
        dropped: 0,
    };
    let report = diagnose(values);
    let telemetry = check(&report, CheckKind::TelemetryDatabase);
    assert_eq!(telemetry.severity, Severity::Ok);
    assert_eq!(telemetry.result, "ok");
    assert!(telemetry.detail.contains("42 rows"));
}

#[test]
fn telemetry_doctor_reports_unavailable_reason() {
    let temp = TempDir::new().unwrap();
    let mut values = input(&temp);
    values.telemetry = TelemetryHealth::Unavailable("could not open telemetry store".into());
    let report = diagnose(values);
    let telemetry = check(&report, CheckKind::TelemetryDatabase);
    assert_eq!(telemetry.severity, Severity::Error);
    assert_eq!(telemetry.result, "unavailable");
    assert!(telemetry.detail.contains("could not open telemetry store"));
}

#[test]
fn summary_counts_are_stable() {
    let healthy = DoctorReport::from_checks(vec![product_doctor::DoctorCheck {
        kind: CheckKind::Version,
        group: "product",
        severity: Severity::Ok,
        result: "available".to_owned(),
        detail: "test".to_owned(),
        remediation: None,
    }]);
    assert_eq!(healthy.summary.ok, 1);
}

#[test]
fn oauth_doctor_prints_record_metadata_and_last_refresh_without_payload() {
    let temp = TempDir::new().unwrap();
    let mut values = input(&temp);
    let record = temp.path().join("credential.json");
    fs::write(
        &record,
        r#"{"format_version":2,"profile_id":"fixture","slot_id":"xai","account_binding":"fixture","generation":40,"status":"refresh_outcome_unknown","payload":"Zml4dHVyZQ=="}"#,
    )
    .unwrap();
    let inspected = inspect_oauth_record(&record);
    match &inspected {
        OauthRecordStatus::Present {
            status, generation, ..
        } => {
            assert_eq!(status, "refresh_outcome_unknown");
            assert_eq!(*generation, 40);
        }
        other => panic!("expected present record, got {other:?}"),
    }
    values.oauth.xai = OauthCredentialProbe {
        provider: "xai",
        credential: Ok(CliStatus {
            compatibility: CompatibilityState::Current,
            credential: CredentialState::Quarantined,
        }),
        record: inspected,
        last_refresh: OauthRefreshLookup::Row(OauthRefreshTelemetry {
            phase: "outcome_unknown".to_owned(),
            failure_class: Some("timeout".to_owned()),
            transport_phase: Some("resolution".to_owned()),
            http_status_class: Some("5xx".to_owned()),
            age_secs: 120,
        }),
    };
    let report = diagnose(values);
    let grok = check(&report, CheckKind::OauthXai);
    assert_eq!(grok.severity, Severity::Warning);
    assert_eq!(grok.result, "refresh_outcome_unknown");
    assert!(grok.detail.contains("generation 40"));
    assert!(
        grok.detail
            .contains("last refresh outcome_unknown timeout 5xx")
    );
    assert!(grok.detail.contains("2m ago"));
    assert!(grok.detail.contains("transport=resolution"));
    assert_eq!(
        grok.remediation.as_deref(),
        Some("run plak refresh grok; run plak login grok if that fails")
    );
    assert!(!grok.detail.contains("payload"));
    assert!(!grok.detail.contains("Zml4dHVyZQ"));
    assert!(!grok.result.contains("Zml4dHVyZQ"));
    assert_eq!(check(&report, CheckKind::OauthCodex).result, "signed_out");
    assert_eq!(CheckKind::OauthXai.key(), "oauth_xai");
}

#[test]
fn oauth_doctor_surfaces_refresh_telemetry_unavailable() {
    let temp = TempDir::new().unwrap();
    let mut values = input(&temp);
    values.oauth.codex.last_refresh = OauthRefreshLookup::Unavailable(
        "telemetry store schema version 6 is stale; the file will be rebuilt on the next session (expected 7)"
            .to_owned(),
    );
    let report = diagnose(values);
    let codex = check(&report, CheckKind::OauthCodex);
    assert_eq!(codex.result, "signed_out");
    assert!(
        codex
            .detail
            .contains("refresh telemetry unavailable: telemetry store schema version 6")
    );
}

#[test]
fn runtime_store_usage_reports_thresholds_file_size_and_prune_failure() {
    let temp = TempDir::new().unwrap();
    let database = temp.path().join("runtime.sqlite3");
    assert_eq!(
        inspect_runtime_store(&database),
        RuntimeStoreStatus::Missing
    );
    assert!(!database.exists());
    plakous::sessions::SessionBackend::sqlite(&database).unwrap();
    let RuntimeStoreStatus::Usage(usage) = inspect_runtime_store(&database) else {
        panic!("a current runtime store reports usage");
    };
    let mut values = input(&temp);
    values.runtime_store = RuntimeStoreStatus::Usage(usage);
    let failed = values.clone();
    let report = diagnose(values);
    let row = check(&report, CheckKind::RuntimeStore);
    assert_eq!(row.severity, Severity::Ok);
    assert!(!row.detail.contains("prune failed"), "{}", row.detail);
    assert!(row.detail.starts_with("checkpoints 0/"));
    assert!(row.detail.contains("new work stops at"));
    assert!(row.detail.contains("object bytes 0/"));
    assert!(row.detail.contains("; file "), "{}", row.detail);
    assert!(row.detail.contains(" free"), "{}", row.detail);
    assert!(
        row.detail.ends_with("; feed 0 rows, 0 bytes"),
        "{}",
        row.detail
    );
    assert_eq!(CheckKind::RuntimeStore.key(), "runtime_store");

    let mut values = failed;
    values.retention_failure_at_ns = Some(0);
    let report = diagnose(values);
    let row = check(&report, CheckKind::RuntimeStore);
    assert!(
        row.detail
            .contains("; the last automatic history prune failed "),
        "{}",
        row.detail
    );
}

/// Doctor reads the one credential classification, so it cannot call a
/// record `plak status` and model runs refuse healthy.
#[test]
fn oauth_doctor_reports_the_same_verdict_as_status() {
    let temp = TempDir::new().unwrap();
    let record = temp.path().join("credential.json");
    fs::write(
        &record,
        r#"{"format_version":2,"profile_id":"xai.supergrok.compat.2026-01-01.r1","slot_id":"primary","account_binding":"fixture","generation":3,"status":"ready","payload":"e30="}"#,
    )
    .unwrap();
    for (credential, compatibility, result, severity, fix) in [
        (
            CredentialState::ProfileChanged,
            CompatibilityState::Current,
            "profile_changed",
            Severity::Error,
            Some("run plak login grok"),
        ),
        (
            CredentialState::LoginRequired,
            CompatibilityState::Current,
            "retired_format",
            Severity::Error,
            Some("run plak login grok"),
        ),
        (
            CredentialState::WrittenByNewerBuild,
            CompatibilityState::Current,
            "written_by_newer_build",
            Severity::Error,
            Some(
                "use the newer plak build that wrote it, or run plak login grok to replace it (that build must then log in again)",
            ),
        ),
        (
            CredentialState::Ready,
            CompatibilityState::Required,
            "acceptance_required",
            Severity::Warning,
            Some("run plak models grok to review and accept the renewed notice; the login is kept"),
        ),
        (
            CredentialState::Ready,
            CompatibilityState::Current,
            "ready",
            Severity::Ok,
            None,
        ),
    ] {
        let home = TempDir::new().unwrap();
        let mut values = input(&home);
        values.oauth.xai = OauthCredentialProbe {
            provider: "xai",
            credential: Ok(CliStatus {
                compatibility,
                credential,
            }),
            record: inspect_oauth_record(&record),
            last_refresh: OauthRefreshLookup::None,
        };
        let report = diagnose(values);
        let grok = check(&report, CheckKind::OauthXai);
        assert_eq!(grok.result, result, "{credential:?}");
        assert_eq!(grok.severity, severity, "{credential:?}");
        assert_eq!(grok.remediation.as_deref(), fix, "{credential:?}");
        assert!(grok.detail.contains("record ready"), "{}", grok.detail);
    }

    let home = TempDir::new().unwrap();
    let mut values = input(&home);
    values.oauth.xai.credential = Err("Unsafe storage · run plak status grok".to_owned());
    let report = diagnose(values);
    let grok = check(&report, CheckKind::OauthXai);
    assert_eq!(grok.result, "unreadable");
    assert_eq!(grok.severity, Severity::Error);
    assert!(grok.detail.contains("Unsafe storage"), "{}", grok.detail);
}

#[test]
fn state_database_corruption_is_an_error_with_a_remedy() {
    use plakous_recursive_subagents_store::SCHEMA_VERSION;
    use plakous_recursive_subagents_store::database::SubagentsDatabase;
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("subagents.sqlite3");
    drop(SubagentsDatabase::open(&path).unwrap());
    assert_eq!(
        product_doctor::inspect_state_database(&path, SCHEMA_VERSION),
        DatabaseStatus::Available
    );
    let connection = rusqlite::Connection::open(&path).unwrap();
    connection.execute_batch("PRAGMA foreign_keys=OFF; INSERT INTO agent_feed_cursors VALUES ('missing','missing',0);").unwrap();
    drop(connection);
    let before = fs::read(&path).unwrap();
    let status = product_doctor::inspect_state_database(&path, SCHEMA_VERSION);
    assert!(matches!(status, DatabaseStatus::Corrupt(_)));
    assert_eq!(fs::read(&path).unwrap(), before);
    assert!(SubagentsDatabase::open(&path).is_ok());
    for kind in [CheckKind::ProductDatabase, CheckKind::SubagentsDatabase] {
        let fixture = TempDir::new().unwrap();
        let mut inputs = input(&fixture);
        if kind == CheckKind::ProductDatabase {
            inputs.product_database = status.clone();
        } else {
            inputs.subagents_database = status.clone();
        }
        let report = diagnose(inputs);
        let check = report
            .checks
            .iter()
            .find(|check| check.kind == kind)
            .unwrap();
        assert_eq!(check.severity, Severity::Error);
        assert_eq!(check.result, "corrupt");
        assert!(check.remediation.is_some());
    }
}

#[test]
fn doctor_detects_index_desynchronization_that_quick_check_misses() {
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("product.sqlite3");
    let connection = rusqlite::Connection::open(&path).unwrap();
    connection.execute_batch("PRAGMA user_version=1; CREATE TABLE entries(value TEXT); CREATE INDEX entries_value ON entries(value); INSERT INTO entries VALUES ('indexed');").unwrap();
    let page_size: i64 = connection
        .query_row("PRAGMA page_size", [], |row| row.get(0))
        .unwrap();
    let root: i64 = connection
        .query_row(
            "SELECT rootpage FROM sqlite_schema WHERE name='entries_value'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let page_size = usize::try_from(page_size).unwrap();
    let root = usize::try_from(root).unwrap();
    drop(connection);
    let mut bytes = fs::read(&path).unwrap();
    let page = &mut bytes[(root - 1) * page_size..root * page_size];
    // Preserve valid structure and entry counts, but desynchronize the indexed value.
    let value = page
        .windows(b"indexed".len())
        .position(|bytes| bytes == b"indexed")
        .unwrap();
    page[value] = b'j';
    fs::write(&path, bytes).unwrap();
    let connection = rusqlite::Connection::open(&path).unwrap();
    let quick: String = connection
        .query_row("PRAGMA quick_check(1)", [], |row| row.get(0))
        .unwrap();
    assert_eq!(quick, "ok");
    drop(connection);
    assert!(matches!(
        product_doctor::inspect_state_database(&path, 1),
        DatabaseStatus::Corrupt(_)
    ));
}

#[test]
fn doctor_reports_non_database_bytes_as_corrupt_and_lock_contention_as_unavailable() {
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("product.sqlite3");
    fs::write(&path, b"not a SQLite database").unwrap();
    assert!(matches!(
        product_doctor::inspect_state_database(&path, 1),
        DatabaseStatus::Corrupt(_)
    ));
    fs::remove_file(&path).unwrap();
    let connection = rusqlite::Connection::open(&path).unwrap();
    connection
        .execute_batch("PRAGMA user_version=1; CREATE TABLE entries(value TEXT); BEGIN EXCLUSIVE;")
        .unwrap();
    assert!(matches!(
        product_doctor::inspect_state_database(&path, 1),
        DatabaseStatus::Unavailable(_)
    ));
    connection.execute_batch("ROLLBACK;").unwrap();
    assert_eq!(
        product_doctor::inspect_state_database(&path, 1),
        DatabaseStatus::Available
    );
}

#[test]
fn jj_cli_doctor_versions_and_missing_are_nonblocking() {
    use std::os::unix::fs::PermissionsExt as _;
    let temp = tempfile::tempdir().unwrap();
    assert_eq!(
        product_doctor::jj_cli::probe(None, temp.path()).result,
        "missing"
    );
    for (text, result, severity) in [
        ("jj 0.44.0", "available", Severity::Ok),
        ("jj 0.45.0", "available", Severity::Ok),
        ("jj 0.46.0", "available", Severity::Ok),
        ("jj 0.43.0", "untested", Severity::Warning),
        ("jj 0.47.0", "untested", Severity::Warning),
        ("not a version", "unavailable", Severity::Warning),
    ] {
        let binary = temp.path().join("jj");
        fs::write(&binary, format!("#!/bin/sh\n[ \"$#\" = 1 ] && [ \"$1\" = --version ] || exit 99\n[ -z \"${{HOME+x}}\" ] || exit 98\nprintf '%s\\n' '{text}'\n")).unwrap();
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).unwrap();
        let row = product_doctor::jj_cli::probe(Some(temp.path().as_os_str()), temp.path());
        assert_eq!(row.kind, CheckKind::JjCli);
        if plakous_hosted::built_jj_library_version().is_none() {
            assert!(row.detail.contains("library not built"));
        }
        assert_eq!(row.result, result, "{text}: {}", row.detail);
        assert_eq!(row.severity, severity);
    }
    let shim = temp.path().join(".plak/bin");
    fs::create_dir_all(&shim).unwrap();
    fs::copy(temp.path().join("jj"), shim.join("jj")).unwrap();
    assert_eq!(
        product_doctor::jj_cli::probe(Some(shim.as_os_str()), temp.path()).result,
        "missing"
    );
}

#[test]
fn jj_cli_doctor_kills_hanging_process_group_at_five_seconds() {
    use std::os::unix::fs::PermissionsExt as _;
    let temp = tempfile::tempdir().unwrap();
    let binary = temp.path().join("jj");
    fs::write(
        &binary,
        "#!/bin/sh\n/bin/sleep 60 &\necho $! >descendant\nwait\n",
    )
    .unwrap();
    fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).unwrap();
    let started = std::time::Instant::now();
    let row = product_doctor::jj_cli::probe(Some(temp.path().as_os_str()), temp.path());
    assert_eq!(row.severity, Severity::Warning);
    assert_eq!(row.result, "unavailable");
    assert!(started.elapsed() >= std::time::Duration::from_secs(5));
    assert!(started.elapsed() < std::time::Duration::from_secs(8));
    #[cfg(target_os = "linux")]
    assert_jj_descendant_retired(temp.path());
}

#[test]
fn jj_cli_doctor_bounds_output_wait_after_leader_exit() {
    use std::os::unix::fs::PermissionsExt as _;
    let temp = tempfile::tempdir().unwrap();
    let binary = temp.path().join("jj");
    fs::write(
        &binary,
        "#!/bin/sh\nprintf 'jj 0.46.0\\n'\n/bin/sleep 60 &\necho $! >descendant\nexit 0\n",
    )
    .unwrap();
    fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).unwrap();
    let started = std::time::Instant::now();
    let row = product_doctor::jj_cli::probe(Some(temp.path().as_os_str()), temp.path());
    assert_eq!(row.severity, Severity::Ok);
    assert!(started.elapsed() < std::time::Duration::from_secs(5));
    #[cfg(target_os = "linux")]
    assert_jj_descendant_retired(temp.path());
}

#[cfg(target_os = "linux")]
fn assert_jj_descendant_retired(path: &std::path::Path) {
    let pid = fs::read_to_string(path.join("descendant")).unwrap();
    let stat_path = format!("/proc/{}/stat", pid.trim());
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
    loop {
        let Ok(stat) = fs::read_to_string(&stat_path) else {
            return;
        };
        if matches!(
            stat.split_once(") ").unwrap().1.chars().next(),
            Some('Z' | 'X')
        ) {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "jj descendant survived: {stat}"
        );
        // SIGKILL delivery and the zombie transition are asynchronous.
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}
