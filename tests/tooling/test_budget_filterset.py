"""The serial budget lane and the durability lane are well formed.

`scripts/dev-check.sh` and the `just test` recipe draw both lanes from
`budget_filterset.py`. `just test` runs three invocations: everything except
the budget and durability lanes, then the budget lane alone, then the
durability lane alone. `just check` runs only the first two. Properties a
careless edit breaks silently:

1. the three filtersets partition the suite, and the budget lane union the
   durability lane is exactly the pinned serial inventory;
2. every test named in either lane still exists in the tree;
3. `default` and `offline` carry the same lanes.
"""

import importlib.util
import re
import sys
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "tools/checks"))
SPEC = importlib.util.spec_from_file_location(
    "budget_filterset", ROOT / "tools/checks/budget_filterset.py"
)
assert SPEC and SPEC.loader
BUDGET = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = BUDGET
SPEC.loader.exec_module(BUDGET)

SOURCE_ROOTS = ("crates", "bin", "tests")

NEXTEST_INVOCATION = re.compile(r"cargo nextest run\b[^\n]*")
FILTERSET_ARGUMENT = re.compile(r"""-E\s+(?:"([^"]*)"|'([^']*)')""")
LITERAL_ASSIGNMENT = re.compile(r"""^\s*([A-Za-z_][A-Za-z0-9_]*)='([^']*)'\s*$""", re.MULTILINE)
SHELL_VARIABLE = re.compile(r"\$\{?([A-Za-z_][A-Za-z0-9_]*)\}?")
# The only command substitutions the gates are allowed to interpolate.
COMMAND_SUBSTITUTIONS = {
    "$(python3 tools/checks/budget_filterset.py --exclude)": BUDGET.exclude_filterset,
    "$(python3 tools/checks/budget_filterset.py --durability)": BUDGET.durability_filterset,
    "$(python3 tools/checks/budget_filterset.py)": BUDGET.budget_filterset,
}
SUBSTITUTION_ASSIGNMENT = re.compile(
    r"""^\s*([A-Za-z_][A-Za-z0-9_]*)="(\$\(python3 tools/checks/budget_filterset\.py[^)]*\))"\s*$""",
    re.MULTILINE,
)


def _shell_sources() -> dict[str, str]:
    return {
        "scripts/dev-check.sh": (ROOT / "scripts/dev-check.sh").read_text(),
        "Justfile": (ROOT / "Justfile").read_text(),
    }


def _offline_invocations(text: str) -> list[str]:
    # Fold shell line continuations so one invocation is one line.
    flattened = re.sub(r"\\\n\s*", " ", text)
    return [
        " ".join(match.group(0).split())
        for match in NEXTEST_INVOCATION.finditer(flattened)
        if "--profile offline" in match.group(0)
    ]


def _bindings(text: str) -> dict[str, str]:
    """Resolve the handful of shell bindings the gates use in their filtersets."""
    bindings = {name: value for name, value in LITERAL_ASSIGNMENT.findall(text)}
    # The runner prefixes both lanes with its optional changed/fast scope.
    # Coverage here asserts the unscoped check/full complement.
    bindings["scope_prefix"] = ""
    for name, substitution in SUBSTITUTION_ASSIGNMENT.findall(text):
        bindings[name] = COMMAND_SUBSTITUTIONS[substitution]()
    return bindings


def _expand(expression: str, bindings: dict[str, str]) -> str:
    for substitution, resolve in COMMAND_SUBSTITUTIONS.items():
        expression = expression.replace(substitution, resolve())
    expression = SHELL_VARIABLE.sub(lambda m: bindings.get(m.group(1), m.group(0)), expression)
    return " ".join(expression.split())


def _filterset(invocation: str, text: str) -> str:
    match = FILTERSET_ARGUMENT.search(invocation)
    assert match, f"no -E filterset in {invocation!r}"
    raw = match.group(1) if match.group(1) is not None else match.group(2)
    return _expand(raw, _bindings(text))



