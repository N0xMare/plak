//! Claude Code OAuth billing attestation (`cch`) and `cc_version` suffix.
//!
//! Native Claude Code emits `cch=00000` in JS, then the Bun HTTP stack
//! overwrites those five zeros with the low 20 bits of xxHash64 over the
//! serialized bytes, excluding native routing fields and model values. The
//! placeholder remains in the hashed bytes; the transmitted fields stay intact.
//! The `cc_version` three-hex suffix is `SHA-256(salt + msg[4,7,20] + VERSION)[:3]`
//! over the first user text. This module is that exact pair, applied to the
//! serialized Messages JSON Plakous sends. The costume pin is Claude Code
//! 2.1.295; the hash seed and salt are the native Bun/JS pair.

use plakous_model::{Actor, ContentBlock, SessionLineage, ValidatedRequestPlan, sha256_hex};
use std::fmt::Write as _;

/// Official Claude Code version the OAuth Messages costume still pins.
pub(crate) const CLAUDE_OAUTH_CC_VERSION: &str = "2.1.295";
/// JS-layer placeholder the native HTTP stack overwrites in place.
pub(crate) const CLAUDE_OAUTH_CCH_PLACEHOLDER: &str = "cch=00000";
#[cfg(test)]
/// Empty first-user-text root billing header before attestation (suffix `8a8`).
pub(crate) const CLAUDE_OAUTH_BILLING_HEADER: &str =
    "x-anthropic-billing-header: cc_version=2.1.295.8a8; cc_entrypoint=cli; cch=00000;";
#[cfg(test)]
/// Empty first-user-text spawned-child billing header before attestation.
pub(crate) const CLAUDE_OAUTH_BILLING_HEADER_SUBAGENT: &str = "x-anthropic-billing-header: cc_version=2.1.295.8a8; cc_entrypoint=cli; cch=00000; cc_is_subagent=true;";

const BILLING_MARKER: &[u8] = b"x-anthropic-billing-header:";
const CCH_PLACEHOLDER: &[u8] = b"cch=00000";
const CCH_SEED: u64 = 0x4D65_9218_E32A_3268;
const CCH_SEARCH_WINDOW: usize = 150;
const VERSION_SALT: &[u8] = b"59cf53e54c78";
const VERSION_INDICES: [usize; 3] = [4, 7, 20];

/// Builds `system[0]` with a version suffix, `cch=00000` placeholder, and
/// optional `cc_is_subagent` from session lineage.
///
/// Claude Code 2.1.295 billing blocks carry a trailing semicolon after `cch`
/// (and after `cc_is_subagent` on spawned children). Empty first-user-text is
/// suffix `8a8`; `"hello"` is `05c` (the 2.1.295 native capture's suffix).
#[must_use]
pub(crate) fn billing_header_prefix(lineage: SessionLineage, first_user_text: &str) -> String {
    let suffix = cc_version_suffix(first_user_text, CLAUDE_OAUTH_CC_VERSION);
    let mut header = format!(
        "x-anthropic-billing-header: cc_version={CLAUDE_OAUTH_CC_VERSION}.{suffix}; cc_entrypoint=cli; {CLAUDE_OAUTH_CCH_PLACEHOLDER};"
    );
    if matches!(lineage, SessionLineage::SpawnedChild) {
        header.push_str(" cc_is_subagent=true;");
    }
    header
}

