//! The encodings of each protocol version, pinned. A gateway serves its own
//! version and the previous one (spec 01 §7.5), so a body that changes
//! without a [`crate::PROTOCOL_VERSION`] bump would meet a peer that reads it
//! differently. Every variant of every call and frame enum is encoded here
//! once; a change to any of them fails this test until the version is
//! bumped and the new version's encodings are pinned beside the old ones.
//! Once a version ships, its pinned files never change.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use serde::Serialize;
use serde::de::DeserializeOwned;
use zeroize::Zeroizing;

use crate::admin::{
    AccountStatusRow, AdminRefusal, AdminRequest, AdminResponse, CallerRow, DisableTarget,
    LoginMethod, LoginPhase, LogoutMode, RefreshObservation, RefreshOutcome,
};
use crate::envelope::{ExpectedFence, RelayEnvelope, RelayLimits, encode_request};
use crate::facts::{
    AccountFacts, Consent, CredentialFacts, CredentialStatus, FactsReply, FactsRequest,
    IdentityFacts, KeyFacts, KeySource, ProfileFacts, RefreshMode, RefreshReply, RefreshRequest,
    WireIdentity,
};
use crate::frame::{
    Data, Frame, Head, RelayEnd, Trailers, WireFailureKind, WirePhase, WireTransmission,
};
use crate::header::{RequestHeader, WireHeader};
use crate::refusal::{GatewayRefusal, RefusalBody};
use crate::scope::{CallerId, ScopeSet, SourceNet};
use crate::{
    BoundedText, GatewayAccount, HexDigest, PROTOCOL_VERSION, SUPPORTED_VERSIONS, SecretText,
    UuidText, VersionsReply,
};

/// Each version's JSON bodies, `label<TAB>json` per line.
const PINNED_JSON: &[(u16, &str)] = &[
    (1, include_str!("pinned/v1.jsonl")),
    (2, include_str!("pinned/v2.jsonl")),
];
/// Each version's binary encodings, `label<TAB>hex` per line.
const PINNED_BINARY: &[(u16, &str)] = &[
    (1, include_str!("pinned/v1-binary.txt")),
    (2, include_str!("pinned/v1-binary.txt")), // v2 has the same binary encoding
];

const ATTEMPT: &str = "3f2e1d0c-9b8a-4766-a554-433221100fff";
const INSTANCE: &str = "5c1d2e3f-4a5b-4c6d-8e7f-901234567890";
const OPERATION: &str = "0b9e5f4c-7d3a-4e21-9f80-1a2b3c4d5e6f";

fn account(text: &str) -> GatewayAccount {
    GatewayAccount::new(text).unwrap()
}

fn digest(byte: u8) -> HexDigest {
    HexDigest::from_bytes(&[byte; 32])
}

fn uuid(text: &str) -> UuidText {
    UuidText::new(text).unwrap()
}

fn text<const MAX: usize>(value: &str) -> BoundedText<MAX> {
    BoundedText::new(value).unwrap()
}

fn secret(value: &str) -> SecretText {
    SecretText::new(Zeroizing::new(value.to_owned())).unwrap()
}

fn header(name: &str, value: &str) -> WireHeader {
    WireHeader::new(name, value.as_bytes().to_vec()).unwrap()
}

fn fence() -> ExpectedFence {
    ExpectedFence {
        account_binding: digest(0xab),
        generation: 7,
    }
}

pub(crate) fn facts_sample() -> AccountFacts {
    AccountFacts {
        account: account("claude/work"),
        instance: uuid(INSTANCE),
        observed_at_unix_ms: 1_800_000_000_000,
        credential: CredentialFacts {
            status: CredentialStatus::Ready,
            generation: Some(7),
            access_expires_at_unix_ms: Some(1_800_000_600_000),
            selection_epoch_digest: Some(digest(0x5e)),
        },
        identity: Some(IdentityFacts {
            account_binding: digest(0xab),
            credential_authority: digest(0xcd),
            email: Some(text("person@example.com")),
            plan: Some(text("max")),
        }),
        wire_identity: WireIdentity::new(BTreeMap::from([
            (
                "claude_account_uuid".to_owned(),
                text("11111111-2222-4333-8444-555555555555"),
            ),
            ("claude_device_id".to_owned(), text(&"d".repeat(64))),
        ]))
        .unwrap(),
        profile: Some(ProfileFacts {
            id: text("anthropic.claude.subscription.2026-11-15"),
            contract_revision: text("r1"),
            consent: Consent::Current,
            review_expires_at_unix_ms: Some(1_802_476_800_000),
        }),
        key: None,
    }
}

