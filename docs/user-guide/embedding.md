# Embedding Plakous in your own Rust program

Plakous is an embeddable single-agent library. Plak is the CLI/TUI product built
on it. Depend on `plakous` from crates.io, or on a source checkout by path.
Use Rust **1.99.0** or later and edition **2024**. Supported platforms are Linux
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
| `composition` | Code Mode, JavaScript and Python PTC over the cell client; no credentials, HTTP or interpreter dependencies |
| `sessions-sqlite` | `SessionBackend::sqlite` |
| `anthropic`, `openrouter` | Provider integrations and explicit credential/model inputs |
| `openai`, `xai` | Provider integration exports; `OpenAiLanguageModel`/`XaiLanguageModel` also require `native-subscriptions` |
| `auth` | Credential store, OAuth, pins and account binding; implies `openai` and `xai` |
| `native-subscriptions` | Subscription admission/authentication; implies `composition` |
| `full` | All of these features, including Python and subscription dependencies |

The always-on `application::run_once_until_signal` takes your cancellation
future. It does not install Ctrl-C handling, open browsers or read terminal
secrets; those are Plak product adapters.

## Your manifest

This complete manifest supports copying a scripted example and
`examples/common/scripted.rs` into a consumer crate. For a source checkout,
replace the version with `path = "../plak/crates/plakous"`:

```toml
[package]
name = "my-agent"
version = "0.1.0"
edition = "2024"
rust-version = "1.99.0"

[dependencies]
plakous = { version = "0.0.2", features = ["composition", "sessions-sqlite"] }
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

No facade feature selection needs a `[patch]` table, including those that reach
the Python evaluator. The evaluators run as worker executables:
`cargo install --locked plakous-js-runtime plakous-py-worker` installs
`plakous-js-host` and `plakous-py-host`.

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

Compile the desired presentation, convert its typed descriptor into
`plakous::tools::CellDescriptor`, and select it with the single profile constructor:

```rust,ignore
let descriptor: plakous::tools::CellDescriptor = compiled.into();
let profile = plakous::agent::LocalCompositionProfile::cell(&descriptor)?;
```

The descriptor selects the exact worker contract; supply a verified QuickJS or
Monty worker artifact accordingly. Product JavaScript and Python require
`ProductExtensionEvidence::cell` and the coding presentation kind. Native
Codex profiles forbid product extension evidence. Python has no separate
component identities: its receipt binds the complete descriptor and its exact
outer/nested projections, just as JavaScript does.

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
`PythonPtcV1` is the explicit Python alternative with its own worker, using
the same `composition` feature. Attach at most one profile/engine; there is no
language switching or fallback.

Select the profile from its compiled descriptor, then verify the explicit
worker path with `LocalCompositionEngineArtifact::quickjs_worker_v1` or
`monty_worker_v1`. For either product-owned profile, derive the receipt with
`ProductExtensionEvidence::cell(&profile, &artifact, issuer, qualification,
&configuration)`; its limits come from the profile. Close the binding with
`LocalCompositionAttachment::builder(profile)`, supplying the artifact,
product evidence and coding presentation kind, and pass it to
`attach_local_composition(builder, Some(attachment))`. Native Codex profiles
use that same builder and attach path but forbid product evidence.

## Typed Python SDK

The SDK is an explicit embedding choice through `composition`. It uses the
same `plakous-py-host` sidecar, but exposes only `run_py` and starts each cell
with fresh globals. It is not a product selector yet.

Register the coding packs and SDK extensions with the same process service:

```rust,ignore
use std::sync::Arc;
use plakous::tools::{
    InMemoryPlanState, ProcessSessionTools, PythonHarnessSdkV1,
    ToolRegistryBuilder, register_packs, standard_coding_packs,
};

let process = Arc::new(ProcessSessionTools::posix_sh()?);
let mut registry = ToolRegistryBuilder::default();
register_packs(
    &standard_coding_packs(Arc::new(InMemoryPlanState::new()), process.clone()),
    &mut registry,
)?;
PythonHarnessSdkV1::register_into(&mut registry, process)?;
let registry = registry.freeze();
let sdk = PythonHarnessSdkV1::compile(job_config, registry.clone(), target)?;
let profile = LocalCompositionProfile::cell(&(&sdk).into())?;
let artifact = LocalCompositionEngineArtifact::monty_worker_v1(&worker_path)?;
let receipt = ProductExtensionEvidence::cell(
    &profile, &artifact, issuer, qualification, &configuration,
)?;
let attachment = LocalCompositionAttachment::builder(profile)
    .engine_artifact(artifact)
    .product_extension_evidence(receipt)
    .build()?;
let builder = attach_local_composition(builder, Some(attachment))?;
```

Here `target` is the exact projection identity for your compiled provider
configuration; expose the `run_py` marker as a local ordinary function. Attach
the matching SDK outer projection, not a Direct coding menu. The SDK needs no
coding-presentation kind. Mutations require an explicitly permitting tool
policy and a required-durability backend, such as `SessionBackend::sqlite`.

SDK code awaits `ctx.read/list/grep`, `fs.apply([fs.Write(...), ...])`,
`proc.run/shell/start/poll/stdin/cancel/join`, `plan.update` and
`state.get/put/list/drop`. For example:

```python
files = await asyncio.gather(ctx.read("a.txt"), ctx.read("b.txt"))
await fs.apply([fs.Write("combined.txt", files[0].text + files[1].text,
                        expect="absent")])
await state.put("last_files", [file.path for file in files])
print(await state.get("last_files"))
```

Cells are type-checked before I/O. Named state survives exact reopen and
publishes only with a successful presented terminal; globals do not survive.
Invalid state keys/values raise `ValueError`, capacity refusals raise
`OverflowError`, and host failures raise `RuntimeError`. Values are bounded
by their encoded representation (1 MiB per result, 16 MiB per cell); the
foreground budget is 300 seconds. There is no wait tool: use `proc.start` for
long jobs. Processes have the user's full authority, including on
process-enabled read-only and execute tiers. File edits remain denied on those
tiers. Output over 16 KiB carries a `Ref` that `ctx.read` can page. Exports do
not carry named state.

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
