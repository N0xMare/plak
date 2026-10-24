# eval: the opt-in, paid Harbor / Terminal-Bench bakeoff. Not in any
# gate tier. See eval/README.md. Runs from the repository root.
set shell := ["bash", "-eu", "-o", "pipefail", "-c"]

[private]
default:
    @just --justfile "{{justfile()}}" --list harbor

# Zero-inference adapter contracts for the Harbor plane.
test:
    python3 -m unittest discover -s eval/tests -t eval/tests

# Pinned public search executable, identical policy for every arm.
tools:
    PYTHONPATH=eval python3 -m harbor_adapter.cell_prep

# No inference: freeze images and generate a private digest-bound plan.
inventory *ARGS:
    PYTHONPATH=eval uv run --project eval python -m harbor_adapter.campaign_builder inventory {{ARGS}}

freeze *ARGS:
    PYTHONPATH=eval uv run --project eval python -m harbor_adapter.campaign_builder freeze {{ARGS}}

campaign *ARGS:
    PYTHONPATH=eval uv run --project eval python -m harbor_adapter.campaign_builder generate {{ARGS}}

# Paid subscription usage, only after explicit operator approval and plan review.
phase0:
    uv run --project eval python eval/scripts/campaign.py calibration

null:
    uv run --project eval python eval/scripts/campaign.py null

codex:
    uv run --project eval python eval/scripts/campaign.py codex

bootstrap:
    eval/scripts/harbor.sh bootstrap

check:
    eval/scripts/harbor.sh check

build-agent:
    eval/scripts/harbor.sh build-agent

vendor-clis:
    eval/scripts/harbor.sh fetch-vendor-clis

pmux:
    eval/scripts/harbor.sh fetch-pmux

eval *ARGS:
    eval/scripts/harbor.sh eval {{ARGS}}

view:
    eval/scripts/harbor.sh view

# Reward plus harness termination per trial, over archived job trees. No cost.
summarize *ARGS:
    python3 eval/scripts/summarize.py {{ARGS}}

smoke *ARGS:
    eval/scripts/smoke.sh {{ARGS}}

# Rung A: plumbing (local tiny tasks) then cheap TB 2.1 A/B. Paid, not just check.
rung-a *ARGS:
    eval/scripts/rung-a.sh {{ARGS}}

# Rung B: one TB 4.0 task to prove wiring. Never the 66-task campaign.
rung-b *ARGS:
    eval/scripts/rung-b.sh {{ARGS}}