fn key_facts_sample() -> AccountFacts {
    AccountFacts {
        account: account("openai"),
        instance: uuid(INSTANCE),
        observed_at_unix_ms: 1_800_000_000_000,
        credential: CredentialFacts {
            status: CredentialStatus::Ready,
            generation: Some(1),
            access_expires_at_unix_ms: None,
            selection_epoch_digest: None,
        },
        identity: Some(IdentityFacts {
            account_binding: digest(0x01),
            credential_authority: digest(0x02),
            email: None,
            plan: None,
        }),
        wire_identity: WireIdentity::default(),
        profile: None,
        key: Some(KeyFacts {
            source: KeySource::File,
            fingerprint: Some(text("sha256:0123456789abcdef")),
            env_var: None,
        }),
    }
}

fn envelope() -> RelayEnvelope {
    RelayEnvelope {
        operation: uuid(OPERATION),
        attempt: 1,
        account: account("claude/work"),
        expected: fence(),
        method: "POST".into(),
        url: "https://api.anthropic.com/v1/messages?beta=true".into(),
        headers: vec![
            RequestHeader::Value(header("content-type", "application/json")),
            RequestHeader::custodied("authorization").unwrap(),
            RequestHeader::Value(header("anthropic-version", "2023-06-01")),
        ],
        body_len: 2,
        limits: RelayLimits {
            head_timeout_ms: 180_000,
            body_idle_timeout_ms: 180_000,
            max_response_bytes: 33_554_432,
            max_chunk_bytes: 1_048_576,
        },
        deadline_unix_ms: Some(1_800_000_120_000),
    }
}

const fn refusal_name(refusal: &GatewayRefusal) -> &'static str {
    refusal.code()
}

pub(crate) fn refusal_samples() -> Vec<GatewayRefusal> {
    vec![
        GatewayRefusal::Unauthenticated {},
        GatewayRefusal::ScopeDenied {},
        GatewayRefusal::VersionUnsupported {
            supported: SUPPORTED_VERSIONS.to_vec(),
        },
        GatewayRefusal::GatewayDisabled {},
        GatewayRefusal::AccountDisabled {},
        GatewayRefusal::AccountUnknown {},
        GatewayRefusal::CredentialNotReady {
            status: CredentialStatus::Quarantined,
        },
        GatewayRefusal::FenceMoved {
            facts: Box::new(facts_sample()),
        },
        GatewayRefusal::AccountChanged {},
        GatewayRefusal::UpstreamNotPinned {},
        GatewayRefusal::ReservedHeader {},
        GatewayRefusal::CustodiedHeadersMismatch {},
        GatewayRefusal::EnvelopeInvalid {},
        GatewayRefusal::LimitExceeded {},
        GatewayRefusal::Busy {
            retry_after_ms: 250,
        },
        GatewayRefusal::Internal {},
    ]
}

const fn request_name(request: &AdminRequest) -> &'static str {
    match request {
        AdminRequest::List {} => "list",
        AdminRequest::Status { .. } => "status",
        AdminRequest::KeySet { .. } => "key_set",
        AdminRequest::KeyClear { .. } => "key_clear",
        AdminRequest::Logout { .. } => "logout",
        AdminRequest::Refresh { .. } => "refresh",
        AdminRequest::Remove { .. } => "remove",
        AdminRequest::LoginStart { .. } => "login_start",
        AdminRequest::LoginComplete { .. } => "login_complete",
        AdminRequest::LoginPoll { .. } => "login_poll",
        AdminRequest::LoginCancel { .. } => "login_cancel",
        AdminRequest::Allowance { .. } => "allowance",
        AdminRequest::Disable { .. } => "disable",
        AdminRequest::Enable { .. } => "enable",
        AdminRequest::CallerAdd { .. } => "caller_add",
        AdminRequest::CallerList {} => "caller_list",
        AdminRequest::CallerRotate { .. } => "caller_rotate",
        AdminRequest::CallerRemove { .. } => "caller_remove",
        AdminRequest::Drain {} => "drain",
    }
}

