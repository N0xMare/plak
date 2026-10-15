//! Claude Code OAuth usage projection from `GET /api/oauth/usage`.
//!
//! This is the same control-plane read Claude Code's `/usage` performs. Flat
//! `five_hour` / `seven_day` windows are the session and all-models weekly
//! meters; `limits[]` `weekly_scoped` rows are extra per-family weekly meters
//! (Fable, Opus, …). The obfuscated flat `iguana_necktie` window is the Fable
//! bucket when `limits[]` does not name it. Unknown top-level keys are ignored.
//! Account identity is a separate `GET /api/oauth/profile` read.

use plakous_model::{
    MAX_SUBSCRIPTION_USAGE_METERS, SubscriptionUsageMeter, SubscriptionUsagePercent,
    SubscriptionUsagePeriod, SubscriptionUsageSnapshot, SubscriptionUsageValueError,
    SubscriptionUsageWindow, Timestamp,
};
use serde::Deserialize;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

/// Exact read-only path in the reviewed Claude Code `/usage` client.
pub(crate) const CLAUDE_SUBSCRIPTION_USAGE_PATH: &str = "/api/oauth/usage";
/// Exact read-only path Claude Code's `/status` uses for account identity.
pub(crate) const CLAUDE_SUBSCRIPTION_PROFILE_PATH: &str = "/api/oauth/profile";
/// Maximum body accepted by the usage control-plane decoder.
pub(crate) const MAX_CLAUDE_SUBSCRIPTION_USAGE_BODY_BYTES: usize = 256 * 1024;
const FIVE_HOUR_SECONDS: u64 = 5 * 60 * 60;
const SEVEN_DAY_SECONDS: u64 = 7 * 24 * 60 * 60;

#[derive(Deserialize)]
struct UsagePayload {
    #[serde(default)]
    five_hour: Option<FlatWindow>,
    #[serde(default)]
    seven_day: Option<FlatWindow>,
    #[serde(default)]
    seven_day_opus: Option<FlatWindow>,
    #[serde(default)]
    seven_day_sonnet: Option<FlatWindow>,
    #[serde(default)]
    seven_day_fable: Option<FlatWindow>,
    /// Obfuscated flat Fable weekly window used alongside `limits[]`.
    #[serde(default)]
    iguana_necktie: Option<FlatWindow>,
    #[serde(default)]
    limits: Vec<StructuredLimit>,
}

#[derive(Deserialize)]
struct FlatWindow {
    utilization: Option<f64>,
    resets_at: Option<String>,
}

#[derive(Deserialize)]
struct StructuredLimit {
    kind: Option<String>,
    percent: Option<f64>,
    /// Some payloads reuse the flat-window key on a structured row.
    utilization: Option<f64>,
    resets_at: Option<String>,
    #[serde(default)]
    is_active: Option<bool>,
    scope: Option<LimitScope>,
}

#[derive(Deserialize)]
struct LimitScope {
    model: Option<LimitModel>,
    display_name: Option<String>,
}

#[derive(Deserialize)]
struct LimitModel {
    display_name: Option<String>,
}

#[derive(Deserialize)]
struct ProfilePayload {
    #[serde(default)]
    account: Option<ProfileAccount>,
    #[serde(default)]
    organization: Option<ProfileOrganization>,
}

#[derive(Deserialize)]
struct ProfileAccount {
    #[serde(default)]
    uuid: Option<crate::ClaudeAccountUuid>,
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    email_address: Option<String>,
}

#[derive(Deserialize)]
struct ProfileOrganization {
    #[serde(default)]
    organization_type: Option<String>,
}

/// Account identity from `GET /api/oauth/profile`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClaudeOauthProfile {
    /// Authenticated provider account UUID, when present.
    pub account_uuid: Option<crate::ClaudeAccountUuid>,
    /// Authenticated account e-mail, when the profile named one.
    pub email: Option<String>,
    /// Closed plan label derived from `organization.organization_type`.
    pub plan: Option<String>,
}

