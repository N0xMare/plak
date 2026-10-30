use std::str::FromStr;

use super::*;
use crate::{ExecutionEnvironmentConfig, ExecutionEnvironmentScope};

#[test]
fn read_observations_are_bounded_shared_contained_and_attachment_local()
-> Result<(), Box<dyn std::error::Error>> {
    let root = tempfile::tempdir()?;
    let run = RunId::from_str("00000000-0000-0000-0000-000000000041")?;
    let other = RunId::from_str("00000000-0000-0000-0000-000000000042")?;
    let environment = ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run(run),
        root.path(),
        ".",
        ExecutionEnvironmentConfig::default(),
    )?;
    let clone = environment.clone();
    let content = WorkspaceContentIdentity::from_bytes(b"old")?;
    for index in 0..=MAX_READ_OBSERVATIONS {
        let path = PathBuf::from(format!("file-{index}"));
        std::fs::write(root.path().join(&path), "old")?;
        environment.record_text_read(run, &path, content);
    }
    assert_eq!(clone.last_text_read(run, Path::new("file-0")), None);
    let path = Path::new("file-1");
    assert_eq!(clone.last_text_read(run, path), Some(content));
    assert_eq!(
        clone.last_text_read(run, &root.path().canonicalize()?.join(path)),
        Some(content)
    );
    assert_eq!(clone.last_text_read(other, path), None);
    assert_eq!(clone.last_text_read(run, Path::new("../outside")), None);
    let fresh = ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run(run),
        root.path(),
        ".",
        ExecutionEnvironmentConfig::default(),
    )?;
    assert_eq!(fresh.last_text_read(run, path), None);
    clone.invalidate_text_read(run, path);
    assert_eq!(environment.last_text_read(run, path), None);
    environment.record_text_read(run, path, content);
    std::fs::remove_file(root.path().join(path))?;
    clone.invalidate_text_read(run, path);
    std::fs::write(root.path().join(path), "old")?;
    assert_eq!(environment.last_text_read(run, path), None);
    Ok(())
}