/// Builds native billing attribution from canonical prompt identity and runtime-proven response metadata.
/// The caller must supply only metadata proven within the current preparation authority.
#[must_use]
///
/// A root human turn also carries `cc_prompt_index` and `cc_turn_index` from
/// `position`, after `cc_turn_origin`, exactly as native emits them.
pub fn claude_oauth_billing_header(
    lineage: SessionLineage,
    plan: &ValidatedRequestPlan,
    previous: Option<&plakous_model::ModelResponseMetadata>,
    position: Option<plakous_model::TurnPosition>,
) -> String {
    let mut header = billing_header_prefix(lineage, first_user_text(plan));
    if let Some(request_id) = previous.and_then(|metadata| metadata.provider_request_id())
        && valid_native_request_id(request_id)
    {
        header.push_str(" cc_prev_req=");
        header.push_str(request_id);
        header.push(';');
    }
    // This is Plak's own durable logical prompt identity, not a native session
    // UUID: the prompt that opened the turn, so it stays fixed across tool
    // rounds, steers and harness notes, as native's does.
    if let Some(item) = plan.request().turn_prompt()
        && item.provenance() == plakous_model::Provenance::UserInput
    {
        header.push_str(" cc_prompt_id=");
        header.push_str(&item.id().to_string());
        header.push(';');
        if matches!(lineage, SessionLineage::Root) {
            header.push_str(" cc_turn_origin=human;");
            if let Some(position) = position {
                let _ = write!(
                    header,
                    " cc_prompt_index={}; cc_turn_index={};",
                    position.prompt_index(),
                    position.turn_index()
                );
            }
        }
    }
    header
}

fn valid_native_request_id(value: &str) -> bool {
    value.strip_prefix("req_").is_some_and(|suffix| {
        (1..=36).contains(&suffix.len())
            && suffix
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    })
}

/// First canonical user text in `plan`. Tool and instruction actors are excluded;
/// imported user text remains ordinary replay text, not a native `isMeta` record.
/// Absence is the empty string (suffix units become `'0'`).
#[must_use]
pub fn first_user_text(plan: &ValidatedRequestPlan) -> &str {
    for item in plan.request().input().items() {
        if item.actor() != Actor::User {
            continue;
        }
        for block in item.blocks() {
            if let ContentBlock::Text(text) = block {
                return text.text();
            }
        }
    }
    ""
}

/// Overwrites `cch=00000` in the billing header with the body attestation.
///
/// The hash is of `body` **with the placeholder still present**. No billing
/// header is a no-op so API-key Messages are untouched. A billing header
/// without the placeholder is a prepare failure: sending that body would be
/// an unattested costume.
pub fn attest_claude_oauth_cch(body: &mut [u8]) -> Result<(), &'static str> {
    let Some(marker) = find_subslice(body, BILLING_MARKER) else {
        return Ok(());
    };
    let search_from = marker.saturating_add(BILLING_MARKER.len());
    let search_to = search_from
        .saturating_add(CCH_SEARCH_WINDOW)
        .min(body.len());
    let Some(rel) = find_subslice(&body[search_from..search_to], CCH_PLACEHOLDER) else {
        return Err("Claude OAuth billing header is missing the cch placeholder");
    };
    let idx = search_from + rel;
    let digest = cch_digest(body);
    let hex = format!("{:05x}", digest & 0xF_FFFF);
    body[idx + 4..idx + 9].copy_from_slice(hex.as_bytes());
    Ok(())
}

/// Reports whether `body` carries a self-consistent billing `cch`.
#[must_use]
pub fn claude_oauth_cch_matches_body(body: &[u8]) -> bool {
    let Some(marker) = find_subslice(body, BILLING_MARKER) else {
        return false;
    };
    let search_from = marker.saturating_add(BILLING_MARKER.len());
    let search_to = search_from
        .saturating_add(CCH_SEARCH_WINDOW)
        .min(body.len());
    let window = &body[search_from..search_to];
    let Some(rel) = find_subslice(window, b"cch=") else {
        return false;
    };
    let hex_at = search_from + rel + 4;
    if hex_at + 5 > body.len() {
        return false;
    }
    let observed = &body[hex_at..hex_at + 5];
    if !observed.iter().all(u8::is_ascii_hexdigit) {
        return false;
    }
    let mut restored = body.to_vec();
    restored[hex_at..hex_at + 5].copy_from_slice(b"00000");
    let digest = cch_digest(&restored);
    let expected = format!("{:05x}", digest & 0xF_FFFF);
    observed == expected.as_bytes()
}

