#!/usr/bin/env python3
"""Draw lma-rs's app icon (#766): an original badge, a football on a pitch-green shield with
"LMA" beneath, so no game artwork is involved. Writes lma-rs/assets/icon/icon.png (1024 px),
icon.ico (16-256 px, the Windows exe) and icon_256.png (the window icon lma-app sets at run time).

    python tools/gen_icon.py

Needs Pillow. Rerun after changing the drawing; the outputs are committed (our own art)."""
import math
import os

from PIL import Image, ImageDraw, ImageFont

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
OUT = os.path.join(ROOT, "lma-rs", "assets", "icon")
S = 1024

GREEN, GREEN_DARK, GOLD, WHITE, INK = (22, 120, 58), (12, 74, 36), (240, 196, 60), (250, 250, 247), (24, 28, 32)


def shield(d, inset, fill):
    w = S - 2 * inset
    top, mid = inset + 0.04 * S, inset + 0.58 * S
    pts = [(inset, top), (S / 2, inset), (S - inset, top), (S - inset, mid)]
    # the lower curve to the point
    for k in range(1, 33):
        t = k / 32
        x = (S - inset) + (S / 2 - (S - inset)) * t
        y = mid + (S - inset - mid) * math.sin(t * math.pi / 2)
        pts.append((x, y))
    for k in range(1, 33):
        t = k / 32
        x = S / 2 + (inset - S / 2) * t
        y = (S - inset) + (mid - (S - inset)) * (1 - math.cos(t * math.pi / 2))
        pts.append((x, y))
    d.polygon(pts, fill=fill)
    return w


def football(d, cx, cy, r):
    d.ellipse([cx - r, cy - r, cx + r, cy + r], fill=WHITE, outline=INK, width=int(r * 0.06))
    # the centre pentagon and five half-pentagons at the rim: the classic ball
    def pent(px, py, pr, rot):
        return [(px + pr * math.sin(rot + k * 2 * math.pi / 5), py - pr * math.cos(rot + k * 2 * math.pi / 5)) for k in range(5)]
    d.polygon(pent(cx, cy, r * 0.36, 0), fill=INK)
    for k in range(5):
        a = k * 2 * math.pi / 5
        px, py = cx + r * 0.86 * math.sin(a), cy - r * 0.86 * math.cos(a)
        d.polygon(pent(px, py, r * 0.26, a + math.pi), fill=INK)
        # seams from the centre pentagon's corners towards the rim patches
        ix, iy = cx + r * 0.36 * math.sin(a), cy - r * 0.36 * math.cos(a)
        d.line([(ix, iy), (cx + r * 0.62 * math.sin(a), cy - r * 0.62 * math.cos(a))], fill=INK, width=int(r * 0.05))


def font(size):
    for name in ("arialbd.ttf", "Arial Bold.ttf", "DejaVuSans-Bold.ttf", "Helvetica.ttc"):
        try:
            return ImageFont.truetype(name, size)
        except OSError:
            continue
    return ImageFont.load_default()


def main():
    os.makedirs(OUT, exist_ok=True)
    img = Image.new("RGBA", (S, S), (0, 0, 0, 0))
    d = ImageDraw.Draw(img)
    shield(d, int(0.04 * S), GOLD)
    shield(d, int(0.075 * S), GREEN_DARK)
    shield(d, int(0.10 * S), GREEN)
    football(d, S / 2, 0.40 * S, 0.21 * S)
    f = font(int(0.20 * S))
    text = "LMA"
    box = d.textbbox((0, 0), text, font=f)
    tw, th = box[2] - box[0], box[3] - box[1]
    d.text(((S - tw) / 2 - box[0], 0.66 * S - box[1]), text, font=f, fill=WHITE, stroke_width=int(0.012 * S), stroke_fill=GREEN_DARK)
    img.save(os.path.join(OUT, "icon.png"))
    img.resize((256, 256), Image.LANCZOS).save(os.path.join(OUT, "icon_256.png"))
    img.save(os.path.join(OUT, "icon.ico"), sizes=[(16, 16), (24, 24), (32, 32), (48, 48), (64, 64), (128, 128), (256, 256)])
    print("wrote", OUT)


if __name__ == "__main__":
    main()
