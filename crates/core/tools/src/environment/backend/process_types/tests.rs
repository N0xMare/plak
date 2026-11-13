use super::*;
use std::os::unix::fs::PermissionsExt as _;

#[test]
fn shell_replacement_keeps_the_contract_and_revalidates_runtime_file_facts()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let shell = directory.path().join("shell");
    let replacement = directory.path().join("replacement");
    for (path, bytes) in [
        (&shell, b"first".as_slice()),
        (&replacement, b"second".as_slice()),
    ] {
        fs::write(path, bytes)?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    let profile = JoinedProcessShellProfile::new(&shell, ["-c".to_owned()])?;
    fs::rename(&replacement, &shell)?;
    let updated = JoinedProcessShellProfile::new(&shell, ["-c".to_owned()])?;
    assert_ne!(
        profile.identity().executable_inode(),
        updated.identity().executable_inode()
    );
    assert_eq!(
        profile.identity().binding_identity(),
        updated.identity().binding_identity()
    );
    assert_eq!(profile.revalidated_executable()?, shell);
    fs::set_permissions(&shell, fs::Permissions::from_mode(0o600))?;
    assert!(profile.revalidated_executable().is_err());
    fs::remove_file(&shell)?;
    assert!(profile.revalidated_executable().is_err());
    fs::create_dir(&shell)?;
    assert!(profile.revalidated_executable().is_err());
    Ok(())
}
