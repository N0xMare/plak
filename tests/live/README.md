# Live scenarios

## Sanitized Codex Catalog

The ignored maintainer capture uses Plak's own compatibility acceptance,
credential loading and catalog admission. It makes no inference call. Supply
the XDG state home used by the installed Plak (not a credential file), after
accepting the current profile with `plak models codex`:

```sh
PLAK_CAPTURE_STATE_HOME="$HOME/.local/state" \
PLAK_CAPTURE_OUTPUT="$PWD/crates/hosting/hosted/src/agent/fixtures/codex-authenticated-catalog.json" \
cargo test --locked -p plakous --features native-subscriptions capture_codex_authenticated_catalog -- --ignored
```

Use the configured state home if it differs. The writer allowlists typed model
facts only; account ids, emails, credential generations, catalog receipts,
ETags, access-window expiry and all other account-scoped values are omitted.
It never prints or exports credentials or the raw authenticated response.


Four opt-in commands that drive the real `plak` against real provider accounts.
They spend provider usage and never run in a gate. `STATUS.md` records what they
last showed.

```sh
just smoke            # grok (grok-4.7, low)
just smoke claude     # Claude OAuth (claude-sonnet-5, low)
just smoke claude-sonnet-medium
just smoke claude-sonnet55-low --only cli_resume,jsonl_task,tui
just smoke claude-sonnet55-medium --only cli_resume,jsonl_task,tui
just smoke claude-haiku55-low    # claude-haiku-5-5
just smoke claude-opus-low       # claude-opus-5-5
just smoke claude-opus-medium
just smoke deepseek-messages --only cli_task,cli_resume,fault_reset
just smoke grok --only cancel,fault_reset
just smoke grok --only wrap_up
just smoke codex --only wrap_up
just smoke grok --only py_ptc
just smoke grok --only in_turn_compaction,overflow,kill_reopen,child_compaction
just soak grok --long-session --only in_turn_compaction
just soak grok --long-session --all
PLAK_LIVE_GROWTH_PAGES=20 just smoke claude-haiku55-low --only in_turn_compaction
just soak claude --hours 2
just tui-soak claude-opus-medium --hours 4
python3 tests/live/tui_soak.py claude-opus-medium --hours 1
just jobs grok
just jobs grok --only promotion_budget,steer_command,ctrl_b
python3 tests/live/jobs.py grok --only crash_jobs
```

Startup lookup fault proof (opt-in, real provider usage, fresh private home):
`python3 tests/live/lookup_retry.py deepseek --fault connect`, or `--fault 503`.
The same driver accepts Codex, Grok and the API-key routes; Claude subscription
has a pinned local catalog and no catalog GET. It requires a retry status, a
completed durable turn and an independently checked file, and reports its
session id and binary hash. It does not alter inference retry budgets.

The Python drivers default to `target/release/plak`. Build it with
`cargo build --release --locked -p plak --features plak/test-hooks` before a
direct Python invocation; `--binary PATH` selects another capture-capable
executable. A `test-hooks` binary only captures under the
`tests/provider-authority` of the checkout that built it, so run the driver
from that checkout: smoke refuses a binary built elsewhere before launching. `test-hooks` enables `PLAK_TEST_FAULTS` (spec 05 §2.8).
The drivers put ordinary artifacts under
`target/live/<kind>-<route>-<stamp>/`: a private `PLAK_HOME`, the repos the
agent worked in, every JSONL transcript and stderr, the TUI's PTY bytes, and a
`report.json`.
Smoke and TUI soak reports pin the executable's SHA-256 at start and end and
fail if it changes mid-run. Headless soak reports both digests, allowing its
intentional byte-distinct rebuild; with `--no-rebuild` a changed executable is
an evidence failure. `task_repo_commit` names the task fixture, **not** Plak's
source. Record the clean Plak source revision and build invocation separately
before a publication run; a digest alone cannot prove how a binary was built.

