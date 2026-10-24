use super::*;
use crate::home::paths::PlakPaths;
use crate::product_config::atomic::AtomicStage;
use crate::product_config::keys::{ConfigField, ConfigScope, config_key};
use crate::product_config::policy::{
    CEILING_MAX_ATTEMPTS, CEILING_MAX_CHILDREN, CEILING_MAX_DEPTH, CEILING_MAX_NODES,
    CEILING_REQUESTS_PER_ACCOUNT, template,
};
use crate::product_config::templates::render_template;
use plakous_hosted::agent_tree_protocol::MAX_TREE_ATTEMPTS;
use std::fs;

#[test]
fn transient_resumes_is_bounded_home_scoped_and_roundtrips() {
    assert_eq!(ProductConfig::default_product().transient_resumes, 1);
    for value in 0..=2 {
        let parsed =
            ProductConfig::parse(&format!("[orchestration]\ntransient_resumes = {value}\n"))
                .unwrap();
        assert_eq!(parsed.transient_resumes, value);
        let rendered = parsed.render();
        assert_eq!(
            ProductConfig::parse(&rendered).unwrap().transient_resumes,
            value
        );
    }
    assert!(ProductConfig::parse("[orchestration]\ntransient_resumes = 3\n").is_err());
    let mut assigned = ProductConfig::default_product();
    assigned
        .assign_key("orchestration.transient_resumes", "0")
        .unwrap();
    assert_eq!(assigned.transient_resumes, 0);
}

#[test]
fn telemetry_bounds_are_settable_and_refuse_zero() {
    let mut cfg = ProductConfig::default_product();
    cfg.assign_key("telemetry.max_bytes", "1000").unwrap();
    cfg.assign_key("telemetry.max_age_days", "7").unwrap();
    assert_eq!(cfg.telemetry_max_bytes, 1000);
    assert_eq!(cfg.telemetry_max_age_days, 7);
    assert!(cfg.assign_key("telemetry.max_bytes", "0").is_err());
    assert!(cfg.assign_key("telemetry.max_age_days", "-1").is_err());
    let zero = cfg.render().replace("max_age_days = 7", "max_age_days = 0");
    assert!(ProductConfig::parse(&zero).is_err());
}

#[test]
fn bounds_reject_outside_their_own_ceiling() {
    for (key, ceiling) in [
        ("max_depth", CEILING_MAX_DEPTH),
        ("max_children", CEILING_MAX_CHILDREN),
        ("max_nodes", CEILING_MAX_NODES),
        ("max_attempts", CEILING_MAX_ATTEMPTS),
        (
            "max_concurrent_requests_per_account",
            CEILING_REQUESTS_PER_ACCOUNT,
        ),
    ] {
        for bad in ["0", "-1"] {
            assert!(
                ProductConfig::parse(&format!("[orchestration]\n{key} = {bad}\n"))
                    .unwrap_err()
                    .contains(&format!("between 1 and {ceiling}")),
                "{key} = {bad}"
            );
        }
        let above = ceiling + 1;
        assert!(
            ProductConfig::parse(&format!("[orchestration]\n{key} = {above}\n"))
                .unwrap_err()
                .contains(&format!("between 1 and {ceiling}")),
            "{key} = {above} must be a load error, never a silent clamp"
        );
        assert!(
            ProductConfig::parse(&format!("[orchestration]\n{key} = {ceiling}\n")).is_ok(),
            "{key} = {ceiling} is the admitted ceiling"
        );
        let mut assigned = ProductConfig::default_product();
        assert!(
            assigned
                .assign_key(&format!("orchestration.{key}"), &above.to_string())
                .is_err(),
            "orchestration.{key} = {above} must be a set error"
        );
    }
    let defaults = ProductConfig::default_product();
    assert_eq!(defaults.max_depth, 1);
    assert_eq!(defaults.max_children, 8);
    assert_eq!(defaults.max_nodes, 32);
}

