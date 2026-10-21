"""Compiled expiry packaging checks are deterministic and fail closed."""

import importlib.util
import contextlib
import io
from pathlib import Path
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location("review_expiry", ROOT / "tools/release/review_expiry.py")
assert SPEC is not None and SPEC.loader is not None
review_expiry = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(review_expiry)


class ReviewExpiryTests(unittest.TestCase):
    def test_exactly_seven_compiled_surfaces(self):
        reviews = review_expiry.compiled_reviews(ROOT)
        self.assertEqual(len(reviews), 7)
        self.assertEqual(len(dict(reviews)), 7)
        self.assertTrue(all(expiry > 0 for _, expiry in reviews))

    def test_threshold_is_inclusive_and_expired_reviews_are_refused(self):
        now = 100
        boundary = now + 21 * review_expiry.DAY
        reviews = [("expired", now - 1), ("at", boundary), ("after", boundary + 1)]
        self.assertEqual(review_expiry.expiring(reviews, now), reviews[:2])

    def test_each_surface_is_independently_checked_without_wall_time(self):
        for surface, expiry in review_expiry.compiled_reviews(ROOT):
            with self.subTest(surface=surface):
                self.assertEqual(review_expiry.expiring([(surface, expiry)], expiry - 21 * review_expiry.DAY - 1), [])
                self.assertEqual(review_expiry.expiring([(surface, expiry)], expiry), [(surface, expiry)])

    def test_derived_expiry_and_profile_field(self):
        source = "const REVIEWED: u64 = 1_000; const EXPIRES: u64 = REVIEWED + 30 * 86_400;"
        self.assertEqual(review_expiry.compiled_expiry(source, "EXPIRES"), 2_593_000)
        self.assertEqual(review_expiry.compiled_expiry(source + "expires_at_unix: EXPIRES - 1,", "expires_at_unix"), 2_592_999)

    def test_source_drift_fails_closed(self):
        for source in ["", "expires_at_unix: unknown,", "expires_at_unix: 1, expires_at_unix: 2,", "const EXPIRES: u64 = clock();", "const EXPIRES: u64 = EXPIRES;", "const EXPIRES: u64 = 0 - 1;"]:
            with self.subTest(source=source), self.assertRaises(ValueError):
                review_expiry.compiled_expiry(source, "EXPIRES" if "const" in source else "expires_at_unix")

    def test_command_names_the_refused_surface_and_utc_expiry(self):
        output = io.StringIO()
        with patch.object(review_expiry, "compiled_reviews", return_value=[("test surface", 0)]), patch.object(review_expiry.time, "time", return_value=0), contextlib.redirect_stderr(output):
            self.assertEqual(review_expiry.main(), 1)
        self.assertIn("test surface review expires 1970-01-01T00:00:00Z", output.getvalue())

    def test_command_refuses_unreadable_compiled_sources(self):
        with patch.object(review_expiry, "compiled_reviews", side_effect=ValueError("missing pin")), contextlib.redirect_stderr(io.StringIO()):
            self.assertEqual(review_expiry.main(), 1)

    def test_qualification_note_never_fails_on_time_or_source_drift(self):
        output = io.StringIO()
        with patch.object(review_expiry, "compiled_reviews", return_value=[("expired", 0)]), contextlib.redirect_stdout(output):
            self.assertEqual(review_expiry.main(now=86_400, note=True), 0)
        self.assertIn("-1 days remaining (informational)", output.getvalue())
        with patch.object(review_expiry, "compiled_reviews", side_effect=ValueError("missing pin")), contextlib.redirect_stderr(io.StringIO()):
            self.assertEqual(review_expiry.main(now=0, note=True), 0)


if __name__ == "__main__":
    unittest.main()
