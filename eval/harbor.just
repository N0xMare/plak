# eval: the opt-in, paid Harbor / Terminal-Bench bakeoff. Not in any
# gate tier. See eval/README.md. Runs from the repository root.
set shell := ["bash", "-eu", "-o", "pipefail", "-c"]
set working-directory := '..'

[private]
default:
    @just --justfile "{{justfile()}}" --list harbor

# Zero-inference adapter contracts for the Harbor plane.
[doc("zero-inference adapter contracts")]
test:
    python3 -m unittest discover -s eval/tests -t eval/tests

# Pinned public search executable, identical policy for every arm.
[doc("pinned public search executable for every arm")]
tools:
    PYTHONPATH=eval python3 -m harbor_adapter.cell_prep

# No inference: freeze images and generate a private digest-bound plan.
[doc("list images and write a digest-bound plan; no inference")]
inventory *ARGS:
    PYTHONPATH=eval uv run --project eval python -m harbor_adapter.campaign_builder inventory {{ARGS}}

[doc("freeze Harbor images for a campaign plan")]
freeze *ARGS:
    PYTHONPATH=eval uv run --project eval python -m harbor_adapter.campaign_builder freeze {{ARGS}}

[doc("generate a campaign plan from frozen images")]
campaign *ARGS:
    PYTHONPATH=eval uv run --project eval python -m harbor_adapter.campaign_builder generate {{ARGS}}

# Paid subscription usage, only after explicit operator approval and plan review.
[doc("paid calibration phase; operator approval required")]
phase0:
    uv run --project eval python eval/scripts/campaign.py calibration

[doc("paid null arm")]
null:
    uv run --project eval python eval/scripts/campaign.py null

[doc("paid Codex arm")]
codex:
    uv run --project eval python eval/scripts/campaign.py codex

[doc("bootstrap the Harbor checkout")]
bootstrap:
    eval/scripts/harbor.sh bootstrap

[doc("check the Harbor checkout")]
check:
    eval/scripts/harbor.sh check

[doc("build the agent image")]
build-agent:
    eval/scripts/harbor.sh build-agent

[doc("fetch pinned vendor CLIs")]
vendor-clis:
    eval/scripts/harbor.sh fetch-vendor-clis

[doc("fetch the pinned pmux binary")]
pmux:
    eval/scripts/harbor.sh fetch-pmux

[doc("paid Harbor evaluation")]
eval *ARGS:
    eval/scripts/harbor.sh eval {{ARGS}}

[doc("open the Harbor viewer")]
view:
    eval/scripts/harbor.sh view

# Reward plus harness termination per trial, over archived job trees. No cost.
[doc("reward and harness termination over archived jobs")]
summarize *ARGS:
    eval/scripts/summarize.py {{ARGS}}

[doc("paid Harbor smoke")]
smoke *ARGS:
    eval/scripts/smoke.sh {{ARGS}}

# Rung A: plumbing (local tiny tasks) then cheap TB 2.1 A/B. Paid, not just check.
[doc("paid Rung A plumbing, then a cheap Terminal-Bench 2.1 A/B")]
rung-a *ARGS:
    eval/scripts/rung-a.sh {{ARGS}}

# Rung B: one TB 4.0 task to prove wiring. Never the 66-task campaign.
[doc("paid Rung B: one Terminal-Bench 4.0 task")]
rung-b *ARGS:
    eval/scripts/rung-b.sh {{ARGS}}