fn request_samples() -> Vec<AdminRequest> {
    vec![
        AdminRequest::List {},
        AdminRequest::Status { accounts: vec![] },
        AdminRequest::Status {
            accounts: vec![account("codex"), account("codex/work")],
        },
        AdminRequest::KeySet {
            account: account("openai/team"),
            secret: secret("not-a-real-key"),
        },
        AdminRequest::KeyClear {
            account: account("openai/team"),
        },
        AdminRequest::Logout {
            account: account("grok"),
            mode: LogoutMode::Remote,
        },
        AdminRequest::Logout {
            account: account("claude"),
            mode: LogoutMode::Local,
        },
        AdminRequest::Refresh {
            account: account("codex"),
        },
        AdminRequest::Remove {
            account: account("codex/old"),
        },
        AdminRequest::LoginStart {
            account: account("codex/work"),
            notice_revision: text("codex-notice-r1"),
            replace_newer: false,
        },
        AdminRequest::LoginComplete {
            attempt: uuid(ATTEMPT),
            pasted: secret("code#state"),
        },
        AdminRequest::LoginPoll {
            attempt: uuid(ATTEMPT),
        },
        AdminRequest::LoginCancel {
            attempt: uuid(ATTEMPT),
        },
        AdminRequest::Allowance {
            account: account("claude/work"),
        },
        AdminRequest::Disable {
            target: DisableTarget::All {},
            cut: true,
        },
        AdminRequest::Disable {
            target: DisableTarget::Account {
                account: account("codex"),
            },
            cut: false,
        },
        AdminRequest::Disable {
            target: DisableTarget::Caller { caller: caller() },
            cut: false,
        },
        AdminRequest::Enable {
            target: DisableTarget::All {},
        },
        AdminRequest::CallerAdd {
            caller: caller(),
            scopes: ScopeSet::parse("relay:*,admin:codex/*").unwrap(),
            sources: vec![
                SourceNet::parse("100.101.102.103").unwrap(),
                SourceNet::parse("fd7a:115c:a1e0::/48").unwrap(),
            ],
        },
        AdminRequest::CallerList {},
        AdminRequest::CallerRotate { caller: caller() },
        AdminRequest::CallerRemove { caller: caller() },
        AdminRequest::Drain {},
    ]
}

fn caller() -> CallerId {
    CallerId::new("host-a").unwrap()
}

const fn response_name(response: &AdminResponse) -> &'static str {
    match response {
        AdminResponse::Accounts { .. } => "accounts",
        AdminResponse::Status { .. } => "status",
        AdminResponse::Changed { .. } => "changed",
        AdminResponse::Refreshed { .. } => "refreshed",
        AdminResponse::Challenge { .. } => "challenge",
        AdminResponse::Progress { .. } => "progress",
        AdminResponse::Allowance { .. } => "allowance",
        AdminResponse::Disabled {} => "disabled",
        AdminResponse::Enabled {} => "enabled",
        AdminResponse::CallerKeyIssued { .. } => "caller_key_issued",
        AdminResponse::Callers { .. } => "callers",
        AdminResponse::CallerRemoved { .. } => "caller_removed",
        AdminResponse::Draining {} => "draining",
        AdminResponse::Refused { .. } => "refused",
    }
}

