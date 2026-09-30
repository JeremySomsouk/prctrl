#!/usr/bin/env python3
"""Render JSON cells from the tui_preview example as a documentation screenshot.

Requires Pillow and a monospace TrueType font. Example:
  cargo run --example tui_preview -- /tmp/prctrl-preview.json 120 32
  python scripts/render_tui_preview.py /tmp/prctrl-preview.json docs/src/assets/tui-review-desk.png
"""
import argparse
import json
from pathlib import Path

from PIL import Image, ImageDraw, ImageFont


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("cells", type=Path)
    parser.add_argument("output", type=Path)
    parser.add_argument("--font", default="/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf")
    parser.add_argument("--bold-font", default="/usr/share/fonts/truetype/dejavu/DejaVuSansMono-Bold.ttf")
    args = parser.parse_args()
    data = json.loads(args.cells.read_text())
    cell_width, cell_height, padding = 10, 20, 18
    image = Image.new("RGB", (data["width"] * cell_width + 2 * padding,
                              data["height"] * cell_height + 2 * padding), (18, 24, 35))
    draw = ImageDraw.Draw(image)
    fonts = {False: ImageFont.truetype(args.font, 16), True: ImageFont.truetype(args.bold_font, 16)}
    for index, cell in enumerate(data["cells"]):
        x = padding + (index % data["width"]) * cell_width
        y = padding + (index // data["width"]) * cell_height
        draw.rectangle((x, y, x + cell_width - 1, y + cell_height - 1), fill=tuple(cell["bg"]))
    for index, cell in enumerate(data["cells"]):
        x = padding + (index % data["width"]) * cell_width
        y = padding + (index // data["width"]) * cell_height
        draw.text((x, y), cell["text"], font=fonts[cell["bold"]], fill=tuple(cell["fg"]))
    args.output.parent.mkdir(parents=True, exist_ok=True)
    image.save(args.output)


if __name__ == "__main__":
    main()
