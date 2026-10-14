# Embedding Plakous in your own Rust program

Plakous is an embeddable single-agent library. Plak is the CLI/TUI product built
on it. Nothing is published to crates.io: depend on a source checkout by path.
Use Rust **1.97.1** or later and edition **2024**. Supported platforms are Linux
x86_64 musl and Apple Silicon macOS.

## Four explicit choices

Choose your model, tool presentation, evaluator and session backend explicitly.
They are separate inputs, not arbitrary independent combinations: admission
refuses unsupported combinations without switching routes or interpreters.

- A Custom `LanguageModel` can expose tools directly without an evaluator.
- Native Codex Code Mode requires an enrolled Codex/OpenAI target. A Custom
  descriptor cannot gain enrollment by copying Codex protocol or model names.
- A Custom model can use `ProductJavaScriptV1` with the neutral
  `CodingPresentation::Pi` vocabulary and its own extension evidence. This is
  consumer-owned composition, not a provider-native claim.
- SQLite sessions can be released and reopened; mutating tools require a
  required-durability backend, not merely the `sessions-sqlite` feature.

## Features

Defaults are empty. Agent, application, construction, model, tools, profiles,
prompts and session workflows (including the feed) are always available.

| Feature | Adds |
| --- | --- |
| `composition` | JavaScript presentations and the sidecar client; no credentials, HTTP or Python |
| `composition-python` | Monty-backed Python PTC; implies `composition` |
| `sessions-sqlite` | `SessionBackend::sqlite` |
| `anthropic`, `openrouter` | Provider integrations and explicit credential/model inputs |
| `openai`, `xai` | Provider integration exports; `OpenAiLanguageModel`/`XaiLanguageModel` also require `native-subscriptions` |
| `auth` | Credential store, OAuth, pins and account binding; implies `openai` and `xai` |
| `native-subscriptions` | Subscription admission/authentication; implies both composition features |
| `full` | All of these features, including Python and subscription dependencies |

The always-on `application::run_once_until_signal` takes your cancellation
future. It does not install Ctrl-C handling, open browsers or read terminal
secrets; those are Plak product adapters.

## Your manifest

This complete manifest supports copying a scripted example and
`examples/common/scripted.rs` into a consumer crate. Adjust the checkout path:

```toml
[package]
name = "my-agent"
version = "0.1.0"
edition = "2024"
rust-version = "1.97.1"

[dependencies]
plakous = { path = "../plak/crates/plakous", features = ["composition", "sessions-sqlite"] }
tokio = { version = "1.48", features = ["rt-multi-thread", "macros", "sync", "time"] }
serde_json = "1.0"
uuid = { version = "1.18", features = ["v4"] }
futures-util = "0.3"
tempfile = "3.23"
```

Select `openrouter` as well for `provider_direct`. These utility dependencies
belong to the examples, not the facade's API. Keep a downstream `Cargo.lock`,
fetch its closure, then use `--locked --offline` for reproducible offline runs.
The manifest plus lockfile in `tests/fixtures/js-embedder-consumer` additionally
pins the JavaScript-only API's external nameability; it is a compile fixture,
not a second runnable agent implementation.

JavaScript-only consumers need no `[patch]` table. Python,
`native-subscriptions` and `full` reach the Monty dependency closure; repeat the
checkout-relative patch tables documented in spec 06 §6.1. Cargo does not
inherit a dependency's patch tables.

## Examples

The repository examples use only the public facade plus the utility crates
above. Their shared scripted model is a deterministic Custom adapter, not a
repository test-support dependency. Run from the checkout root:

```sh
cargo run --locked -p plakous --example cancellation_shutdown
cargo run --locked -p plakous --example direct_custom_tool --features sessions-sqlite
cargo run --locked -p plakous --example durable_session --features sessions-sqlite
cargo build --locked -p plakous-js-runtime --bin plakous-js-host
cargo run --locked -p plakous --example worker_js_sidecar \
  --features composition,sessions-sqlite -- target/debug/plakous-js-host
cargo run --locked -p plakous --example provider_direct --features openrouter \
  -- ./openrouter-key openai/gpt-4o-mini
```

