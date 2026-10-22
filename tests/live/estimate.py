#!/usr/bin/env python3
"""Dry-run spend for the planned live matrix. No network.

Usage: estimate.py --rate-card PATH --soak-row ROUTE [--contingency 0.20]
       [--openrouter-ceiling N]

The rate card is a JSON object with `unit` equal to `USD per million tokens`
and `models` mapping a model id to `input`, `output`, `cache_read` and
`cache_write` prices. Extra fields are ignored. This script never fetches the
card's source URL and never invents a price.

A cell is one platform, lane and route:

- every planned route: one Mac smoke
- `full`: also Mac long-session, jobs, two extra crash_jobs runs, and Linux smoke;
  a lane the route marks informational is priced and shows class `informational`
- one selected `full` row: Mac headless soak for one hour
- optional `--tui-rows a,b`: half-hour Mac TUI sessions on those full routes
- the four direct API rows (`api-openai`, `api-openai-medium`,
  `api-anthropic`, `api-xai`): also a Linux smoke

Linux does not run long-session, jobs or either soak (spec 05 §3.4).

Input is an uncached share plus a cached share. Cached tokens are billed at
`cache_read` instead of `input`. The default cached share is 0.9 on soak and
TUI soak. Smoke and jobs use the measured short-run share 0.64. Long-session
uses the measured DeepSeek share 0.22. Cache writes default to zero.
Reasoning has no rate-card field.

Defaults are measured volumes where a trial recorded them, and the previous
upper band where it did not:

- smoke, 12 scenarios: 241,039 input and 5,500 output. The DeepSeek trial
  measured 201,039 input (64% cached). The compaction scenario now folds a
  20,000-token read that the compact request sees again (+40,000). Output
  was not remeasured
- headless soak, 1 hour: 5,600,000 input and 60,000 output
- TUI soak, 2 hours: 8,600,000 input; output is that input times the soak
  output/input ratio, because the TUI logs did not report output
- long-session, all four scenarios: 750,415 input and 4,673 output on the
  measured DeepSeek window (1,048,576). Other windows scale by
  catalog_window / that window. The previous per-scenario million-token
  bands priced one DeepSeek cell like the whole OpenRouter matrix
- jobs, 11 scenarios: the smoke per-scenario share, at the smoke cache share.
  The jobs trial recorded no tokens, so this is the smoke measurement applied
  to the jobs scenario count

An API-key row is priced when the card names its model. A subscription row
is quota and stays unpriced even if the card names the model. The OpenRouter
subtotal is printed beside `--openrouter-ceiling`. Crossing the ceiling warns
and does not hide the cells. The ceiling is not a spec amount.

Every cell reserves one extra attempt for a first MODEL or PROVIDER. This
reserve is explicit, not hidden in contingency; reruns cannot chain.
"""

from __future__ import annotations

import argparse
import json
import sys
from dataclasses import dataclass
from decimal import Decimal, ROUND_HALF_UP
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import live  # noqa: E402

UNIT = "USD per million tokens"
MILLION = Decimal(1_000_000)
DISPLAY = Decimal("0.000001")
LINUX_SMOKE_ACCOUNTS = frozenset({"openai", "anthropic", "xai-api"})
API_ACCOUNTS = frozenset(live.API_KEY_ENV)
CLASS_ORDER = {live.FULL: 0, live.LIGHT: 1, live.INFORMATIONAL: 2}
LANE_ORDER = {"smoke": 0, "long-session": 1, "jobs": 2, "crash-jobs": 3, "soak": 4, "tui-soak": 5}
PLATFORM_ORDER = {"mac": 0, "linux": 1}