const fn admin_refusal_name(refusal: &AdminRefusal) -> &'static str {
    match refusal {
        AdminRefusal::WrongAccountClass {} => "wrong_account_class",
        AdminRefusal::AccountSignedIn {} => "account_signed_in",
        AdminRefusal::AccountBusy {} => "account_busy",
        AdminRefusal::DefaultNotRemovable {} => "default_not_removable",
        AdminRefusal::EnvironmentOverride { .. } => "environment_override",
        AdminRefusal::NoticeMismatch {} => "notice_mismatch",
        AdminRefusal::WrittenByNewerBuild {} => "written_by_newer_build",
        AdminRefusal::DeviceLoginUnavailable {} => "device_login_unavailable",
        AdminRefusal::PasteInvalid {} => "paste_invalid",
        AdminRefusal::AttemptUnknown {} => "attempt_unknown",
        AdminRefusal::AttemptLimit {} => "attempt_limit",
        AdminRefusal::ProviderRejected {} => "provider_rejected",
        AdminRefusal::OutcomeUnknown {} => "outcome_unknown",
        AdminRefusal::Unavailable {} => "unavailable",
        AdminRefusal::LocalAdminRequired {} => "local_admin_required",
        AdminRefusal::CallerExists {} => "caller_exists",
        AdminRefusal::CallerUnknown {} => "caller_unknown",
    }
}

fn admin_refusal_samples() -> Vec<AdminRefusal> {
    vec![
        AdminRefusal::WrongAccountClass {},
        AdminRefusal::AccountSignedIn {},
        AdminRefusal::AccountBusy {},
        AdminRefusal::DefaultNotRemovable {},
        AdminRefusal::EnvironmentOverride {
            variable: text("OPENAI_API_KEY"),
        },
        AdminRefusal::NoticeMismatch {},
        AdminRefusal::WrittenByNewerBuild {},
        AdminRefusal::DeviceLoginUnavailable {},
        AdminRefusal::PasteInvalid {},
        AdminRefusal::AttemptUnknown {},
        AdminRefusal::AttemptLimit {},
        AdminRefusal::ProviderRejected {},
        AdminRefusal::OutcomeUnknown {},
        AdminRefusal::Unavailable {},
        AdminRefusal::LocalAdminRequired {},
        AdminRefusal::CallerExists {},
        AdminRefusal::CallerUnknown {},
    ]
}

const LOGIN_PHASES: [LoginPhase; 9] = [
    LoginPhase::Starting,
    LoginPhase::AwaitingInput,
    LoginPhase::Exchanging,
    LoginPhase::Cancelling,
    LoginPhase::Succeeded,
    LoginPhase::Failed,
    LoginPhase::OutcomeUnknown,
    LoginPhase::Cancelled,
    LoginPhase::Expired,
];

const fn login_phase_name(phase: LoginPhase) -> &'static str {
    match phase {
        LoginPhase::Starting => "starting",
        LoginPhase::AwaitingInput => "awaiting_input",
        LoginPhase::Exchanging => "exchanging",
        LoginPhase::Cancelling => "cancelling",
        LoginPhase::Succeeded => "succeeded",
        LoginPhase::Failed => "failed",
        LoginPhase::OutcomeUnknown => "outcome_unknown",
        LoginPhase::Cancelled => "cancelled",
        LoginPhase::Expired => "expired",
    }
}

fn status_rows() -> Vec<AccountStatusRow> {
    [
        RefreshOutcome::Completed,
        RefreshOutcome::Restored,
        RefreshOutcome::OutcomeUnknown,
        RefreshOutcome::Quarantined,
    ]
    .into_iter()
    .map(|outcome| AccountStatusRow {
        facts: key_facts_sample(),
        last_refresh: Some(RefreshObservation {
            outcome,
            observed_at_unix_ms: 1_800_000_000_000,
            failure_class: Some(text("provider_rejected")),
        }),
    })
    .chain([AccountStatusRow {
        facts: facts_sample(),
        last_refresh: None,
    }])
    .collect()
}

