# plakd and Plakous

plakd is a host daemon/server and operator CLI for software-engineering agents.
Plakous is the embeddable Rust harness library underneath it. The repository
ships no terminal or web application client: integrators build and maintain
their own against the host wire protocol.

The supported product package is `plakd`; its library target and executable
use the same name. This repository provides no `plak` executable shim or
package alias.

Plakous owns the single-agent loop, tools, providers, admission, persistence
and recovery. plakd adds product configuration, hosted sessions, paired runners
and recursive orchestration above the public facade. Responses, Messages and
Chat Completions have separate protocol kernels so provider-specific behavior
does not disappear behind a lowest-common-denominator interface.

This is a rapidly changing pre-release prototype, not a hardened coding-agent
product. Agents execute commands and change files with the host user's full
authority, without an OS sandbox or tool-approval gates. Use a container or VM
when you need an isolation boundary. See the
[security model](docs/user-guide/known-limits-and-security-model.md).

## Install and operate

Supported platforms are Apple Silicon macOS and Linux x86_64 musl. The release
installer installs the product and matching JavaScript/Python workers under
`~/.local/lib/plak`, with links in `~/.local/bin`:

```sh
curl -fsSL https://raw.githubusercontent.com/N0xMare/plak/v0.0.4/install.sh | PLAK_VERSION=0.0.4 sh
```

For a source checkout with Rust 1.100.0 and the documented build prerequisites,
use `just install release`. Detailed installation, Cargo alternatives and
updates belong in [getting started](docs/user-guide/getting-started.md).

```sh
plakd help
plakd login codex
plakd models codex
plakd doctor
plakd host
```

Bare `plakd` prints help and exits successfully; it starts no host or session.
The host serves sessions over its private control socket and can optionally
serve an external web build. It owns accounts/auth, settings, catalogs,
session operations, runner placement and upgrade authority.

The operator CLI also provides `plakd run` for bounded batch tasks and
`plakd agent` for the long-lived JSONL session projection. Resume is explicit
through run, agent or wire operations, not a terminal launcher.

Subscription accounts are `codex`, `grok` and `claude`; API-key accounts
are `openai`, `xai-api`, `anthropic` and `openrouter`. Direct API accounts
have separate credentials and billing. Claude OAuth remains a first-class
route; use it at your own risk. See
[providers and accounts](docs/user-guide/providers-and-accounts.md) for login,
review windows, exact model selection and supported compositions, and
[live status](tests/live/STATUS.md) for current qualification evidence.

Each built-in provider pin carries a review date. The subscription profiles (Codex,
Grok, Claude) expire 90 days after their review, at 00:00 UTC: all three were reviewed
on 2026-11-15 and are valid until 2027-02-13T00:00:00Z. After its date a build refuses
that subscription route until a newer build renews the review; see
[troubleshooting](docs/user-guide/troubleshooting.md) ("native compatibility profile
compiled into this build is inactive"). The documented API routes (OpenAI Platform,
Anthropic API, xAI Platform and OpenRouter Chat) are due for review 90 days after theirs
(2027-02-13 for every route in this build) but never refuse on the calendar. `plakd
doctor` and `plakd status` warn within 14 days of either date; publication dry runs and
uploads require more than a third of every review window remaining. A re-review that
changes no pinned contract does not ask you to accept the notice again.

## Build your own client

Use `plakd wire schema` to obtain the exact build's schema. Spawn
`plakd host connect --stdio` from your application and exchange wire frames on
its pipes, or connect to the host's control socket or authenticated WebSocket.
The stdio bridge handles discovery; only explicit local `--auto-start`
permits it to start a missing host. It has no embedded-session fallback.

The current wire protocol is exactly 10, with no version negotiation.
Clients own rendering, navigation, input and local preferences; settings
metadata, credential operations, admission and durable authority remain on
the host. Start with [Integrating](docs/user-guide/integrating.md) for a
handshake/attach transcript and client implementation responsibilities, and
[host service setup](docs/user-guide/host-service.md) for installation as a
user service. There is no bundled application-client SDK.

