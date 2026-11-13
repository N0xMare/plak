//! Decoded semantic refusals identify fields without returning their values.

use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn profile_put_names_empty_and_unknown_template_fields_without_echoing_values() -> TestResult
{
    let (_root, console) = fixture();
    publish_named_worker_profile(console.clone()).await?;
    for (root, working_set, field, code) in [
        ("worker", vec![], "body.working_set", ValidationCode::Empty),
        (
            "private-missing-root",
            vec!["worker"],
            "body.root",
            ValidationCode::UnknownTemplate,
        ),
        (
            "worker",
            vec!["private-missing-card"],
            "body.working_set[0]",
            ValidationCode::UnknownTemplate,
        ),
        (
            "worker",
            vec!["private invalid card"],
            "body.working_set",
            ValidationCode::InvalidValue,
        ),
        // A real profile ID is still the wrong namespace for a template field.
        (
            "work",
            vec!["worker"],
            "body.root",
            ValidationCode::UnknownTemplate,
        ),
        (
            "worker",
            vec!["work"],
            "body.working_set[0]",
            ValidationCode::UnknownTemplate,
        ),
    ] {
        let etag = revision(&console);
        let request = HostRequest::ProfilePut {
            id: "work".into(),
            body: ProfileBody {
                root: root.into(),
                working_set: working_set.iter().map(|name| (*name).into()).collect(),
            },
            if_match: etag.clone(),
            confirmation: consent(
                &console,
                1,
                OperatorAction::ProfilePut {
                    id: "work".into(),
                    if_match: etag.clone(),
                },
            ),
        };
        let response = console.clone().request(owner(1), request).await.unwrap();
        let encoded = serde_json::to_string(&response)?;
        assert!(
            !encoded.contains("private"),
            "a refusal must not echo supplied template values"
        );
        let HostResponse::Refused {
            reason: ReadRefusal::ValidationFailed { issues },
        } = response
        else {
            panic!("expected a structured profile refusal")
        };
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].field, field);
        assert_eq!(issues[0].code, code);
        assert_ne!(issues[0].detail, "");
        assert!(issues[0].detail.len() <= 512);
        if code == ValidationCode::Empty {
            assert!(issues[0].detail.contains("at least one template"));
        }
        assert_eq!(
            revision(&console),
            etag,
            "validation must not publish a definition"
        );
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_decoded_config_value_of_the_wrong_kind_names_the_setting_without_its_value() -> TestResult
{
    let (_root, console) = fixture();
    let etag = revision(&console);
    let request = config(
        &console,
        1,
        etag.clone(),
        "roster.working_set",
        SettingValue::String("private-value-must-not-escape".into()),
    );
    let response = console.clone().request(owner(1), request).await.unwrap();
    assert!(!serde_json::to_string(&response)?.contains("private-value"));
    let HostResponse::Refused {
        reason: ReadRefusal::ValidationFailed { issues },
    } = response
    else {
        panic!("expected a structured type refusal")
    };
    assert_eq!(issues.len(), 1);
    assert_eq!(issues[0].field, "roster.working_set");
    assert_eq!(issues[0].code, ValidationCode::TypeMismatch);
    assert!(issues[0].detail.contains("wrong kind"));
    assert_eq!(revision(&console), etag);
    Ok(())
}
