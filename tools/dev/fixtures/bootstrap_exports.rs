//! Inject only into the matching predecessor's carry tests, never a foreign decoder.

use super::*;

#[test]
#[ignore = "run through the bootstrap binding qualification recipe"]
fn export_bootstrap_frozen_heads() -> Result<(), Box<dyn std::error::Error>> {
    use plakous::sessions::{AgentSessions, SessionLocator};
    let output = std::path::PathBuf::from(std::env::var("PLAK_BOOTSTRAP_EXPORT_OUTPUT")?);
    fs::create_dir_all(&output)?;
    let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../crates/hosting/hosted/tests/fixtures/previous-release");
    let manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(fixture.join("manifest.json"))?)?;
    assert_eq!(manifest["runtime_schema"], 27);
    assert_eq!(manifest["runtime_epoch"], 37);
    let bytes = fs::read(fixture.join("previous-runtime.sqlite3"))?;
    assert_eq!(plakous::model::sha256_hex(&[&bytes]), manifest["sha256"]);
    let root = tempfile::tempdir()?;
    let paths = PlakPaths::explicit(root.path().join("home")).for_workspace(root.path());
    paths.ensure_layout()?;
    fs::create_dir_all(paths.runtime_database().parent().ok_or("database parent")?)?;
    fs::write(paths.runtime_database(), bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(paths.runtime_database(), fs::Permissions::from_mode(0o600))?;
    }
    let mut exports = Vec::new();
    for case in manifest["cases"].as_array().ok_or("fixture cases")? {
        let locator: SessionLocator = case["locator"].as_str().ok_or("locator")?.parse()?;
        // The old producer decodes its actual frozen native head. The new build
        // receives only the export, not this fixture or its native binding bytes.
        let history = AgentSessions::export_history_sqlite(paths.runtime_database(), locator)?;
        let mut record = SessionRecord::new(std::path::PathBuf::from("/"));
        record.plakous_locator = Some(locator);
        record.model = Some(history.replay_model().as_str().to_owned());
        record.effort = Some("high".to_owned());
        let composition = case["composition"].as_str().unwrap_or("direct");
        record.composition =
            crate::product_config::ProductCompositionSelection::parse(composition)?;
        record.account = Some(
            case["provider"]
                .as_str()
                .unwrap_or(if composition == "native-ptc" {
                    "codex"
                } else {
                    "claude"
                })
                .to_owned(),
        );
        if case["platform"] == true {
            record.target_selection = crate::product_config::ProductTargetSelection::Platform {
                provider: plakous_hosted::binding::PlatformProvider::OpenAi,
            };
        }
        record.title = case["authority"].as_str().map(str::to_owned);
        record.root_profile = Some(plakous_hosted::sessions::RootProfile {
            root_template: None,
            working_set: Vec::new(),
        });
        crate::store::sessions::save_session(&paths, &mut record)?;
        let document = export_session(&paths, &record).map_err(|error| error.to_string())?;
        assert_eq!(document.version, 12);
        let name = format!("{}.json", case["authority"].as_str().ok_or("authority")?);
        write_document(&output.join(&name), &document)?;
        exports.push(serde_json::json!({
            "case": case, "document": name, "source": record.id,
        }));
    }
    assert_eq!(exports.len(), 17);
    fs::write(
        output.join("manifest.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "producer": std::env::var("PLAK_BOOTSTRAP_SOURCE_COMMIT")?,
            "native_fixture": manifest["source_commit"],
            "native_sha256": manifest["sha256"],
            "exports": exports,
        }))?,
    )?;
    Ok(())
}
