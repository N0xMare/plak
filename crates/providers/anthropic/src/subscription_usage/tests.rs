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
    assert!((clamped.meters()[0].windows()[0].used_percent().get() - 100.0).abs() < f64::EPSILON);
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
fn iguana_necktie_is_fable_when_limits_do_not_name_it() -> Result<(), Box<dyn std::error::Error>> {
    let body = br#"{
          "iguana_necktie": {"utilization": 12.0, "resets_at": "2026-09-08T22:00:00Z"}
        }"#;
    let snapshot = decode_claude_subscription_usage(body, Timestamp::new(1, 0)?)?;
    assert_eq!(snapshot.meters().len(), 2);
    assert_eq!(snapshot.meters()[1].id(), "fable");
    assert_eq!(snapshot.meters()[1].name(), Some("Fable"));
    assert!((snapshot.meters()[1].windows()[0].used_percent().get() - 12.0).abs() < f64::EPSILON);
    Ok(())
}

#[test]
fn iguana_necktie_does_not_duplicate_a_scoped_fable_row() -> Result<(), Box<dyn std::error::Error>>
{
    let body = br#"{
          "iguana_necktie": {"utilization": 12.0, "resets_at": "2026-09-08T22:00:00Z"},
          "limits": [
            {"kind": "weekly_scoped", "percent": 96, "resets_at": "2026-09-08T22:00:00Z",
             "scope": {"model": {"display_name": "Fable"}}}
          ]
        }"#;
    let snapshot = decode_claude_subscription_usage(body, Timestamp::new(1, 0)?)?;
    assert_eq!(snapshot.meters().len(), 2);
    assert!((snapshot.meters()[1].windows()[0].used_percent().get() - 96.0).abs() < f64::EPSILON);
    Ok(())
}

#[test]
fn profile_email_address_and_closed_plan_are_projected() -> Result<(), Box<dyn std::error::Error>> {
    let profile = decode_claude_oauth_profile(
        br#"{"account":{"email_address":" user@example.com "},"organization":{"organization_type":"claude_max"}}"#,
    )?;
    assert_eq!(profile.email.as_deref(), Some("user@example.com"));
    assert_eq!(profile.plan.as_deref(), Some("Claude Max"));
    let email_field = decode_claude_oauth_profile(br#"{"account":{"email":"ops@example.test"}}"#)?;
    assert_eq!(email_field.email.as_deref(), Some("ops@example.test"));
    assert_eq!(email_field.plan, None);
    let unknown = decode_claude_oauth_profile(
        br#"{"organization":{"organization_type":"internal_unmapped_token"}}"#,
    )?;
    assert_eq!(unknown.plan, None);
    Ok(())
}