/// Decodes the Claude Code `/usage` JSON into the provider-neutral snapshot.
///
/// Flat `five_hour` / `seven_day` become the primary Claude meter. A
/// `weekly_scoped` `limits[]` row becomes its own meter named from
/// `scope.model.display_name` (the Fable weekly window), including a named
/// row whose percent or reset is omitted — Claude Code still draws that bar
/// at 0%. `session` and `weekly_all` structured rows are the same facts as
/// the flat keys and are not duplicated. The obfuscated `iguana_necktie`
/// window is Fable when `limits[]` did not already name it.
pub fn decode_claude_subscription_usage(
    body: &[u8],
    observed_at: Timestamp,
) -> Result<SubscriptionUsageSnapshot, SubscriptionUsageValueError> {
    if body.is_empty() || body.len() > MAX_CLAUDE_SUBSCRIPTION_USAGE_BODY_BYTES {
        return Err(SubscriptionUsageValueError::Collection);
    }
    let payload: UsagePayload =
        serde_json::from_slice(body).map_err(|_| SubscriptionUsageValueError::Collection)?;
    let mut meters = Vec::new();
    let mut primary = Vec::with_capacity(2);
    if let Some(window) = project_flat("five_hour", FIVE_HOUR_SECONDS, payload.five_hour)? {
        primary.push(window);
    }
    if let Some(window) = project_flat("seven_day", SEVEN_DAY_SECONDS, payload.seven_day)? {
        primary.push(window);
    }
    meters.push(SubscriptionUsageMeter::new(
        "claude".to_owned(),
        None,
        primary,
    )?);
    if let Some(window) = project_flat("seven_day", SEVEN_DAY_SECONDS, payload.seven_day_opus)? {
        meters.push(SubscriptionUsageMeter::new(
            "opus".to_owned(),
            Some("Opus".to_owned()),
            vec![window],
        )?);
    }
    if let Some(window) = project_flat("seven_day", SEVEN_DAY_SECONDS, payload.seven_day_sonnet)? {
        meters.push(SubscriptionUsageMeter::new(
            "sonnet".to_owned(),
            Some("Sonnet".to_owned()),
            vec![window],
        )?);
    }
    let mut scoped: Vec<_> = payload
        .limits
        .into_iter()
        .filter(|limit| limit.kind.as_deref() == Some("weekly_scoped"))
        .collect();
    // Prefer the currently-limiting row when the same family appears twice.
    scoped.sort_by_key(|limit| std::cmp::Reverse(limit.is_active));
    let mut seen_scoped: Vec<String> = Vec::new();
    for limit in scoped {
        let Some(name) = scoped_display_name(&limit) else {
            continue;
        };
        let key = name.to_ascii_lowercase();
        if seen_scoped.contains(&key) {
            continue;
        }
        // A named weekly_scoped row is a fact even at 0% used: Claude Code
        // still draws "Current week (Fable)" when percent or resets_at is
        // omitted. Missing percent is 0, matching proto3 omission of a true
        // zero; missing reset omits the clock rather than dropping the bar.
        let percent = scoped_percent(&limit).or(Some(0.0));
        let Some(window) =
            project_percent("seven_day", SEVEN_DAY_SECONDS, percent, limit.resets_at)?
        else {
            continue;
        };
        if meters.len() >= MAX_SUBSCRIPTION_USAGE_METERS {
            return Err(SubscriptionUsageValueError::Collection);
        }
        seen_scoped.push(key);
        meters.push(SubscriptionUsageMeter::new(
            scoped_meter_id(&name),
            Some(name),
            vec![window],
        )?);
    }
    push_fable_alias(
        &mut meters,
        payload.seven_day_fable.or(payload.iguana_necktie),
    )?;
    SubscriptionUsageSnapshot::new(observed_at, None, meters)
}

/// Decodes Claude Code `/status` identity from `GET /api/oauth/profile`.
///
/// `account.email` and `account.email_address` are both accepted. Plan is
/// only the closed `organization.organization_type` set; unknown tokens are
/// omitted rather than printed raw.
pub fn decode_claude_oauth_profile(
    body: &[u8],
) -> Result<ClaudeOauthProfile, SubscriptionUsageValueError> {
    if body.is_empty() || body.len() > MAX_CLAUDE_SUBSCRIPTION_USAGE_BODY_BYTES {
        return Err(SubscriptionUsageValueError::Collection);
    }
    let payload: ProfilePayload =
        serde_json::from_slice(body).map_err(|_| SubscriptionUsageValueError::Collection)?;
    let email = payload
        .account
        .as_ref()
        .and_then(|account| {
            account
                .email
                .as_deref()
                .or(account.email_address.as_deref())
        })
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned);
    let plan = payload
        .organization
        .as_ref()
        .and_then(|organization| organization.organization_type.as_deref())
        .and_then(claude_plan_from_organization_type);
    Ok(ClaudeOauthProfile {
        account_uuid: payload.account.and_then(|account| account.uuid),
        email,
        plan,
    })
}

