# 0.0.2 Readiness Ledger

This ledger distinguishes implementation evidence from live calibration.
`fixed-before` means the inherited validity gates already addressed the item;
`fixed-now` names the additional implementation. Neither label asserts a paid
trial has run. Historical job trees remain diagnostic and private.

| Defect | Disposition | Evidence / Remaining Condition |
| --- | --- | --- |
| D1 | fixed-before | Omitted-effort OpenRouter comparisons are disabled, not relabeled as equivalent. |
| D2 | fixed-now | HTTPS CONNECT recorder, native Plak capture, reduced first-request pins; Phase 0 must establish actual pinned-CLI proxy compatibility. Native declarations cannot pass. |
| D3 | fixed-now | Reducer and trial gate preserve requested/served tiers and served identity; recorder streams responses without retaining content. Provider rejection status is distinguished from a successfully served generation. |
| D4 | deferred-with-reason | OpenRouter comparisons stay disabled; no gateway routing or upstream equivalence is claimed. Reducer retains provider identity if present. |
| D5 | deferred-with-reason | Wire output limits are recorded; internal client context ceilings cannot be inferred from a wire request. The affected OpenRouter comparison is disabled. Context policy remains a harness property, not an asserted shared limit. |
| D6 | fixed-now | Inherited lifetime checks and serial host refresh; account leases prevent concurrent campaign processes on the same host. Operators must not reuse accounts concurrently across hosts. |
| D7 | fixed-before | Codex auth write-back, serial slots, credential errors unscored. |
| D8 | fixed-before | Symmetric infra / agent / unknown attribution and bounded slot reruns. |
| D9 | fixed-before | Private cell roots and allowlisted harvest with final secret scan. Historical private credential copies are deliberately not opened or deleted; owner rotation/removal remains an operator security task. |
| D10 | fixed-before | Same pinned ripgrep on every arm; checked executable bytes and startup. |
| D11 | fixed-now | Freeze common packages once in each task image; adapters refuse missing prerequisites instead of apt-installing different packages. Package/resource probe is hash-bound. |
| D12 | fixed-now | pmux runs as root with IS_SANDBOX=1; no user creation, sudo, ACL/chown sweep or arm-only ulimit. Acceptance by the frozen Claude CLI is a Phase 0 assertion, not assumed live evidence. |
| D13 | fixed-now | No adapter leftover-process kill; symmetric process/socket samples. Plak's leave-jobs-running argv belongs to the amp-jobs change and must be present in the final built source; a zero-inference capability probe refuses older binaries. |
| D14 | fixed-now | Pi sessions and OpenCode XDG data are cell-local too; copies exclude auth files and undergo the common secret scan. |
| D15 | fixed-before | Immutable local image IDs and no per-trial rebuild; freeze/generator now acquire and bind them without inference. |
| D16 | fixed-now | Codex web_search=disabled, Grok's native disable switch, Claude's first-party disallowedTools launcher; wire tool list must confirm disabled web tools. Ordinary shell networking remains available equally. |
| D17 | fixed-now | Before/after cgroup v2 memory.peak, memory.events oom/oom_kill and cpu.stat usage_usec, without command lines. |
| D18 | fixed-now | Task-derived pmux timeout; obsolete 900-second default removed. |
| D19 | fixed-before | Agent timeout/nonzero exits retain assertion-bearing verifier rewards. |
| D20 | fixed-now | In-cell GNU timeout owns the agent shell process group, with TERM/KILL before Harbor's outer timeout. Normal exit does not kill surviving services. |
| D21 | deferred-with-reason | Symmetric content-free wire retry lower bounds and causes are recorded; completeness is explicitly false. Exact native retry counters across every vendor are not established by the frozen log formats. Do not report the lower bound as an exact zero or use it for retry-efficiency claims. Retry behavior stays a scored harness property. |
| D22 | fixed-before | Native first-party terminal parsers feed one taxonomy, including clean-exit Pi errors. |
| D23 | fixed-before | Harbor transcript-regex exception subtypes do not determine provider attribution. |
| D24 | fixed-before | Plak fallback uses its final diagnostic, not tool chatter. |
| D25 | fixed-now | Removed pmux score-through; campaigns require the Docker daemon's native architecture. Daemon loss is infra. |
| D26 | fixed-before | Task-weighted paired bootstrap and symmetric selection; now also within-task permutation p, exact majority McNemar, 90% equivalence interval and Holm p correction. Population claims still require more tasks. Publication-quality forest plots and secondary Hodges-Lehmann summaries are not a launch gate and remain analysis work. |
| D27 | fixed-before | Whole vendor/Node archive pins and executed binary checks. Generator computes archive member hashes; version strings alone never admit bytes. |
| D28 | fixed-now | Inherited clean build manifest; generator additionally requires Plak's source commit to equal the campaign commit. Rebuild after integration, never approve an old artifact by changing a pin. |
| D29 | fixed-before | No composition override; actual resolved composition is checked against the recorded shipped route pack. |
| D30 | fixed-now | First-wire instruction/tool-schema digests include Responses developer input and nested tool names; Claude uses only the reviewed three-field billing-nonce projection while retaining full wire digests; actual CLI argv is recorded without the task prompt or shell IO. |
| D31 | fixed-before | Serial interleaved alternating arm-order blocks with aligned task slots. |
| D32 | fixed-before | All subscription arms use concurrency one and one attempt per slot. |

## Before The First Scored Campaign

Rebase onto integrated source (including GPT-6 prompt and amp-jobs), pass the
ordinary repository gate and offline adapter suite, and build new clean Linux
artifacts. Old local vendor archives are not automatically approved: their
versions and bytes must satisfy the current pins. Freeze task images on the
native Docker architecture and review each generated private plan and SHA-256.

Paid steps deliberately separate from preparation: Phase 0 calibration (one
plumbing trial per selected arm), A/A (8 tasks x 3 blocks x 2 identical Plak
labels = 48 trials), then the Codex pair (16 x 5 x 2 = 160 trials). A/A must
retain all eight tasks, cover zero in the paired CI and have <5% infra per arm.
The Codex recipe refuses a missing, changed, failed or mismatched A/A receipt.
No subscription usage or other inference is part of `harbor-test`.

The recorder is an HTTP/1.1 CONNECT proxy, not a WebSocket recorder. A CLI that
bypasses proxy variables or uses an unsupported transport must fail calibration;
never replace absent bytes with its requested options. Resolve that specific
pin's first-party transport/logging configuration before scoring it. The proxy
itself is tested with local TLS and synthetic SSE, without provider traffic.

Before paid use, recheck Docker lifecycle latency: a credential-free tars2
Compose probe under concurrent gate load took about nine minutes to clean up.
It eventually completed; its container, image and network were verified
removed. This is infrastructure latency, not an agent failure or successful
campaign evidence. `docker info` alone is not a sufficient preflight.