fn cc_version_suffix(first_user_text: &str, version: &str) -> String {
    // Native JavaScript indexes UTF-16 units, then UTF-8 encodes the joined
    // selection. Unpaired selected surrogates become replacement characters.
    let units: Vec<u16> = first_user_text
        .encode_utf16()
        .take(VERSION_INDICES[2] + 1)
        .collect();
    let picked = String::from_utf16_lossy(
        &VERSION_INDICES.map(|index| units.get(index).copied().unwrap_or(u16::from(b'0'))),
    );
    let mut hex = sha256_hex(&[VERSION_SALT, picked.as_bytes(), version.as_bytes()]);
    hex.truncate(3);
    hex
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn cch_digest(body: &[u8]) -> u64 {
    // Native hashes a stream of retained byte ranges, not reserialized JSON.
    // Its literal matching and comma rules also apply to nested fields.
    let mut retained = Vec::with_capacity(body.len());
    let mut cursor = 0;
    let mut field = next_cch_field(body, cursor);
    loop {
        let model = find_subslice(&body[cursor..], br#""model":""#).and_then(|offset| {
            let start = cursor + offset + br#""model":""#.len();
            let end = start + body[start..].iter().position(|byte| *byte == b'"')?;
            Some(start..end)
        });
        let (excluded, consumed_field) = match (model, field.clone()) {
            (Some(model), Some(field)) if field.start < model.start => (field, true),
            (Some(model), _) => (model, false),
            (None, Some(field)) => (field, true),
            (None, None) => {
                retained.extend_from_slice(&body[cursor..]);
                return xxh64(&retained, CCH_SEED);
            }
        };
        retained.extend_from_slice(&body[cursor..excluded.start]);
        cursor = excluded.end;
        // As in native, model-value skips do not rescan the tail for fields.
        if consumed_field {
            field = next_cch_field(body, cursor);
        }
    }
}

fn next_cch_field(body: &[u8], cursor: usize) -> Option<std::ops::Range<usize>> {
    let fallback = find_subslice(&body[cursor..], br#""fallbacks":["#).and_then(|offset| {
        let start = cursor + offset;
        let end = cch_array_end(body, start + br#""fallbacks":["#.len())?;
        Some(cch_field_separator(body, cursor, start..end))
    });
    let credit =
        find_subslice(&body[cursor..], br#""fallback_credit_token":""#).and_then(|offset| {
            let start = cursor + offset;
            let value = start + br#""fallback_credit_token":""#.len();
            let end = value + body[value..].iter().position(|byte| *byte == b'"')? + 1;
            Some(cch_field_separator(body, cursor, start..end))
        });
    let mut search = cursor;
    let mut max_tokens = None;
    while let Some(offset) = find_subslice(&body[search..], br#""max_tokens":"#) {
        let start = search + offset;
        let value = start + br#""max_tokens":"#.len();
        let digits = body[value..]
            .iter()
            .take_while(|byte| byte.is_ascii_digit())
            .count();
        if digits > 0 {
            max_tokens = Some(cch_field_separator(body, cursor, start..value + digits));
            break;
        }
        search = value;
    }
    [fallback, credit, max_tokens]
        .into_iter()
        .flatten()
        .min_by_key(|range| range.start)
}

fn cch_field_separator(
    body: &[u8],
    cursor: usize,
    mut range: std::ops::Range<usize>,
) -> std::ops::Range<usize> {
    if body.get(range.end) == Some(&b',') {
        range.end += 1;
    } else if range.start > cursor && body.get(range.start - 1) == Some(&b',') {
        range.start -= 1;
    }
    range
}

