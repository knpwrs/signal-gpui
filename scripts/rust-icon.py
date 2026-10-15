# Recolors one of Signal Desktop's icon PNGs (build/icons/png) from blue to rust,
# for crates/app/assets/app-icon:
#   uv run --with pillow scripts/rust-icon.py SRC.png OUT.png
# then oxipng -o max --strip safe OUT.png.
# Each pixel is white mixed with the blue gradient; the mix is kept and the
# gradient swapped.
import sys
from PIL import Image
src, out = sys.argv[1], sys.argv[2]
TOP, BOTTOM = (0xD9, 0x55, 0x2A), (0xA8, 0x38, 0x0F)
im = Image.open(src).convert("RGBA")
w, h = im.size
px = im.load()
for y in range(h):
    f = y / max(h - 1, 1)
    rust = tuple(round(a + (b - a) * f) for a, b in zip(TOP, BOTTOM))
    for x in range(w):
        r, g, b, a = px[x, y]
        if a == 0:
            continue
        hi = max(r, g, b)
        sat = (hi - min(r, g, b)) / hi if hi else 0
        t = min(1.0, sat / 0.76)  # 0.76: the blue's lightest saturation
        px[x, y] = tuple(round(255 * (1 - t) + c * t) for c in rust) + (a,)
im.save(out)