fn caller_responses() -> Vec<AdminResponse> {
    vec![
        AdminResponse::Enabled {},
        AdminResponse::CallerKeyIssued {
            caller: caller(),
            key: secret("plakgw1.host-a.not-a-real-key"),
        },
        AdminResponse::Callers {
            callers: vec![
                CallerRow {
                    caller: caller(),
                    scopes: ScopeSet::parse("relay:*").unwrap(),
                    sources: vec![SourceNet::parse("100.64.0.0/10").unwrap()],
                    issued_at_unix_ms: 1_800_000_000_000,
                    revoked_at_unix_ms: None,
                },
                CallerRow {
                    caller: CallerId::new("old-host").unwrap(),
                    scopes: ScopeSet::parse("relay:codex,admin:codex").unwrap(),
                    sources: vec![SourceNet::parse("100.101.102.104").unwrap()],
                    issued_at_unix_ms: 1_790_000_000_000,
                    revoked_at_unix_ms: Some(1_795_000_000_000),
                },
            ],
        },
        AdminResponse::CallerRemoved { caller: caller() },
        AdminResponse::Draining {},
    ]
}

fn response_samples() -> Vec<AdminResponse> {
    let mut samples = vec![
        AdminResponse::Accounts {
            accounts: vec![account("claude"), account("claude/work"), account("openai")],
        },
        AdminResponse::Status {
            rows: status_rows(),
        },
        AdminResponse::Changed {
            account: account("claude"),
            remote_grant_may_remain: Some(true),
        },
        AdminResponse::Changed {
            account: account("openai/team"),
            remote_grant_may_remain: None,
        },
        AdminResponse::Refreshed {
            facts: Box::new(facts_sample()),
        },
        AdminResponse::Challenge {
            attempt: uuid(ATTEMPT),
            method: LoginMethod::DeviceCode,
            authorize_url: secret("https://auth.example.invalid/device"),
            user_code: Some(secret("ABCD-EFGH")),
            expires_in_ms: 900_000,
            warning: None,
        },
        AdminResponse::Challenge {
            attempt: uuid(ATTEMPT),
            method: LoginMethod::Paste,
            authorize_url: secret("https://auth.example.invalid/authorize?state=s"),
            user_code: None,
            expires_in_ms: 900_000,
            warning: Some(text("an unusable record was retired")),
        },
    ];
    samples.extend(
        LOGIN_PHASES
            .into_iter()
            .map(|phase| AdminResponse::Progress {
                attempt: uuid(ATTEMPT),
                phase,
                warning: None,
            }),
    );
    samples.extend([
        AdminResponse::Allowance {
            account: account("claude/work"),
            observed_at_unix_ms: Some(1_800_000_000_000),
            view: Some(serde_json::json!({"windows": []})),
            unavailable: None,
        },
        AdminResponse::Allowance {
            account: account("codex"),
            observed_at_unix_ms: None,
            view: None,
            unavailable: Some(text("refresh_required")),
        },
        AdminResponse::Disabled {},
    ]);
    samples.extend(caller_responses());
    samples.extend(
        admin_refusal_samples()
            .into_iter()
            .map(|refusal| AdminResponse::Refused { refusal }),
    );
    samples
}

const PHASES: [WirePhase; 8] = [
    WirePhase::Resolution,
    WirePhase::Connect,
    WirePhase::Tls,
    WirePhase::RequestWrite,
    WirePhase::RequestWriteOrResponseHead,
    WirePhase::ResponseHead,
    WirePhase::ResponseBody,
    WirePhase::Cleanup,
];

const TRANSMISSIONS: [WireTransmission; 3] = [
    WireTransmission::NotStarted,
    WireTransmission::RequestIncomplete,
    WireTransmission::MayHaveTransmitted,
];

const KINDS: [WireFailureKind; 10] = [
    WireFailureKind::Cancelled,
    WireFailureKind::DeadlineExceeded,
    WireFailureKind::LimitExceeded,
    WireFailureKind::Resolution,
    WireFailureKind::AddressRefused,
    WireFailureKind::Io,
    WireFailureKind::Tls,
    WireFailureKind::Protocol,
    WireFailureKind::InvalidResponse,
    WireFailureKind::Internal,
];