fn cch_array_end(body: &[u8], mut cursor: usize) -> Option<usize> {
    let mut depth = 1_usize;
    let mut quoted = false;
    while let Some(byte) = body.get(cursor) {
        cursor += 1;
        match byte {
            b'\\' if quoted => cursor = cursor.saturating_add(1),
            b'"' => quoted = !quoted,
            b'[' if !quoted => depth += 1,
            b']' if !quoted => {
                depth -= 1;
                if depth == 0 {
                    return Some(cursor);
                }
            }
            _ => {}
        }
    }
    None
}

fn xxh64(input: &[u8], seed: u64) -> u64 {
    const P1: u64 = 0x9E37_79B1_85EB_CA87;
    const P2: u64 = 0xC2B2_AE3D_27D4_EB4F;
    const P3: u64 = 0x1656_67B1_9E37_79F9;
    const P4: u64 = 0x85EB_CA77_C2B2_AE63;
    const P5: u64 = 0x27D4_EB2F_1656_67C5;

    fn round(mut acc: u64, lane: u64) -> u64 {
        acc = acc.wrapping_add(lane.wrapping_mul(P2));
        acc = acc.rotate_left(31);
        acc.wrapping_mul(P1)
    }
    fn merge(mut acc: u64, lane: u64) -> u64 {
        acc ^= round(0, lane);
        acc.wrapping_mul(P1).wrapping_add(P4)
    }
    fn read_u64(bytes: &[u8]) -> u64 {
        let mut lane = [0_u8; 8];
        lane.copy_from_slice(&bytes[..8]);
        u64::from_le_bytes(lane)
    }
    fn read_u32(bytes: &[u8]) -> u32 {
        let mut lane = [0_u8; 4];
        lane.copy_from_slice(&bytes[..4]);
        u32::from_le_bytes(lane)
    }

    let len = input.len();
    let mut remaining = input;
    let mut acc = if len >= 32 {
        let mut v1 = seed.wrapping_add(P1).wrapping_add(P2);
        let mut v2 = seed.wrapping_add(P2);
        let mut v3 = seed;
        let mut v4 = seed.wrapping_sub(P1);
        while remaining.len() >= 32 {
            v1 = round(v1, read_u64(&remaining[0..8]));
            v2 = round(v2, read_u64(&remaining[8..16]));
            v3 = round(v3, read_u64(&remaining[16..24]));
            v4 = round(v4, read_u64(&remaining[24..32]));
            remaining = &remaining[32..];
        }
        let mut acc = v1
            .rotate_left(1)
            .wrapping_add(v2.rotate_left(7))
            .wrapping_add(v3.rotate_left(12))
            .wrapping_add(v4.rotate_left(18));
        acc = merge(acc, v1);
        acc = merge(acc, v2);
        acc = merge(acc, v3);
        merge(acc, v4)
    } else {
        seed.wrapping_add(P5)
    };
    acc = acc.wrapping_add(len as u64);
    while remaining.len() >= 8 {
        acc ^= round(0, read_u64(&remaining[0..8]));
        acc = acc.rotate_left(27).wrapping_mul(P1).wrapping_add(P4);
        remaining = &remaining[8..];
    }
    if remaining.len() >= 4 {
        acc ^= u64::from(read_u32(&remaining[0..4])).wrapping_mul(P1);
        acc = acc.rotate_left(23).wrapping_mul(P2).wrapping_add(P3);
        remaining = &remaining[4..];
    }
    for &byte in remaining {
        acc ^= u64::from(byte).wrapping_mul(P5);
        acc = acc.rotate_left(11).wrapping_mul(P1);
    }
    acc ^= acc >> 33;
    acc = acc.wrapping_mul(P2);
    acc ^= acc >> 29;
    acc = acc.wrapping_mul(P3);
    acc ^= acc >> 32;
    acc
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "unit tests fail fast on invalid fixtures"
)]
mod tests {
    use super::*;