fn scoped_display_name(limit: &StructuredLimit) -> Option<String> {
    let name = limit
        .scope
        .as_ref()
        .and_then(|scope| {
            scope
                .model
                .as_ref()
                .and_then(|model| model.display_name.as_deref())
                .or(scope.display_name.as_deref())
        })
        .map(str::trim)
        .filter(|name| !name.is_empty())?;
    if name.eq_ignore_ascii_case("all models") {
        return None;
    }
    Some(name.to_owned())
}

fn scoped_percent(limit: &StructuredLimit) -> Option<f64> {
    limit
        .percent
        .or(limit.utilization)
        .filter(|value| value.is_finite())
}

fn already_has_feature(meters: &[SubscriptionUsageMeter], id: &str, name: &str) -> bool {
    meters
        .iter()
        .any(|meter| meter.id() == id || meter.name() == Some(name))
}

fn push_fable_alias(
    meters: &mut Vec<SubscriptionUsageMeter>,
    window: Option<FlatWindow>,
) -> Result<(), SubscriptionUsageValueError> {
    if already_has_feature(meters, "fable", "Fable") {
        return Ok(());
    }
    let Some(window) = project_flat("seven_day", SEVEN_DAY_SECONDS, window)? else {
        return Ok(());
    };
    if meters.len() >= MAX_SUBSCRIPTION_USAGE_METERS {
        return Err(SubscriptionUsageValueError::Collection);
    }
    meters.push(SubscriptionUsageMeter::new(
        "fable".to_owned(),
        Some("Fable".to_owned()),
        vec![window],
    )?);
    Ok(())
}

fn claude_plan_from_organization_type(value: &str) -> Option<String> {
    Some(match value.trim() {
        "claude_max" | "max" => "Claude Max".to_owned(),
        "claude_pro" | "pro" => "Claude Pro".to_owned(),
        "claude_team" | "team" => "Claude Team".to_owned(),
        "claude_enterprise" | "enterprise" => "Claude Enterprise".to_owned(),
        _ => return None,
    })
}

fn scoped_meter_id(display_name: &str) -> String {
    let mut id = String::new();
    for ch in display_name.chars() {
        if ch.is_ascii_alphanumeric() {
            id.push(ch.to_ascii_lowercase());
        } else if !id.ends_with('-') && !id.is_empty() {
            id.push('-');
        }
    }
    let id = id.trim_matches('-').to_owned();
    if id.is_empty() {
        "scoped".to_owned()
    } else {
        id
    }
}

fn project_flat(
    name: &str,
    duration_seconds: u64,
    window: Option<FlatWindow>,
) -> Result<Option<SubscriptionUsageWindow>, SubscriptionUsageValueError> {
    let Some(window) = window else {
        return Ok(None);
    };
    project_percent(name, duration_seconds, window.utilization, window.resets_at)
}

fn project_percent(
    name: &str,
    duration_seconds: u64,
    percent: Option<f64>,
    resets_at: Option<String>,
) -> Result<Option<SubscriptionUsageWindow>, SubscriptionUsageValueError> {
    let Some(percent) = percent.filter(|value| value.is_finite()) else {
        return Ok(None);
    };
    let ends_at = match resets_at.as_deref().filter(|value| !value.is_empty()) {
        Some(value) => Some(parse_timestamp(value)?),
        None => None,
    };
    let window = SubscriptionUsageWindow::new(
        Some(name.to_owned()),
        SubscriptionUsagePercent::new(percent.clamp(0.0, 100.0))?,
        Some(SubscriptionUsagePeriod::Rolling { duration_seconds }),
        ends_at,
    )?;
    Ok(Some(window))
}