/// Names each variant, so a new one fails to compile until it has a sample.
const fn end_name(end: RelayEnd) -> &'static str {
    match end {
        RelayEnd::Complete {} => "complete",
        RelayEnd::UpstreamFailed {
            phase:
                WirePhase::Resolution
                | WirePhase::Connect
                | WirePhase::Tls
                | WirePhase::RequestWrite
                | WirePhase::RequestWriteOrResponseHead
                | WirePhase::ResponseHead
                | WirePhase::ResponseBody
                | WirePhase::Cleanup,
            transmission:
                WireTransmission::NotStarted
                | WireTransmission::RequestIncomplete
                | WireTransmission::MayHaveTransmitted,
            kind:
                WireFailureKind::Cancelled
                | WireFailureKind::DeadlineExceeded
                | WireFailureKind::LimitExceeded
                | WireFailureKind::Resolution
                | WireFailureKind::AddressRefused
                | WireFailureKind::Io
                | WireFailureKind::Tls
                | WireFailureKind::Protocol
                | WireFailureKind::InvalidResponse
                | WireFailureKind::Internal,
            ..
        } => "upstream_failed",
        RelayEnd::Cancelled {} => "cancelled",
        RelayEnd::GatewayFailed {} => "gateway_failed",
    }
}

const fn frame_name(frame: &Frame) -> &'static str {
    match frame {
        Frame::Head(_) => "head",
        Frame::Data(_) => "data",
        Frame::Trailers(_) => "trailers",
        Frame::End(_) => "end",
    }
}

fn frame_samples() -> Vec<Frame> {
    let mut samples = vec![
        Frame::Head(Head {
            status: 429,
            headers: vec![
                header("content-type", "application/json"),
                header("retry-after", "5"),
            ],
        }),
        Frame::Data(Data {
            seq: 0,
            offset: 0,
            bytes: b"data: {}\n\n".to_vec(),
        }),
        Frame::Data(Data {
            seq: 1,
            offset: 10,
            bytes: vec![0x00, 0xff],
        }),
        Frame::Trailers(Trailers {
            headers: vec![header("x-trailer", "1")],
        }),
        Frame::End(RelayEnd::Complete {}),
        Frame::End(RelayEnd::Cancelled {}),
        Frame::End(RelayEnd::GatewayFailed {}),
    ];
    samples.extend(KINDS.iter().enumerate().map(|(index, kind)| {
        Frame::End(RelayEnd::UpstreamFailed {
            phase: PHASES[index % PHASES.len()],
            transmission: TRANSMISSIONS[index % TRANSMISSIONS.len()],
            kind: *kind,
            timeout: index % 2 == 0,
        })
    }));
    samples
}

/// Encodes `value`, checks it decodes back to itself, and appends its line.
fn pin<T: Serialize + DeserializeOwned + PartialEq + std::fmt::Debug>(
    lines: &mut String,
    label: &str,
    value: &T,
) {
    let json = serde_json::to_string(value).unwrap();
    assert_eq!(&serde_json::from_str::<T>(&json).unwrap(), value, "{label}");
    writeln!(lines, "{label}\t{json}").unwrap();
}

fn json_lines() -> String {
    let mut lines = String::new();
    pin(
        &mut lines,
        "versions",
        &VersionsReply {
            versions: SUPPORTED_VERSIONS.to_vec(),
            instance: uuid(INSTANCE),
        },
    );
    pin(&mut lines, "envelope", &envelope());
    pin(
        &mut lines,
        "envelope_without_deadline",
        &RelayEnvelope {
            deadline_unix_ms: None,
            ..envelope()
        },
    );
    for refusal in refusal_samples() {
        let label = format!("refusal.{}", refusal_name(&refusal));
        pin(
            &mut lines,
            &label,
            &RefusalBody {
                gateway_error: refusal,
            },
        );
    }
    pin(&mut lines, "facts.subscription", &facts_sample());
    pin(&mut lines, "facts.key", &key_facts_sample());
    pin(
        &mut lines,
        "facts.request",
        &FactsRequest {
            accounts: vec![account("claude/work"), account("openai")],
        },
    );
    pin(
        &mut lines,
        "facts.reply",
        &FactsReply {
            facts: vec![facts_sample(), key_facts_sample()],
        },
    );
    pin(
        &mut lines,
        "refresh.if_due",
        &RefreshRequest {
            account: account("claude/work"),
            mode: RefreshMode::IfDue { expected: fence() },
        },
    );
    pin(
        &mut lines,
        "refresh.after_rejection",
        &RefreshRequest {
            account: account("codex"),
            mode: RefreshMode::AfterRejection {
                rejected: fence(),
                selection_epoch_digest: Some(digest(0x5e)),
            },
        },
    );
    pin(
        &mut lines,
        "refresh.reply",
        &RefreshReply {
            facts: facts_sample(),
        },
    );
    for request in request_samples() {
        pin(
            &mut lines,
            &format!("admin.request.{}", request_name(&request)),
            &request,
        );
    }
    for response in response_samples() {
        let mut label = format!("admin.response.{}", response_name(&response));
        if let AdminResponse::Progress { phase, .. } = &response {
            write!(label, ".{}", login_phase_name(*phase)).unwrap();
        }
        if let AdminResponse::Refused { refusal } = &response {
            write!(label, ".{}", admin_refusal_name(refusal)).unwrap();
        }
        pin(&mut lines, &label, &response);
    }
    lines
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut text, byte| {
        write!(text, "{byte:02x}").unwrap();
        text
    })
}