    const CAPTURED_258_BILLING_HEADER: &str =
        "x-anthropic-billing-header: cc_version=2.1.258.1e2; cc_entrypoint=cli; cch=00000;";

    #[test]
    fn empty_prompt_suffix_matches_the_current_native_version() {
        assert_eq!(cc_version_suffix("", "2.1.257"), "9c3");
        assert_eq!(cc_version_suffix("hello", "2.1.257"), "468");
        assert_eq!(cc_version_suffix("", "2.1.258"), "1e2");
        assert_eq!(cc_version_suffix("hello", "2.1.258"), "59d");
        assert_eq!(cc_version_suffix("", "2.1.261"), "6af");
        assert_eq!(cc_version_suffix("", "2.1.270"), "ffc");
        assert_eq!(cc_version_suffix("hello", "2.1.270"), "4c5");
        assert_eq!(cc_version_suffix("", "2.1.284"), "12a");
        assert_eq!(cc_version_suffix("hello", "2.1.284"), "b93");
        assert_eq!(cc_version_suffix("hello", "2.1.289"), "7e2");
        assert_eq!(cc_version_suffix("", "2.1.293"), "9b8");
        assert_eq!(cc_version_suffix("hello", "2.1.293"), "72d");
        assert_eq!(cc_version_suffix("", "2.1.295"), "8a8");
        assert_eq!(cc_version_suffix("hello", "2.1.295"), "05c");
        assert_eq!(
            billing_header_prefix(SessionLineage::Root, ""),
            CLAUDE_OAUTH_BILLING_HEADER
        );
        assert_eq!(
            billing_header_prefix(SessionLineage::SpawnedChild, ""),
            CLAUDE_OAUTH_BILLING_HEADER_SUBAGENT
        );
        assert!(billing_header_prefix(SessionLineage::Root, "hello").contains("2.1.295.05c;"));
        assert!(
            billing_header_prefix(SessionLineage::SpawnedChild, "hello")
                .ends_with("cch=00000; cc_is_subagent=true;")
        );
    }

    #[test]
    fn native_version_suffix_indexes_utf16_before_utf8_encoding() {
        // Generated using the pinned native Nct expression in Node.js.
        for (text, expected) in [
            ("😀hello world abcdefghijkl", "f97"),
            ("abc😀defghijklmnopqrstuvwxyz", "493"),
            ("abcd😀x😀abcdefghijklmno", "13b"),
            ("abcd😀😀abcdefghijklmno", "fde"),
        ] {
            assert_eq!(cc_version_suffix(text, "2.1.258"), expected);
        }
    }

    #[test]
    fn native_request_id_grammar_is_bounded_ascii() {
        for valid in [
            "req_a",
            "req_abc-XYZ_123",
            &format!("req_{}", "a".repeat(36)),
        ] {
            assert!(valid_native_request_id(valid));
        }
        for invalid in [
            "req_",
            "msg_abc",
            "req_é",
            "req_a;",
            "req_a\n",
            &format!("req_{}", "a".repeat(37)),
        ] {
            assert!(!valid_native_request_id(invalid));
        }
    }

