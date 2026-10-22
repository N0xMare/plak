"""Task-weighted paired estimator with deterministic two-stage bootstrap."""

from __future__ import annotations

import itertools
import math
import random
import statistics
from collections import defaultdict
from typing import Any

from harbor_adapter.pricing import resolve_model_key


def estimate(tasks: dict[str, tuple[list[bool], list[bool]]], *, replicates: int = 10_000,
             seed: int = 0) -> dict[str, Any]:
    included = {task: values for task, values in sorted(tasks.items())
                if min(map(len, values)) >= 3}
    dropped = sorted(set(tasks) - set(included))
    if not included:
        return {"tasks": 0, "difference": None, "dropped_tasks": dropped,
                "task_population_ci95": None, "fixed_tasks_ci95": None}
    if replicates < 100:
        raise ValueError("bootstrap needs at least 100 replicates")
    rng = random.Random(seed)
    values = list(included.values())
    difference = statistics.mean(statistics.mean(a) - statistics.mean(b) for a, b in values)
    population, fixed = [], []
    for _ in range(replicates):
        for samples, resample_tasks in ((population, True), (fixed, False)):
            selected = rng.choices(values, k=len(values)) if resample_tasks else values
            samples.append(statistics.mean(
                statistics.mean(rng.choices(a, k=len(a))) - statistics.mean(rng.choices(b, k=len(b)))
                for a, b in selected
            ))
    def interval(samples):
        samples.sort()
        def percentile(q):
            index = (len(samples) - 1) * q
            lower = int(index)
            upper = min(lower + 1, len(samples) - 1)
            return samples[lower] + (samples[upper] - samples[lower]) * (index - lower)
        return [percentile(.025), percentile(.975)]
    permutation_extreme = 0
    for _ in range(replicates):
        differences = []
        for a, b in values:
            pooled = list(a) + list(b)
            rng.shuffle(pooled)
            differences.append(statistics.mean(pooled[:len(a)]) - statistics.mean(pooled[len(a):]))
        permutation_extreme += abs(statistics.mean(differences)) >= abs(difference) - 1e-12
    left_only = sum(statistics.mean(a) > .5 >= statistics.mean(b) for a, b in values)
    right_only = sum(statistics.mean(b) > .5 >= statistics.mean(a) for a, b in values)
    discordant = left_only + right_only
    mcnemar = min(1, 2 * sum(math.comb(discordant, i) for i in range(min(left_only, right_only) + 1)) /
                  2 ** discordant) if discordant else 1.0
    population.sort()
    ci90 = [population[int((len(population) - 1) * q)] for q in (.05, .95)]
    return {"tasks": len(included), "difference": difference, "dropped_tasks": dropped,
            "task_population_ci95": interval(population), "fixed_tasks_ci95": interval(fixed),
            "replicates": replicates, "seed": seed,
            "permutation_p": (permutation_extreme + 1) / (replicates + 1),
            "mcnemar_majority_p": mcnemar,
            "task_population_ci90": ci90,
            "equivalent_within_10pp": ci90[0] > -.10 and ci90[1] < .10,
            "per_task": {task: statistics.mean(a) - statistics.mean(b)
                         for task, (a, b) in included.items()}}


def paired_reports(rows: list[dict[str, Any]], pairs: list[dict[str, Any]]) -> list[dict[str, Any]]:
    eligible = {(p["slice"], p["cohort"], p["repeat_block"], p["task"])
                for p in pairs if p["comparison_eligible"]}
    families = defaultdict(lambda: defaultdict(lambda: defaultdict(dict)))
    for row in rows:
        if (row["slice"], row["cohort"], row["repeat_block"], row["task"]) not in eligible:
            continue
        context = row["comparison_context"] or {}
        family = (row.get("campaign_sha256"), row["slice"], row["cohort"], context.get("arch"),
                  context.get("account"), resolve_model_key(context.get("model") or "") or context.get("model"), context.get("effort"))
        slot = row.get("selection_slot", row.get("repeat_block"))
        if slot is None or row["repair_of"]:
            continue
        families[family][row["task"]][row["arm"]][slot] = row
    results = []
    for family, tasks in sorted(families.items(), key=str):
        arms = sorted({arm for groups in tasks.values() for arm in groups})
        for left, right in itertools.combinations(arms, 2):
            primary, sensitivity = {}, {}
            for task, groups in tasks.items():
                a, b = groups.get(left, {}), groups.get(right, {})
                slots = sorted(set(a) & set(b), key=str)
                slots = [s for s in slots if a[s]["eligible_pass_rate"] and b[s]["eligible_pass_rate"]]
                primary[task] = ([a[s]["pass"] for s in slots], [b[s]["pass"] for s in slots])
                no_provider = [s for s in slots if not any(
                    row["termination"].startswith("turn_failed:provider") for row in (a[s], b[s]))]
                sensitivity[task] = ([a[s]["pass"] for s in no_provider], [b[s]["pass"] for s in no_provider])
            results.append({"family": list(family), "arms": [left, right],
                            "primary": estimate(primary), "exclude_provider_failures": estimate(sensitivity)})
    # Correct the predeclared pair family, never task-level diagnostic tests.
    tested = sorted((result for result in results if result["primary"].get("permutation_p") is not None),
                    key=lambda result: result["primary"]["permutation_p"])
    adjusted = 0.0
    for index, result in enumerate(tested):
        adjusted = max(adjusted, min(1.0, (len(tested) - index) * result["primary"]["permutation_p"]))
        result["holm_permutation_p"] = adjusted
    return results
