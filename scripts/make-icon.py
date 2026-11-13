#!/usr/bin/env python3
"""Draws hirc's app icon: a speech bubble holding '#', in the default theme's
colours. Writes macos/AppIcon.png (1024²) and, on macOS, macos/AppIcon.icns.

Run: uv run --with pillow scripts/make-icon.py
"""
import pathlib
import shutil
import subprocess
import tempfile

from PIL import Image, ImageDraw

BG = "#121218"  # general.background
BUBBLE = "#c792ea"  # text.tertiary, the accent
MARK = "#121218"
SIZE = 1024

root = pathlib.Path(__file__).resolve().parent.parent
out = root / "macos"
out.mkdir(exist_ok=True)

img = Image.new("RGBA", (SIZE, SIZE), (0, 0, 0, 0))
d = ImageDraw.Draw(img)
# The macOS icon grid: a rounded square inset 100px, radius 185.
d.rounded_rectangle((100, 100, 924, 924), radius=185, fill=BG)
# The bubble, and its tail at the lower left.
d.rounded_rectangle((220, 250, 804, 690), radius=110, fill=BUBBLE)
d.polygon([(300, 640), (300, 800), (430, 680)], fill=BUBBLE)
# '#': two tilted strokes up, two across.
w = 46
for x in (420, 560):
    d.polygon([(x + 30, 320), (x + 30 + w, 320), (x - 10 + w, 620), (x - 10, 620)], fill=MARK)
for y in (400, 510):
    d.rectangle((350, y, 680, y + w), fill=MARK)
png = out / "AppIcon.png"
img.save(png)

if shutil.which("iconutil"):
    with tempfile.TemporaryDirectory() as tmp:
        iconset = pathlib.Path(tmp) / "AppIcon.iconset"
        iconset.mkdir()
        for px in (16, 32, 128, 256, 512):
            img.resize((px, px), Image.LANCZOS).save(iconset / f"icon_{px}x{px}.png")
            img.resize((px * 2, px * 2), Image.LANCZOS).save(iconset / f"icon_{px}x{px}@2x.png")
        subprocess.run(["iconutil", "-c", "icns", str(iconset), "-o", str(out / "AppIcon.icns")], check=True)
print(f"wrote {png}")
