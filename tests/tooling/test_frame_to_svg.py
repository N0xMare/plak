import pathlib
import sys
import unittest

ROOT = pathlib.Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "tools" / "tui"))

from frame_to_svg import render


class FrameToSvgTests(unittest.TestCase):
    def test_frame_svg_is_deterministic_and_escaped(self):
        frame = "frame 3x1\np0 Rgb(1, 2, 3)/Rgb(20, 22, 27)\n00 symbols <&>\n00 colors  0-2:p0\n"
        self.assertEqual(render(frame), render(frame))
        self.assertIn("&lt;&amp;&gt;", render(frame))
        self.assertIn("#010203", render(frame))


if __name__ == "__main__":
    unittest.main()