## Embed Plakous

A Rust application that owns construction, credentials, tools and persistence
can depend on the `plakous` facade instead of launching the product.
Default features are empty. Provider access, local composition, MCP and SQLite
are explicit feature choices; complete-workflow consumers need only the facade.

```sh
cargo run --locked -p plakous --example durable_session --features sessions-sqlite
```

The [embedding guide](docs/user-guide/embedding.md) covers manifests, public
examples, worker artifacts and lifecycle. Cross-build session transfer uses
the versioned export document, not another build's native stores.

## Documentation and development

- [User guide](docs/user-guide/README.md): setup, configuration, operator commands,
  client integration, orchestration and troubleshooting.
- [Behavioral specification](docs/spec/README.md): normative architecture,
  persistence, security and supported surfaces. Begin with the
  [overview and crate map](docs/spec/00-overview.md).
- [Reference configuration](docs/reference-config/) and
  [scenario homes](tests/fixtures/config-scenarios/): complete configuration examples.
- [Changelog](CHANGELOG.md), [contributing](.github/CONTRIBUTING.md) and
  [security policy](.github/SECURITY.md).

Product code lives in [`bin/plakd/`](bin/plakd/); libraries in
[`crates/`](crates/), with the public facade in
[`crates/plakous/`](crates/plakous/). [`tests/`](tests/) contains offline
integration and consumer contracts. [`tools/`](tools/) supplies build, gate,
release and profiling entrypoints. [`eval/`](eval/) is an opt-in Harbor
bakeoff, not a gate or a second runtime.

Use `just dev-tools install` for pinned development tools and
`just fast <packages>` for worker evidence, plus the groups your change
needs. Integration and publication gates are defined in
[spec 06](docs/spec/06-build-and-support-surface.md).
Live tests are opt-in and may incur provider usage; see
[testing and evidence](docs/spec/05-testing-and-evidence.md).

## License

plakd and Plakous are licensed under the [MIT License](LICENSE).

Third-party material keeps its upstream license:

- OpenAI Codex prompt and code-mode protocol material (Apache-2.0) carries
  its upstream license and attribution.
- The Python worker embeds micro-monty (`MIT AND PSF-2.0 AND Unicode-3.0`).
  The release `THIRD-PARTY` inventory records that expression; its license
  directory includes the upstream notices and the supplemental license texts
  from `crates/runtimes/py-worker/`.
- Excerpts reproduced from the OpenAI Codex CLI (Apache-2.0, Copyright 2025
  OpenAI; license and notice in `crates/harness/prompts/vendor/codex-prompt/`): the `apply_patch`
  grammar and description (`crates/harness/tool-files/src/codex_patch.rs`,
  `crates/harness/tool-files/src/text.rs`), the `exec_command` description
  (`crates/harness/tool-process/src/text.rs`), and the model catalog facts in
  `crates/hosting/hosted/src/agent/fixtures/codex-reference-catalog.json`.
- Tool schemas adapted from xAI's [Grok Build](https://github.com/xai-org/grok-build)
  1.0.46 (Apache-2.0; license text in `crates/harness/prompts/vendor/codex-prompt/LICENSE`):
  `crates/harness/tool-process/src/grok.rs`.
- Prompt text and tool descriptions adapted from
  [pi](https://github.com/earendil-works/pi) 1.1.0, Copyright (c) 2025 Mario
  Zechner, used under the MIT License (the same terms as [LICENSE](LICENSE)):
  `crates/harness/prompts/src/bases/pi.rs`, `crates/harness/tool-process/src/pi.rs`,
  `crates/harness/tool-process/src/text.rs` and the `PI_*_DESCRIPTION` texts in
  `crates/harness/tool-files/src/lib.rs`.
