"""Compiled review-window packaging checks are deterministic and fail closed."""

import importlib.util
import contextlib
import io
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location("review_expiry", ROOT / "tools/release/review_expiry.py")
assert SPEC is not None and SPEC.loader is not None
review_expiry = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(review_expiry)
DAY = review_expiry.DAY


class ReviewExpiryTests(unittest.TestCase):
    def test_exactly_seven_compiled_surfaces_in_two_classes(self):
        reviews = review_expiry.compiled_reviews(ROOT)
        self.assertEqual(len(reviews), 7)
        self.assertEqual(len({surface for surface, *_ in reviews}), 7)
        classes = [review_class for _, review_class, _, _ in reviews]
        self.assertEqual(classes.count(review_expiry.HARD_EXPIRY), 3)
        self.assertEqual(classes.count(review_expiry.REVIEW_DUE), 4)

    def test_every_surface_has_a_ninety_day_window_ending_at_midnight_utc(self):
        for surface, _, reviewed, deadline in review_expiry.compiled_reviews(ROOT):
            with self.subTest(surface=surface):
                self.assertEqual(deadline - reviewed, 90 * DAY)
                self.assertEqual(deadline % DAY, 0)

    def test_floor_is_a_third_of_the_window_and_refuses_at_equality(self):
        reviewed, deadline = 1_000, 1_000 + 90 * DAY
        review = ("surface", review_expiry.HARD_EXPIRY, reviewed, deadline)
        boundary = deadline - 30 * DAY
        self.assertEqual(review_expiry.refused([review], boundary - 1), [])
        self.assertEqual(review_expiry.refused([review], boundary), [review])
        self.assertEqual(review_expiry.refused([review], deadline + 1), [review])
        odd = ("odd", review_expiry.REVIEW_DUE, 0, 10)
        self.assertEqual(review_expiry.refused([odd], 6), [])
        self.assertEqual(review_expiry.refused([odd], 7), [odd])

    def test_each_surface_is_independently_checked_without_wall_time(self):
        for review in review_expiry.compiled_reviews(ROOT):
            surface, _, reviewed, deadline = review
            floor = review_expiry.floor_seconds(reviewed, deadline)
            with self.subTest(surface=surface):
                self.assertEqual(review_expiry.refused([review], deadline - floor - 1), [])
                self.assertEqual(review_expiry.refused([review], deadline - floor), [review])

    def test_constants_resolve_through_the_shared_auth_window(self):
        with tempfile.TemporaryDirectory() as scratch:
            root = Path(scratch)
            auth = root / review_expiry.WINDOW_CONSTANTS
            auth.parent.mkdir(parents=True)
            auth.write_text("pub const DAY_SECONDS: u64 = 86_400;\n"
                            "pub const SUBSCRIPTION_REVIEW_WINDOW_SECONDS: u64 = 90 * DAY_SECONDS;\n")
            for _, path, reviewed, deadline, _ in review_expiry.SURFACES:
                source = root / "crates/providers" / path
                source.parent.mkdir(parents=True, exist_ok=True)
                source.write_text(f"const {reviewed}: u64 = 1_000;\n"
                                  f"const {deadline}: u64 = {reviewed} + SUBSCRIPTION_REVIEW_WINDOW_SECONDS;\n"
                                  if reviewed.isupper() else
                                  "const REVIEWED: u64 = 1_000;\n"
                                  f"{reviewed}: REVIEWED,\n{deadline}: REVIEWED + SUBSCRIPTION_REVIEW_WINDOW_SECONDS,\n")
            reviews = review_expiry.compiled_reviews(root)
            self.assertEqual({(reviewed, deadline) for *_, reviewed, deadline in reviews}, {(1_000, 1_000 + 90 * DAY)})
            auth.write_text("")
            with self.assertRaises(ValueError):
                review_expiry.compiled_reviews(root)
            auth.unlink()
            with self.assertRaises(OSError):
                review_expiry.compiled_reviews(root)

    def test_derived_values_and_profile_fields(self):
        source = "const REVIEWED: u64 = 1_000; const EXPIRES: u64 = REVIEWED + 30 * 86_400;"
        self.assertEqual(review_expiry.compiled_value(source, "EXPIRES"), 2_593_000)
        self.assertEqual(review_expiry.compiled_value(source + "expires_at_unix: EXPIRES - 1,", "expires_at_unix"), 2_592_999)
        shared = {"WINDOW": "2 * 86_400"}
        self.assertEqual(review_expiry.compiled_value("const DUE: u64 = 1 + WINDOW;", "DUE", shared), 172_801)
        self.assertEqual(review_expiry.compiled_value("const DUE: u64 = 1 + crate_a::pins::WINDOW;", "DUE", shared), 172_801)
        with self.assertRaises(ValueError):
            review_expiry.compiled_value("#[cfg(test)]\nconst DUE: u64 = 1;", "DUE")

    def test_source_drift_fails_closed(self):
        for source in ["", "expires_at_unix: unknown,", "expires_at_unix: 1, expires_at_unix: 2,", "const EXPIRES: u64 = clock();",
                       "const EXPIRES: u64 = EXPIRES;", "const EXPIRES: u64 = 0 - 1;", "const EXPIRES: u64 = a::B;"]:
            with self.subTest(source=source), self.assertRaises((ValueError, SyntaxError)):
                review_expiry.compiled_value(source, "EXPIRES" if "const" in source else "expires_at_unix")

    def test_command_names_the_refused_surface_class_date_and_floor(self):
        reviews = [("test surface", review_expiry.HARD_EXPIRY, 0, 90 * DAY),
                   ("test api", review_expiry.REVIEW_DUE, 0, 90 * DAY)]
        output = io.StringIO()
        with patch.object(review_expiry, "compiled_reviews", return_value=reviews), \
                patch.object(review_expiry.time, "time", return_value=60 * DAY), contextlib.redirect_stderr(output):
            self.assertEqual(review_expiry.main(), 1)
        self.assertIn("publish: test surface review expires 1970-04-01T00:00:00Z; require more than 30 days "
                      "(a third of the 90-day window) remaining", output.getvalue())
        self.assertIn("publish: test api review is due 1970-04-01T00:00:00Z", output.getvalue())
        with patch.object(review_expiry, "compiled_reviews", return_value=reviews), contextlib.redirect_stderr(io.StringIO()):
            self.assertEqual(review_expiry.main(now=60 * DAY - 1), 0)

    def test_command_refuses_unreadable_compiled_sources(self):
        with patch.object(review_expiry, "compiled_reviews", side_effect=ValueError("missing pin")), contextlib.redirect_stderr(io.StringIO()):
            self.assertEqual(review_expiry.main(), 1)

    def test_qualification_note_never_fails_on_time_or_source_drift(self):
        output = io.StringIO()
        with patch.object(review_expiry, "compiled_reviews", return_value=[("expired", review_expiry.HARD_EXPIRY, 0, 90 * DAY)]), \
                contextlib.redirect_stdout(output):
            self.assertEqual(review_expiry.main(now=91 * DAY, note=True), 0)
        self.assertIn("-1 days remaining; publication requires more than 30 (informational)", output.getvalue())
        with patch.object(review_expiry, "compiled_reviews", side_effect=ValueError("missing pin")), contextlib.redirect_stderr(io.StringIO()):
            self.assertEqual(review_expiry.main(now=0, note=True), 0)


if __name__ == "__main__":
    unittest.main()
