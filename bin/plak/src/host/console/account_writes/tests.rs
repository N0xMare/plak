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
    assert_eq!(secrets::resolve_api_key(&paths, "openai/missing")?, None);
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
