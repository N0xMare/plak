use super::*;
use crate::home::{paths::PlakPaths, secrets};
use plakd_wire_types::console::{ConfiguredSwitches, SecretInput};
use zeroize::Zeroizing;

fn owner() -> HostRequestContext {
    HostRequestContext {
        principal: HostPrincipal::Socket(7),
        connection_id: 3,
        host_generation: 4,
        request_id: 5,
    }
}
fn confirmation(console: &Console, action: OperatorAction) -> String {
    let Ok(HostResponse::OperatorPrepared { consent }) = console.prepare(owner(), action) else {
        panic!("prepare")
    };
    let Ok(HostResponse::OperatorConfirmed { confirmation, .. }) =
        console.confirm(&owner(), &consent.nonce)
    else {
        panic!("confirm")
    };
    confirmation
}
fn secret(value: &str) -> SecretInput {
    SecretInput::new(Zeroizing::new(value.into())).expect("bounded fixture")
}

fn seed_default_key(paths: &PlakPaths) -> std::io::Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(paths.api_key_file("openai"))?
        .write_all(b"synthetic-default-key")
}

#[test]
fn successful_key_writes_emit_audit_without_secret_values() -> Result<(), Box<dyn std::error::Error>>
{
    let output = std::process::Command::new(std::env::current_exe()?)
        .args(["--exact", "host::console::account_writes::tests::wire_key_creation_is_write_only_exact_named_and_confirmation_is_consumed", "--nocapture"])
        .output()?;
    assert!(output.status.success(), "isolated key fixture failed");
    assert!(String::from_utf8_lossy(&output.stderr).contains("\"audit\":\"operator\""));
    for bytes in [&output.stdout, &output.stderr] {
        assert!(!String::from_utf8_lossy(bytes).contains("synthetic-console-secret"));
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wire_key_creation_is_write_only_exact_named_and_confirmation_is_consumed()
-> Result<(), Box<dyn std::error::Error>> {
    let root = tempfile::tempdir()?;
    let paths = PlakPaths::explicit(root.path().join("home"));
    let console = Console::new(
        paths.clone(),
        root.path().join("state"),
        Some(tokio::runtime::Handle::current()),
        ConfiguredSwitches {
            web_accounts_read: false,
            web_config_read: false,
            web_credentials_write: false,
            web_config_write: false,
        },
    );
    let account_id = "openai/work".to_owned();
    let token = confirmation(
        &console,
        OperatorAction::AccountKeySet {
            account_id: account_id.clone(),
        },
    );
    let request = HostRequest::AccountKeySet {
        account_id: account_id.clone(),
        secret: secret("synthetic-console-secret"),
        confirmation: token,
    };
    assert!(!format!("{request:?}").contains("synthetic-console-secret"));
    let response = console
        .request(owner(), request.clone())
        .await
        .map_err(|_| "request")?;
    assert!(matches!(response, HostResponse::AccountChanged { .. }));
    assert!(!serde_json::to_string(&response)?.contains("synthetic-console-secret"));
    assert_eq!(
        secrets::resolve_api_key(&paths, &account_id)?.as_deref(),
        Some("synthetic-console-secret")
    );
    seed_default_key(&paths)?;
    assert_eq!(secrets::resolve_api_key(&paths, "openai/missing")?, None);
    assert_eq!(
        secrets::key_status(&paths, "openai/missing")?.source,
        secrets::KeySource::Missing
    );
    assert!(matches!(
        console.request(owner(), request).await,
        Ok(HostResponse::Refused {
            reason: ReadRefusal::ConfirmationRequired {}
        })
    ));
    let action = OperatorAction::AccountRemove {
        account_id: account_id.clone(),
    };
    let request = HostRequest::AccountRemove {
        account_id: account_id.clone(),
        confirmation: confirmation(&console, action),
    };
    assert!(matches!(
        console.request(owner(), request).await,
        Ok(HostResponse::AccountChanged { .. })
    ));
    assert_eq!(secrets::resolve_api_key(&paths, &account_id)?, None);
    assert!(matches!(
        console.prepare(
            owner(),
            OperatorAction::AccountRemove {
                account_id: "openai".into()
            }
        ),
        Ok(HostResponse::Refused { .. })
    ));
    Ok(())
}

#[test]
fn secret_decode_bounds_whitespace_and_never_includes_input_in_debug_or_errors() {
    let secret = secret("synthetic-private-paste");
    assert!(!format!("{secret:?}").contains("synthetic-private-paste"));
    let too_large =
        serde_json::to_string(&" ".repeat(SecretInput::MAX_BYTES + 1)).expect("fixture");
    let error = serde_json::from_str::<SecretInput>(&too_large).expect_err("byte bound");
    assert!(!error.to_string().contains(&" ".repeat(100)));
    assert!(SecretInput::new(Zeroizing::new("x".repeat(SecretInput::MAX_BYTES + 1))).is_err());
}

#[test]
fn an_effectful_completion_is_not_downgraded_by_its_error_wording() {
    use crate::commands::application::host::HostAccountError;
    let cause = || {
        plakous_hosted::error::CliFailure::from(
            plakous_hosted::capabilities::AccountAdmissionError::AccountBusy {
                account: "claude/work".into(),
            },
        )
    };
    assert!(matches!(
        host_failure(HostAccountError::Refused(cause())).response(),
        HostResponse::Refused {
            reason: ReadRefusal::Busy { .. }
        }
    ));
    assert!(matches!(
        host_failure(HostAccountError::OutcomeUnknown(cause())).response(),
        HostResponse::OperationOutcomeUnknown {}
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn environment_override_proves_no_effect_for_wire_set_and_clear()
-> Result<(), Box<dyn std::error::Error>> {
    const CHILD: &str = "PLAK_CONSOLE_ENV_FIXTURE";
    if std::env::var_os(CHILD).is_none() {
        let output = std::process::Command::new(std::env::current_exe()?)
            .args(["--exact", "host::console::account_writes::tests::environment_override_proves_no_effect_for_wire_set_and_clear", "--nocapture"])
            .env(CHILD, "1").env("OPENAI_API_KEY", "synthetic-override-secret").output()?;
        assert!(
            output.status.success(),
            "isolated environment fixture failed"
        );
        assert!(!String::from_utf8_lossy(&output.stderr).contains("synthetic-override-secret"));
        return Ok(());
    }
    let root = tempfile::tempdir()?;
    let paths = PlakPaths::explicit(root.path().join("home"));
    paths.ensure_layout()?;
    let path = paths.api_key_file("openai");
    std::fs::write(&path, "unchanged-file")?;
    let console = Console::new(
        paths,
        root.path().join("state"),
        Some(tokio::runtime::Handle::current()),
        ConfiguredSwitches {
            web_accounts_read: false,
            web_config_read: false,
            web_credentials_write: false,
            web_config_write: false,
        },
    );
    for set in [true, false] {
        let action = if set {
            OperatorAction::AccountKeySet {
                account_id: "openai".into(),
            }
        } else {
            OperatorAction::AccountKeyClear {
                account_id: "openai".into(),
            }
        };
        let token = confirmation(&console, action);
        let request = if set {
            HostRequest::AccountKeySet {
                account_id: "openai".into(),
                secret: secret("synthetic-new-secret"),
                confirmation: token,
            }
        } else {
            HostRequest::AccountKeyClear {
                account_id: "openai".into(),
                confirmation: token,
            }
        };
        assert!(matches!(
            console.request(owner(), request).await,
            Ok(HostResponse::Refused {
                reason: ReadRefusal::EnvironmentOverride {
                    proven_no_effect: true,
                    ..
                }
            })
        ));
        assert_eq!(std::fs::read_to_string(&path)?, "unchanged-file");
    }
    Ok(())
}

#[tokio::test]
async fn key_store_path_refusals_are_no_effect_not_unknown()
-> Result<(), Box<dyn std::error::Error>> {
    let root = tempfile::tempdir()?;
    let paths = PlakPaths::explicit(root.path().join("home"));
    std::fs::write(paths.home(), "not-a-directory")?;
    let console = Console::new(
        paths.clone(),
        root.path().join("state"),
        None,
        ConfiguredSwitches {
            web_accounts_read: false,
            web_config_read: false,
            web_credentials_write: false,
            web_config_write: false,
        },
    );
    for set in [true, false] {
        let account_id = "openai/work".to_owned();
        let action = if set {
            OperatorAction::AccountKeySet {
                account_id: account_id.clone(),
            }
        } else {
            OperatorAction::AccountKeyClear {
                account_id: account_id.clone(),
            }
        };
        let token = confirmation(&console, action);
        let request = if set {
            HostRequest::AccountKeySet {
                account_id,
                secret: secret("synthetic-key"),
                confirmation: token,
            }
        } else {
            HostRequest::AccountKeyClear {
                account_id,
                confirmation: token,
            }
        };
        assert!(matches!(
            Arc::clone(&console).account_write(owner(), request).await,
            Ok(HostResponse::Refused {
                reason: ReadRefusal::AccountOperationFailed {}
            })
        ));
        assert_eq!(std::fs::read_to_string(paths.home())?, "not-a-directory");
    }
    Ok(())
}