Raw exchange capture is automatic and required on **every** real launch:
`plak run`, `plak agent`, and TUI launch/reopen, in smoke, both soaks, and the job driver.
There is no capture opt-in flag. Each run gets a uniquely named private root
under ignored `tests/provider-authority/`, with fresh owned mode-0700 process
segments. Inherited capture paths are replaced, never reused; non-private,
foreign-owned, symlinked or nonempty segments are refused before launch.
Reports name the segments and exchange counts and always check their integrity.
Smoke refuses qualification if any launched process has missing or incomplete
raw evidence, even if its functional oracle passes or its failure is pending;
TUI soak likewise requires a response in every process. Headless soak retains
its per-task authority-witness versus unexplained-no-response classification.
A physical failure can still leave a partial response; the capture result does
not imply model success.
Each exchange must have matching `.request`, `.body` and `.info` basenames.
The request body and pending/incomplete metadata are persisted before dispatch;
terminal metadata must no longer be pending, must say `incomplete=false`, and
must record exactly the `.body` file's byte length. Missing or malformed metadata,
unpaired files and segments at the 512-exchange process limit fail integrity;
old response-only captures are not accepted. The required capture sink refuses
dispatch when it cannot persist a request or has exhausted that limit.
A fully recorded transport failure can pass physical capture integrity without
being a successful model turn.
Captured runs also keep private xAI authority diagnostics in the scratch
`PLAK_HOME/logs/plak.log`. The headless report classifies each task's evidence
separately, so an uncaptured task cannot be hidden by responses from later
tasks; the TUI report requires the diagnostics file and gives process IDs for
correlation. A local authority witness without a response is a clue, not proof
of the entire failure sequence; missing diagnostics or unexplained absence of
a response is an evidence gap. An empty segment still needs that diagnostic
authority-witness classification; physical integrity alone does not qualify it.
These captures contain inference request bodies, not proof of transmission, and
only response bytes delivered to the consumer, not every network byte. They do
not capture HTTP headers (including auth), token refresh, or failures in
unrelated provider paths.
Use a `test-hooks` binary and a fresh directory for each process.
Read [spec 05 §3.5](../../docs/spec/05-testing-and-evidence.md#35-private-provider-wire-captures) before inspecting or retaining
these sensitive raw bytes; never attach them to a public report.

**Accounts.** The scratch home reuses the real logins: OAuth credentials are
XDG state outside `PLAK_HOME`, so a token refresh during a run writes the real
store. For OpenRouter, first use `plak key set openrouter`; the harness copies
`~/.plak/secrets/openrouter.key` into its private home and removes
`OPENROUTER_API_KEY` from child environments so a stale variable cannot shadow
the working file. Direct API accounts are the same: `openai` copies
`openai.key` and drops `OPENAI_API_KEY`, `anthropic` copies `anthropic.key`
and drops `ANTHROPIC_API_KEY`, and `xai-api` copies the shared `xai.key` slot
and drops `XAI_API_KEY`. Credential values are never printed. Every route name
in the catalog below is accepted by all three drivers. Claude OAuth inference
is targeted at `api.anthropic.com/v1/messages?beta=true`, including the query
in injected-fault selection so token and control requests are not counted.
The Anthropic API-key route uses `/v1/messages` without that query. OpenAI API
inference is `api.openai.com/v1/responses` and xAI API inference is
`api.x.ai/v1/responses`.

**Smoke** (`smoke.py`, about three minutes) runs twelve scenarios, each in its own
small git repo:

| Scenario | Surface | Passes when |
| --- | --- | --- |
| `cli_task` | `plak run` | a planted bug is fixed, a test is added, the suite passes |
| `cli_resume` | `plak run --session` | the second run remembers the first |
| `jsonl_task` | `plak agent` | the same coding task over JSONL |
| `jsonl_resume` | `plak agent --session` | a reopened process continues the session |
| `compaction` | `compact` command or capacity recovery | the first turn reads `fold.txt` (10,000 tokens, bytes/4, and it does not contain the code word); manual compaction commits, or on a catalog window unknown or at most 65,536, a first-turn `ContextLimit` has a failed lifecycle terminal followed by a capacity start and committed compact; the latter reports `capacity recovery compaction, context carried`. Known larger windows remain harness failures. The second turn and fact question complete, and `ZEBRA-42` survives without appearing in any repository file before the question |
| `cancel` | `interrupt` | the turn ends `cancelled` mid-tool and the next prompt completes |
| `fault_429`, `fault_529` | injected before dispatch | the turn still completes; the fault is one response, so a turn that fails after more than one HTTP retry was refused by the real provider and scores `provider` |
| `fault_reset` | injected after 3 SSE events | a cost-only turn request replays once with its partial output discarded (a `retry` event, `stream_interrupted`, `output_discarded`), the turn completes with one effect, the session continues; a real provider HTTP error (an `http_status` retry) that ends the faulted request before the reset fires scores `provider` |
| `fault_stall` | 20 s stall after 2 events | the turn completes |
| `subagents` | `spawn_agent` × 2 | two forked children, both candidates adopted |
| `tui` | TUI on a PTY | two turns, then a clean `Ctrl+Q` |

`wrap_up` is opt-in (`--only wrap_up`) and is not one of the twelve. It uses a
private `PLAK_HOME` with `[turn] max_model_rounds = 2`, asks `plak run` to `ls`
and then `cat` each file one at a time, and passes when that turn completes
with assistant text, the session log records `turn.operation_budget_exhausted`,
neither the CLI streams nor the session log contain `ProtectedHistory` or
`stable-prefix binding differs`, and a second `plak run --session` prompt
`say continued` completes. `ModelArtifactPrefixDrift` is not persisted
(spec 01 §11.2), so the scenario cannot see artifact reuse except by that
binding error staying absent and the next turn completing. The default run
does not list it: `--only` selects it from the opt-in set.

`py_ptc` is opt-in too (`--only py_ptc`, any route). It switches a private
`PLAK_HOME`'s templates to `composition = "py-ptc"`, passes
`--composition py-ptc`, and asks one `plak run` turn to read `token.txt` with a
nested tool inside an `exec_py` cell, print it, handle a `ValueError`, keep the
token in a global, and return it reversed from a second cell. It passes when
the turn completes, the captured exchanges show `exec_py`, the second cell's
`TOKEN[::-1]` and the `handled ValueError` output, and the answer carries the
reversed token, which exists only in the repository.

**Feature scenarios** (`features.py`) are opt-in through `--only` and are
not in the twelve. Each is one `plak run` (the image round trip is two) in a
fresh repo with a private `PLAK_HOME` whose root template lists exactly the
optional tools it needs (`tools = [...]`, spec 07 §3.4 and §3.5). Oracles read
`plak session log <id> --json`, the repository, the raw exchange capture and
the MCP servers' own records, never the model's claim alone. A bind or
admission refusal of a route row (`<tool> is not yet qualified on <route>`,
`is not available on`, `does not accept image input`) scores `refused:
route row unsupported`, distinct from a failure; smoke exits 0 on it.

```sh
just smoke claude-sonnet55-low --only image_input,fetch_url,mcp_stdio,mcp_http
just smoke codex --only mcp_github_stdio,mcp_github_http
just smoke api-anthropic --only web_search,web_fetch
just smoke grok --only x_search
just smoke claude --only profile_researcher,multi_roles,worker_spawns_researcher
PLAK_LIVE_VISION_ROUTE=claude-haiku55-low just smoke api-openai --only image_gen_roundtrip
```

| Scenario | Tools | Passes when |
| --- | --- | --- |
| `image_input` | none | `--image` of a cat photo; the answer names a cat, and a captured request carries an image block (Messages `image`, Responses `input_image` or Chat `image_url`) that decodes to an image; the report says whether it is the exact file bytes |
| `fetch_url` | `fetch_url` | the session log has a `fetch_url` call of the raw GitHub URL of `vendor/monty-typeshed/PLAK-PATCH.md` at a public commit, and the answer carries the one SHA-256 that file records |
| `mcp_stdio`, `mcp_http` | `mcp:everything` | the reference server `@modelcontextprotocol/server-everything` (pinned, installed once under `target/live/assets/`) over stdio or streamable HTTP on loopback, allowlisted to `get-sum` and `get-env`; the log has both calls, the answer has the sum of two random operands and a token that only the server's environment holds; stdio also needs the server's stdin to show `tools/call get-sum` with those arguments. On Codex and the OpenAI API the prompt asks for the calls inside a Code Mode cell, and a captured `exec` cell source must call `mcp__everything__get-sum` (exact name) or its JavaScript identifier `mcp__everything__get_sum`. An allowlisted tool Plak leaves out of the model's catalog is `harness` |
| `mcp_github_stdio`, `mcp_github_http` | `mcp:github` | the official GitHub MCP server (stdio: the digest-pinned `ghcr.io/github/github-mcp-server` image with `--read-only --tools get_file_contents,list_commits,search_repositories`; HTTP: `https://api.githubcopilot.com/mcp/x/repos/readonly`), Plak allowlisted to those read tools; Plak offers `mcp__github__get_file_contents` (else `harness`), the log has the call and the answer carries the same SHA-256 |
| `mcp_everything_all`, `mcp_github_all_stdio`, `mcp_github_all_http` | `mcp:everything`, `mcp:github` | the same oracles with no allowlist anywhere: every tool the reference server lists over stdio, the GitHub image's whole `--read-only` default tool set, and the remote `https://api.githubcopilot.com/mcp/readonly` endpoint; the session must open with the whole catalog |
| `web_search` | `web_search` | the log has the search call, its result lists sources, the answer names a Rust version and that exact string is on a cited page the driver fetches itself |
| `web_fetch` | `web_fetch` | the log has the fetch call and the answer carries the marker SHA-256 |
| `x_search` | `x_search` | the log has the call, its sources include X post URLs, and the answer names one of those posts |
| `image_gen_roundtrip` | `image_gen` | the log has an `image_gen` call, the workspace gains a PNG or JPEG, and a second run on `PLAK_LIVE_VISION_ROUTE` (default `claude-haiku55-low`) with that file as `--image` names a cat |
| `child_own_tools` | `fetch_url` on the worker only | a root with no optional tools spawns a worker whose template lists `fetch_url`: the root's requests never offer `fetch_url`, the worker's do, and the answer carries the marker SHA-256 the worker fetched |
| `profile_researcher` | the built-in profile's | a home laid out as applying the route's built-in profile writes (`<profile>-<role>` templates from `bin/plak/presets/`); the root spawns `<profile>-researcher`, whose requests offer `web_search` (the root's do not), its search result lists sources (Grok: including an X post from `x_search`), and the repo gains the `research/rust-release/report.md` its task asks for, citing URLs |
| `multi_roles` | `multi-default`'s | the `profile_researcher` oracle on `multi-default`, then a second run where the Claude root spawns the Codex worker (writes `hello.txt`) and the Claude reviewer; both roles' requests are captured on their own models |
| `worker_spawns_researcher` | the built-in profile's | the route's built-in profile (`multi-default` on other routes) with `[orchestration] max_depth = 2` and a user-authored `<profile>-worker` whose `spawns` names `<profile>-researcher` (the built-in worker is a leaf): the worker, not the root, spawns the researcher, with the `profile_researcher` oracle |

The marker commit is the newest first-parent ancestor of `HEAD` that
`raw.githubusercontent.com` serves (`PLAK_LIVE_PUBLIC_REV` pins one), so an
unpushed lane commit uses the public commit it is built on. The cat photo is
`Cat03.jpg` from Wikimedia Commons, downloaded once to
`target/live/assets/` (`PLAK_LIVE_CAT_IMAGE` names another file). The GitHub
scenarios need `gh` logged in and, for stdio, Docker: the driver pipes
`gh auth token` into `plak secret set github` inside the scenario's private
home only; the token is never printed or written anywhere else. The server is
read-only and allowlisted to read tools, so no write tool is ever offered.

**Long session** (`long_session.py`) is opt-in and is not one of the twelve, and
it is not the hour soak loop. `just smoke <route> --only <names>` and
`just soak <route> --long-session --only <names>` run a named subset.
`just soak <route> --long-session --all` runs exactly the four scenarios and
is what fills a `full` cell (spec 05 §3.4). `--all` and `--only` are mutually
exclusive. The direct script refuses a bare launch. Each scenario uses a
private home copied from the scratch home. Spec 07's only compaction key is `[compaction] enabled`
(pinned `true`). `[turn] max_model_rounds` is pinned to 128 so a growth turn
cannot run away and is not stopped before a handful of read pages. There is
no key that lowers a context window or a compaction trigger. Growth is pack-aware. Codex and the OpenAI API are native Code Mode and have
no `read_file`: those routes print `bulk.txt` with `exec_command`, at most
20 lines per call. Other packs use their read tool (`read_file`, `Read`, or
`read`) with the same 20-line limit on a sequential growth turn. `bulk.txt` is
random base64 lines, except on the Claude pack: Anthropic's classifier refused a
growth turn over base64 pages (`category=reasoning_extraction`), so Claude and
Anthropic API routes read random five-digit groups at 40 lines per call, which
count_tokens prices like 20 base64 lines (about 29k tokens). The product
caps the text one model round observes at `clamp(window * 4 * 8 / 100, 32 KiB,
256 KiB)` bytes (spec 01 observation policy), so one parallel batch cannot fill
a large window. The overflow turn therefore reads in rounds: each round is one
parallel batch of two reads that together fill three quarters of that cap, and
the rounds cover the compaction trigger at the pack's measured bytes per token
plus two more (for example 7 rounds of 2 x 20 lines on grok-4.7, 12 of 2 x 65
on the 1M Claude rows). Codex refused every batched wording, so on Codex the
same total is 36 sequential 21-line reads, one `exec_command` per Code Mode
exec (a multi-slice exec passes the round cap and is clipped). The child of
`child_compaction` reads its own `bulk.txt`: pages whose count passes its
trigger at the pack's bytes per token, each a header line naming a random
check word and the next page (or saying it is the last), then one read's worth
of bulk lines. The page count is not in the prompt. The child writes each
page's check word into `pages/<k>.txt`, so every read is a deliverable the
oracle counts. A Codex child page is two halves of 26 base64 lines read by one
exec through `page.py`, one half returned with `notify` and one with `text`:
one `exec_command` result stops at 40,000 bytes and two halves stay under the
round cap, so the 272k trigger is six pages (ten for 500k) instead of dozens of 30 KB reads
(told only an exact read count, a Codex child stopped after 3 to 9 of them). The default page
count follows the route's catalog window: 6 for the 256k–272k grok-4.7 and
Codex/OpenAI rows, 8 for 500k `grok-4.6` and `gpt-6.1-sol` (`codex-sol`), 16 for the 1M Claude rows, and 6
when this driver does not know the window. DeepSeek reports 1M, but Plak's
OpenRouter input admission uses the discovered window minus output and all
endpoint limits. Set `PLAK_LIVE_INPUT_CEILING` to the session's admitted input
ceiling (from its Context view) for OpenRouter long-session runs; the driver
sizes it with `pages_for_window`: ceil(window / 65,536) pages, at least 2 for
windows up to two pages and at least 6 otherwise, capped at 28. Per-call read
sizes also follow the admitted window, so a fold leaves room below the trigger. `PLAK_LIVE_GROWTH_PAGES`
(1..=28) overrides that. 28 pages are 7 MiB, under the 8 MiB capture cap.
The pre-compaction fact is a neutral code word (`ZEBRA-` and six digits) that lives only in `codeword.txt`; prompts call it a code word and ask for it from memory, because "token" wording drew Codex refusals ("I can't write the previously disclosed token into another file"). A pass does not accept
the model's own claim that it compacted or remembered. Reports name
`route_report` (class, account, effort, protocol, `composition_requested`, which
the private home pins as `plakous-default`, and `composition_bound`, the
compositions the run's `plak agent` hellos reported) plus `token_telemetry` from
the shared home and the scenario private home.

| Scenario | Surface | Passes when |
| --- | --- | --- |
| `in_turn_compaction` | `plak agent` | one turn pages `bulk.txt` until the session log shows `compact.started` reason `scheduled` and `compact.finished` outcome `committed` inside that turn's admitted..terminal bracket; prepared input shrinks when both figures are present; `recalled.txt` matches `codeword.txt`; the fact files are not read again after that commit; a follow-up writes `followup.txt` with the same code word |
| `overflow` | `plak agent` | rounds of parallel read batches, each under the round cap, then the log shows a remedy: an in-turn `scheduled` compact, a `context.placeholders` shrink inside that turn, or a `failed` / `ContextLimit` turn followed by `compact.started` reason `capacity` and a committed finish; `acknowledged.txt` is unchanged, and a later turn writes `continuation.txt` containing `CONTINUED` |
| `kill_reopen` | `plak agent` | SIGKILL after the marker process is observed running, or after 15s if it never appears (the marker is an argv token, not a shell comment); reopen emits JSONL `recovered` and the session log has `turn.abandoned` with `cause=process_lost` and `claim=running_tool`, `nothing_started` or `tool_round_completed` (spec 02 §2.4: Code Mode can abandon as `nothing_started` when exec was shown before the cell was durably prepared; a kill after a finalized tool round and before the next model request abandons as `tool_round_completed`; process restart does not log `session.recovered`); the follow-up writes `after_reopen.txt` containing `REOPENED` |
| `child_compaction` | `plak agent` | after the root quits, `plak session log <root>.cN --json` contains a committed compaction segment and the adopted checkout has `acknowledged.txt` and `recalled.txt` matching `codeword.txt` and every `pages/<k>.txt` holding page k's check word. `cN` comes from the root JSONL `tree` frames or spawn/wait receipts, otherwise from `plak session show <root>`. The segment projects as `kind` `compaction` with text beginning `compacted ` (the summary is not display text; an empty assistant item is not the segment; the harness `compact` row is not on this log). `pending` only when that show/log channel fails. A child tool failure on a `tool` event (a root call the model retried successfully, such as an adopt refused as `target_changed` and then made with the current target, does not count), a topology node, or a wait receipt (`agents[].blocked` with `account`, `quota`, `resets_at`) is that failure. `blocked.quota` of `rate_limited` or a quota exhaustion, or provider-unavailable fault text, is `provider` (spec 05 §3.2); any other child failure is `harness`. The projection has no prepared-input row, so a missing segment is `model`, as is a missing or wrong page check word |

A growth turn that finishes without that compaction is `harness` only when `model.operation_usage` shows prepared input at the lower of the compaction trigger and the catalog window minus one read page, and `model` otherwise. The trigger is spec 01 §10.3: 80% for `grok-4.6`, otherwise the window minus a tenth (about 90%) when that exceeds the semantic-summary headroom. A peak between that trigger and the window minus one page is harness, not a model miss. The same split applies to an overflow turn that completes without a remedy, so sequential reads that never reach the bar are not a harness failure. It does not apply to `child_compaction`: the child history projection has no `model.operation_usage` row, and a history without the compaction segment is `model`. A `ContextLimit` stop without a committed recovery is `harness`. Provider refusal is classified the same way as smoke.

After every fault the session must answer a plain follow-up: a failure that
leaves it needing `/clear` is a dead end, and a harness failure.

**Soak** (`soak.py`) is headless: it keeps one session working through a cycle of tasks on
more-itertools pinned at v10.5.0: explore, fix a planted bug, add a function with
tests, fix a second bug, document the function. Every task reopens the session in
a new process. Faults rotate across tasks, one task in four freezes the process
group with SIGSTOP for 20 s mid-tool, every third task is followed by a manual
compaction (a session still under 20,000 prepared tokens first reads `fold.txt`
and a one-line tail, so the fold is older than the protected latest intent),
and halfway through `plak` is rebuilt and the session reopens on the
new, byte-distinct binary. It reports throughput, tokens, peak context,
compactions, errors and dead ends. The exploration turn checks both the named
module and test file in the answer; coding turns check the repository test suite
and task-specific file/function outcomes. Reports made before this answer check
prove that exploration turns completed, not that their answers were correct.
Planting a bug resets only that function to the pinned commit's text and then
breaks it, so a later cycle plants again after an earlier fix that was correct
but worded differently, and functions earlier tasks added stay. A plant that
cannot run is that task's `harness` result; a driver exception still writes
`report.json`, with the traceback in `summary.crash` and every task that ran.

**TUI soak** (`tui_soak.py`) keeps one logical session on the real TUI for the
requested duration. It repeatedly adds a function and tests across two files,
checks the full suite plus a task-specific Python oracle, and waits for durable
`turn.lifecycle` terminals rather than treating a file write or screen paint as
completion. The first turn is suspended with SIGSTOP for 10 seconds and resumed;
the TUI is then cleanly quit and reopened on the same session, and a separate
file oracle verifies the follow-up. It uses the same route/model/effort defaults
as the other live drivers. If a provider turn fails, it tests whether the same
TUI session can complete the task on a follow-up prompt, recording the failed
turn separately. This is paid evidence and never runs in a gate.

**Jobs** (`jobs.py`) runs eleven bounded turns that exercise background-process
promotion. It is paid. The pre-release bar runs all eleven on every `full`
row (spec 05 §3.4); any other invocation is opt-in. `estimate.py` prices
that full-row cell. Each scenario gets a private home whose `[jobs] foreground_seconds`
is `1` (the default budget promotes a short sleep) or `45`
(a steer or Ctrl+B lands while the command is still foreground). The bound is
not a kill timer. The fixture is `hold.py`: it records its wall start, sleeps
the requested seconds, then writes `job-finished`. Each scenario appends eight
hex characters from `os.urandom` to its process token, so two routes running
at once never `ps` or `pkill` each other's fixture. The model's reply is never
the oracle.
`/stop` is not a slash command. Background processes stop from Activity
Processes with `S`, which asks `Stop all processes?`. The idle HUD reads
`1 agent` and `1 process`. `plak run` waits for a promoted job and continues
when it exits; it does not stop the job at the end of the first turn.

| Scenario | Surface | Passes when |
| --- | --- | --- |
| `promotion_budget` | `plak agent` | the fixture is promoted about 1 s after it starts, still running, with no immediate-background hint |
| `steer_command` | `plak agent` | a steer during a 45 s foreground command returns it within 8 s and leaves the process running. On Codex and OpenAI Code Mode the command is promoted by `yield_time_ms` 0 and the steer is asserted at the outer-cell boundary, not as a direct-tool preemption |
| `steer_await` | `plak agent` | a steer returns a wait on an already promoted job within 8 s and leaves that process running. Code Mode uses the same hint promotion and outer-cell steer |
| `notice_while_busy` | `plak agent` | a short job exits during a later command; the driver then steers, and the exchange that contains that steer also contains `<process_notice id="pN">`. The check parses the request JSON and reads message text, so a serialized `\"` still counts (spec 01 §8.5 delivers busy notices only at a steering boundary) |
| `run_promoted` | `plak run` | the prompt requests the pack's immediate background hint and ends without polling; stderr says `waiting for p` and then `continuing` with `exited`, and the fixture finishes |
| `crash_jobs` | `plak agent` | SIGKILL only after the root turn's exchange info is `terminal=Complete` or `ProtocolComplete` and `incomplete=false`, while the promoted job is still live; reopen reports `outcome_unknown`, and the next request contains `Plak stopped unexpectedly while this command was running.` |
| `child_settle` | `plak agent` | the child promotes with the pack's immediate hint and calls `submit_result` while that job is live. The first submit is refused `live_work` (spec 03 §4.6). The child then stops the job (or, on Codex, waits until it exits) and submits successfully, and the job is terminal when the attempt settles. A second worker promotes another job and does not submit; the driver `close_node`s it, and that job stops with `stop_reason=attempt_ended` (interrupt would keep the job). Model only when the child ignores those instructions. A provider refusal in the job output is provider |
| `ctrl_b` | TUI | Ctrl+B during a 45 s foreground command returns the tool within 8 s and leaves the process running. Code Mode asks for hint promotion and asserts Ctrl+B leaves that job running |
| `stop_jobs` | TUI | after the turn, Processes `S` shows `Stop all processes?`, the process is gone before `job-finished`, and the idle root admits another turn. A user stop is a process notice (`wake.rs` lines 156-158 keep every job that is not running); `[turn] auto_continue` defaults true |
| `wake_while_idle` | TUI | after the root terminal, wait until the latest HUD paint shows `1 process` (a paint from before the terminal counts when nothing repainted) and `ps` still shows the fixture (bounded); the job finishing then admits a second turn that is not recorded as `user_prompt` |
| `idle_badge` | TUI | the root's latest admitted turn has completed; the child started the hold with the pack's immediate background hint and is waiting on that job; a frame taken while that job is still live shows `1 agent` and `1 process` on the composer row. A cursor update that repaints one counter does not drop the other. A child that never promoted is model |

A missing telemetry row is not a failure: the writer drops a queued row on
exit. When a row is present it corroborates the hint, the notice delivery
point, or the wake origin. Reports land in `target/live/jobs-<route>-<stamp>/`.

### Refused-reopen triage

`crash_jobs` and long-session `kill_reopen` keep a refused reopen as `harness`.
The detail adds a read-only `plak session show <session>` decode against the
crashed scenario's private home and cwd, using the selected `--binary`:
status, phase and the ordered `(operation, action)` obligations. Request
identities are omitted. If supported, `plak session rehearse <session>` adds
its one-line offline root verdict; an unavailable rehearsal is silently skipped.
Neither command opens a runtime, calls a provider or repairs the session.

The diagnostic bucket is `covered` for a quiescent root (offline recovery
already closes that shape), `known-uncovered` for explicit Code Cell abandonment or
effect-decision/tool-batch/server-pause restore obligations, and `unknown`
otherwise. The show projection cannot distinguish hosted ToolCall finals,
Code Cell finals, non-Stop finals, steers, retried provider operations,
finalized in-turn compaction or a live process at the final. In particular,
`Finalization` alone is **not** evidence of a covered shape. Inspection
failure is unknown, not a changed score. Precise shape tables belong to the
offline crash-point sweep, not this live diagnostic.

The durable `turn.lifecycle` row stores `turn_id` and `phase` only
(`emit_turn_lifecycle` in `crates/hosting/hosted/src/runtime/turn.rs` lines
204-208), so a TUI scenario cannot read `model_error_kind` or `http_status`
from it. A failed TUI turn is `provider` when that row does carry them, or
when the turn's last `model_retries` row has `cause_kind` `http_status`,
`connect`, `record_rejected_in_transit`, `transport_after_send`, or
`stream_interrupted`, and either `next_attempt` equals `max_attempts` on a
dispatched row or the row was declined for `budget` (its proposed
`next_attempt` is `max_attempts + 1`). A failed turn
after a transient retry that did not exhaust that budget, and a failed turn
with neither a status nor such a row, is `harness`.

**Outcome classes.** `pass`; `harness`, Plak's failure (a crash, no terminal, a
dead end, a refused reopen); `model`, a completed turn whose work misses the
oracle, or invalid tool arguments proven by a finished capture (spec 05 §3.2);
model-invalid argument evidence must be in the failing process's highest-numbered
exchange, whose body shows the arguments finished (a fail-closed `ClosedEarly`
counts), never an earlier scenario, process or exchange;
`provider`, the real provider refusing with no injected fault; and
`pending`, a failure a named in-flight slice is expected to fix
(`EXPECTED_PENDING` in `smoke.py`).
For the headless soak, `dead_end` means the immediate follow-up in the same
process also failed; it does not by itself prove a later, fault-free reopen
cannot continue the durable session.

## Catalog

Route class is the planned depth, not a claim that the row has passed. The
pre-release bar, the platforms and the pass rule are spec 05 §3.4. On Apple
Silicon, `full` is the twelve smoke scenarios, the four long-session
scenarios and the eleven jobs scenarios, with `crash_jobs` run three times.
Only the least-used `full` row also runs the headless soak (default one hour).
There is no per-row two-hour TUI soak: the maintainer's TUI review replaces it.
`light` is the twelve smoke scenarios.
`informational` is that smoke, recorded: a model or provider outcome does not
block, and a harness outcome does. Linux `x86_64-unknown-linux-musl` runs the
twelve-scenario smoke on each `full` row and on `api-openai`,
`api-openai-medium`, `api-anthropic` and `api-xai` only. `or-mimo`, `or-glm`
and `or-qwen37` are best-effort light rows: a second provider outcome is
recorded and does not block. Ids below are
the `ROUTES` fields. Messages is planned only for
`deepseek/deepseek-v4.1-flash`; every other OpenRouter row stays on Chat, the
product default. `inclusionai/ling-3.0-flash-vl` and `qwen/qwen3.8-27b` are
paid ids at informational depth.

| Route | Account | Model | Effort | Class |
| --- | --- | --- | --- | --- |
| `codex` | `codex` | `gpt-6-luna` | low | full |
| `codex-luna-medium` | `codex` | `gpt-6-luna` | medium | light |
| `codex-sol` | `codex` | `gpt-6.1-sol` | low | light |
| `codex-astra` | `codex` | `gpt-6-astra` | low | light |
| `grok` | `grok` | `grok-4.7` | low | full |
| `grok-46` | `grok` | `grok-4.6` | low | light |
| `claude-sonnet55-low` | `claude` | `claude-sonnet-5-5` | low | full |
| `claude-sonnet55-medium` | `claude` | `claude-sonnet-5-5` | medium | light |
| `claude-haiku55-low` | `claude` | `claude-haiku-5-5` | low | light |
| `claude-haiku55-medium` | `claude` | `claude-haiku-5-5` | medium | light |
| `claude-opus-low` | `claude` | `claude-opus-5-5` | low | light |
| `claude-opus-medium` | `claude` | `claude-opus-5-5` | medium | light |
| `claude-fable` | `claude` | `claude-fable-5-1` | low | light |
| `deepseek` | `openrouter` | `deepseek/deepseek-v4.1-flash` | | full, Chat |
| `deepseek-messages` | `openrouter` | `deepseek/deepseek-v4.1-flash` | | full, Messages |
| `or-mimo` | `openrouter` | `xiaomi/mimo-v2.6-flash` | | light |
| `or-glm` | `openrouter` | `z-ai/glm-5.3-flash` | | light |
| `or-qwen37` | `openrouter` | `qwen/qwen3.7-flash` | | light |
| `or-nemotron-free` | `openrouter` | `nvidia/nemotron-3-ultra-550b-a55b:free` | | informational |
| `or-laguna-free` | `openrouter` | `poolside/laguna-s-2.1:free` | | informational |
| `or-lfm-free` | `openrouter` | `liquid/lfm-2.5-2.6b:free` | | informational |
| `or-ling` | `openrouter` | `inclusionai/ling-3.0-flash-vl` | | informational |
| `or-qwen38` | `openrouter` | `qwen/qwen3.8-27b` | | informational |
| `api-openai` | `openai` | `gpt-6-luna` | low | light |
| `api-openai-medium` | `openai` | `gpt-6-luna` | medium | light |
| `api-anthropic` | `anthropic` | `claude-haiku-5-5` | medium | light |
| `api-xai` | `xai-api` | `grok-4.7` | low | light |

`claude` and `claude-sonnet-medium` (`claude-sonnet-5` low and medium) still
run, but they are not in this matrix. Nor are two feature-scenario roster
rows: `api-anthropic-low` (`anthropic`, `claude-haiku-5-5`, low) and
`or-gemma4-free` (`openrouter`, `google/gemma-4-31b-it:free`). The four direct API rows are light and
not best-effort: `api-openai` is `gpt-6-luna` low, `api-openai-medium` is
`gpt-6-luna` medium, `api-anthropic` is `claude-haiku-5-5` medium, and
`api-xai` is `grok-4.7` low.

The pre-release bar runs once per release candidate, not per install. It
binds the same clean source revision, binary/sidecar hashes and platform
tuple across the supported-platform integrated gates (including the full
offline crash sweep), live matrix and the maintainer's TUI sign-off. Spec 05 §3.4 owns the bar and later revision's
per-row rerun rule.

On Apple Silicon the full cell of a `full` row is:

```text
just smoke <route>
just soak <route> --long-session --all
just jobs <route>
just jobs <route> --only crash_jobs    # twice more: three crash_jobs runs
just soak <route> --hours 1            # least-used full row only
```

Linux musl's cell is `just smoke <route>` on each `full` row and on the four
API rows. A refused crash reopen is still a blocking `harness` outcome;
the diagnostic bucket above does not waive it.

### Qualification attempts

The commands above are single-attempt drivers. For qualification, use the
cell driver to apply spec 05 §3.4's bounded rerun policy automatically:

```text
python3 tests/live/qualification.py smoke <route>
python3 tests/live/qualification.py long-session <route> --all
python3 tests/live/qualification.py jobs <route>
python3 tests/live/qualification.py jobs <route> --only crash_jobs
python3 tests/live/qualification.py soak <route> --hours 1
python3 tests/live/qualification.py tui-soak <route> --hours 0.5
python3 tests/live/qualification.py smoke <route> --dry-run
```

A first `MODEL` may rerun once; this driver takes that allowance. The whole
selected cell starts fresh, in a new artifact directory and home with a new
session, using the same route, build and prompt. Long-session and jobs randomized
fixtures replay the same seed. Its rerun is final: `PASS` qualifies, and
anything else counts. `HARNESS` never reruns. A first `PROVIDER` still reruns
once under its existing rule; a MODEL on that rerun cannot earn another
attempt, nor can a PROVIDER on a MODEL rerun. At most two attempts run.
Informational outcomes and the second-PROVIDER best-effort waiver retain
their existing class rules; a PROVIDER on a MODEL rerun is not that waiver.

The cell's `report.json` embeds both original reports and their artifact and
capture references, in order, including the first MODEL's capture-proven
reason. The printed summary shows `PASS after MODEL rerun` separately from
clean `PASS`. Raw lane reports are never overwritten. Missing capture
evidence, driver errors or a changed build are HARNESS, not retry candidates.
The soak's intentional byte-distinct rebuild remains part of its oracle.
The driver retains its original starting binary and restores only that
recorded intentional rebuild before a rerun; any unexpected replacement is
HARNESS. Both transitions are recorded. `--dry-run` performs no inference and prints the
command and maximum attempt count.

## Daily-driver evidence

```sh
just daily-driver --since 2026-10-09
python3 tests/live/daily_driver.py --since 2026-10-09
```

This is read-only over `~/.plak/logs/plak.log`: no build, provider call, TUI
launch or database mutation. `just daily-driver --since <date>` selects the
window.
`--since YYYY-MM-DD` is inclusive; earlier
reload/version markers still establish the first included build. `--log`
selects a synthetic or alternate log. The markdown tables show per installed
build: active days, root routes, distinct root/child session bindings by
provider/model, admitted turns when logged, WARN/ERROR counts by target
(capture noise separate), retries by stable cause, reloads/carries, and
observed recovered/abandoned turns. No prompts, paths, session IDs, request
details, credentials or environment values are printed.

Build attribution prefers `plak.start` startup binary IDs, then uses
`plak.reload: exec installed plak` maintainer hashes and logged `plak version`
binary IDs. A snapshot directory hash identifies all three executables, not
the Plak binary. If that snapshot still exists under
`~/.local/lib/plak/maintainer`, its `plak` is hashed read-only for the 12-hex
binary ID; missing snapshots stay unknown. Pre-boundary use stays unknown;
concurrent older processes cannot be separated without logged build IDs.
Only registered provider/model names are printed; other bindings become
unknown. Active days exclude capture-only noise. Repeated binding spans are
deduplicated by session ID, and turns are `n/a` when the log has no admitted
lifecycle records. The report does not read unattributable telemetry rows:
missing turn/recovery/carry records are evidence gaps, not proof of zero.
Provider/model bindings do not establish effort, protocol or composition;
in particular DeepSeek Chat and Messages cannot be distinguished here.

The bar's third part is the maintainer's sign-off after using the candidate's
maintainer build in the TUI on their own work. This report is a diagnostic for
that review and for choosing the least-used `full` row for the one-hour
headless soak; it is never a substitute for a missing live cell.

## Token telemetry

Smoke, TUI soak and the job driver read `turn.usage` from the session log (input, output,
cached input, reasoning). Each smoke scenario gets the delta of that log
across the scenario, including a failed one, and the report's
`token_telemetry` is the sum. Each TUI soak task, recovery and reopen
follow-up gets the delta across that wait; the report total is the whole
session log, which can include a turn that never became a task row. Headless
soak keeps summing the JSONL `turn` usage event per task, and now also keeps
cached input and reasoning from that event. A missing field is zero. Reports
also name `route_class`, account, effort, protocol, `composition_requested`
(`plakous-default` unless the scenario passed another) and, for smoke, soak,
jobs and long-session runs, `composition_bound`: the sorted compositions the
run's `plak agent` hellos reported (Codex binds `plakous-default` as
`native-ptc`). The TUI soak has no JSONL hello and names only the request.

## Dry-run estimate

`estimate.py` prices the planned matrix from a rate-card JSON and prints each
cell, the clean-attempt subtotal, an explicit one-extra-attempt rerun reserve,
the OpenRouter subtotal, a contingency and the total. Both MODEL and PROVIDER
share this one-attempt reserve; they do not chain. Contingency and the ceiling
include the reserve. It does not build `plak`, talk to a provider, or read a default card:
pass `--rate-card`. The card's `unit` must be `USD per million tokens`. Per
model it needs `input`, `output`, `cache_read` and `cache_write`. Input is an
uncached share plus a cached share; cached tokens are billed at `cache_read`
instead of `input`. Reasoning is not priced again. An API-key model the card
omits is `unpriced` and stays out of the total. A subscription row is `quota`
even when the card names its model. `--openrouter-ceiling N` warns on stderr
and in the report when the OpenRouter cells, including contingency, exceed it.
The warning does not drop cells and the exit status stays 0.

```sh
python3 tests/live/estimate.py --rate-card rates.json --soak-row deepseek
python3 tests/live/estimate.py --rate-card rates.json --soak-row deepseek --contingency 0.25 --openrouter-ceiling 40
just estimate --rate-card rates.json --soak-row deepseek --tui-rows codex,grok
```

Defaults are measured volumes where a trial recorded them, all overridable.
Smoke (12 scenarios): 241,039 input and 5,500 output, cached share 0.64.
The compaction scenario's 20,000-token fold accounts for 40,000 of that input.
Headless soak (1 hour): 5,600,000 input and 60,000 output, cached share 0.9.
TUI soak (2 hours): 8,600,000 input, output scaled by the soak output/input
ratio, cached share 0.9. Long-session, on each `full` row: the measured
DeepSeek four-scenario cell, 750,415 input and 4,673 output at 22% cached,
scaled by catalog window / 1,048,576 for every other window. Jobs (11
scenarios) uses the smoke per-scenario share and the smoke cache share.
Two further `crash_jobs` runs are priced on every `full` row at two elevenths
of the jobs volume, so that scenario runs three times including the jobs cell.
`--soak-row <route>` is required: it must name the least-used planned `full`
row and prices ONE Mac headless soak (one hour by default). No TUI soak is
priced unless `--tui-rows a,b` names distinct planned `full` rows; each is
0.5 hours by default. Linux prices a smoke only, on the
`full` rows and `api-openai`, `api-openai-medium`, `api-anthropic` and
`api-xai`. Contingency is
0.20. Soak and TUI totals scale with `--soak-hours` and `--tui-soak-hours`.

`tests/tooling/test_live_oracles.py` covers the offline pieces (fault plans,
session-log parsing, the table) in the ordinary tooling tests. Catalog,
telemetry and the estimator are covered beside it.

## API model census

`just api-census` (or `python3 scripts/api_model_census.py`) diffs the checked-in
OpenAI, Anthropic and xAI tables against each provider's model list and docs.
It is opt-in and networked, and it is not a gate. The key comes from the
environment or `~/.plak/secrets/<account>.key` and is sent only as a header.
`--write` refreshes factual fields. A new id is reported needs-review and is
not marked usable; `--include-new` records one the family rules would not
admit, and `--promote <id>` is required before an admitting id is added.
`--probe-efforts` cross-checks OpenAI ladders, at most `--probe-limit` models.
It sends schema-valid efforts that are often unsupported (`minimal`, then
`none`, `max`, `xhigh`), with input `x` and `max_output_tokens` 16. A 400 that
names the model is that model's ladder and costs nothing; a 200 means that
value is supported. The doc page stays the source of the table. `--models-dev` is a
report-only cross-check. Offline: `--fixture tests/tooling/fixtures/api_model_census`.

## MCP OAuth (No Accounts)

`just mcp-oauth-e2e` runs the opt-in [MCP OAuth lane](mcp_oauth/README.md):
Keycloak, strict node-oidc-provider resource indicators, an official-SDK MCP
server and Playwright login. It uses a private disposable home and Plak's
scripted model, never a provider account or paid inference. It is separate
from smoke/soak and every gate tier. Run on a native Linux Docker host, or on
Docker Desktop with host networking enabled. Only its parser self-tests enter tooling.
