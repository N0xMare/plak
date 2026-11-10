use super::test_kit::{runner_serving, write_skill};
use super::*;

#[test]
fn remote_skills_raw_instruction_reads_refuse_escaping_links_and_non_root_paths() {
    let root = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(root.path()).unwrap();
    let workspace = root.join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let home = root.join("runner-home");
    write_skill(&home, "inside");
    let outside = root.join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("SKILL.md"), "not served").unwrap();
    let user_root = home.join(".agents/skills");
    std::os::unix::fs::symlink(&outside, user_root.join("escape")).unwrap();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let (services, client) = runner_serving(&runtime, &workspace, Some(&home));
    for (path, reason) in [
        (
            user_root.join("escape/SKILL.md"),
            "outside the skills directory",
        ),
        (outside.join("SKILL.md"), "does not serve this workspace"),
    ] {
        for op in ["read_file", "directory_entries"] {
            let payload = json!({"op": op, "path": bytes(&path), "max_len": 1024, "limit": 1024});
            let failure = runtime
                .block_on(
                    services
                        .backend()
                        .link()
                        .call(RunnerCall::unkeyed(RunnerVerb::ReadInstructions, payload)),
                )
                .unwrap_err();
            assert!(
                matches!(failure, RunnerFailure::Backend(ref error)
                if error["message"].as_str().is_some_and(|message| message.contains(reason))),
                "{op}: {failure:?}"
            );
        }
    }
    client.disconnect();
}
