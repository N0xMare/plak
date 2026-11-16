//! Actual predecessor exports enter the ordinary product importer, not native reopen.

use super::*;

#[test]
fn host_upgrade_bootstrap_imports_all_seventeen_frozen_predecessor_presentations() {
    let Some(fixture) = fixture() else { return };
    let bundle: serde_json::Value =
        serde_json::from_str(include_str!("../../../fixtures/bootstrap-v12.json")).unwrap();
    let exports = bundle["exports"].as_array().unwrap();
    assert_eq!(exports.len(), 17);
    let paths = plakd::home::paths::PlakPaths::explicit(fixture.home());
    for (index, exported) in exports.iter().enumerate() {
        let mut document = exported["document"].clone();
        assert_eq!(document["version"], 12);
        assert_eq!(document["source"]["state_key"], "r27.e37.p37.t10");
        // The old binding fixture's inert `/` cannot be an executable product
        // workspace. Relocate only that portable placement into this scratch
        // workspace; no predecessor checkpoint or binding is rewritten.
        document["workspace_root"] = json!(fixture.workspace());
        let input = fixture.root.join(format!("bootstrap-{index}.json"));
        write_private(&input, &document.to_string());
        let imported = output(
            &fixture,
            &["session", "import", input.to_str().unwrap()],
            false,
        );
        assert!(
            imported.status.success(),
            "case {}: {imported:?}",
            exported["case"]["authority"]
        );
        let id = String::from_utf8(imported.stdout)
            .unwrap()
            .lines()
            .find_map(|line| line.strip_prefix("resume with: plakd run --session "))
            .unwrap()
            .to_owned();
        assert_ne!(id, document["source"]["session_id"]);
        let record = plakd::store::sessions::load_session(&paths, &id).unwrap();
        assert_eq!(
            record.carried_forward_from(),
            Some((
                document["source"]["session_id"].as_str().unwrap(),
                "r27.e37.p37.t10",
            ))
        );
        let source: plakous::sessions::SessionLocator = exported["case"]["locator"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap();
        let head = record.plakous_locator.unwrap();
        assert_ne!(source.session_id(), head.session_id());
        assert_ne!(source.branch_id(), head.branch_id());
        let carried = export(
            &fixture,
            &id,
            false,
            &format!("bootstrap-result-{index}.json"),
        );
        assert_eq!(carried["binding"], document["binding"]);
        assert_eq!(carried["named_state"], document["named_state"]);
        assert_eq!(carried["history"]["groups"], document["history"]["groups"]);
    }
    assert_eq!(crash::dispatches(&fixture), 0);
}
