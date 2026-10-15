# The TUI

## Starting

```
plak            # new session in the current directory
plak resume     # picker over this directory's sessions
plak resume <uuid>
```

`plak` needs an interactive terminal; in a pipe it refuses rather than degrading.
Use a release build (`just install`; see [getting started](getting-started.md#build)) — the debug
binary is large and visibly slow to start. An empty session shows a six-row pixel PLAK wordmark, with a double-line
outline, at the top of the transcript (version, build, a short session id, and
`powered by plakous` on the right) with the composer underneath. Conversation
starts on the next row below the banner. Once the transcript fills the pane,
the banner scrolls off with the rest of the session; scrolling back to the top
brings it back. There is no setting for it: it is drawn whenever the pane is
big enough to hold it.

An opening note stays below the history restored at open and above every later
turn; refreshing the transcript does not move it to the bottom or duplicate it.
If the bounded transcript loses its pre-open anchor, the launch-only note is
dropped rather than moved into later turns.

## The composer

The composer is the bordered box at the bottom. Its top-left corner carries the
context meter (`15%/272k`, or `awaiting context` until a window is reported;
`0%/272k` once the window is known but occupancy is not)
and a short activity HUD. The profile id sits at the top right; hover it for
role models, efforts and flags from this session's frozen roster. `custom`
means no saved profile matches both this root and those cards; it does not
replace the session's roles with the current configuration's roles.
The plain bottom border carries the workspace, `~/path · branch`, at its
right; a narrow terminal shortens the path to its tail first and shows the
branch longest. The branch is refreshed after every turn. The hint line below
the border has a bare turn timer at its right while busy; while idle it
shows `last user prompt · 25m ago` once you have sent a prompt or steer in
this session; after `/reload` or resume it is read back from the session's
stored history (empty only when none of your prompts is left in it). A live `N agents`
or `N processes` count highlights on hover and opens the Agents list or
Processes, even while the root is idle; only the count text is the button.

Every list follows one mouse rule: a click selects a row, a double-click does
what `Enter` does. That holds for the slash menu, Settings and its choosers
(double-click a theme to apply it), Agents profiles, the resume picker,
Activity's Agents and Processes, and the queue panel (where a double-click
sends the prompt now, as `Ctrl+S` and `Enter` on the focused queue do). A Settings
switch or a resume binding column still acts on one click.

Drag a selection above or below the composer's visible rows to scroll the
draft one visual row per tick and keep selecting. Moving back inside or
releasing the button stops scrolling. Copy includes the selected text that
has scrolled out of view; a plain click only moves the caret.
Opening Menu, Activity or another overlay, or switching panes, also stops the
drag without losing the selected text.

While the root is idle and work is waiting, that HUD shows a steady `◆` and
what is pending, joined by ` · `: unread child results and questions, process
exits the next turn has not seen, `auto-continue paused`, and
`agents paused · /tree resume`. A running turn hides the line. Esc pauses
auto-continue until the next prompt, and so does a wake that did not fit the
window; when auto-continue is on and a child or
background process could still wake the root, the HUD says so and the
transcript keeps one `auto-continue paused until the next prompt` line. A
paused tree leaves one `agents paused · /tree resume` line. Both lines come
back the same after a restart or `/resume`, without a second copy.

- `Enter` sends while idle and **queues a follow-up** while busy.
- `Shift+Enter` / `Alt+Enter` inserts a newline.
- `Ctrl+Enter` / `Ctrl+S` steers while busy, sends now while idle.
- `Ctrl+B` moves currently running foreground commands to the background; otherwise it does nothing. Later commands are unaffected.
- `↑` on an empty draft recalls the previous prompt.
- `Esc` while idle on an empty draft cancels a pending automatic continuation
  and pauses auto-continue; with nothing pending it changes nothing visible.
  With a draft, a single Esc keeps it and `Esc Esc` clears it (`Ctrl+Z`
  undoes it); neither touches a pending continuation.
- `/` opens commands and `?` opens Keybinds only when the composer is empty
  and you have not typed in the last 300 ms or so (a `?` typed into a draft
  you just emptied stays text).
- `Esc` while busy opens the stop pop-up: `[ Cancel ]`, `[ Stop turn ]` and,
  with background processes running, `[ Stop turn + processes ]` (see
  [the confirmation pop-up](#activity)). `Ctrl+C` on an empty draft while busy
  opens the same pop-up.

Queued follow-ups appear above the composer with hover-highlighted `[now]` and
`[drop]` buttons. A sent steer stays there as `sent · waiting for the current
reply` until the model receives it; it cannot be sent or dropped again.
The composer border carries no queue count; this panel is where the queue
shows. With only steers, the panel has no queue-focus or send-now controls.
A follow-up you type while a turn runs is sent when the turn ends. Only prompts
that were already waiting when a session was restored, or when a turn failed or
compaction was refused, stay paused until you press `Ctrl+R` on the focused
queue.
Definite admission refusals keep the item labeled `rejected:` without restoring
a composer copy; storage-wide refusals stop that live drain pass.
Click a chip, or `Tab` to focus the queue: there only `↑`/`↓` (select) and
`Shift+↑`/`Shift+↓` (reorder) act. Every other key, letters, `Backspace`,
`Delete` and `←`/`→` included, leaves the queue and goes to your draft,
however slowly you type, so typing never drops, edits or sends a queued
prompt. `Enter` sends the selected one now, the same as `Ctrl+S`, including
while the drain is paused, and puts focus back on the composer without
touching your draft. A second or held `Enter` then reaches the composer:
with an empty draft it sends nothing within the two-second gap; with text
in the draft it sends that draft. When delivery is unavailable, `Enter`
only leaves the queue. The queue's commands
are chords on the focused queue: `Ctrl+D` drops the selected one, `Ctrl+E`
loads it into your draft to edit (the queue title then reads `editing queued
follow-up · Enter saves · Esc cancels`; when you save or cancel, the draft it
replaced comes back, and `Ctrl+Z` undoes that),
`Ctrl+R` resumes a paused queue, and `Ctrl+S` (or `Ctrl+Enter`) sends the
selected one now, even with text in your draft. With the queue unfocused, `Ctrl+D` deletes the character
after the caret, as in readline. A chip fires only after it has sat in place
for about 300 ms, so a click aimed at a row the queue just moved does nothing.
While busy, `Enter` on an empty draft sends the oldest queued follow-up now
only after two seconds without typing or `Enter`, so a doubled `Enter`, even a
beat apart, sends nothing more.

A busy root keeps refreshing its queue while tools wait. If send-now is waiting
for an earlier command's durable acknowledgement to appear, the TUI says
`queue projection is catching up; prompt remains queued` once, rather than
repeatedly claiming the queue changed. Your follow-ups remain queued. A direct
steer from a non-empty, non-editing composer does not wait for that projection:
`wait_agent` wakes with `steer`, and the text reaches the model after the current
tool batch closes. Streaming provider responses are not interrupted by a steer.

When a tool needs your approval, its row reads `awaiting decision · [allow]
Alt+A · [deny] Alt+D`: click `[allow]` or `[deny]` (once the chip has been on
screen for about 300 ms; a new approval's chips start fresh), or press `Alt+A` / `Alt+D` from the composer, which
asks in the confirmation pop-up naming the tool and its subject. Many
terminals send a typed `Esc` and the next letter as one `Alt` chord, so the
chord never resolves on its own. Over the composer, typing never acts
destructively: a printable key closes that pop-up, and any other pop-up,
into your draft. In Activity, the agent view, process output and Menu a
mapped key acts at once, and `Enter` presses the pop-up's focused button.
Without an open approval, `Alt+D` deletes the word after
the caret. A bare `a` or `d` is always typed. On macOS, `Alt` is the
Option key, and the terminal must send it as Meta: Terminal.app → Settings →
Profiles → Keyboard → "Use Option as Meta key"; iTerm2 → Profiles → Keys →
Left Option key: Esc+; Ghostty `macos-option-as-alt = true`; kitty
`macos_option_as_alt yes`. Otherwise use the chips.
If an old queued prompt already ran through another face and its result is no
longer retained, Plak consumes it without rerunning it and says
`queued prompt already ran; its result is no longer kept`.

### The hint line and `/reload`

The line under the composer border reads `/ commands · Ctrl+X menu · ?
keybinds`, plus a red `Esc stop` while a turn runs. When a newer `plak` is
installed on `PATH` it says `new build installed · /reload`. `/reload` restarts
on that build and resumes this session; it is refused, with a transcript note,
while:

- a turn runs: `reload refused: wait for the turn to finish or cancel it`;
- background processes run: `reload refused: background processes are
  running; stop them first (/ps)`;
- sub-agents run: `reload refused: sub-agents are still running`;
- no `plak` is installed on `PATH`: `reload refused: no installed plak found
  on PATH`.

A refusal never closes the TUI. When the installed build cannot read this
build's state, `/reload` carries the session forward into a linked session
built from a bounded checkpoint; the old archive stays available only by
explicit reference.

## Slash commands

Type `/` to open the palette; it fuzzy-matches as you type. `Tab` completes
and `Enter` runs the highlighted command only when what you typed is the start
of its name (`/he` runs `/help`); otherwise `Tab` does nothing and `Enter`
submits the line as typed, so `/tmp` never runs `/fork-then-compact`. The
commands that act for good, `/clear`, `/compact`, `/fork-then-compact`,
`/fork`, `/drop` and `/recover`, run from `Enter` only when you typed the
full name: `Enter` on a shorter word (`/co`) completes it, as `Tab` does, and
runs nothing, and an `Enter` within about 600 ms of that completion runs
nothing either; look, then press `Enter` again. There are 22 commands:

| Command | Effect |
| --- | --- |
| `/help` | Show all commands |
| `/keybinds` | View Plak key bindings |
| `/status` | View session status |
| `/settings` | Go to interactive settings menu |
| `/agents` | Choose an agent profile |
| `/providers` | View connected providers |
| `/usage` | View usage data for connected providers |
| `/context` | View context window data |
| `/attach` | Reconnect the agent |
| `/recover` | Recover an interrupted turn |
| `/clear` | Start a new conversation |
| `/compact` | Compact this conversation |
| `/config` | Open `config.toml` in your editor |
| `/resume` | Resume a previous session |
| `/tree` | Open Activity: Agents |
| `/ps` | Open Activity: Processes |
| `/stop-all` | Stop all non-root agents |
| `/fork` | Fork this conversation |
| `/fork-then-compact` | Fork, then compact the new session |
| `/drop` | Drop the last queued message |
| `/reload` | Restart on the installed Plak and resume this session |
| `/quit` | Exit Plak |

After a restart, a `/clear` request that never started leaves one notice:
`clear did not start; run /clear again`. It is not automatically retried.

Notes worth knowing:

- `/recover` reopens the session, records once what the interrupted turn left
  unobserved, and continues; the next prompt does this on its own.
- `/reload`: see [the hint line and `/reload`](#the-hint-line-and-reload).
- `/resume` waits briefly if this host is still releasing that session's old
  attachment. If it says the previous switch is still finishing, retry shortly;
  the resumed history is rebuilt from the committed runtime feed.
- `/providers` is read-only. Per vertical it names the account, its status, and
  the CLI command that connects it. Logging in and setting keys are CLI-only:
  `plak login`, `plak logout`, `plak key set|clear`.
- Templates, the roster and orchestration are config-only. `/config` opens
  `config.toml`; there is no roster editor.
- Skills, MCP and workflows are not implemented and have no
  command.
- There is no `/model`, `/account` or `/effort`. The session's account, model,
  effort, Fast mode, composition and protocol change only through profiles:
  the Agents tab's **Fork now**, **Use now** on a fresh session with no
  conversation, or **Next session**. On a fresh home, pick a profile and
  **Use now**; the agent attaches in the same session. Both accept a profile
  whose root is an OpenRouter or direct API account (`openai`, `anthropic`,
  `xai-api`).
- `/agents <profile>` asks whether to use that profile now or save it for the
  next time you start `plak`. The name can be a profile id or one of the short
  names the list shows (`codex`, `claude`, `grok`, ...).
- `/fork` keeps the current binding and the conversation, and recompiles this session's current configuration: your `AGENTS.md` files as they are now, harness settings and tool presentation. If an `AGENTS.md` changed since this session started, the transcript names it. If this session's tools changed (for example a tool added or removed in configuration), `/fork` says `cannot fork: this session's tools changed; start a new session`. `/fork-then-compact` does that and then compacts only the new session.
  After a crash, an unstarted fork is withdrawn with `fork did not start; run /fork again`. An already-created fork stays available as its own session; reopening the source does not switch to it or rerun `/fork`.
- Two aliases are accepted but are not extra commands: `/stop_all` for
  `/stop-all`, and `/exit` for `/quit`.
- If a restart finds a compaction request that never started, it does not
  retry it automatically. It shows `compaction did not start; run /compact
  again` once. An already-started compaction is settled without another paid
  dispatch.
- An automatic fold that cannot shrink the conversation pays nothing. If
  the prompt fits, it is sent without the fold; the no-op request does not
  block later prompts or `/compact`.
- A compaction refused before it starts leaves no pending request after the
  session view refresh. If its reply is lost or its outcome is unknown, Plak
  keeps the request pending until runtime facts or reopening settle it; it
  does not assume that no paid work occurred or retry automatically.
- Only the slash spelling quits. Typing `quit`, `exit` or `:q` sends it as a
  prompt.
- A command that takes no arguments refuses extra text instead of ignoring
  it: `/compact now` or `/clear all` prints
  `usage: /compact  —  takes no arguments` and does nothing.
- Closing the terminal window (`SIGHUP`) or stopping `plak` with `SIGTERM`
  quits like `Ctrl+Q`: the terminal is restored, the resume hint is printed,
  and the session can be resumed. The exit status is 4.

## Key bindings

`Ctrl+X` opens Menu on the last-used tab (initially Session), unless a composer
selection is active, in which case it cuts. `?` on an empty draft opens the
short **While typing** Keybinds page; `Enter` opens **All keybinds** and `Esc`
returns. `/help` also prints the full sheet. The pages are generated from the
same registry the input layer uses. Bindings are grouped by context: global, composer, transcript and
queue, agent transcript, Menu, Agents list, Activity processes, resume picker,
help overlay, confirmation. On macOS the Command key (Super elsewhere) copies,
cuts, selects all, undoes and redoes, like the matching `Ctrl` chords; the
sheet lists these as `Cmd/Super+…`.

The global ones:

| Chord | Effect |
| --- | --- |
| `Esc` | Close the focused owner; confirm stopping the turn while busy |
| `Esc Esc` | Clear the draft while idle (undo with `Ctrl+Z`) |
| `Ctrl+C` | Copy a selection; otherwise clear the draft, ask to stop the turn in the pop-up, quit a stuck cancel, or quit while idle — second press within 1000 ms |
| `Ctrl+Q` | Quit; asks first while a turn, an agent or a background process runs |
| `Ctrl+X` | Cut the composer selection; with no selection, reopen Menu |
| `Ctrl+T` | Open Activity on the last tab, or close it when Activity is open |
| `Ctrl+O` | Open or close the focused row: a group, a call, a code cell, a thought or an agent row |
| `Ctrl+D` | Drop the selected queued follow-up when the queue is focused; otherwise delete the character after the caret |
| `Ctrl+B` | Background in-flight foreground commands; no-op otherwise |
| `Ctrl+Shift+O` / `Alt+O` | Fold everything that is open; with nothing open, open every group, code cell and thought |
| `/` | Open the slash palette with an empty draft |
| `?` | Open Keybinds with an empty draft after a pause in typing; otherwise type `?` |

A few chords need the kitty keyboard protocol (`Ctrl+Shift+A`,
`Ctrl+Shift+O`). `Alt+O` is the fallback for terminals without it.

In the transcript, the wheel scrolls; while a reply is streaming, a small
down-scroll keeps its place and moves by the configured notch rather than
fighting the growing tail.

Tool calls form groups. While calls run, a group shows one summary line for
the calls that already finished and, under it, one line per call still
running with a spinner and its elapsed time; parallel calls each get a line.
A call that finishes folds into the summary straight away. When no call is
running the group is one line, such as
`✓ Read 3 files, ran 4 commands   1m02s ▸`, where the time runs from the
first call's start to the last call's finish; a group of one call shows that
call's own line. Summaries name what happened (`read N files`, `ran N
searches`, `edited N files (+a −b)`, `wrote N files`, `ran N commands`,
`CodeMode · N calls` (`CodeMode ×2 · 7 calls`, omitting the tally when zero), `checked N processes`, `stopped N processes`, `messaged N agents`,
`updated plan`, or a tool's own name) and never say "actions". Failed and
interrupted calls stay visible as `✗ … (1 failed)` with their exit codes at
the right. Your prompts, assistant text, notices and errors end a
group; retries and process exits stay inside it. In every model family, sealed
thoughts between calls or immediately before the first call fold into that
group: the summary counts them (`Read 2 files, 1 thought`) and opening the
group shows them in order on its rail. A one-call or one-cell group keeps
the item or outline, with its non-empty thoughts visible as separate rows.
Before a summary exists, a leading thought stays above the live tip. A thought leading into the reply stays
visible. A streaming thought stays at the tip until sealed and followed by a
call. Empty sealed thoughts render no row and never split a group; inside a
group they only count. Hidden thinking ends nothing. A process exit names its command, redacted and cut to one line,
with the outcome and duration kept at the right: `p261 just check`
then `exited 0 · 21m40s`. A long command is what a narrow row cuts. A session
carried from an older build, which stored no command, shows
`job p261 exited 0 · 21m40s`.

Every call is exactly one line: status glyph, verb and subject, then the exit
code and duration at the right edge and `▸` (past a minute the duration reads
`21m40s` or `1h05m`). Long subjects end in `…`,
embedded newlines appear as `↵`, and on a narrow terminal the exit code and
duration drop before the subject gets short. Commands are coloured: `$` muted,
the program in the accent colour, quoted strings in the string colour.
Process waits say `Waiting on process <id> · <command>` and finish as
`Waited on process <id> · <command> · exited <code>` or `· still running`.
Wait outcomes describe that wait's result, even if the process exits just after
it times out; termination says `· stopped`. Non-blocking reads say
`Read output of process`. Input says `Sent input to process <id> · <N> bytes`,
without revealing the text. Sending an interrupt byte says `Sent interrupt to
process`, not `Stopped process`; only explicit stops use the latter label.
Commands come from the same session-and-branch-scoped registry as Processes,
with the start row's command as a fallback; when unknown, only the id is shown.
Resolved approvals and interruption markers (`⊘ interrupted`) remain
visible when folded. A steer is your prompt band with a small `steer` tag, kept
on its first line however long the steer is, the same after resume. Prompts
and steers show when you sent them, dimmed at the right of their first line:
`14:32` today, `Oct 7 14:32` on an earlier day, in local time, at the offset
that moment had (a daylight-saving change does not move an older time). A queued prompt
shows when you queued it. On a narrow terminal the time is dropped first;
prompts carried from an older build have none. Unread child-result rows also survive resume;
topology-control calls never invent finished-agent counts, and adoption calls
name their candidate. An unavailable agent navigator belongs to the Agents
view, not the transcript.

Click a summary or press `Enter` / `Ctrl+O` to open it (`▾`) to every call;
inside, consecutive reads and searches merge into `Explored N files, M
searches` and agent spawns into `N agents spawned`, which open to list each
call. Every call opens on its own: click its line, or focus it and press
`Enter` / `Ctrl+O`. An open call shows its input first — the full command
(coloured, over several lines when it has them), an edit as a coloured diff,
the first lines of a write, other tools as `key: value` arguments, never raw
JSON — then, after a `┄` row, its output as the model saw it, in the muted
output colour. Long output shows its first 20 and last 10 lines around a
`… N lines hidden ▸` row; click it to show the rest. Every detail line keeps
the rail, and wrapped lines hang under it. The full text comes from the
session's history when you open the call; until it arrives, and for calls no
longer in history (carried, compacted or pruned), the line's saved preview is
shown and labelled `preview`. A side cut at the size limit says `… input
truncated` or `… output truncated`. Hover highlights the line a click would
toggle; the row you last opened or closed keeps a faint highlight. Copying a
selection that covers a whole line copies its full text (a cut command with its
real line breaks), while a selection inside a line copies just what it covers.
A prompt or steer line copies without its send time or the `steer` tag.
Copying an assistant reply drops the two-space indent, and drops a quote rail
or a code rail, so a fenced block of commands pastes as the commands. A code
line cut off at the pane edge still copies the whole line when the selection
runs through that row; a selection that starts inside the line copies only the
cells it covers. The language on the fence (`bash`, `rust`) is left out when
the selection covers that label row. List markers and table borders stay. An
agent's transcript copies the same way.
The scrollbar shows where you are; there is no percentage badge. An opened call that answered `preview` or was
refused while the session was busy asks again when you close and reopen it.

Code mode cells (`exec`/`wait`, `exec_js`/`wait_js`, `exec_py`/`wait_py`)
show a header (`✓ CodeMode · 3 calls   4.2s ▸`) and their calls as one-line
tree rows one level deeper. A zero call tally is omitted from the header.
Nested cells keep recursive call order at the same cell rail indentation.
The outline keeps at most 10 calls, prioritizing
running calls, then failures and interruptions, then the rest; `… N more`
counts the hidden calls and opens the cell on click. Nested failures make the
finished header `✗` and carry their exit codes; a running cell keeps its spinner. Running cells grow their outline at the
tip, then fold into the group; a one-cell group keeps its outline.
Opening a cell keeps the outline, then shows highlighted JavaScript/Python
source and a separate `output ▸` row. Toggle that row to read the cell's
output. Each call opens its own detail. Opening a group does not open its
cells, and opening a cell does not open its calls or output. Whole-row copies
keep the full header or call text; source copies like a fenced code block.
Thinking shows `Thinking…` while streaming, then `◆ ` and the first non-empty
body line, with markdown markers removed and whitespace collapsed. The summary
preserves angle-bracket text, including Rust generics and inline HTML;
empty, block-HTML-only or comment-only lines give way to the next text line. The elapsed
time sits at the right. Long summaries cut with `…`; copying a whole row keeps
the full summary. Multi-line thoughts have `▸` to open the full body under a
four-space indent; a one-line thought has no caret and does not toggle.

Waits are one line with what they asked for and what happened:
`Waited for agents · up to 60m · woke after 12m (c39 finished)`; a failed wait
shows its error. Child agents get their own rows, which end a group:
`◆ spawned c23 · worker · <brief>`, `◇ re-tasked c23 · …`, `✓ c23 finished ·
14m`, `✗ c23 failed · <reason>`, `⚠ c23 needs recovery · recover_agent`,
`? c23 asks · <question>`, `⇲ adopted cand-c23-a1 · 16 files` and
`✗ adopt conflict · <path>`. A row with a brief, result, reason, question or
several paths ends `▸` and opens to all of it, and an adopted row opens to
its candidate, the child and its template, and how many files landed; a finished child's JSON result
opens indented, and a result longer than 16 KiB (2 KiB for a reason) ends with
`… cut at 16 KiB of N KiB`. When the root wakes for finished work, the
woken turn simply continues the transcript; no row marks the wake. The
session header at the top is muted: `session 0a1b2c3d`, or
`carried from 4e5f6a7b (r21.e19.p27.t2) · session 0a1b2c3d`. Errors render in
the error colour. Resume uses the same rows and grouping, so a resumed
transcript looks like the live one. Turn off **Group tool calls**
(`tui.group_tool_calls`) to keep every call unfolded. `Alt+A` / `Alt+D` ask
to allow or deny an open effect decision in the confirmation pop-up; a bare
`a` or `d` is always typed, even with the approval row focused.

Only mouse gestures starting in the transcript hide the composer caret. It
returns when a wheel burst settles, or after a transcript click or selection
on the next key press or composer click. A stray drag does not hide it.

## Menu

Menu fills the whole terminal, the same frame as Activity: one title row, one tab row, the same footer and close glyph. Only the body differs. On a wide terminal the border and close button still fill the screen, and the title, the tab row, the tab divider and the footer center with the body, which stays at most 120 columns wide so a value stays next to its label. At 120 columns and narrower nothing moves. Activity is not capped. Nothing of the transcript or the composer is drawn underneath. Seven tabs, in this order: **Settings, Agents, Providers, Usage, Context,
Session, Keybinds**. `←`/`→` or `Tab`/`Shift+Tab` cycles, clicking a tab jumps,
`/` filters inside a tab that supports it, `Esc` or the close glyph closes. A terminal too small to draw the frame says `Menu · terminal too small`.

Menu and other overlays have no scrollbar track;
wheel and keyboard scrolling still work. Only the main transcript and an agent
transcript keep a scrollbar. Usage rows wrap complete words, including reset times.

- **Settings** — the rows below; changes apply immediately.
- **Agents** — the agent profiles: the three built-ins (`codex-default`,
  `claude-default`, `grok-default`) and any `~/.plak/profiles/<name>.toml`.
  Type to filter; `Esc` clears the filter. The box under the list shows the
  root and each sub-agent. A profile file that could not be used is listed
  under the profiles with the reason. See [Agent profiles](#agent-profiles).
- **Providers** — read-only credential status per vertical.
- **Usage** — subscription plan limits and the turn's usage.
- **Context** — the 100-cell breakdown of what is occupying the window.
- **Session** — the Plak version, the running build's id (with
  `new build installed · /reload` when a newer one is installed), session id
  and working directory, then the model, template, effort, thinking, tool
  policy, options, API kernel, turn, context, queue and tree epoch. Click a
  value to copy it.
- **Keybinds** — the generated key sheet.

### Agent profiles

`Enter` (or a double-click) on a profile asks how to use it, in the
confirmation pop-up: focus starts on Fork now, so `Enter` forks at once and
`→` then `Enter` chooses the other action, or click it:

- **Fork now** branches this session onto the profile. The new session starts
  from this conversation with the profile's root and its sub-agent cards; this
  session stays as it was and can be resumed later with its own cards. It needs
  an idle root with no running sub-agent and no queued prompt. If the session
  already runs that profile, nothing branches and the transcript says so. The
  new session reads your `AGENTS.md` files as they are now; if you edited one
  since this session started, the transcript names it (for example
  `AGENTS.md re-read for this branch: workspace root changed`), and so does the
  recovery that finishes a Fork now a crash interrupted. `/fork` and
  `/fork-then-compact` do the same on the current binding. Resuming a session
  keeps the instructions it started with. An OpenRouter root uses the
  protocol and composition its template names; a direct API root uses its
  template's effort, which the template must set to a level on that model's
  ladder (`plak models <account>` lists it).
- **Use now** appears instead when the session has no model yet (a fresh home):
  the session takes the profile's root and the agent attaches, including an
  OpenRouter or direct API-account root.
- **Next session** saves the profile for the next time you start `plak`.

The profile becomes the default for new sessions only once it has landed: a
refused Fork now (an account that is not logged in, a running turn, an effort
the model does not offer) changes nothing, and the transcript says why. The
`current` mark moves only when the product confirms the change.

### Settings

Every row writes `config.toml` and takes effect on change; nothing needs a
save. `d` on a row opens the confirmation pop-up (`Reset <label>?`, focused
on `[ Reset ]`), naming that setting. `Enter` confirms from the value as it
is then, so a change that lands while the pop-up is open is kept except for
the row you are resetting. `Esc` cancels, and a click presses the button it
lands on. Cancel leaves the value. Only Mouse capture waits for a restart before it takes hold.

| Row | Key | Default |
| --- | --- | --- |
| Theme | `tui.theme` | `default-dark` |
| Mouse capture | `tui.mouse` | `true` (restart) |
| Scroll lines | `tui.scroll_lines` | `3` (1–10) |
| Show thinking blocks | `tui.show_thinking` | `true` |
| Group tool calls | `tui.group_tool_calls` | `true` |
| Auto compaction | `compaction.enabled` | `true` |
| Reduced motion | `tui.reduced_motion` | `false` |
| Edit raw config | — | opens `config.toml`, same as `/config` |

Turning off **Mouse capture** hands text selection back to your terminal, which
is what you want for copy-paste. That selection is the terminal's, so it copies
the painted rails and indent rather than the code text Plak's own copy returns. **Reduced motion** suppresses
the spinner, the pulse and camera easing — never the PLAK banner, which has no
row of its own.

### Themes

Six, in menu order: `default-dark`, `default-light`, `catppuccin-latte`,
`catppuccin-frappe`, `catppuccin-macchiato`, `catppuccin-mocha`. The chooser
previews live as you move through it. `dark` and `light` remain accepted aliases
of the two defaults in `config.toml`.

## Activity

`Ctrl+T` opens the last Activity tab, or closes Activity when it is already open. `/tree` opens Agents; `/ps` opens
Processes. The live HUD agent/process counts also open their corresponding
tabs. `Tab` or `Shift+Tab` switches tabs. On an agent's transcript either one leaves the transcript; coming back to Agents shows the list on that agent, and a click on the Agents tab does the same.

Activity uses the same full-page frame as Menu: the centered **Activity** title, the Agents and Processes tabs, the footer and the close glyph, filling the whole screen. No
composer or hint line sits beneath them, at every terminal width. A terminal too small to draw the frame says `Activity · terminal too small` and forgets the previous body's clicks. If the root
needs you (an approval, a context decision), the footer starts with a red
`root needs approval · Esc`; the top border shows the root's turn timer or how
long ago you last prompted when there is room. The live/paused tree state appears
in the Agents body. If sub-agents are unavailable, their reason appears there
rather than in the transcript. Escape hints belong to the focused body when it
handles back or cancel; the close glyph always closes Activity and brightens
on hover.

Processes shows running jobs plus those that finished in the last five minutes,
one row each: status, owner and elapsed time in aligned columns, then the
command cut with `…` (the full command is in the output view), including jobs parked without a
live child owner. Failed or recovery-required child turns stop their jobs;
interrupting a child leaves its jobs running. `Enter` opens live output;
`↑`/`↓`, `PageUp`/`PageDown`, `Home`/`End` and the mouse wheel scroll it, and
`Esc` goes back. In the list, `s` asks to stop the selected running process and
`S` to stop all background processes, in the confirmation pop-up below;
clicking `s stop` or `S stop all` in the footer does the same (on a narrow
terminal the footer reads `Enter · s · S`, still clickable). A stopped process
gets `SIGTERM` and two seconds to exit before `SIGKILL`; its row reads
`stopping` meanwhile, and the TUI stays responsive. `Esc` closes
Activity; `q` does not.

Activity, the agent view and process output are full pages with no prompt and
nothing that looks like one, so typing there is never prompt typing. A key the
view maps acts at once; a key it does not map, and a paste, does nothing, and
the footer never says anything about pausing or typing. The keys that ask to
stop, interrupt, close or resume (and `x`, `y` and `Enter` in the Agents list,
`y` in the agent view) act the moment you press them. The resume picker maps
no letter either: `Enter` resumes the moment you press it; select with
`↑`/`↓`, decline with `Esc`.
Your root draft is never typed into, edited or sent from there: `Ctrl+S`,
`Ctrl+Enter`, `Ctrl+D` and the draft-editing chords do nothing; `Ctrl+T`,
`Ctrl+X`, `Ctrl+O`, `Ctrl+B` and `Ctrl+Q` work as everywhere.

Stopping the root turn (busy `Esc` or `Ctrl+C`), interrupting, closing,
stopping or resuming agents, stopping processes, quitting over running
processes, and applying an agent profile always go through one confirmation
pop-up. It says what will happen (the agent id or
how many agents or processes, and that work in flight is lost) and has
`[ Cancel ]` and the action buttons; the busy `Esc` one offers `[ Stop turn ]`
(the turn and its running agents, which stay resumable) and, while background
processes run, `[ Stop turn + processes ]`. Focus starts on the primary
action, so `Enter` confirms the moment the pop-up opens (it says
`Enter confirms · Esc cancels`). `←` and `→` move focus one button without
wrapping, and `Tab` moves right while `Shift+Tab` moves left, so a `←` then
`Enter` cancels; a held arrow walks the buttons at once. Over the composer a
printable key or `Backspace` closes the pop-up and is typed into your draft,
so typing never confirms it. In Activity, the agent view, process output and
Menu a typed key or `Backspace` does nothing and is not typed into your draft.
`Esc`, a `Ctrl` or
`Alt` chord or a click outside cancels. In
the pop-up, the resume picker, Menu and the help overlay an `Alt` chord acts
as `Esc` (many terminals send `Esc` and the next key together as one), so it
closes or declines and never confirms or resumes. `/stop-all`, and
`/quit` and `Ctrl+Q` while a turn, an agent or a background process runs,
open the same pop-up (its lines name what stops); idle with nothing running
they quit at once. The pop-up
closes by itself when its target stops being active (the turn settles, the
agent finishes, the process exits). SIGTERM/SIGHUP stop without asking.

### Agents

`/tree` opens the Agents list: the agents that matter right now, as an
indented outline. It shows running and waiting agents, agents that finished in
the last five minutes (dimmed after two), agents that need your attention, and
the parents needed to place them. The header counts
`● 3 active   ✓ 21 completed   ⚠ 1 attention`; the footer says how many older
agents are hidden, and `f` shows every agent.

An agent needs attention when its current attempt failed, was blocked by a
quota, ended without submitting a result, or needs recovery (the inspector
names the `recover_agent` remedy). The root does not, and neither does an
agent you closed or whose attempt was cancelled. It stays listed with `⚠`
until you press `x`, which records that one occurrence — the attempt, why it
needs attention, and when it ended — in Plak's store, so it survives a
restart. The same attempt needs `x` again if it later fails differently. The
header's completed count is attempts that finished successfully or whose
candidate was adopted, not every agent that stopped.
Re-tasking the agent with a follow-up clears its attention
without an acknowledgement; a later failure needs a new one.

Each row shows the status glyph, how long the agent has run and its `#id`,
then its role in the session's profile (`worker` for
`my-profile-worker`; other templates in full), model, turns,
tokens and `· N processes` when the width allows. What the agent is doing now
(waiting, thinking, running a command) sits in a right-aligned column at the
end of the row, so the row does not shift as it changes; `⚠` follows it. The description is
in the inspector below the list. Running
agents animate unless reduced motion is on. The selection follows the agent,
not the row: when it leaves the list its parent is selected instead.

| Key | Effect |
| --- | --- |
| `↑` / `k`, `↓` / `j` | Previous/next row |
| `←` / `h` | Parent |
| `→` / `l` | Last-visited child (or first child) |
| `Home` | Root |
| `Enter` | Show the agent's transcript in Activity (on root: close Activity and show the root's) |
| `x` | Acknowledge the selected agent's attention |
| `f` | Switch between the live list and every agent |
| `y` | Copy the agent id to the clipboard |
| `R` | Ask the host for a fresh snapshot and re-read acknowledgements |
| `r` | Resume a restored-paused tree (in the confirmation pop-up; also `/tree resume`) — the only exit from it |
| `i` | Interrupt the selected agent (in the confirmation pop-up; not the root) |
| `c` | Close the selected agent (in the confirmation pop-up; not the root) |
| `s` | Stop every non-root agent (in the confirmation pop-up) |
| `Esc` | Leave the list |

The footer's `i interrupt`, `c close` and `s stop all` are clickable too; on a
narrow terminal the footer keeps every key in short form (`… · i · c · s · …`),
still clickable.

`Esc` in the list never cancels the root turn — that is deliberate, so
closing the view cannot kill your work.

An agent's transcript is the Agents tab's body, drawn exactly like
the root's: live while the agent runs, readable after it finishes. Finished
code cells show their nested calls when the session retains the explicit
owning-cell receipt and definitive tool outcome, including failed calls. Missing
or oversized older outcomes are not reconstructed. Calls whose receipt was pruned
or whose parent is outside the visible history remain ordinary rows; the cell
keeps its own result status. The Activity title, the Agents and Processes tabs and the close glyph stay. The body starts with `root › #<id> · <description>`, then the transcript, then the agent's row from the Agents list
(click it to return to the list on that agent). The transcript's scrollbar pages when you click the track and follows when you drag the thumb. `Tab`, `Shift+Tab` or a click on Processes leaves the transcript; a click on Agents, or coming back to Agents, shows the list on this agent. An agent that needs recovery shows the remedy above that row. The footer carries the keys below; `i`, `c`, `y` and `x` are clickable.

| Key | Effect |
| --- | --- |
| `Esc` | Back to the Agents list on this agent, however you opened it; never stops the root turn |
| `↑` / `↓`, `PageUp` / `PageDown` | Scroll |
| `Enter` | Open or close the focused row |
| `[` / `]` | Previous / next agent in the Agents list's order |
| `i` / `c` | Interrupt / close this agent (in the confirmation pop-up) |
| `x` | Acknowledge this agent's attention |
| `y` | Copy the agent id |
| `Ctrl+T`, close glyph | Close Activity and return to the root transcript |
| `Ctrl+C` | Back, as `Esc` |

Typing does nothing here (see above); your root draft waits for you. If the root needs
you (an approval, a context decision), the footer starts with a red `root
needs approval · Esc`. Everything the root
does meanwhile is waiting there too. `/clear`, `/resume` or a fork closes the
agent view. See
[agents and orchestration](agents-and-orchestration.md) for what those nodes
are.

## Context and compaction

What you see:

- The composer meter, `15%/272k`: how full the model's context window is.
- The Context tab (`/context`): the same number as a 100-cell grid by
  category (system prompt, tool definitions, `AGENTS.md` when enrolled,
  messages, reasoning, an unattributed remainder), with the autocompact buffer
  reserved past the fence when Auto compaction is on. Every occupying row is
  labelled `est.`; anything the provider does not report reads `not reported`.
- A Compact row in the transcript for each compaction, with before and after
  figures.

Details:

- Occupancy is the provider's input-token count for the last answered request
  plus UTF-8 bytes divided by four for what was appended since. Before the
  first answered request, and right after a compaction or branch switch, it is
  bytes over four of the whole request, so the meter may read high until the
  next request re-anchors it. It survives quitting and reopening the session.
- With **Auto compaction** on, Plak compacts at the route's token trigger,
  which the Context tab shows, but only once a provider count anchors the
  measure; a request already over the ceiling, or a provider overflow, compacts
  and resends once. A failed paid fold is never redispatched; repeated failures
  back off and stop paid folds for the rest of the turn. Sub-agents compact at
  the same trigger when they start each turn.
- `/compact` compacts by hand at the next safe idle boundary, or says
  `nothing in this conversation can be compacted yet`. Compaction never buys
  more of a subscription limit.
- A prompt that does not fit the window is withheld and put back in the
  composer (after any new draft, separated by a blank line) to shorten and
  resend. Sub-agent results that do not fit with it stay unread for a later
  prompt; a wake that does not fit pauses auto-continue until your next prompt.
  Follow the recovery panel: shorten an oversized prompt, compact, or choose a
  larger model, fork or `/clear`. The failed turn is not replayed.
- After a compaction the model gets its latest task list back as a short
  harness note, so it continues the plan.

The exact accounting, triggers and backoff are in
[the context gauge](../spec/02-plak-product.md#56-the-context-gauge).
