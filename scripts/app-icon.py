# Draws the app icon: Signal Desktop's macOS logo (build/icons/mac/
# AppIcon.icon/Assets/logo.svg, white) on a rust gradient, in the shape and
# size macOS gives app icons, for crates/app/assets/app-icon:
#   uv run --with pillow scripts/app-icon.py LOGO.svg OUT_DIR
# then oxipng -o max --strip safe OUT_DIR/*.png. Needs rsvg-convert
# (brew install librsvg).
#
# Desktop's icon is an Icon Composer file, whose shape macOS draws; an .icns
# draws its own. Apple's grid: on a 1024 canvas, an 824 square at 100 with
# continuous corners (a superellipse here) and a soft shadow below.
import math
import subprocess
import sys
import tempfile
from pathlib import Path

from PIL import Image, ImageChops, ImageDraw, ImageFilter

logo_svg, out_dir = Path(sys.argv[1]), Path(sys.argv[2])
CANVAS, BODY, INSET = 1024, 824, 100
TOP, BOTTOM = (0xD9, 0x55, 0x2A), (0xA8, 0x38, 0x0F)
SIZES = [16, 24, 32, 48, 64, 128, 256, 512, 1024]


def squircle(size: int, exponent: float = 5.0, supersample: int = 4) -> Image.Image:
    """A superellipse mask, |x|^n + |y|^n <= 1, antialiased by drawing big."""
    big = size * supersample
    mask = Image.new("L", (big, big), 0)
    points = []
    steps = 4096
    for i in range(steps):
        t = 2 * math.pi * i / steps
        c, s = math.cos(t), math.sin(t)
        x = abs(c) ** (2 / exponent) * (1 if c >= 0 else -1)
        y = abs(s) ** (2 / exponent) * (1 if s >= 0 else -1)
        points.append(((x + 1) * big / 2, (y + 1) * big / 2))
    ImageDraw.Draw(mask).polygon(points, fill=255)
    return mask.resize((size, size), Image.LANCZOS)


def gradient(size: int) -> Image.Image:
    image = Image.new("RGBA", (size, size))
    pixels = image.load()
    for y in range(size):
        f = y / (size - 1)
        color = tuple(round(a + (b - a) * f) for a, b in zip(TOP, BOTTOM)) + (255,)
        for x in range(size):
            pixels[x, y] = color
    return image


with tempfile.TemporaryDirectory() as tmp:
    logo_png = Path(tmp) / "logo.png"
    # The logo is drawn on the shape's own 1024 square; scale it with it.
    subprocess.run(["rsvg-convert", "-w", str(BODY), "-h", str(BODY), "-o", str(logo_png), str(logo_svg)], check=True)
    logo = Image.open(logo_png).convert("RGBA")

shape = squircle(BODY)
body = gradient(BODY)
body.alpha_composite(logo)
body.putalpha(ImageChops.multiply(body.getchannel("A"), shape))

icon = Image.new("RGBA", (CANVAS, CANVAS), (0, 0, 0, 0))
# A soft shadow, a little below, as macOS icons have.
shadow = Image.new("RGBA", (CANVAS, CANVAS), (0, 0, 0, 0))
shadow_mask = Image.new("L", (CANVAS, CANVAS), 0)
shadow_mask.paste(shape, (INSET, INSET + 12))
shadow.putalpha(shadow_mask.point(lambda a: a * 0.35).filter(ImageFilter.GaussianBlur(14)))
icon.alpha_composite(shadow)
icon.alpha_composite(body, (INSET, INSET))

out_dir.mkdir(parents=True, exist_ok=True)
for size in SIZES:
    icon.resize((size, size), Image.LANCZOS).save(out_dir / f"{size}x{size}.png")