#[test]
fn max_attempts_and_require_review_evidence_are_orchestration_keys() {
    let defaults = ProductConfig::default_product();
    assert_eq!(defaults.max_attempts, 64);
    assert!(!defaults.require_review_evidence);
    let parsed = ProductConfig::parse(
        "[orchestration]\nmax_attempts = 12\nrequire_review_evidence = true\n",
    )
    .unwrap();
    assert_eq!(parsed.max_attempts, 12);
    assert!(parsed.require_review_evidence);
    assert!(
        ProductConfig::parse("[orchestration]\nmax_attempts = 0\n")
            .unwrap_err()
            .contains("between 1 and")
    );
    // Above the ceiling is a load error now; the snapshot clamp stays on
    // as defense in depth for a value written around the parser.
    let mut raised = ProductConfig::default_product();
    raised.max_attempts = 900;
    let snapshot = crate::roster_input::snapshot_from_config(&raised, "codex", "gpt", None);
    assert_eq!(usize::from(snapshot.max_attempts), MAX_TREE_ATTEMPTS);
    let rendered = parsed.render();
    assert!(rendered.contains("max_attempts = 12\n"));
    assert!(rendered.contains("require_review_evidence = true\n"));
    let reparsed = ProductConfig::parse(&rendered).unwrap();
    assert_eq!(reparsed.max_attempts, 12);
    assert!(reparsed.require_review_evidence);
    let mut assigned = ProductConfig::default_product();
    assigned
        .assign_key("orchestration.max_attempts", "7")
        .unwrap();
    assigned
        .assign_key("orchestration.require_review_evidence", "true")
        .unwrap();
    assert_eq!(assigned.max_attempts, 7);
    assert!(assigned.require_review_evidence);
}

#[test]
fn child_wake_policy_defaults_true_and_round_trips_as_a_home_key() {
    let mut config = ProductConfig::default_product();
    assert!(config.auto_continue);
    config.assign_key("turn.auto_continue", "false").unwrap();
    assert!(!config.auto_continue);
    assert!(!config.session_policy().auto_continue);
    let parsed = ProductConfig::parse(&config.render_annotated()).unwrap();
    assert!(!parsed.auto_continue);
    assert!(ConfigField::AutoContinue.is_home_only());
    assert_eq!(
        config_key("turn.auto_continue").unwrap().scope,
        ConfigScope::Immediate
    );
    assert!(ProductConfig::parse("[turn]\nauto_continue = 1\n").is_err());
}

#[test]
fn auto_continue_is_immediate_home_only_and_rejects_obsolete_key() {
    let mut config = ProductConfig::default_product();
    config.assign_key("turn.auto_continue", "false").unwrap();
    assert!(!config.session_policy().auto_continue);
    assert!(ConfigField::AutoContinue.is_home_only());
    assert_eq!(
        config_key("turn.auto_continue").unwrap().scope,
        ConfigScope::Immediate
    );
    assert!(ProductConfig::parse("[orchestration]\nwake_on_child_result = false\n").is_err());
}

