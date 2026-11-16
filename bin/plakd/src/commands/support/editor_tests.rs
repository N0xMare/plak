use super::editor_command;
use std::path::Path;

#[test]
fn editor_arguments_and_path_are_literal_not_shell_source() {
    let path = Path::new("config with spaces;$(touch nope).toml");
    let command = editor_command("'editor with spaces' -w '$HOME' '$(touch nope)'", path).unwrap();
    assert_eq!(command.get_program(), "editor with spaces");
    assert_eq!(
        command.get_args().collect::<Vec<_>>(),
        ["-w", "$HOME", "$(touch nope)", path.to_str().unwrap()]
    );
    let command = editor_command("code -w", path).unwrap();
    assert_eq!(command.get_program(), "code");
    assert_eq!(
        command.get_args().collect::<Vec<_>>(),
        ["-w", path.to_str().unwrap()]
    );
}

#[test]
fn invalid_editor_is_refused() {
    for editor in ["", "''", "code 'unclosed", "code \"unclosed"] {
        assert!(
            editor_command(editor, Path::new("config.toml")).is_err(),
            "{editor}"
        );
    }
}

#[test]
fn editor_process_receives_path_without_executing_substitutions() {
    use std::os::unix::fs::PermissionsExt as _;
    let root = tempfile::tempdir().unwrap();
    let program = root.path().join("fake editor");
    let output = root.path().join("arguments");
    let marker = root.path().join("must-not-exist");
    std::fs::write(
        &program,
        "#!/bin/sh\nprintf '%s\\n' \"$@\" >\"$EDITOR_TEST_ARGS\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
    let editor = format!("'{}' -w $(touch {})", program.display(), marker.display());
    let path = root.path().join("config with spaces.toml");
    let status = editor_command(&editor, &path)
        .unwrap()
        .env("EDITOR_TEST_ARGS", &output)
        .status()
        .unwrap();
    assert!(status.success());
    assert!(!marker.exists());
    let arguments = std::fs::read_to_string(output).unwrap();
    assert!(arguments.starts_with("-w\n$(touch\n"));
    assert!(arguments.ends_with(&format!("{}\n", path.display())));
}
