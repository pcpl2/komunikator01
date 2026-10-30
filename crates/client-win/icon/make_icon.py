"""Draws komunikator.ico (16-256 px) next to this script: a speech bubble with sound bars.

Run: python crates/client-win/icon/make_icon.py  (needs Pillow). The .ico is committed, so the
build never needs Python; rerun only after changing the drawing.
"""
from pathlib import Path

from PIL import Image, ImageDraw

S = 1024  # drawn large, then scaled down per size
TOP, BOTTOM = (0x2B, 0x8C, 0xEE), (0x15, 0x4F, 0xB0)
BARS = (0x1A, 0x5F, 0xC8)


def draw() -> Image.Image:
    img = Image.new("RGBA", (S, S), (0, 0, 0, 0))
    # Background: rounded square with a vertical gradient.
    grad = Image.new("RGBA", (S, S))
    gd = ImageDraw.Draw(grad)
    for y in range(S):
        t = y / (S - 1)
        gd.line([(0, y), (S, y)], fill=tuple(round(a + (b - a) * t) for a, b in zip(TOP, BOTTOM)) + (255,))
    mask = Image.new("L", (S, S), 0)
    ImageDraw.Draw(mask).rounded_rectangle([0, 0, S - 1, S - 1], radius=S * 0.22, fill=255)
    img.paste(grad, (0, 0), mask)

    d = ImageDraw.Draw(img)
    # White speech bubble with a tail at the bottom left.
    box = [S * 0.15, S * 0.18, S * 0.85, S * 0.72]
    d.rounded_rectangle(box, radius=S * 0.16, fill="white")
    d.polygon([(S * 0.28, S * 0.66), (S * 0.24, S * 0.86), (S * 0.46, S * 0.70)], fill="white")
    # Sound bars inside the bubble.
    w = S * 0.075
    cy = (box[1] + box[3]) / 2
    for x, h in [(0.32, 0.12), (0.43, 0.24), (0.54, 0.32), (0.65, 0.18)]:
        cx = S * (x + 0.035)
        d.rounded_rectangle([cx - w / 2, cy - S * h / 2, cx + w / 2, cy + S * h / 2], radius=w / 2, fill=BARS)
    return img


def main() -> None:
    out = Path(__file__).with_name("komunikator.ico")
    big = draw()
    sizes = [16, 20, 24, 32, 40, 48, 64, 128, 256]
    big.resize((256, 256), Image.LANCZOS).save(out, sizes=[(s, s) for s in sizes])
    print(out)


if __name__ == "__main__":
    main()
