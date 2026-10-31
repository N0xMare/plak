# Known limits and security model

Plak is an agent harness, not a sandbox. This page summarizes what it does and
does not protect; the [security specification](../spec/04-security-and-boundaries.md)
is the authority.

## Process tools run with your host authority

Shell and other process tools run as the user who started Plak, with that
user's full authority, on Linux exactly as on macOS. A model-launched command
can read every file you can read, write anywhere you can write, signal your
processes, and use the network. There is no OS sandbox, no sandbox setting, and
no degraded containment mode.

Only the file tools are confined: they resolve paths relative to the workspace
root and refuse symlinks and escapes. That confinement does not extend to
commands a process tool runs. Worker isolation for JavaScript and Python
evaluators is not workspace isolation.

If you need a real boundary around your machine, run Plak in a container or a
VM.

## No approval gates

By default nothing pauses a model-requested tool call for your confirmation.
Coordination-effect decisions are the only approvals. Agent tiers and
`access = "read-execute"` narrow which tools are offered; neither is a security
boundary, because a shell can still do anything you can.

## Credentials are plaintext files

Subscription OAuth credentials live under `$XDG_STATE_HOME/plak/subscriptions/`
and API key files under `<plak home>/secrets/`, as plaintext files, mode `0600`
inside `0700` directories. There is no OS keychain integration, so any process
running as you, including a process tool, can read them.

## Claude subscription OAuth

The `claude` route uses the Claude Code client ID and sends Messages requests
identifying as Claude Code, including its client headers, system identity text,
and billing attestation. Plak is not Claude Code or an official Anthropic
client. The separate `anthropic` API-key route does not use this OAuth
identity.

## Very long turns can need recovery

A session records every tool call, model round and command of a turn and
reclaims that record only between turns. A single turn with a very large
number of rounds and parallel tool calls can exhaust the per-session record
limit before it finishes; the turn then ends as recovery-required, and
reopening the session settles it. Splitting very long work across turns avoids
this.

## Many short turns need compaction

A session keeps at most 4,096 history segments, and each short prompt adds
about three, so a long session of small turns reaches the limit with very few
tokens in use. With auto compaction on, Plak compacts once the session holds
3,072 segments, even far below the token trigger. Near the limit (4,032
segments) a new prompt is refused with a context-capacity error. Run
`/compact` to compact this session, `/fork-then-compact` to compact a fork and
keep this session as it is, or `/clear` to start a new conversation, or turn
auto compaction on. The refused session still resumes and exports. A session an earlier build left stuck at
the limit (a turn that never finished, with every later resume failing)
cannot be resumed or exported.

## Compaction summaries depend on the model

When a session compacts, the model writes the summary that replaces the earlier
conversation (Codex compaction is written by the Codex service). A model that
recalls poorly across large tool outputs can leave details out, and after the
compaction it re-reads files or redoes steps. DeepSeek V4.1 Flash
(`deepseek/deepseek-v4.1-flash-1017`) on OpenRouter does this once one or two
large tool results follow an earlier fact, far below its 1M-token window: its
summary keeps the first request and the latest tool call and drops the steps in
between. Grok and Claude summaries kept those details in the same tests. When
earlier values matter in a long session, write them to a file the later steps
read, or use a model that keeps them.

## Plak's own git steps

When Plak mints, seals or adopts a copy, it runs `git` with inherited `GIT_*`
steering variables removed, hooks and fsmonitor disabled, external diff drivers and
textconv ignored, and scratch files private to you. This keeps repository
configuration from running code inside Plak's own steps; it does not constrain
what a model-launched shell does.

## Write reads the file before it overwrites it

Write refuses to overwrite a file the agent has not read in its current form
in this attachment; reads are not persisted, so after resume or reload the
agent reads again. Results name each changed path and its action.
