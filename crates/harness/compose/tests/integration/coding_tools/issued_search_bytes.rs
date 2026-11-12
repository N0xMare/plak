//! Issued logs preserve lossy-text matches but report binary bytes explicitly.

use super::*;

#[cfg(unix)]
#[tokio::test]
async fn issued_search_reports_nul_as_binary_for_literal_and_regex() -> TestResult {
    for bytes in [
        b"needle-before\nbinary\0line\nneedle-after".as_slice(),
        b"needle-before\nneedle-\0-after",
    ] {
        let harness = LargeLogHarness::with_log(0xe1, bytes, Vec::new())?;
        let regex = harness
            .grep(
                0xe1,
                serde_json::json!({"pattern":"needle", "path":harness.path}),
            )
            .await?;
        assert!(
            regex.contains("1 binary or non-UTF-8 files were not searched"),
            "{regex}"
        );
        assert!(
            !regex.contains('\0') && !regex.contains("needle-before"),
            "{regex}"
        );
        let literal = execute(
            &harness.registry,
            harness.environment.clone(),
            ids(0xe2)?,
            "search_repository",
            &serde_json::json!({"query":"needle", "path":harness.path}).to_string(),
        )
        .await?;
        let literal = text(&literal)?;
        assert!(literal.contains("skipped_binary=1"), "{literal}");
        assert!(
            !literal.contains('\0') && !literal.contains("needle-before"),
            "{literal}"
        );
        assert_workspace_binary(&harness, bytes).await?;
        harness.sessions.join_all()?;
    }
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn issued_search_keeps_later_matches_after_invalid_utf8_in_literal_and_regex() -> TestResult {
    for bytes in [
        b"needle-before\ninvalid \xff\nneedle-after".as_slice(),
        b"needle-before\nneedle-after \xe2\x82",
    ] {
        let harness = LargeLogHarness::with_log(0xe3, bytes, Vec::new())?;
        let regex = harness
            .grep(
                0xe3,
                serde_json::json!({"pattern":"needle", "path":harness.path}),
            )
            .await?;
        let literal = execute(
            &harness.registry,
            harness.environment.clone(),
            ids(0xe4)?,
            "search_repository",
            &serde_json::json!({"query":"needle", "path":harness.path}).to_string(),
        )
        .await?;
        for rendered in [&regex, text(&literal)?] {
            assert!(
                rendered.contains("1 lines with invalid UTF-8 were matched with U+FFFD"),
                "{rendered}"
            );
            assert!(
                rendered.contains("needle-before") && rendered.contains("needle-after"),
                "{rendered}"
            );
            assert!(!rendered.contains('\0'), "{rendered}");
        }
        assert_workspace_binary(&harness, bytes).await?;
        harness.sessions.join_all()?;
    }
    Ok(())
}

#[cfg(unix)]
async fn assert_workspace_binary(harness: &LargeLogHarness, bytes: &[u8]) -> TestResult {
    fs::write(harness.workspace.path().join("bytes.log"), bytes)?;
    let outcome = execute(
        &harness.registry,
        harness.environment.clone(),
        ids(0xe5)?,
        "search_repository",
        r#"{"query":"needle", "path":"."}"#,
    )
    .await?;
    let rendered = text(&outcome)?;
    assert!(
        rendered.contains("matches=0") && rendered.contains("skipped_binary=1"),
        "{rendered}"
    );
    Ok(())
}
