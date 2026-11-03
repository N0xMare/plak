use super::*;
use plakous_model::{CancellationToken, RunId};
use plakous_tools::{
    ExecutionEnvironment, ExecutionEnvironmentConfig, ExecutionEnvironmentScope, OperationContext,
    WorkspaceContentIdentity, WorkspaceMutation, WorkspaceReplacement,
};
use std::str::FromStr;

#[tokio::test]
async fn commit_preflight_conflicts_name_the_guarded_target_and_change_no_files()
-> Result<(), Box<dyn std::error::Error>> {
    for exists in [false, true] {
        let root = tempfile::tempdir()?;
        let path = PathBuf::from("z-guarded");
        let expected = if exists {
            std::fs::write(root.path().join(&path), "old")?;
            WorkspaceExpectedFileState::Content(WorkspaceContentIdentity::from_bytes(b"old")?)
        } else {
            WorkspaceExpectedFileState::Missing
        };
        let transaction = WorkspaceMutationTransaction::new(vec![
            WorkspaceMutation::new(
                "a-unrelated",
                WorkspaceExpectedFileState::Missing,
                WorkspaceReplacement::content(b"no effect".to_vec()),
            )?,
            WorkspaceMutation::new(
                &path,
                expected,
                WorkspaceReplacement::content(b"replacement".to_vec()),
            )?,
        ])?;
        // Another writer changes/creates the target after preparation but
        // before the backend's all-file commit preflight.
        std::fs::write(root.path().join(&path), "new")?;
        let run = RunId::from_str("00000000-0000-0000-0000-000000000041")?;
        let environment = ExecutionEnvironment::host(
            ExecutionEnvironmentScope::Run(run),
            root.path(),
            ".",
            ExecutionEnvironmentConfig::default(),
        )?;
        let result = environment
            .mutate_workspace(transaction, OperationContext::new(CancellationToken::new()))
            .await;
        let error = result.err().ok_or("expected conflict")?;
        let guards = [(path.clone(), expected)].into_iter().collect();
        assert_eq!(
            guarded_conflict(&error, &guards),
            Some(write_refusal(&path, exists))
        );
        assert_eq!(std::fs::read(root.path().join(&path))?, b"new");
        assert!(!root.path().join("a-unrelated").exists());
    }
    Ok(())
}

#[test]
fn oversized_named_result_is_refused_before_mutation() -> Result<(), Box<dyn std::error::Error>> {
    let mutations = (0..256)
        .map(|index| {
            WorkspaceMutation::new(
                format!("{index:03}-{}", "\n".repeat(200)),
                WorkspaceExpectedFileState::Missing,
                WorkspaceReplacement::content(Vec::new()),
            )
        })
        .collect::<Result<Vec<_>, _>>()?;
    let transaction = WorkspaceMutationTransaction::new(mutations)?;
    assert!(preflight_result(&transaction).is_err());
    Ok(())
}
