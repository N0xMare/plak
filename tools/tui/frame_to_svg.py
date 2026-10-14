#!/usr/bin/env python3
"""Render Ratatui .frame goldens to dependency-free deterministic SVG."""
from __future__ import annotations

import argparse
import html
import re
from pathlib import Path

DEFAULT_FOREGROUND = "#f0f2f8"
DEFAULT_BACKGROUND = "#14161b"
CELL_WIDTH = 9


def svg_color(value: str, default: str) -> str:
    if value == "Reset":
        return default
    match = re.fullmatch(r"Rgb\((\d+), (\d+), (\d+)\)", value)
    if not match:
        raise ValueError(f"unsupported frame color {value!r}")
    channels = [int(channel) for channel in match.groups()]
    if any(channel > 255 for channel in channels):
        raise ValueError(f"invalid frame color {value!r}")
    return "#" + "".join(f"{channel:02x}" for channel in channels)


def render(source: str) -> str:
    lines = source.splitlines()
    match = re.fullmatch(r"frame (\d+)x(\d+)", lines[0])
    if not match:
        raise ValueError("invalid frame header")
    width, height = map(int, match.groups())
    rows: dict[int, str] = {}
    colors: dict[int, list[tuple[int, int, str]]] = {}
    palette: dict[str, tuple[str, str]] = {}
    for line in lines[1:]:
        found = re.fullmatch(r"(p\d+) ([^/]+)/(.+)", line)
        if found:
            palette[found.group(1)] = (
                svg_color(found.group(2), DEFAULT_FOREGROUND),
                svg_color(found.group(3), DEFAULT_BACKGROUND),
            )
            continue
        found = re.fullmatch(r"(\d+) symbols (.*)", line)
        if found:
            rows[int(found.group(1))] = found.group(2).replace("·", " ")
            continue
        found = re.fullmatch(r"(\d+) colors  (.*)", line)
        if found:
            spans = []
            for item in found.group(2).split(", "):
                span = re.fullmatch(r"(\d+)-(\d+):(p\d+)", item)
                if not span:
                    raise ValueError("invalid color span")
                spans.append((int(span.group(1)), int(span.group(2)), span.group(3)))
            colors[int(found.group(1))] = spans
    if set(rows) != set(range(height)) or any(len(row) != width for row in rows.values()):
        raise ValueError("incomplete frame")
    if set(colors) != set(range(height)):
        raise ValueError("incomplete frame colors")

    backgrounds = []
    text = []
    for y in range(height):
        for start, end, palette_id in colors[y]:
            if not 0 <= start <= end < width or palette_id not in palette:
                raise ValueError("invalid color span bounds or palette")
            foreground, background = palette[palette_id]
            backgrounds.append(
                f'<rect x="{8 + start * CELL_WIDTH}" y="{4 + y * 18}" '
                f'width="{(end - start + 1) * CELL_WIDTH}" height="18" fill="{background}"/>'
            )
            value = html.escape(rows[y][start : end + 1]).replace(" ", "&#160;")
            text.append(
                f'<text x="{8 + start * CELL_WIDTH}" y="{18 + y * 18}" '
                f'fill="{foreground}">{value}</text>'
            )
    body = "".join(backgrounds + text)
    return (
        f'<svg xmlns="http://www.w3.org/2000/svg" width="{width * CELL_WIDTH + 16}" '
        f'height="{height * 18 + 8}" viewBox="0 0 {width * CELL_WIDTH + 16} {height * 18 + 8}">'
        f'<rect width="100%" height="100%" fill="{DEFAULT_BACKGROUND}"/>'
        f'<g font-family="monospace" font-size="15" xml:space="preserve">{body}</g></svg>\n'
    )


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--input", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(render(args.input.read_text()), encoding="utf-8")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
