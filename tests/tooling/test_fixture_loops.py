"""Fixture shell loops must carry a pass bound (spec 05 §1)."""

import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "tools/gate"))
import check_workspace as checker


class FixtureLoopTests(unittest.TestCase):
    def errors(self, source, path="src/tests.rs", package="library"):
        return checker.validate_fixture_loops(package, {path: source})

    def test_an_unbounded_sentinel_poll_is_refused(self):
        source = 'let c = "while [ ! -f finish ]; do sleep 0.01; done";'
        self.assertEqual(len(self.errors(source)), 1)

    def test_an_endless_loop_is_refused(self):
        source = 'let c = "trap \'\' TERM; while :; do sleep 1; done";'
        self.assertEqual(len(self.errors(source)), 1)

    def test_a_counted_loop_passes(self):
        source = (
            'let c = "n=0; while [ ! -f finish ] && [ -d \\"$PWD\\" ] '
            '&& [ $((n += 1)) -le 60000 ]; do sleep 0.01; done";'
        )
        self.assertEqual(self.errors(source), [])

    def test_a_guarded_counter_and_a_reader_pass(self):
        source = (
            'let a = "while [ $i -lt 4000 ]; do echo x; i=$((i+1)); done";\n'
            'let b = "while [ ! -f f ]; do w=$((w + 1)); [ \\"$w\\" -lt 1000 ] || exit 70; done";\n'
            'let c = "while IFS= read -r rest; do :; done";'
        )
        self.assertEqual(self.errors(source), [])

    def test_prose_and_comments_are_not_loops(self):
        source = '// while waiting, do nothing until done\nlet note = "polls while the job runs";'
        self.assertEqual(self.errors(source), [])

    def test_until_and_arithmetic_for_loops_are_checked(self):
        self.assertEqual(len(self.errors('let c = "until [ -f done.txt ]; do sleep 1; done";')), 1)
        self.assertEqual(len(self.errors('let c = "for ((;;)); do sleep 1; done";')), 1)
        bounded = (
            'let a = "n=0; until [ -f f ] || [ $((n += 1)) -gt 600 ]; do sleep 1; done";\n'
            'let b = "for ((i = 0; i < 10; i++)); do echo $i; done";\n'
            'let c = "for name in a b c; do echo $name; done";'
        )
        self.assertEqual(self.errors(bounded), [])

    def test_a_loop_split_across_literals_is_joined(self):
        concat = 'let c = concat!("while :; ", "do sleep 1; done");'
        argv = 'let c = ["sh", "-c", "while :;", "do sleep 1; done"];'
        self.assertEqual(len(self.errors(concat)), 1)
        self.assertEqual(len(self.errors(argv)), 1)

    def test_a_read_loop_needs_an_input_that_ends(self):
        self.assertEqual(len(self.errors('let c = "tail -f log | while read l; do :; done";')), 1)
        self.assertEqual(len(self.errors('let c = "yes | while read l; do :; done";')), 1)
        ends = (
            'let a = "printf x | while read l; do :; done";\n'
            'let b = "while read -r l; do :; done < input";'
        )
        self.assertEqual(self.errors(ends), [])

    def test_a_comparison_bounds_only_a_stepped_counter(self):
        size = 'let c = "while [ $(wc -c < f) -lt 100 ]; do sleep 1; done";'
        unstepped = 'let c = "while [ $i -lt 100 ]; do sleep 1; done";'
        stepped_elsewhere = 'let c = "while [ $i -lt 100 ]; do j=$((j + 1)); done";'
        for source in (size, unstepped, stepped_elsewhere):
            self.assertEqual(len(self.errors(source)), 1, source)

    def test_an_upper_guard_in_the_body_bounds_the_loop(self):
        source = (
            'let c = "while :; do n=$((n + 1)); [ \\"$n\\" -ge 100 ] && exit 1; sleep 1; done";'
        )
        self.assertEqual(self.errors(source), [])
        wrong_way = 'let c = "while :; do n=$((n + 1)); [ \\"$n\\" -ge 100 ] || exit 1; done";'
        self.assertEqual(len(self.errors(wrong_way)), 1)

    def test_an_unbounded_inner_loop_is_refused_inside_a_bounded_one(self):
        source = 'let c = "while [ $((n += 1)) -le 9 ]; do while :; do sleep 1; done; done";'
        self.assertEqual(len(self.errors(source)), 1)

    def test_loops_in_inline_test_modules_are_checked(self):
        source = (
            "fn product() {}\n"
            "#[cfg(test)]\n"
            "mod tests {\n"
            '    const C: &str = "while :; do sleep 1; done";\n'
            "}\n"
        )
        self.assertEqual(len(self.errors(source, "src/lib.rs")), 1)

    def test_a_fixture_sleep_is_at_most_600_seconds(self):
        for source in (
            'let c = "sleep 3600";',
            'let c = "exec sleep 11m";',
            'let c = "sleep infinity";',
            'let c = ["sleep", "601"];',
        ):
            self.assertEqual(len(self.errors(source)), 1, source)
        self.assertEqual(self.errors('let c = "exec sleep 600; sleep 0.5; sleep 10s";'), [])

    def test_prose_that_reads_like_a_loop_is_not_one(self):
        source = 'let s = "failed while waiting; do not retry until the job is done";'
        self.assertEqual(self.errors(source), [])

    def test_displayed_shell_is_exempt(self):
        source = 'const SAMPLE: &str = "while :; do sleep 0.1; done";'
        self.assertEqual(self.errors(source, "src/tui/highlight.rs", "plak"), [])


if __name__ == "__main__":
    unittest.main()