`provider_direct` uses a caller-supplied key-file path and one exact model
literal. With no arguments it prints what it needs and dispatches nothing. Its
live path is opt-in, not an offline test. Other embedders must not use
`with_plak_attribution`: those headers identify Plak product traffic.

`worker_js_sidecar` canonicalizes its explicit argument before attachment. The
facade itself requires an **absolute, canonical path without symlink
components**, records the regular file's SHA-256 and rejects noncanonical paths.
It re-checks the canonical path and hash before spawning, not per cell, but
cannot rule out a replacement between that check and OS execution. It does not
discover a worker or prove the file is a genuine `plakous-js-host`.
The offline gate compiles this example; a dedicated path test pins canonical
attachment/refusal. The command above runs the actual worker workflow.

## Pick a presentation

Use Direct for the smallest Custom-tool integration. Put your frozen registry's
selected definitions into `ProviderBehaviorConfiguration.tools` as local
`ConfiguredTool` entries; registering an executor alone does not expose it.
`direct_custom_tool` shows this, together with streamed turn events and a
reopen from a persisted locator string.

For enrolled Codex/OpenAI Code Mode, `CodexMinimalLocalCodeModeV1` exposes
`exec`/`wait` over a minimal nested coding catalog. Use
`CodexNativeLocalCodeModeV1` only when you need the exact seven-tool reference.
Custom models instead select `ProductJavaScriptCompositionV1` (`exec_js` and
`wait_js`), compile the Pi nested vocabulary, and attach independent
`ProductExtensionEvidence` with caller issuer and qualification-candidate
identities. Such a receipt is self-attestation, not successful live qualification.
`PythonPtcV1` is the explicit Python alternative with its own worker and
`composition-python` feature. Attach at most one profile/engine; there is no
language switching or fallback.

## Construction

`plakous::construction` computes facts the library already owns:
`CustomProviderConfigurationRequest` derives descriptor selection fields;
`bind_custom_model_authority` binds authority after the context contract is
sealed; `provider_model_projection_identity` derives a projection target; and
`compile_local_composition_plan` compiles the no-Direct-exposure harness plan
for an attached evaluator. You still supply your own issuer identities, policy
and evidence. Conflicting inputs are refused, not repaired.

## Durability and lifecycle

Mutating tools need `LocalDurabilityDisposition::DurableRequired` **and** a
session store. Select `sessions-sqlite` and construct through `AgentSessions`.
Store and workspace paths must not pass through a symlink; the store refuses
them as storage unavailable. On macOS, `/tmp` and `/var` (and so the default
temporary directory) are symlinks: canonicalize such a path first, as the
examples do with `std::fs::canonicalize`.
SQLite stores are format-epoch bound: old schemas/checkpoints are not migrated
or decoded. The session export document is the cross-build carrier, supporting
its current and immediately previous document version; a `SessionLocator` is
not a cross-build export.

Within the same format/build, persist a locator with `to_string()` after
release and recover it with `parse::<SessionLocator>()` on restart. Keep the
backend and immutable runtime binding consistent. `SessionOperationId` is an
idempotency key: reuse it for an identical logical operation's retry, never for
different inputs.

Take a turn's `TurnEvents` to stream observations; the final `TurnResult`, not
an event, is terminal authority. Observation loss must not replace the result.
Cancellation settles one turn and leaves the agent usable. Shutdown is
terminal: it cancels and joins owned work and refuses later prompts. See
`cancellation_shutdown` for both paths, `durable_session` for exact reopening
and `direct_custom_tool` for events and a persisted locator.

## References

- [Build and support](../spec/06-build-and-support-surface.md): features, patch tables and public contracts.
- [Library semantics](../spec/01-plakous-library.md): admission, engines, sessions and lifetime.
