import pathlib
import sys
import unittest

ROOT = pathlib.Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "tools" / "tui"))

from frame_to_svg import render


class FrameToSvgTests(unittest.TestCase):
    def test_named_story_frames_render_in_order_without_losing_a_story(self):
        first = "frame 3x1\np0 Rgb(1, 2, 3)/Reset\n00 symbols one\n00 colors  0-2:p0\n"
        second = first.replace("one", "two").replace("1, 2, 3", "4, 5, 6")
        rendered = render("first <story>\n" + first + "second story\n" + second)
        self.assertEqual(rendered, render("first <story>\n" + first + "second story\n" + second))
        self.assertIn("first &lt;story&gt;", rendered)
        self.assertLess(rendered.index(">one</text>"), rendered.index(">two</text>"))
        self.assertIn("#010203", rendered)
        self.assertIn("#040506", rendered)

    def test_process_activity_fixture_renders_every_named_frame(self):
        source = (ROOT / "bin/plak/tests/goldens/activity-processes.frame").read_text()
        rendered = render(source)
        self.assertEqual(rendered.count("<svg "), 25)
        self.assertIn("DefaultDark 48 hud", rendered)
        self.assertIn("DefaultDark 120 quit", rendered)

    def test_frame_svg_is_deterministic_and_escaped(self):
        frame = "frame 3x1\np0 Rgb(1, 2, 3)/Rgb(20, 22, 27)\n00 symbols <&>\n00 colors  0-2:p0\n"
        self.assertEqual(render(frame), render(frame))
        self.assertIn("&lt;&amp;&gt;", render(frame))
        self.assertIn("#010203", render(frame))


if __name__ == "__main__":
    unittest.main()