    #[test]
    fn cch_matches_published_xxhash64_vectors() {
        let cases: &[(&[u8], &str)] = &[
            (b"cch=00000", "a47f7"),
            (br#"{"messages":[],"cch=00000","x":1}"#, "3073d"),
            (
                b"x-anthropic-billing-header: cc_version=2.1.158; cc_entrypoint=cli; cch=00000;",
                "f2b0b",
            ),
        ];
        for (body, expected) in cases {
            let got = format!("{:05x}", xxh64(body, CCH_SEED) & 0xF_FFFF);
            assert_eq!(got, *expected, "{}", std::str::from_utf8(body).unwrap());
        }
    }

    #[test]
    fn native_executed_cch_excludes_model_values_and_max_tokens_fields() {
        // Fully authored local /v1/messages fixtures, captured from native
        // 2.1.258 and 2.1.270. Both emitted the identical Opus bytes below;
        // 2.1.270 also emitted the Sonnet variant with the same checksum.
        // All identity metadata is synthetic; no provider was contacted.
        for (model, max_tokens, wire_sha256) in [
            (
                "claude-opus-5",
                4096,
                "59848468ba54a5bbd704e43fdfb10bddc103db7ff5d193af050339d096b38e1f",
            ),
            (
                "claude-sonnet-5",
                8192,
                "d06fdb77b22e2dc2df1b88270f9c52b1c0a06ec56cd34c52d22d287ba92816e7",
            ),
        ] {
            let mut body = format!(
                r#"{{"model":"{model}","messages":[{{"role":"user","content":"hello"}}],"system":[{{"type":"text","text":"{CAPTURED_258_BILLING_HEADER}"}}],"tools":[],"metadata":{{"user_id":"synthetic"}},"max_tokens":{max_tokens},"thinking":{{"type":"disabled"}},"context_management":{{"edits":[]}},"stream":true,"output_config":{{"effort":"low"}}}}"#
            )
            .into_bytes();
            attest_claude_oauth_cch(&mut body).expect("patch");
            assert!(std::str::from_utf8(&body).unwrap().contains("cch=51c04;"));
            assert_eq!(sha256_hex(&[&body]), wire_sha256);
        }
    }

    #[test]
    fn native_cch_routing_fields_preserve_exact_retained_bytes() {
        // Source-derived exclusions; expected hashes are independent libxxhash
        // vectors over literal retained bytes, not this implementation's output.
        for body in [
            r#"{"model":"x","payload":true}"#,
            r#"{"max_tokens":1,"model":"x","payload":true}"#,
            r#"{"model":"x","max_tokens":99999,"payload":true}"#,
            r#"{"model":"x","payload":true,"max_tokens":42}"#,
            r#"{"fallbacks":[],"model":"x","payload":true}"#,
            r#"{"model":"x","fallback_credit_token":"synthetic","payload":true}"#,
            r#"{"model":"x","payload":true,"fallback_credit_token":"synthetic"}"#,
            r#"{"model":"x","payload":true,"fallbacks":[{"model":"y","nested":[["]\\\"["]]}]}"#,
            r#"{"max_tokens":1,"fallbacks":[],"fallback_credit_token":"synthetic","model":"x","payload":true,"max_tokens":2}"#,
        ] {
            serde_json::from_str::<serde_json::Value>(body).expect("valid synthetic JSON");
            assert_eq!(
                format!("{:05x}", cch_digest(body.as_bytes()) & 0xF_FFFF),
                "a611d",
                "{body}"
            );
        }
        assert_eq!(
            format!(
                "{:05x}",
                cch_digest(br#"{"model":"x","nested":{"model":"y","max_tokens":3,"keep":1}}"#)
                    & 0xF_FFFF
            ),
            "af1bf"
        );
        // Native does not remove a comma before the current scan cursor.
        // These adjacent terminal exclusions retain the bytes `true,}`.
        assert_eq!(
            format!(
                "{:05x}",
                cch_digest(br#"{"model":"x","payload":true,"max_tokens":1,"fallbacks":[],"fallback_credit_token":"synthetic"}"#) & 0xF_FFFF
            ),
            "5abe4"
        );
    }

    #[test]
    fn native_cch_literal_matching_does_not_reserialize_or_guess_invalid_values() {
        for (body, expected) in [
            (r#"{"model":"x","payload":true,"max_tokens": 4}"#, "ef489"),
            (r#"{"model":"x","payload":true,"max_tokens":"4"}"#, "5d5ed"),
            (r#"{"model":"x","payload":true,"max_tokens":-4}"#, "3f1db"),
            (r#"{"model":"x","payload":true,"fallbacks":[}"#, "01aa4"),
            (
                r#"{"model":"x","payload":true,"fallback_credit_token":"unterminated}"#,
                "5a131",
            ),
            (r#"{"model":"unterminated}"#, "945cb"),
        ] {
            assert_eq!(
                format!("{:05x}", cch_digest(body.as_bytes()) & 0xF_FFFF),
                expected,
                "{body}"
            );
        }
    }

    #[test]
    fn attestation_checks_included_content_but_ignores_native_routing_changes() {
        let original = format!(
            r#"{{"model":"claude-opus-5","max_tokens":4096,"system":[{{"type":"text","text":"{CAPTURED_258_BILLING_HEADER}"}}],"messages":[{{"role":"user","content":"hello"}}]}}"#
        );
        let mut attested = original.as_bytes().to_vec();
        attest_claude_oauth_cch(&mut attested).expect("patch");
        let attested = String::from_utf8(attested).unwrap();
        assert_eq!(attested, original.replace("cch=00000", "cch=14d76"));
        assert!(claude_oauth_cch_matches_body(
            attested
                .replace("claude-opus-5", "claude-sonnet-5")
                .replace("4096", "8192")
                .as_bytes()
        ));
        for changed in [
            attested.replace("hello", "world"),
            attested.replace("cc_entrypoint=cli", "cc_entrypoint=other"),
            attested.replace(r#""messages":"#, r#""messages": "#),
        ] {
            assert!(!claude_oauth_cch_matches_body(changed.as_bytes()));
        }
    }

    #[test]
    fn absent_billing_and_out_of_window_placeholders_never_mutate_the_body() {
        let no_billing =
            br#"{"model":"x","max_tokens":4,"messages":[{"role":"user","content":"cch=00000"}]}"#;
        let mut body = no_billing.to_vec();
        attest_claude_oauth_cch(&mut body).expect("not an OAuth billing body");
        assert_eq!(body, no_billing);
        assert!(!claude_oauth_cch_matches_body(&body));

        let outside = format!(
            "x-anthropic-billing-header:{}cch=00000",
            " ".repeat(CCH_SEARCH_WINDOW)
        );
        let mut body = outside.as_bytes().to_vec();
        assert!(attest_claude_oauth_cch(&mut body).is_err());
        assert_eq!(body, outside.as_bytes());
    }

    #[test]
    fn attestation_overwrites_only_the_billing_placeholder() {
        let mut body =
            br#"{"system":[{"type":"text","text":"x-anthropic-billing-header: cc_version=2.1.258.1e2; cc_entrypoint=cli; cch=00000;"}],"messages":[{"role":"user","content":"cch=00000"}]}"#
                .to_vec();
        attest_claude_oauth_cch(&mut body).expect("patch");
        let text = std::str::from_utf8(&body).unwrap();
        assert!(text.contains(r#""content":"cch=00000""#));
        assert!(!text.contains("cch=00000;"));
        assert!(claude_oauth_cch_matches_body(&body));
    }

    #[test]
    fn attestation_keeps_cc_is_subagent_after_the_hash() {
        let mut body =
            br#"{"system":[{"type":"text","text":"x-anthropic-billing-header: cc_version=2.1.258.59d; cc_entrypoint=cli; cch=00000; cc_is_subagent=true;"}]}"#
                .to_vec();
        attest_claude_oauth_cch(&mut body).expect("patch");
        let text = std::str::from_utf8(&body).unwrap();
        assert!(text.contains("cc_is_subagent=true;"));
        assert!(!text.contains("cch=00000"));
        assert!(claude_oauth_cch_matches_body(&body));
    }

    #[test]
    fn missing_placeholder_on_a_billing_header_fails_closed() {
        let mut body = br#"{"text":"x-anthropic-billing-header: cc_version=2.1.258.1e2; cc_entrypoint=cli; cch=dead0"}"#
            .to_vec();
        assert!(attest_claude_oauth_cch(&mut body).is_err());
    }
}
