#!/usr/bin/env python3
"""Rebuild the 640x480 software splash with bundled Nunito (requires Pillow)."""
from pathlib import Path
import re
import tomllib
from PIL import Image, ImageDraw, ImageFont

ROOT = Path(__file__).resolve().parents[1]
PRODUCT = (ROOT / "crates/common/src/product.rs").read_text()
NAME = re.search(r'pub const NAME: &str = "([^"]+)";', PRODUCT).group(1)
VERSION = tomllib.loads((ROOT / "crates/common/Cargo.toml").read_text())["package"]["version"]
FONT = ROOT / "static/.allium/fonts/Nunito.ttf"
OUTPUT = ROOT / "static/.allium/images/orbitos-splash.png"


def generate():
    scale = 3
    image = Image.new("RGB", (640 * scale, 480 * scale), "#f1f7f5")
    draw = ImageDraw.Draw(image)
    for text, size, y, color in [(NAME, 64, 194, "#007b79"), (VERSION, 20, 278, "#496460")]:
        font = ImageFont.truetype(str(FONT), size * scale)
        draw.text((320 * scale, y * scale), text, font=font, fill=color, anchor="mt")
    OUTPUT.parent.mkdir(parents=True, exist_ok=True)
    image.resize((640, 480), Image.Resampling.LANCZOS).save(OUTPUT, optimize=True)
    print(OUTPUT.relative_to(ROOT))


if __name__ == "__main__":
    generate()
