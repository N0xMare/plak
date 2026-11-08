//! Attachment-free product reads. Totals describe measured retained facts,
//! never a provider invoice or a promise of whole-home coverage.

use serde::{Deserialize, Serialize};

mod accounts;
pub use accounts::*;
mod welcome;
pub use welcome::*;
mod definitions;
pub use definitions::*;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "request", rename_all = "snake_case", deny_unknown_fields)]
pub enum HostRequest {
    ProfilesList {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cursor: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        limit: Option<u64>,
    },
    ProfilesGet {
        id: String,
    },
    TemplatesList {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cursor: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        limit: Option<u64>,
    },
    TemplatesGet {
        id: String,
    },
    ConfigGet {},
    ConsoleMetadataGet {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        account_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cursor: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        limit: Option<u64>,
    },
    AccountsStatus {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        account_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cursor: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        limit: Option<u64>,
    },
    AccountsUsage {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        account_id: Option<String>,
        window: UsageWindow,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cursor: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        limit: Option<u64>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "response", rename_all = "snake_case", deny_unknown_fields)]
pub enum HostResponse {
    ProfilesList {
        etag: String,
        #[serde(deserialize_with = "required")]
        #[schemars(required, schema_with = "nullable::<String>")]
        current: Option<String>,
        rows: Vec<ProfileSummary>,
        diagnostics: Vec<Diagnostic>,
        page: Page,
    },
    ProfilesGet {
        profile: ProfileDefinition,
    },
    TemplatesList {
        etag: String,
        rows: Vec<TemplateSummary>,
        diagnostics: Vec<Diagnostic>,
        page: Page,
    },
    TemplatesGet {
        template: Box<TemplateDefinition>,
    },
    ConfigGet {
        etag: String,
        scope: HomeScope,
        fields: Vec<ConfigFieldValue>,
        diagnostics: Vec<Diagnostic>,
    },
    ConsoleMetadata {
        etag: String,
        needs_setup: bool,
        settings: Vec<SettingRow>,
        catalog: CatalogPage,
        diagnostics: Vec<Diagnostic>,
    },
    AccountsStatus {
        as_of: String,
        rows: Vec<AccountStatus>,
        diagnostics: Vec<Diagnostic>,
        page: Page,
    },
    AccountsUsage {
        as_of: String,
        window: UsageWindow,
        from: String,
        to: String,
        rows: Vec<AccountUsage>,
        unattributed: Box<RecordedUsage>,
        diagnostics: Vec<Diagnostic>,
        page: Page,
    },
    Refused {
        reason: ReadRefusal,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Page {
    #[serde(deserialize_with = "required")]
    #[schemars(required, schema_with = "nullable::<String>")]
    pub next: Option<String>,
    pub truncated: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Diagnostic {
    /// Closed per producer; never a raw provider response or parser message.
    pub code: String,
    #[serde(deserialize_with = "required")]
    #[schemars(required, schema_with = "nullable::<String>")]
    pub subject: Option<String>,
    /// Sanitized operator text, at most 512 UTF-8 bytes.
    pub detail: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "reason", rename_all = "snake_case", deny_unknown_fields)]
pub enum ReadRefusal {
    ValidationFailed {
        issues: Vec<ValidationIssue>,
    },
    Busy {
        scope: ReadScope,
        retry_after_ms: u64,
    },
    Unavailable {
        scope: ReadScope,
        cause: UnavailableCode,
        remedies: Vec<ReadRemedy>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ValidationIssue {
    pub field: String,
    pub code: ValidationCode,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ValidationCode {
    InvalidAccount,
    UnknownAccount,
    InvalidId,
    UnknownId,
    InvalidWindow,
    InvalidLimit,
    InvalidCursor,
    CursorExpired,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReadScope {
    Accounts,
    Usage,
    Profiles,
    Templates,
    Config,
    Metadata,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum UnavailableCode {
    Storage,
    Configuration,
    NotCompiled,
    DefinitionTooLarge,
    DependencyLimit,
    Deadline,
    CatalogMissing,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReadRemedy {
    pub action: ReadAction,
    #[serde(deserialize_with = "required")]
    #[schemars(required, schema_with = "nullable::<String>")]
    pub account_id: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReadAction {
    Login,
    SetKey,
    Refresh,
    UseMatchingBuild,
    RestoreAccount,
    Retry,
}

fn required<'de, D: serde::Deserializer<'de>, T: serde::de::DeserializeOwned>(
    deserializer: D,
) -> Result<T, D::Error> {
    T::deserialize(deserializer)
}

fn nullable<T: schemars::JsonSchema>(
    generator: &mut schemars::SchemaGenerator,
) -> schemars::Schema {
    <Option<T> as schemars::JsonSchema>::json_schema(generator)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn usage_measurements_distinguish_unknown_zero_and_incomplete_subtotal() {
        let unknown = json!({
            "input_tokens":null,"output_tokens":null,"reasoning_tokens":null,
            "cached_input_tokens":null,"cache_write_tokens":null,"calls":null,"cost":null,
            "coverage":{"from":null,"to":null,"complete":false,"reasons":["writer_gap"]}
        });
        let Ok(usage) = serde_json::from_value::<RecordedUsage>(unknown.clone()) else {
            panic!("valid unknown measurements")
        };
        assert_eq!(usage.calls, None);
        let mut measured = unknown.clone();
        measured["calls"] = json!("0");
        measured["input_tokens"] = json!("9007199254740993");
        let Ok(usage) = serde_json::from_value::<RecordedUsage>(measured) else {
            panic!("valid measured subtotal")
        };
        assert_eq!(usage.calls.map(String::from), Some("0".into()));
        assert_eq!(
            usage.input_tokens.map(String::from),
            Some("9007199254740993".into())
        );
        assert!(!usage.coverage.complete);
        let mut omitted = unknown;
        assert!(
            omitted
                .as_object_mut()
                .is_some_and(|object| object.remove("calls").is_some())
        );
        assert!(serde_json::from_value::<RecordedUsage>(omitted).is_err());
    }

    #[test]
    fn decimal_and_windows_are_closed_and_exact() {
        for invalid in [
            "",
            "-1",
            "+1",
            "01",
            "1.0",
            "1e3",
            " 1",
            "340282366920938463463374607431768211456",
        ] {
            assert!(
                serde_json::from_value::<Decimal>(json!(invalid)).is_err(),
                "{invalid}"
            );
        }
        assert!(serde_json::from_value::<Decimal>(json!(1)).is_err());
        assert_eq!(UsageWindow::Hours24.seconds(), 86_400);
        assert_eq!(UsageWindow::Days7.seconds(), 604_800);
        assert_eq!(UsageWindow::Days30.seconds(), 2_592_000);
        assert!(
            serde_json::from_value::<HostRequest>(
                json!({"request":"accounts_usage","window":"today"})
            )
            .is_err()
        );
        assert!(
            serde_json::from_value::<HostRequest>(
                json!({"request":"accounts_status","session_id":"ignored"})
            )
            .is_err()
        );
    }

    #[test]
    fn output_nullable_fields_are_required_in_schema_and_decoder()
    -> Result<(), Box<dyn std::error::Error>> {
        let schema = serde_json::to_value(schemars::schema_for!(RecordedUsage))?;
        let Some(required) = schema["required"].as_array() else {
            panic!("required fields")
        };
        for field in ["input_tokens", "calls", "cost", "coverage"] {
            assert!(required.contains(&json!(field)), "{field}");
        }
        assert_eq!(
            serde_json::from_value::<Page>(json!({"next":null,"truncated":false}))?.next,
            None
        );
        assert!(serde_json::from_value::<Page>(json!({"truncated":false})).is_err());
        let decimal = serde_json::to_value(schemars::schema_for!(Decimal))?;
        assert_eq!(decimal["type"], "string");
        assert_eq!(decimal["pattern"], "^(0|[1-9][0-9]*)$");
        Ok(())
    }
}