#[test]
fn publication_faults_report_the_actual_pointer_commit() {
    for template_target in [true, false] {
        for stage in [
            AtomicStage::Write,
            AtomicStage::FileSync,
            AtomicStage::Publish,
            AtomicStage::ParentSync,
        ] {
            let (_tmp, paths) = PlakPaths::isolated_test_home();
            let mut old = ProductConfig::default_product();
            old.bind_root_tuple("grok", "old-model", None);
            old.save(&paths).unwrap();
            let mut candidate = ProductConfig::load(&paths).unwrap();
            candidate.bind_root_tuple("grok", "new-model", None);
            let mut seen = Vec::new();
            let error = candidate
                .save_with_fault(&paths, &mut |path, current| {
                    let is_template = path.parent() == Some(paths.user_agents_dir().as_path());
                    seen.push((is_template, current));
                    if is_template == template_target && current == stage {
                        Err("injected publication fault".into())
                    } else {
                        Ok(())
                    }
                })
                .unwrap_err();
            let committed = !template_target && stage == AtomicStage::ParentSync;
            assert_eq!(
                matches!(error, ConfigSaveError::PublishedDurabilityUncertain(_)),
                committed,
                "{template_target} {stage:?}: {error:?}"
            );
            let disk = ProductConfig::load(&paths).unwrap();
            assert_eq!(
                disk.default_model(),
                Some(if committed { "new-model" } else { "old-model" })
            );
            assert_eq!(
                fs::read_to_string(paths.user_agents_dir().join("default.toml")).unwrap(),
                render_template(old.root_template().unwrap())
            );
            if !template_target {
                let template_sync = seen
                    .iter()
                    .position(|row| *row == (true, AtomicStage::ParentSync))
                    .unwrap();
                let pointer_write = seen
                    .iter()
                    .position(|row| *row == (false, AtomicStage::Write))
                    .unwrap();
                assert!(template_sync < pointer_write);
            }
        }
    }
}

#[test]
fn second_template_failure_keeps_the_old_pointer() {
    let (_tmp, paths) = PlakPaths::isolated_test_home();
    let mut old = ProductConfig::default_product();
    old.bind_root_tuple("grok", "old-model", None);
    old.save(&paths).unwrap();
    let mut candidate = ProductConfig::load(&paths).unwrap();
    candidate
        .templates
        .insert("a-new".into(), template("grok", "new-a"));
    candidate
        .templates
        .insert("b-new".into(), template("grok", "new-b"));
    candidate.root_agent = Some("a-new".into());
    let error = candidate
        .save_with_fault(&paths, &mut |path, stage| {
            if path.file_name().is_some_and(|name| name == "b-new.toml")
                && stage == AtomicStage::Write
            {
                Err("second template refused".into())
            } else {
                Ok(())
            }
        })
        .unwrap_err();
    assert!(matches!(error, ConfigSaveError::BeforePublication(_)));
    assert!(paths.user_agents_dir().join("a-new.toml").is_file());
    assert!(!paths.user_agents_dir().join("b-new.toml").exists());
    assert_eq!(
        ProductConfig::load(&paths).unwrap().default_model(),
        Some("old-model")
    );
}

#[test]
fn failed_pointer_publication_keeps_existing_root_binding() {
    let (_tmp, paths) = PlakPaths::isolated_test_home();
    let mut cfg = ProductConfig::default_product();
    cfg.bind_root_tuple("grok", "grok-4.6", None);
    cfg.save(&paths).unwrap();
    let before = ProductConfig::load(&paths).unwrap();
    let mut candidate = before.clone();
    candidate.bind_root_tuple("grok", "new-model", None);
    // Stop after prerequisite template publication, before the pointer
    // commit. This is exactly the config-write failure window.
    candidate.save_templates(&paths).unwrap();
    let reloaded = ProductConfig::load(&paths).unwrap();
    assert_eq!(reloaded.default_model(), before.default_model());
    assert_eq!(reloaded.root_agent, before.root_agent);
}

#[test]
fn parent_sync_failure_is_reported_after_config_publication() {
    let (_tmp, paths) = PlakPaths::isolated_test_home();
    let mut cfg = ProductConfig::default_product();
    cfg.bind_root_tuple("grok", "grok-4.6", None);
    cfg.save_templates(&paths).unwrap();

    let error = cfg
        .save_with_fault(&paths, &mut |target, stage| {
            if target == paths.config_file() && stage == AtomicStage::ParentSync {
                Err("injected directory sync failure".into())
            } else {
                Ok(())
            }
        })
        .unwrap_err();

    assert_eq!(
        error,
        ConfigSaveError::PublishedDurabilityUncertain("injected directory sync failure".into())
    );
    assert_eq!(
        ProductConfig::load(&paths).unwrap().default_account(),
        Some("grok")
    );
}
