#!/usr/bin/env python3
"""Generate the PWA icon set from a single vector-ish drawing (Pillow).

Outputs into ../../icons/ (repo root `icons/`, which Trunk copies verbatim into
dist via `rel="copy-dir"`). Re-run after changing the design:

    python tools/icons/generate.py

Why hand-drawn instead of a checked-in source SVG + rasterizer: the repo has no
SVG toolchain (no rsvg/inkscape/magick on the dev box), but Pillow is available,
so we draw the headphones glyph directly. The four outputs cover both platforms:

  icon-192.png            Android/Chrome install icon (purpose "any")
  icon-512.png            Android/Chrome splash + high-res (purpose "any")
  icon-maskable-512.png   Android adaptive icon — full-bleed bg, glyph kept
                          inside the central safe circle so a circular/squircle
                          mask never clips it (purpose "maskable")
  apple-touch-icon-180.png  iOS home screen — opaque, full-bleed (iOS rounds it
                            itself; transparency/pre-rounding would look wrong)
"""
from __future__ import annotations

import os

from PIL import Image, ImageDraw

# Match the app's dark theme (styles.css background #111 / manifest theme_color).
BG = (17, 17, 17, 255)        # #111111
ACCENT = (124, 176, 255, 255)  # soft blue, kin to the app's #9cf podcast titles

OUT_DIR = os.path.normpath(os.path.join(os.path.dirname(__file__), "..", "..", "icons"))

# Supersampling factor: draw big, downscale → smooth arcs/edges without antialias
# hints (Pillow's arc/rounded_rectangle are aliased at 1x).
SS = 4


def _draw(size: int, *, maskable: bool, opaque: bool) -> Image.Image:
    """Render one icon at `size`px. `maskable` shrinks the glyph into the safe
    zone and forces a full-bleed background; `opaque` drops the alpha channel."""
    s = size * SS
    img = Image.new("RGBA", (s, s), (0, 0, 0, 0))
    d = ImageDraw.Draw(img)

    # Background: full-bleed for maskable/opaque (no corner may be transparent or
    # the mask/iOS would show artifacts); a rounded square otherwise.
    if maskable or opaque:
        d.rectangle([0, 0, s, s], fill=BG)
    else:
        d.rounded_rectangle([0, 0, s - 1, s - 1], radius=int(s * 0.22), fill=BG)

    # Glyph diameter as a fraction of the icon. Maskable stays well inside the
    # central 80%-diameter safe circle; "any"/apple can breathe a bit larger.
    g = 0.46 if maskable else 0.58
    cx, cy = s / 2.0, s / 2.0
    band_r = s * g / 2.0           # radius of the headband arc
    band_w = max(2, int(s * 0.055))  # stroke width

    # Headband: top semicircle, nudged up so the whole glyph sits optically centred.
    top = cy - band_r * 0.78
    box = [cx - band_r, top, cx + band_r, top + 2 * band_r]
    d.arc(box, start=180, end=360, fill=ACCENT, width=band_w)

    # Ear cups: rounded bars hanging from each end of the band.
    cup_w = s * 0.135
    cup_h = s * 0.30
    cup_top = top + band_r - cup_h * 0.18
    for ex in (cx - band_r, cx + band_r):
        d.rounded_rectangle(
            [ex - cup_w / 2, cup_top, ex + cup_w / 2, cup_top + cup_h],
            radius=cup_w / 2,
            fill=ACCENT,
        )

    img = img.resize((size, size), Image.LANCZOS)
    if opaque:
        flat = Image.new("RGB", (size, size), BG[:3])
        flat.paste(img, (0, 0), img)
        return flat
    return img


def main() -> None:
    os.makedirs(OUT_DIR, exist_ok=True)
    jobs = [
        ("icon-192.png", 192, dict(maskable=False, opaque=False)),
        ("icon-512.png", 512, dict(maskable=False, opaque=False)),
        ("icon-maskable-512.png", 512, dict(maskable=True, opaque=False)),
        ("apple-touch-icon-180.png", 180, dict(maskable=False, opaque=True)),
    ]
    for name, size, kw in jobs:
        path = os.path.join(OUT_DIR, name)
        _draw(size, **kw).save(path, "PNG", optimize=True)
        print(f"wrote {path} ({size}x{size})")


if __name__ == "__main__":
    main()