# The serial lane before the durability split, plus enrolled durability tests.
# Moving a predicate is allowed; adding or dropping one updates this inventory.
SERIAL_PREDICATES = (
    "a_stuck_worker_request_is_killed_and_reported_as_session_loss",
    "product_terminal_e2e_tests::",
    "real_composition_quit_releases_nested_process",
    "real_process_quit_releases_and_resumes_exact_session",
    "real_process_jsonl_quit_releases_and_resumes_exact_session",
    "real_promoted_job_quit_releases_and_resumes_exact_session",
    "tui_exit_bound_idle_and_stalled_turn",
    "tui_local_launch_budget_new_and_resume_with_network_faults",
    "a_reload_onto_the_same_state_key_resumes_exactly",
    "tui_shutdown_deadlines_are_at_most_one_second",
    "interactive_exit_batches_real_root_and_child_leases_without_settling",
    "thinking_settle_tests::",
    "quit_projection_stages_share_the_original_deadline",
    "interactive_observation_timeout_releases_gate_and_joins_without_masking_failure",
    "failed_interactive_publication_recovers_in_place_on_the_next_prompt",
    "fresh_real_binary_shell_keeps_controls_and_draft_usable",
    "deferred_startup_never_claims_attachment_before_failed_open_falls_back",
    "crash_sweep::durability_subset",
    "actual_host_sigkill_relaunch_matrix_uses_real_handlers_and_reconcilers",
    "gc_after_a_compaction_cut_before_projection_prunes_at_the_compacted_head",
    "sqlite_paid_compaction_sigkill_reopens_exactly_once",
    "sqlite_no_locator_paid_compaction_sigkill_is_never_redispatched",
    "exec_command_with_zero_yield_returns_the_joined_output",
    "host_process_bounds_live_output_and_enforces_timeout",
    "joined_process_pty_cancellation_and_timeout_join_the_group",
    "cancel_during_a_cell_running_a_real_process_command_joins_inside_the_settle_window",
    "terminal_status_without_joined_owner_is_not_durable_proof",
    "contended_operation_does_not_stall_joined_proof_probe",
    "admitted_exchange_deadline_is_conservative_and_joins_work",
    "flushed_request_head_deadline_is_precise_and_joins_work",
    "stalled_request_write_after_body_poll_keeps_write_deadline",
    "slow_allowed_write_does_not_spend_response_head_budget",
    "asymmetric_write_and_head_timeouts_are_sequential",
    "cancelling_claimed_refresh_after_request_observation_restores_ready",
    "completed_terminal_outcome_remains_authoritative_when_exit_was_requested",
    "scheduled_compaction_failure_keeps_host_worker_alive",
    "interrupt_then_quit_publishes_the_cancelled_terminal",
    "quit_during_active_turn_waits_for_cancelled_terminal_projection",
    "product_session_facade_create_turn_shutdown_and_reopen",
    "retiring_a_live_child_advances_its_session_locator",
    "a_held_lease_is_refused_as_recovery_lease_held",
    "real_session_late_start_ack_releases_after_deadline_without_redispatch",
    "complete_run_is_one_exact_new_state_and_probe_is_inert",
    "every_runtime_commit_cut_reopens_without_redispatch",
    "bounded_async_sigkill_campaign_accepts_only_exact_old_or_new_commit",
    "sigkill_gc_cuts_preserve_queue_counters_and_oracle",
    "observation_parity_tests::",
    "local_browser_rejects_wrong_host_without_dispatch_and_returns_no_store",
    "terminal_utf8_larger_than_observation_bound_returns_without_deadline_spin",
    "terminal_hint_drains_more_than_one_record_batch_without_waiting",
    "contended_observation_does_not_block_the_async_worker",
    "promoted_short_job_is_retired_without_observer_polling",
    "promoted_monitor_exits_on_owner_drop_without_terminal_publication",
    "process_await_wait_does_not_reserve_a_blocking_worker",
    "process_session_missing_cwd_is_correctable_before_pipes_or_pty_spawn",
    "canonical_process_operations_close_poll_terminate_and_reject_stale_authority",
    "process_session_executor_rejects_stale_poll_cursor_as_model_failure",
    "running_children_still_count_against_the_session_limit",
    "host_cancel_and_future_drop_join_the_owned_process_group",
    "joined_process_sessions_join_cancelled_group",
    "a_slow_jsonl_reader_does_not_make_the_writer_fail_the_stream",
    "paging_to_the_head_of_a_long_transcript_and_back_moves_on_every_key",
    "control_center_chords_switch_tabs_edit_rows_search_scroll_and_copy",
    "the_launch_picker_lists_earlier_sessions_and_reopens_the_chosen_one",
    "durability::",
)