fn binary_lines() -> String {
    let mut lines = String::new();
    for frame in frame_samples() {
        let mut label = format!("frame.{}", frame_name(&frame));
        if let Frame::End(end) = &frame {
            write!(label, ".{}", end_name(*end)).unwrap();
        }
        writeln!(lines, "{label}\t{}", hex(&frame.encode().unwrap())).unwrap();
    }
    let request = encode_request(&envelope(), b"{}").unwrap();
    writeln!(lines, "relay_request\t{}", hex(&request)).unwrap();
    lines
}

fn assert_pinned(kind: &str, pinned: &[(u16, &str)], actual: &str) {
    let current = pinned
        .iter()
        .find(|(version, _)| *version == PROTOCOL_VERSION)
        .map(|(_, body)| *body);
    assert_eq!(
        current,
        Some(actual),
        "the v{PROTOCOL_VERSION} {kind} encodings changed; bump the version and pin these:\n\
         ----- BEGIN v{PROTOCOL_VERSION} {kind} -----\n{actual}----- END -----"
    );
}

#[test]
fn the_current_version_json_bodies_are_pinned() {
    assert_pinned("json", PINNED_JSON, &json_lines());
}

#[test]
fn the_current_version_binary_encodings_are_pinned() {
    assert_pinned("binary", PINNED_BINARY, &binary_lines());
}

#[test]
fn every_variant_has_a_pinned_sample() {
    let refusals: BTreeSet<_> = refusal_samples().iter().map(refusal_name).collect();
    assert_eq!(refusals.len(), 16);
    let requests: BTreeSet<_> = request_samples().iter().map(request_name).collect();
    assert_eq!(requests.len(), 19);
    let responses: BTreeSet<_> = response_samples().iter().map(response_name).collect();
    assert_eq!(responses.len(), 14);
    let admin_refusals: BTreeSet<_> = admin_refusal_samples()
        .iter()
        .map(admin_refusal_name)
        .collect();
    assert_eq!(admin_refusals.len(), 17);
    let phases: BTreeSet<_> = LOGIN_PHASES.into_iter().map(login_phase_name).collect();
    assert_eq!(phases.len(), 9);
    let frames = frame_samples();
    let frame_kinds: BTreeSet<_> = frames.iter().map(frame_name).collect();
    assert_eq!(frame_kinds.len(), 4);
    let ends: BTreeSet<_> = frames
        .iter()
        .filter_map(|frame| match frame {
            Frame::End(end) => Some(end_name(*end)),
            _ => None,
        })
        .collect();
    assert_eq!(ends.len(), 4);
}

#[test]
fn pinned_versions_include_the_current_one_and_parse() {
    for (version, body) in PINNED_JSON.iter().chain(PINNED_BINARY) {
        assert!(*version <= PROTOCOL_VERSION);
        for line in body.lines() {
            let (label, value) = line.split_once('\t').unwrap();
            assert!(!label.is_empty() && !value.is_empty(), "v{version}: {line}");
        }
    }
}