# DeepSeek smoke, 2026-10-09 trial (201,039), plus 40,000 for the compaction
# scenario's 20,000-token fold (the read turn and the compact request).
# Output was not remeasured; 5,500 stays.
SMOKE_INPUT = 241_039
SMOKE_OUTPUT = 5_500
SMOKE_CACHE_SHARE = Decimal("0.64")
SOAK_INPUT = 5_600_000
SOAK_OUTPUT = 60_000
SOAK_CACHE_SHARE = Decimal("0.9")
SOAK_HOURS = Decimal(1)
TUI_INPUT = 8_600_000
TUI_OUTPUT = TUI_INPUT * SOAK_OUTPUT // SOAK_INPUT
TUI_CACHE_SHARE = Decimal("0.9")
TUI_HOURS = Decimal(2)
# Four scenarios on deepseek/deepseek-v4.1-flash, same trial: 22% cached.
GROWTH_SCENARIOS = ("in_turn_compaction", "overflow", "kill_reopen", "child_compaction")
LONG_SESSION_INPUT = 750_415
LONG_SESSION_OUTPUT = 4_673
LONG_SESSION_CACHE_SHARE = Decimal("0.22")
# `input_window` for that model. Other cells scale by their catalog window.
LONG_SESSION_MEASURED_WINDOW = 1_048_576
JOBS_SCENARIOS = 11
JOBS_INPUT = (SMOKE_INPUT // 12) * JOBS_SCENARIOS
JOBS_OUTPUT = (SMOKE_OUTPUT // 12) * JOBS_SCENARIOS
JOBS_CACHE_SHARE = SMOKE_CACHE_SHARE


class EstimateError(Exception):
    """The rate card or the assumptions cannot be priced."""


@dataclass(frozen=True)
class Assumptions:
    """Lane totals. Soak and TUI figures are for the default duration and scale with hours."""

    smoke_input_tokens: int = SMOKE_INPUT
    smoke_output_tokens: int = SMOKE_OUTPUT
    smoke_cache_share: Decimal = SMOKE_CACHE_SHARE
    smoke_cache_write_tokens: int = 0
    soak_hours: Decimal = SOAK_HOURS
    soak_input_tokens: int = SOAK_INPUT
    soak_output_tokens: int = SOAK_OUTPUT
    soak_cache_share: Decimal = SOAK_CACHE_SHARE
    tui_soak_hours: Decimal = Decimal("0.5")
    tui_soak_input_tokens: int = TUI_INPUT
    tui_soak_output_tokens: int = TUI_OUTPUT
    tui_cache_share: Decimal = TUI_CACHE_SHARE
    long_session_input_tokens: int = LONG_SESSION_INPUT
    long_session_output_tokens: int = LONG_SESSION_OUTPUT
    long_session_measured_window: int = LONG_SESSION_MEASURED_WINDOW
    long_session_cache_share: Decimal = LONG_SESSION_CACHE_SHARE
    jobs_input_tokens: int = JOBS_INPUT
    jobs_output_tokens: int = JOBS_OUTPUT
    jobs_cache_share: Decimal = JOBS_CACHE_SHARE
    contingency: Decimal = Decimal("0.20")
    openrouter_ceiling: Decimal | None = None


@dataclass(frozen=True)
class Charge:
    input_tokens: int
    output_tokens: int
    cached_input_tokens: int
    cache_write_tokens: int


@dataclass(frozen=True)
class Rate:
    input: Decimal
    output: Decimal
    cache_read: Decimal
    cache_write: Decimal


@dataclass(frozen=True)
class Cell:
    platform: str
    lane: str
    route_name: str
    route_class: str
    model: str
    account: str


@dataclass(frozen=True)
class PricedCell:
    """`usd` is the base price. None means quota or a missing API rate."""

    cell: Cell
    charge: Charge
    usd: Decimal | None
    billing: str

    def with_contingency(self, contingency: Decimal) -> Decimal | None:
        if self.usd is None:
            return None
        return self.usd * 2 * (Decimal(1) + contingency)


@dataclass(frozen=True)
class Estimate:
    cells: tuple[PricedCell, ...]
    contingency: Decimal
    subtotal: Decimal
    rerun_reserve: Decimal
    contingency_usd: Decimal
    total: Decimal
    openrouter_subtotal: Decimal
    openrouter_with_contingency: Decimal
    openrouter_ceiling: Decimal | None
    exceeds_openrouter_ceiling: bool
    unpriced_models: tuple[str, ...]
    subscription_models: tuple[str, ...]


def planned_cells(routes: dict[str, live.Route] | None = None, *,
                  soak_row: str | None = None, tui_rows: tuple[str, ...] = ()) -> list[Cell]:
    """Lean pre-release plan; choose the least-used full row from daily use."""
    routes = routes if routes is not None else live.ROUTES
    full = {name for name, route in routes.items() if route.planned and route.route_class == live.FULL}
    if full and soak_row is None:
        raise EstimateError("--soak-row is required for the full plan")
    if soak_row is not None and soak_row not in full:
        raise EstimateError("--soak-row must name a planned full route")
    if len(set(tui_rows)) != len(tui_rows) or any(name not in full for name in tui_rows):
        raise EstimateError("--tui-rows must name distinct planned full routes")
    cells: list[Cell] = []
    for name in routes:
        route = routes[name]
        if not route.planned:
            continue
        if route.route_class not in live.ROUTE_CLASSES:
            raise EstimateError(f"{name} has no route class")
        cells.append(_cell("mac", "smoke", route))
        if route.route_class == live.FULL:
            cells.append(_cell("mac", "long-session", route))
            cells.append(_cell("mac", "jobs", route))
            cells.append(_cell("mac", "crash-jobs", route))
            if name == soak_row:
                cells.append(_cell("mac", "soak", route))
            if name in tui_rows:
                cells.append(_cell("mac", "tui-soak", route))
            cells.append(_cell("linux", "smoke", route))
        elif route.account in LINUX_SMOKE_ACCOUNTS:
            cells.append(_cell("linux", "smoke", route))
    cells.sort(key=lambda cell: (
        PLATFORM_ORDER[cell.platform],
        CLASS_ORDER[cell.route_class],
        cell.route_name,
        LANE_ORDER[cell.lane],
    ))
    return cells


def load_rate_card(path: Path) -> dict[str, Rate]:
    """Read prices from `path` only. The card's source URL is not fetched."""
    try:
        data = json.loads(path.read_text(), parse_float=Decimal)
    except (OSError, UnicodeError, json.JSONDecodeError) as error:
        raise EstimateError(f"cannot read rate card: {error}") from error
    if not isinstance(data, dict) or data.get("unit") != UNIT:
        raise EstimateError(f"rate card unit must be {UNIT!r}")
    models = data.get("models")
    if not isinstance(models, dict):
        raise EstimateError("rate card models must be an object")
    rates: dict[str, Rate] = {}
    for model, row in models.items():
        if not isinstance(model, str) or not isinstance(row, dict):
            raise EstimateError("each rate-card model must be an object")
        rates[model] = Rate(
            _price(row, "input", model),
            _price(row, "output", model),
            _price(row, "cache_read", model),
            _price(row, "cache_write", model),
        )
    return rates


def input_window(model: str) -> int:
    """Catalog input window used to pick the long-session growth band.

    Claude is 1,000,000. `grok-4.7` is 256,000 and `grok-4.6` is 500,000
    (spec 01). Codex and OpenAI API rows are 272,000.
    `deepseek/deepseek-v4.1-flash-1017` is 1,048,576 in the OpenRouter catalog.
    """
    if model == "grok-4.7":
        return 256_000
    if model == "grok-4.6":
        return 500_000
    if model.startswith("claude-"):
        return 1_000_000
    if model.startswith("gpt-"):
        return 272_000
    if model == "deepseek/deepseek-v4.1-flash-1017":
        return 1_048_576
    raise EstimateError(f"no input window for {model}")


def lane_charge(cell: Cell, assumptions: Assumptions | None = None) -> Charge:
    assumptions = assumptions or Assumptions()
    if cell.lane == "smoke":
        return _from_share(assumptions.smoke_input_tokens, assumptions.smoke_output_tokens,
                           assumptions.smoke_cache_share, assumptions.smoke_cache_write_tokens)
    if cell.lane == "soak":
        return _duration(assumptions.soak_input_tokens, assumptions.soak_output_tokens,
                         assumptions.soak_hours, SOAK_HOURS, assumptions.soak_cache_share)
    if cell.lane == "tui-soak":
        return _duration(assumptions.tui_soak_input_tokens, assumptions.tui_soak_output_tokens,
                         assumptions.tui_soak_hours, TUI_HOURS, assumptions.tui_cache_share)
    if cell.lane == "jobs":
        return _from_share(assumptions.jobs_input_tokens, assumptions.jobs_output_tokens,
                           assumptions.jobs_cache_share, 0)
    if cell.lane == "crash-jobs":
        return _from_share(_scale_tokens(assumptions.jobs_input_tokens, Decimal(2), Decimal(JOBS_SCENARIOS)),
                           _scale_tokens(assumptions.jobs_output_tokens, Decimal(2), Decimal(JOBS_SCENARIOS)),
                           assumptions.jobs_cache_share, 0)
    if cell.lane == "long-session":
        return _long_session(cell.model, assumptions)
    raise EstimateError(f"unknown lane {cell.lane}")


def price_charge(charge: Charge, rate: Rate) -> Decimal:
    if charge.cached_input_tokens > charge.input_tokens:
        raise EstimateError("cached input cannot exceed input")
    fresh = charge.input_tokens - charge.cached_input_tokens
    return (
        Decimal(fresh) * rate.input
        + Decimal(charge.cached_input_tokens) * rate.cache_read
        + Decimal(charge.cache_write_tokens) * rate.cache_write
        + Decimal(charge.output_tokens) * rate.output
    ) / MILLION


def estimate_matrix(cells: list[Cell], rates: dict[str, Rate],
                    assumptions: Assumptions | None = None) -> Estimate:
    assumptions = assumptions or Assumptions()
    if assumptions.contingency < 0:
        raise EstimateError("contingency must be >= 0")
    if assumptions.openrouter_ceiling is not None and assumptions.openrouter_ceiling < 0:
        raise EstimateError("openrouter ceiling must be >= 0")
    priced: list[PricedCell] = []
    subtotal = Decimal(0)
    openrouter = Decimal(0)
    for cell in cells:
        charge = lane_charge(cell, assumptions)
        usd, billing = _bill(cell, charge, rates)
        if usd is not None:
            subtotal += usd
            if cell.account == "openrouter":
                openrouter += usd
        priced.append(PricedCell(cell, charge, usd, billing))
    rerun_reserve = subtotal
    contingency_usd = (subtotal + rerun_reserve) * assumptions.contingency
    openrouter_with = openrouter * 2 * (Decimal(1) + assumptions.contingency)
    ceiling = assumptions.openrouter_ceiling
    exceeds = ceiling is not None and openrouter_with > ceiling
    unpriced = tuple(sorted({
        cell.model for cell in cells
        if cell.account in API_ACCOUNTS and cell.model not in rates
    }))
    subscription = tuple(sorted({
        cell.model for cell in cells if cell.account not in API_ACCOUNTS
    }))
    return Estimate(
        tuple(priced),
        assumptions.contingency,
        subtotal,
        rerun_reserve,
        contingency_usd,
        subtotal + rerun_reserve + contingency_usd,
        openrouter,
        openrouter_with,
        ceiling,
        exceeds,
        unpriced,
        subscription,
    )


def render(report: Estimate) -> str:
    header = ("platform", "lane", "class", "route", "model", "usd", "rerun_reserve",
              "with_contingency")
    body = [header]
    for item in report.cells:
        cell = item.cell
        body.append((
            cell.platform, cell.lane, cell.route_class, cell.route_name, cell.model,
            _usd(item.usd, item.billing), _usd(item.usd, item.billing),
            _usd(item.with_contingency(report.contingency), item.billing),
        ))
    widths = [max(len(row[index]) for row in body) for index in range(len(header))]
    lines = ["  ".join(cell.ljust(width) for cell, width in zip(row, widths)).rstrip()
             for row in body]
    lines.append(f"subtotal {_usd(report.subtotal)}")
    lines.append(f"rerun_reserve {_usd(report.rerun_reserve)}")
    lines.append("attempts per cell: up to 2 (first MODEL or PROVIDER; no chained reruns)")
    lines.append(f"openrouter_subtotal {_usd(report.openrouter_subtotal)}")
    lines.append(
        f"openrouter_with_contingency {_usd(report.openrouter_with_contingency)}")
    if report.openrouter_ceiling is None:
        lines.append("openrouter_ceiling unset")
    else:
        lines.append(f"openrouter_ceiling {_usd(report.openrouter_ceiling)}")
    if report.exceeds_openrouter_ceiling:
        lines.append("warning: openrouter estimate exceeds ceiling")
    lines.append(f"contingency {report.contingency} {_usd(report.contingency_usd)}")
    lines.append(f"total {_usd(report.total)}")
    if report.unpriced_models:
        lines.append("unpriced " + ", ".join(report.unpriced_models))
    if report.subscription_models:
        lines.append("subscription " + ", ".join(report.subscription_models))
    return "\n".join(lines) + "\n"


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--rate-card", type=Path, required=True)
    parser.add_argument("--soak-row", required=True, help="least-used full route; one Mac headless soak")
    parser.add_argument("--tui-rows", default="", help="comma-separated full routes lacking root use")
    parser.add_argument("--contingency", type=Decimal, default=Decimal("0.20"))
    parser.add_argument("--openrouter-ceiling", type=Decimal, default=None)
    parser.add_argument("--smoke-input", type=int, default=SMOKE_INPUT)
    parser.add_argument("--smoke-output", type=int, default=SMOKE_OUTPUT)
    parser.add_argument("--smoke-cache-share", type=Decimal, default=SMOKE_CACHE_SHARE)
    parser.add_argument("--smoke-cache-write", type=int, default=0)
    parser.add_argument("--soak-hours", type=Decimal, default=SOAK_HOURS)
    parser.add_argument("--soak-input", type=int, default=SOAK_INPUT)
    parser.add_argument("--soak-output", type=int, default=SOAK_OUTPUT)
    parser.add_argument("--soak-cache-share", type=Decimal, default=SOAK_CACHE_SHARE)
    parser.add_argument("--tui-soak-hours", type=Decimal, default=Decimal("0.5"))
    parser.add_argument("--tui-soak-input", type=int, default=TUI_INPUT)
    parser.add_argument("--tui-soak-output", type=int, default=TUI_OUTPUT)
    parser.add_argument("--tui-cache-share", type=Decimal, default=TUI_CACHE_SHARE)
    parser.add_argument("--long-session-input", type=int, default=LONG_SESSION_INPUT)
    parser.add_argument("--long-session-output", type=int, default=LONG_SESSION_OUTPUT)
    parser.add_argument("--long-session-cache-share", type=Decimal,
                        default=LONG_SESSION_CACHE_SHARE)
    parser.add_argument("--jobs-input", type=int, default=JOBS_INPUT)
    parser.add_argument("--jobs-output", type=int, default=JOBS_OUTPUT)
    parser.add_argument("--jobs-cache-share", type=Decimal, default=JOBS_CACHE_SHARE)
    args = parser.parse_args(argv)
    assumptions = Assumptions(
        smoke_input_tokens=args.smoke_input,
        smoke_output_tokens=args.smoke_output,
        smoke_cache_share=args.smoke_cache_share,
        smoke_cache_write_tokens=args.smoke_cache_write,
        soak_hours=args.soak_hours,
        soak_input_tokens=args.soak_input,
        soak_output_tokens=args.soak_output,
        soak_cache_share=args.soak_cache_share,
        tui_soak_hours=args.tui_soak_hours,
        tui_soak_input_tokens=args.tui_soak_input,
        tui_soak_output_tokens=args.tui_soak_output,
        tui_cache_share=args.tui_cache_share,
        long_session_input_tokens=args.long_session_input,
        long_session_output_tokens=args.long_session_output,
        long_session_cache_share=args.long_session_cache_share,
        jobs_input_tokens=args.jobs_input,
        jobs_output_tokens=args.jobs_output,
        jobs_cache_share=args.jobs_cache_share,
        contingency=args.contingency,
        openrouter_ceiling=args.openrouter_ceiling,
    )
    try:
        tui_rows = tuple(args.tui_rows.split(",")) if args.tui_rows else ()
        report = estimate_matrix(planned_cells(soak_row=args.soak_row, tui_rows=tui_rows),
                                 load_rate_card(args.rate_card), assumptions)
    except EstimateError as error:
        print(f"estimate: {error}", file=sys.stderr)
        return 2
    if report.exceeds_openrouter_ceiling:
        print("estimate: openrouter estimate exceeds ceiling "
              f"{_usd(report.openrouter_with_contingency)} > {_usd(report.openrouter_ceiling)}",
              file=sys.stderr)
    sys.stdout.write(render(report))
    return 0


def _cell(platform: str, lane: str, route: live.Route) -> Cell:
    return Cell(platform, lane, route.name, route.lane_class(lane), route.model, route.account)


def scaled_long_session(total: int, window: int, measured_window: int) -> int:
    """Scale the measured four-scenario total by catalog window / measured window."""
    if measured_window <= 0:
        raise EstimateError("measured long-session window must be positive")
    scaled = Decimal(total) * Decimal(window) / Decimal(measured_window)
    return int(scaled.to_integral_value(rounding=ROUND_HALF_UP))


def _long_session(model: str, assumptions: Assumptions) -> Charge:
    window = input_window(model)
    total_in = scaled_long_session(
        assumptions.long_session_input_tokens, window, assumptions.long_session_measured_window)
    total_out = scaled_long_session(
        assumptions.long_session_output_tokens, window, assumptions.long_session_measured_window)
    return _from_share(total_in, total_out, assumptions.long_session_cache_share, 0)


def _duration(total_in: int, total_out: int, hours: Decimal, default_hours: Decimal,
              share: Decimal) -> Charge:
    if hours < 0:
        raise EstimateError("hours must be >= 0")
    scaled_in = _scale_tokens(total_in, hours, default_hours)
    scaled_out = _scale_tokens(total_out, hours, default_hours)
    return _from_share(scaled_in, scaled_out, share, 0)


def _scale_tokens(total: int, hours: Decimal, default_hours: Decimal) -> int:
    if default_hours <= 0:
        raise EstimateError("default duration must be positive")
    scaled = Decimal(total) * hours / default_hours
    return int(scaled.to_integral_value(rounding=ROUND_HALF_UP))


def _from_share(total_in: int, total_out: int, share: Decimal, cache_write: int) -> Charge:
    if min(total_in, total_out, cache_write) < 0:
        raise EstimateError("token assumptions must be >= 0")
    if share < 0 or share > 1:
        raise EstimateError("cache share must be between 0 and 1")
    cached = int((Decimal(total_in) * share).to_integral_value(rounding=ROUND_HALF_UP))
    if cached > total_in:
        raise EstimateError("cached input cannot exceed input")
    return Charge(total_in, total_out, cached, cache_write)


def _bill(cell: Cell, charge: Charge, rates: dict[str, Rate]) -> tuple[Decimal | None, str]:
    if cell.account not in API_ACCOUNTS:
        return None, "quota"
    rate = rates.get(cell.model)
    if rate is None:
        return None, "unpriced"
    return price_charge(charge, rate), "priced"


def _price(row: dict, key: str, model: str) -> Decimal:
    value = row.get(key)
    if type(value) is bool or not isinstance(value, (int, Decimal)):
        raise EstimateError(f"{model} {key} must be a non-negative number")
    number = Decimal(value)
    if number < 0:
        raise EstimateError(f"{model} {key} must be a non-negative number")
    return number


def _usd(value: Decimal | None, billing: str = "priced") -> str:
    if value is None:
        return "quota" if billing == "quota" else "unpriced"
    return f"{value.quantize(DISPLAY, rounding=ROUND_HALF_UP)}"


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
