Synthetic HTTP 429 body matching the upstream Codex response shape. All values
are invented; no captured account, request or credential data is present.

Source: openai/codex commit 9756b4937926e1202a4dcaafe8b0f7ba4e7dbb80,
`codex-rs/codex-api/src/api_bridge.rs`: `UsageErrorResponse`, `UsageErrorBody`
and the HTTP 429 mapping. The label is `error.type = usage_limit_reached`;
`resets_at` is optional signed Unix seconds. `plan_type` is optional and does
not change classification. The error is terminal, unlike an ordinary 429.
