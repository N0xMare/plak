use super::*;
use plakous_host_protocol::notice_remedy::{AccountTarget, NoticeRemedy};

#[test]
fn action_refusals_keep_typed_facts_in_each_frontend_without_parsing_prose() {
    let facts = [
        RefusalAdvice::CheckStorage,
        RefusalAdvice::UnknownModel {
            model: "missing".into(),
            account: "custom".into(),
            catalog: "reviewed".into(),
        },
        RefusalAdvice::ExplicitEffort {
            account: "custom".into(),
            model: "model".into(),
            offered: vec!["high".into(), "max".into()],
        },
        RefusalAdvice::McpLogin {
            server: "docs".into(),
            scopes: vec!["mcp:tools".into(), "offline_access".into()],
        },
        RefusalAdvice::SecretMissing {
            server: "docs".into(),
            name: "client-secret".into(),
        },
        RefusalAdvice::ChildCredential {
            template: "review".into(),
            account: AccountTarget::Grok,
            state: plakous_host_protocol::refusal::ChildCredentialState::Unavailable,
        },
        RefusalAdvice::ExportSession {
            session_id: "00112233-4455-6677-8899-aabbccddeeff".parse().unwrap(),
        },
    ];
    for frontend in [Frontend::Tui, Frontend::Run, Frontend::Jsonl] {
        for advice in &facts {
            let failure =
                crate::CliFailure::new("opaque neutral diagnosis").with_advice(advice.clone());
            let rendered = failure_text(
                &failure,
                RemedyContext {
                    frontend,
                    session_id: Some("current"),
                },
            );
            if matches!(advice, RefusalAdvice::ExplicitEffort { .. }) && frontend != Frontend::Tui {
                assert!(rendered.starts_with("custom model `model` requires --effort "));
            } else {
                assert!(rendered.starts_with("opaque neutral diagnosis; "));
            }
            assert!(rendered.contains(&advice_text(advice, frontend)));
            assert_eq!(failure.host_failure().advice.as_deref(), Some(advice));
        }
        let raw = serde_json::json!({"code":"tree_restored_paused","message":"opaque"}).to_string();
        let restored = tool_error_text(
            "spawn_agent",
            &raw,
            RemedyContext {
                frontend,
                session_id: Some("exact-root"),
            },
        )
        .unwrap();
        assert!(restored.contains(&crate::notice_text::remedy_text(
            &NoticeRemedy::ResumeTree {},
            RemedyContext {
                frontend,
                session_id: Some("exact-root")
            }
        )));
        assert!(
            tool_error_text(
                "spawn_agent",
                "opaque /tree resume",
                RemedyContext {
                    frontend,
                    session_id: None
                }
            )
            .is_none()
        );
        assert!(
            tool_error_text(
                "read_file",
                &raw,
                RemedyContext {
                    frontend,
                    session_id: None
                }
            )
            .is_none()
        );
    }
}

#[test]
fn mcp_login_help_preserves_scope_quoting_in_every_frontend() {
    let scoped = RefusalAdvice::McpLogin {
        server: "docs".into(),
        scopes: vec!["mcp:tools".into(), "offline_access".into()],
    };
    assert!(
        advice_text(&scoped, Frontend::Tui)
            .contains("plak mcp login docs --scope 'mcp:tools offline_access'")
    );
    let quoted = RefusalAdvice::McpLogin {
        server: "docs".into(),
        scopes: vec!["scope'word".into()],
    };
    assert!(advice_text(&quoted, Frontend::Run).contains(r"--scope 'scope'\''word'"));
    for frontend in [Frontend::Tui, Frontend::Run, Frontend::Jsonl] {
        let raw = serde_json::json!({"mcp_authorization":{"message":"opaque authorization refusal; the call did not run","server":"docs","scope":"mcp:tools offline_access"}}).to_string();
        let text = tool_error_text(
            "mcp__docs__read",
            &raw,
            RemedyContext {
                frontend,
                session_id: None,
            },
        )
        .unwrap();
        assert!(text.contains("the call did not run"));
        assert!(text.contains("plak mcp login docs --scope 'mcp:tools offline_access'"));
    }
}

#[test]
fn live_credential_activity_and_abandonment_provenance_remain_typed() {
    let progress = plakous_host_protocol::views::ChildProgress {
        activity: Some("opaque".into()),
        credential_wait: Some("grok".into()),
        ..Default::default()
    };
    assert_eq!(
        child_progress(progress).activity.as_deref(),
        Some("waiting for login: run `plak login grok`")
    );
}

#[test]
fn hosted_tool_previews_preserve_refusal_targets_before_frontend_formatting() {
    use plakous_host_protocol::topology::{TopologyError, TopologyErrorCode};
    let raw = TopologyError::new(
        TopologyErrorCode::ModelNotAdmitted,
        "opaque failure\nwith facts",
    )
    .with_advice(RefusalAdvice::ChildCredential {
        template: "review".into(),
        account: AccountTarget::Grok,
        state: plakous_host_protocol::refusal::ChildCredentialState::Unavailable,
    })
    .to_value()
    .to_string();
    let detail = plakous_hosted::activity::tool_detail_text(
        Some(r#"{"template":"review","prompt":"work"}"#),
        Some(&raw),
    )
    .unwrap();
    assert_eq!(
        detail, raw,
        "argument and readable-preview projection must not erase typed repair data"
    );
    for frontend in [Frontend::Tui, Frontend::Run, Frontend::Jsonl] {
        let text = tool_error_text(
            "spawn_agent",
            &detail,
            RemedyContext {
                frontend,
                session_id: Some("root"),
            },
        )
        .unwrap();
        assert!(text.contains("opaque failure\nwith facts") && text.contains("plak login grok"));
    }
    let scope = vec!["a"; 512].join(" ");
    let raw = serde_json::json!({"mcp_authorization":{"message":"additional scope required; the call did not run","server":"docs","scope":scope}}).to_string();
    assert!(raw.len() <= plakous_hosted::sessions::MAX_TOOL_ACTIVITY_DETAIL_BYTES);
    let detail =
        plakous_hosted::activity::tool_detail_text(Some(r#"{"query":"work"}"#), Some(&raw))
            .unwrap();
    for frontend in [Frontend::Tui, Frontend::Run, Frontend::Jsonl] {
        let text = tool_error_text(
            "mcp__docs__read",
            &detail,
            RemedyContext {
                frontend,
                session_id: None,
            },
        )
        .unwrap();
        assert!(text.contains(&scope));
    }
}

#[test]
fn an_mcp_secret_payload_renders_secret_set_for_the_same_server() {
    let raw = serde_json::json!({
        "mcp_secret": {
            "message": "secret token is not set; the call was not sent",
            "server": "docs",
            "name": "token"
        }
    })
    .to_string();
    let text = tool_error_text(
        "mcp__docs__read",
        &raw,
        RemedyContext {
            frontend: Frontend::Jsonl,
            session_id: None,
        },
    )
    .unwrap();
    assert!(text.contains("plak secret set token"), "{text}");
    assert!(
        tool_error_text(
            "mcp__other__read",
            &raw,
            RemedyContext {
                frontend: Frontend::Jsonl,
                session_id: None,
            },
        )
        .is_none()
    );
}