fn parse_timestamp(value: &str) -> Result<Timestamp, SubscriptionUsageValueError> {
    let parsed =
        OffsetDateTime::parse(value, &Rfc3339).map_err(|_| SubscriptionUsageValueError::Window)?;
    let seconds =
        u64::try_from(parsed.unix_timestamp()).map_err(|_| SubscriptionUsageValueError::Window)?;
    Timestamp::new(seconds, parsed.nanosecond()).map_err(|_| SubscriptionUsageValueError::Window)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_flat_windows_and_fable_scoped_limit() -> Result<(), Box<dyn std::error::Error>> {
        let body = br#"{
          "five_hour": {"utilization": 5.0, "resets_at": "2026-09-04T22:20:00Z"},
          "seven_day": {"utilization": 72.0, "resets_at": "2026-09-08T22:00:00Z"},
          "seven_day_opus": null,
          "seven_day_sonnet": null,
          "extra_usage": {"is_enabled": false},
          "tangelo": null,
          "limits": [
            {"kind": "session", "group": "session", "percent": 5, "resets_at": "2026-09-04T22:20:00Z", "is_active": false},
            {"kind": "weekly_all", "group": "weekly", "percent": 72, "resets_at": "2026-09-08T22:00:00Z", "is_active": false},
            {"kind": "weekly_scoped", "group": "weekly", "percent": 96, "resets_at": "2026-09-08T22:00:00Z", "is_active": true,
             "scope": {"model": {"id": null, "display_name": "Fable"}, "surface": null}}
          ]
        }"#;
        let snapshot = decode_claude_subscription_usage(body, Timestamp::new(1_788_000_000, 0)?)?;
        assert_eq!(snapshot.meters().len(), 2);
        let primary = &snapshot.meters()[0];
        assert_eq!(primary.id(), "claude");
        assert_eq!(primary.windows().len(), 2);
        assert_eq!(primary.windows()[0].name(), Some("five_hour"));
        assert!((primary.windows()[0].used_percent().get() - 5.0).abs() < f64::EPSILON);
        assert_eq!(primary.windows()[1].name(), Some("seven_day"));
        assert!((primary.windows()[1].used_percent().get() - 72.0).abs() < f64::EPSILON);
        let fable = &snapshot.meters()[1];
        assert_eq!(fable.id(), "fable");
        assert_eq!(fable.name(), Some("Fable"));
        assert_eq!(fable.windows()[0].name(), Some("seven_day"));
        assert!((fable.windows()[0].used_percent().get() - 96.0).abs() < f64::EPSILON);
        Ok(())
    }

    #[test]
    fn preserves_independent_window_absence_and_clamps_over_100()
    -> Result<(), Box<dyn std::error::Error>> {
        let observed = Timestamp::new(1, 0)?;
        let weekly_only = decode_claude_subscription_usage(
            br#"{"seven_day":{"utilization":33.5,"resets_at":"2026-09-08T22:00:00Z"}}"#,
            observed,
        )?;
        assert_eq!(weekly_only.meters()[0].windows().len(), 1);
        assert_eq!(
            weekly_only.meters()[0].windows()[0].name(),
            Some("seven_day")
        );
        let clamped = decode_claude_subscription_usage(
            br#"{"five_hour":{"utilization":140.0,"resets_at":"2026-09-08T22:00:00Z"}}"#,
            observed,
        )?;
        assert!(
            (clamped.meters()[0].windows()[0].used_percent().get() - 100.0).abs() < f64::EPSILON
        );
        let empty = decode_claude_subscription_usage(br"{}", observed)?;
        assert_eq!(empty.meters()[0].windows(), []);
        assert!(decode_claude_subscription_usage(b"", observed).is_err());
        assert!(
            decode_claude_subscription_usage(
                &vec![b' '; MAX_CLAUDE_SUBSCRIPTION_USAGE_BODY_BYTES + 1],
                observed
            )
            .is_err()
        );
        Ok(())
    }

    #[test]
    fn unnamed_scoped_limits_are_omitted() -> Result<(), Box<dyn std::error::Error>> {
        let body = br#"{
          "limits": [
            {"kind": "weekly_scoped", "percent": 10, "resets_at": "2026-09-08T22:00:00Z", "is_active": true,
             "scope": {"model": {"display_name": "  "}}},
            {"kind": "weekly_scoped", "percent": 10, "resets_at": "2026-09-08T22:00:00Z", "is_active": true,
             "scope": {"model": {"display_name": "All models"}}}
          ]
        }"#;
        let snapshot = decode_claude_subscription_usage(body, Timestamp::new(1, 0)?)?;
        assert_eq!(snapshot.meters().len(), 1);
        assert_eq!(snapshot.meters()[0].windows(), []);
        Ok(())
    }

    #[test]
    fn a_named_fable_row_survives_a_missing_reset_and_a_zero_percent()
    -> Result<(), Box<dyn std::error::Error>> {
        let body = br#"{
          "five_hour": {"utilization": 0.0, "resets_at": "2026-09-04T22:20:00Z"},
          "seven_day": {"utilization": 0.0, "resets_at": "2026-09-08T22:00:00Z"},
          "limits": [
            {"kind": "weekly_scoped", "is_active": false,
             "scope": {"model": {"display_name": "Fable"}}}
          ]
        }"#;
        let snapshot = decode_claude_subscription_usage(body, Timestamp::new(1_788_000_000, 0)?)?;
        assert_eq!(snapshot.meters().len(), 2);
        let fable = &snapshot.meters()[1];
        assert_eq!(fable.id(), "fable");
        assert_eq!(fable.name(), Some("Fable"));
        assert!((fable.windows()[0].used_percent().get() - 0.0).abs() < f64::EPSILON);
        assert_eq!(fable.windows()[0].ends_at(), None);
        Ok(())
    }

    #[test]
    fn iguana_necktie_is_fable_when_limits_do_not_name_it() -> Result<(), Box<dyn std::error::Error>>
    {
        let body = br#"{
          "iguana_necktie": {"utilization": 12.0, "resets_at": "2026-09-08T22:00:00Z"}
        }"#;
        let snapshot = decode_claude_subscription_usage(body, Timestamp::new(1, 0)?)?;
        assert_eq!(snapshot.meters().len(), 2);
        assert_eq!(snapshot.meters()[1].id(), "fable");
        assert_eq!(snapshot.meters()[1].name(), Some("Fable"));
        assert!(
            (snapshot.meters()[1].windows()[0].used_percent().get() - 12.0).abs() < f64::EPSILON
        );
        Ok(())
    }

    #[test]
    fn iguana_necktie_does_not_duplicate_a_scoped_fable_row()
    -> Result<(), Box<dyn std::error::Error>> {
        let body = br#"{
          "iguana_necktie": {"utilization": 12.0, "resets_at": "2026-09-08T22:00:00Z"},
          "limits": [
            {"kind": "weekly_scoped", "percent": 96, "resets_at": "2026-09-08T22:00:00Z",
             "scope": {"model": {"display_name": "Fable"}}}
          ]
        }"#;
        let snapshot = decode_claude_subscription_usage(body, Timestamp::new(1, 0)?)?;
        assert_eq!(snapshot.meters().len(), 2);
        assert!(
            (snapshot.meters()[1].windows()[0].used_percent().get() - 96.0).abs() < f64::EPSILON
        );
        Ok(())
    }

    #[test]
    fn profile_email_address_and_closed_plan_are_projected()
    -> Result<(), Box<dyn std::error::Error>> {
        let profile = decode_claude_oauth_profile(
            br#"{"account":{"email_address":" user@example.com "},"organization":{"organization_type":"claude_max"}}"#,
        )?;
        assert_eq!(profile.email.as_deref(), Some("user@example.com"));
        assert_eq!(profile.plan.as_deref(), Some("Claude Max"));
        let email_field =
            decode_claude_oauth_profile(br#"{"account":{"email":"ops@example.test"}}"#)?;
        assert_eq!(email_field.email.as_deref(), Some("ops@example.test"));
        assert_eq!(email_field.plan, None);
        let unknown = decode_claude_oauth_profile(
            br#"{"organization":{"organization_type":"internal_unmapped_token"}}"#,
        )?;
        assert_eq!(unknown.plan, None);
        Ok(())
    }
}