class BudgetFiltersetTests(unittest.TestCase):
    def test_durability_subset_keeps_all_four_shapes_with_bounded_workers(self) -> None:
        sweep = ROOT / "bin/plak/tests/integration/crash_sweep"
        source = (sweep / "mod.rs").read_text()
        self.assertRegex(source, r'fn durability_subset\(\)\s*\{\s*let jobs = std::thread::available_parallelism\(\)\.unwrap\(\)\.get\(\)\.min\(16\);\s*run\(&shapes::names\(true\), jobs, 1, false, "subset"\);')
        shapes = re.findall(r'name: "([a-z0-9_]+)",\s*subset: true,',
                            (sweep / "shapes.rs").read_text())
        self.assertEqual(shapes, ["final_only", "direct_tool_then_final",
                                  "promoted_background_job", "sealed_family_artifacts"])
        self.assertIn("crash_sweep::durability_subset", BUDGET.durability_test_names())

    def test_barrier_synchronized_native_lease_test_is_not_a_budget(self) -> None:
        self.assertNotIn(
            "native_ready_lease_is_held_through_head_and_released_before_stream_body",
            BUDGET.budget_test_names(),
        )

    def test_lane_is_non_empty_and_has_no_duplicates(self) -> None:
        names = BUDGET.budget_test_names()
        self.assertGreater(len(names), 0)
        self.assertEqual(len(names), len(set(names)), "duplicate predicate in the lane")
        for name in names:
            self.assertRegex(
                name,
                r"^[a-z0-9_]+(::)?$",
                f"{name!r} is neither a bare test name nor a `<module>::` predicate",
            )

    def test_exclude_is_the_complement_of_budget_and_durability(self) -> None:
        union = f"{BUDGET.budget_filterset()} | {BUDGET.durability_filterset()}"
        self.assertEqual(BUDGET.exclude_filterset(), f"not ({union})")

    def test_default_and_offline_profiles_carry_the_same_lane(self) -> None:
        self.assertEqual(BUDGET.budget_filterset("default"), BUDGET.budget_filterset("offline"))
        self.assertEqual(BUDGET.durability_filterset("default"), BUDGET.durability_filterset("offline"))
        for getter in (BUDGET.budget_override, BUDGET.durability_override):
            default_override = getter("default")
            offline_override = getter("offline")
            self.assertEqual(default_override["threads-required"], "num-test-threads")
            self.assertEqual(default_override["slow-timeout"], offline_override["slow-timeout"])
            self.assertEqual(offline_override["slow-timeout"]["period"], "240s")
            self.assertNotIn("test-group", default_override)

    def test_cli_contract_serialization_is_scoped_to_its_integration_module(self) -> None:
        for profile in ("default", "offline"):
            with self.subTest(profile=profile):
                filters = [
                    override["filter"]
                    for override in BUDGET._overrides(profile)
                    if override.get("test-group") == "cli-contract-serial"
                ]
                self.assertEqual(
                    filters,
                    ["package(=plak) & binary(=integration) & test(/^cli_contract::/)"],
                )

    def test_every_lane_test_exists_in_the_tree(self) -> None:
        functions: set[str] = set()
        modules: set[str] = set()
        names = [*BUDGET.budget_test_names(), *BUDGET.durability_test_names()]
        qualified = {name for name in names if "::" in name and not name.endswith("::")}
        qualified_functions: set[str] = set()
        for root in SOURCE_ROOTS:
            for path in (ROOT / root).rglob("*.rs"):
                text = path.read_text()
                file_functions = set(re.findall(r"\bfn\s+([a-z0-9_]+)\s*\(", text))
                functions.update(file_functions)
                modules.update(re.findall(r"^\s*(?:pub(?:\([^)]*\))?\s+)?mod\s+([a-z0-9_]+)\s*[;{]", text, re.MULTILINE))
                module_path = path.parent.parts if path.stem in {"main", "mod"} else path.with_suffix("").parts
                for name in qualified:
                    prefix, function = name.rsplit("::", 1)
                    parts = tuple(prefix.split("::"))
                    if module_path[-len(parts):] == parts and function in file_functions:
                        qualified_functions.add(name)
        missing = []
        for name in names:
            # Qualified functions must be defined in the named source module,
            # not merely share a bare function name elsewhere in the tree.
            if name.endswith("::"):
                if name[:-2] not in modules:
                    missing.append(name)
            elif "::" in name:
                if name not in qualified_functions:
                    missing.append(name)
            elif name not in functions:
                missing.append(name)
        self.assertEqual(
            missing, [], f"lane predicates with no `fn`/`mod` definition under {SOURCE_ROOTS}"
        )

    def test_the_pty_module_predicate_is_in_the_lane(self) -> None:
        # The whole real-PTY end-to-end module is a budget family; naming
        # individual members of it invites the same load flake back.
        self.assertIn("product_terminal_e2e_tests::", BUDGET.budget_test_names())

    def test_real_process_cancel_settle_window_is_serial(self) -> None:
        self.assertIn(
            "cancel_during_a_cell_running_a_real_process_command_joins_inside_the_settle_window",
            BUDGET.budget_test_names(),
        )

    def test_stuck_python_worker_request_is_serial(self) -> None:
        self.assertIn(
            "a_stuck_worker_request_is_killed_and_reported_as_session_loss",
            BUDGET.budget_test_names(),
        )

    def test_gates_partition_the_offline_suite_across_three_invocations(self) -> None:
        # A wrapping pair of parentheses leaves room for the fast tier's prefix.
        expected = {
            "parallel": {BUDGET.exclude_filterset(), f"({BUDGET.exclude_filterset()})"},
            "budget": {BUDGET.budget_filterset(), f"({BUDGET.budget_filterset()})"},
            "durability": {BUDGET.durability_filterset(), f"({BUDGET.durability_filterset()})"},
        }
        for source, text in _shell_sources().items():
            with self.subTest(source=source):
                invocations = _offline_invocations(text)
                self.assertEqual(len(invocations), 3, source)
                kinds = []
                for invocation in invocations:
                    filt = _filterset(invocation, text)
                    matched = [name for name, options in expected.items() if filt in options]
                    self.assertEqual(matched, [matched[0]] if matched else [], (source, filt))
                    self.assertEqual(len(matched), 1, (source, invocation, filt))
                    kinds.append(matched[0])
                    if matched[0] == "parallel":
                        self.assertNotIn("--test-threads", invocation, source)
                    else:
                        self.assertIn("--test-threads=1", invocation, source)
                self.assertEqual(kinds, ["parallel", "budget", "durability"], source)

    def test_gates_do_not_restate_the_lane_inline(self) -> None:
        for source, text in _shell_sources().items():
            with self.subTest(source=source):
                for invocation in _offline_invocations(text):
                    match = FILTERSET_ARGUMENT.search(invocation)
                    assert match
                    raw = match.group(1) if match.group(1) is not None else match.group(2)
                    self.assertNotIn(
                        "test(",
                        raw,
                        f"{source}: the lane must come from budget_filterset.py, not inline",
                    )



    def test_budget_and_durability_partition_the_serial_inventory(self) -> None:
        budget = BUDGET.budget_test_names()
        durability = BUDGET.durability_test_names()
        self.assertEqual(set(budget) & set(durability), set())
        self.assertEqual(set(budget) | set(durability), set(SERIAL_PREDICATES))
        self.assertEqual(len(budget) + len(durability), len(SERIAL_PREDICATES))
        self.assertEqual([name for name in SERIAL_PREDICATES if name in set(budget)], budget)
        self.assertEqual(
            [name for name in SERIAL_PREDICATES if name in set(durability)],
            durability,
        )
        self.assertEqual(durability, [
            "crash_sweep::durability_subset",
            "actual_host_sigkill_relaunch_matrix_uses_real_handlers_and_reconcilers",
            "gc_after_a_compaction_cut_before_projection_prunes_at_the_compacted_head",
            "sqlite_paid_compaction_sigkill_reopens_exactly_once",
            "sqlite_no_locator_paid_compaction_sigkill_is_never_redispatched",
            "complete_run_is_one_exact_new_state_and_probe_is_inert",
            "every_runtime_commit_cut_reopens_without_redispatch",
            "bounded_async_sigkill_campaign_accepts_only_exact_old_or_new_commit",
    "sigkill_gc_cuts_preserve_queue_counters_and_oracle",
        ])
        # Product quit/resume scenarios stay wall-clock budgets, not crash matrices.
        self.assertIn("durability::", budget)
        self.assertNotIn("durability::", durability)

if __name__ == "__main__":
    unittest.main()
