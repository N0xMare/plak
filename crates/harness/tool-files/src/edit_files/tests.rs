use super::*;
use plakous_model::{CancellationToken, RunId};
use plakous_tools::{
    ExecutionEnvironment, ExecutionEnvironmentConfig, ExecutionEnvironmentScope,
    ExecutionOperationControl,
};
use std::str::FromStr;

#[tokio::test]
async fn sdk_dot_path_commit_races_keep_the_exact_guard_refusal()
-> Result<(), Box<dyn std::error::Error>> {
    for exists in [false, true] {
        let root = tempfile::tempdir()?;
        let path = PathBuf::from("./a");
        let original = if exists {
            std::fs::write(root.path().join(&path), "old")?;
            WorkspaceExpectedFileState::Content(WorkspaceContentIdentity::from_bytes(b"old")?)
        } else {
            WorkspaceExpectedFileState::Missing
        };
        let files = [(
            path.clone(),
            FileState {
                original,
                original_is_binary: false,
                bytes: Some(b"replacement".to_vec()),
            },
        )]
        .into_iter()
        .collect();
        let mut guards = [(path, original)].into_iter().collect();
        let transaction = build_transaction(files, &mut guards)?;
        assert_eq!(transaction.mutations()[0].path(), Path::new("a"));
        std::fs::write(root.path().join("a"), "race")?;
        let run = RunId::from_str("00000000-0000-0000-0000-000000000041")?;
        let environment = ExecutionEnvironment::host(
            ExecutionEnvironmentScope::Run(run),
            root.path(),
            ".",
            ExecutionEnvironmentConfig::default(),
        )?;
        let error = environment
            .mutate_workspace(
                transaction,
                ExecutionOperationControl::new(CancellationToken::new()),
            )
            .await
            .err()
            .ok_or("expected commit conflict")?;
        let outcome = mutation_failure(error, &guards)?;
        assert_eq!(outcome.kind(), plakous_tools::ToolOutcomeKind::Failure);
        let expected = crate::mutation::write_refusal(Path::new("a"), exists);
        assert!(
            matches!(outcome.parts().first(), Some(plakous_model::ToolResultPart::Text(text)) if text.text() == expected)
        );
        assert_eq!(std::fs::read(root.path().join("a"))?, b"race");
    }
    Ok(())
}
