"""Meteor Browser icon generator.

Draws the app icon (a meteor streaking across a night-sky tile) with signed
distance fields, so every size is rendered natively instead of being scaled
down from one bitmap. The design is the same at every size; small sizes
only get a bolder head and tail.

    python tools/make_icon.py            -> assets/mbrowser.ico + assets/mbrowser-*.png
    python tools/make_icon.py --preview DIR   also writes a contact sheet to DIR

Needs numpy and Pillow (Pillow is used only to write PNG files).
"""

import io
import struct
import sys
from pathlib import Path

import numpy as np
from PIL import Image

ICO_SIZES = [16, 20, 24, 32, 40, 48, 64, 96, 128, 256]
PNG_SIZES = [256, 512, 1024]


def hexc(s):
    s = s.lstrip("#")
    return np.array([int(s[i:i + 2], 16) / 255.0 for i in (0, 2, 4)])


# Palette: one hue family, taken from the browser's accent (#58b4f0).
SKY_TOP = hexc("#183257")
SKY_BOT = hexc("#0a1530")
GLOW = hexc("#58b4f0")
TAIL_NEAR = hexc("#8fd6ff")
TAIL_MID = hexc("#58b4f0")
TAIL_FAR = hexc("#2f7fd6")
ICE = hexc("#dff4ff")
CORE = hexc("#f7fcff")
WHITE = np.ones(3)


# ---------------------------------------------------------------- SDFs (unit space)

def sd_round_rect(px, py, x0, y0, x1, y1, r):
    cx, cy = (x0 + x1) / 2, (y0 + y1) / 2
    hx, hy = (x1 - x0) / 2 - r, (y1 - y0) / 2 - r
    qx, qy = np.abs(px - cx) - hx, np.abs(py - cy) - hy
    out = np.hypot(np.maximum(qx, 0), np.maximum(qy, 0))
    return out + np.minimum(np.maximum(qx, qy), 0) - r


def sd_circle(px, py, c, r):
    return np.hypot(px - c[0], py - c[1]) - r


def sd_cone(px, py, a, b, ra, rb):
    """Uneven capsule: circle ra at a, circle rb at b, joined by tangents."""
    pbx, pby = b[0] - a[0], b[1] - a[1]
    h = pbx * pbx + pby * pby
    x, y = px - a[0], py - a[1]
    qx = np.abs(x * pby - y * pbx) / h
    qy = (x * pbx + y * pby) / h
    d = ra - rb
    cx, cy = np.sqrt(h - d * d), d
    k = cx * qy - cy * qx
    m = cx * qx + cy * qy
    n = qx * qx + qy * qy
    return np.where(
        k < 0, np.sqrt(h * n) - ra,
        np.where(k > cx, np.sqrt(h * (n + 1 - 2 * qy)) - rb, m - ra))


def along(px, py, a, b):
    """0 at a, 1 at b (clamped): position along a segment."""
    vx, vy = b[0] - a[0], b[1] - a[1]
    return np.clip(((px - a[0]) * vx + (py - a[1]) * vy) / (vx * vx + vy * vy), 0, 1)


def ramp(t, stops):
    """Piecewise-linear color ramp; stops = [(t, rgb), ...]."""
    out = np.zeros(t.shape + (3,))
    for (t0, c0), (t1, c1) in zip(stops, stops[1:]):
        w = np.clip((t - t0) / (t1 - t0), 0, 1)[..., None]
        seg = c0 + (c1 - c0) * w
        mask = ((t >= t0) & (t <= t1))[..., None]
        out = np.where(mask, seg, out)
    return out


# ---------------------------------------------------------------- drawing

class Canvas:
    def __init__(self, n):
        self.n = n
        g = (np.arange(n) + 0.5) / n
        self.x, self.y = np.meshgrid(g, g)
        self.px = 1.0 / n  # one sample in unit space
        self.c = np.zeros((n, n, 3))  # premultiplied color
        self.a = np.zeros((n, n))

    def cov(self, sd, soft=1.0):
        return np.clip(0.5 - sd / (self.px * soft), 0, 1)

    def over(self, color, alpha):
        alpha = np.clip(alpha, 0, 1)
        color = np.broadcast_to(color, self.c.shape)
        self.c = color * alpha[..., None] + self.c * (1 - alpha[..., None])
        self.a = alpha + self.a * (1 - alpha)

    def add(self, color, alpha):
        """Additive light (glow); keeps alpha."""
        self.c = np.clip(self.c + np.broadcast_to(color, self.c.shape) * alpha[..., None], 0, 1)


def profile(size):
    """One design at every size; only optical weight changes (bolder when small)."""
    if size <= 24:
        return dict(inset=0.0, r_tail=0.19, r_core=0.115, fade=0.65, glow=0.10, rim=False)
    if size <= 48:
        return dict(inset=1 / 32, r_tail=0.165, r_core=0.10, fade=0.8, glow=0.16, rim=True)
    return dict(inset=1 / 32, r_tail=0.15, r_core=0.092, fade=0.95, glow=0.20, rim=True)


HEAD, TIP = (0.635, 0.365), (0.12, 0.88)


def render(size):
    p = profile(size)
    ss = 8 if size <= 48 else 4 if size <= 256 else 2
    cv = Canvas(size * ss)
    x, y = cv.x, cv.y
    H, T = HEAD, TIP

    # Night-sky tile: diagonal gradient, a little light where the meteor is.
    i0, i1 = p["inset"], 1 - p["inset"]
    tile = sd_round_rect(x, y, i0, i0, i1, i1, 0.225 * (i1 - i0))
    t = np.clip((x * 0.35 + y * 0.65 - 0.05) / 0.9, 0, 1)
    cv.over(ramp(t, [(0, SKY_TOP), (1, SKY_BOT)]), np.ones_like(x))
    rg = np.hypot(x - H[0], y - H[1])
    cv.add(GLOW, p["glow"] * np.exp(-(rg / 0.28) ** 2))

    # Tail: a cone that cools from ice-blue to deep blue and fades out.
    u = along(x, y, H, T)
    outer = sd_cone(x, y, H, T, p["r_tail"], 0.006)
    col = ramp(u, [(0, TAIL_NEAR), (0.35, TAIL_MID), (1, TAIL_FAR)])
    cv.over(col, cv.cov(outer) * np.clip((1 - u) ** p["fade"] * 1.1, 0, 1))
    # Hot inner trail.
    T2 = tuple(np.array(H) + (np.array(T) - np.array(H)) * 0.62)
    u2 = along(x, y, H, T2)
    cv.over(ramp(u2, [(0, ICE), (1, TAIL_NEAR)]), cv.cov(sd_cone(x, y, H, T2, p["r_core"], 0.004)) * (1 - u2) ** 0.9)
    # Head.
    cv.over(CORE, cv.cov(sd_circle(x, y, H, p["r_core"])))

    # One-pixel rim light, brighter at the top: keeps the tile edge visible on dark taskbars.
    if p["rim"]:
        w = 1.0 / size
        band = np.clip(1 - np.abs(tile + w * 0.5) / (w * 0.5 + cv.px), 0, 1)
        cv.add(WHITE, band * (0.16 * (1 - y) + 0.03))

    # Clip to the tile.
    m = cv.cov(tile)
    cv.c *= m[..., None]
    cv.a = cv.a * m

    # Downsample (box filter on premultiplied color).
    n = size
    c = cv.c.reshape(n, ss, n, ss, 3).mean(axis=(1, 3))
    a = cv.a.reshape(n, ss, n, ss).mean(axis=(1, 3))
    rgb = np.where(a[..., None] > 0, c / np.maximum(a[..., None], 1e-6), 0)
    img = np.dstack([rgb, a])
    return (np.clip(img, 0, 1) * 255 + 0.5).astype(np.uint8)


# ---------------------------------------------------------------- file writers

def png_bytes(rgba):
    buf = io.BytesIO()
    Image.fromarray(rgba).save(buf, "PNG", optimize=True)
    return buf.getvalue()


def bmp_icon_bytes(rgba):
    """Classic 32-bit DIB icon entry (BGRA, bottom-up, with AND mask)."""
    h, w = rgba.shape[:2]
    header = struct.pack("<IiiHHIIiiII", 40, w, h * 2, 1, 32, 0, 0, 0, 0, 0, 0)
    bgra = rgba[::-1, :, [2, 1, 0, 3]].tobytes()
    row = ((w + 31) // 32) * 4
    mask = bytearray()
    for r in rgba[::-1, :, 3]:
        bits = bytearray(row)
        for i, av in enumerate(r):
            if av == 0:
                bits[i // 8] |= 0x80 >> (i % 8)
        mask += bits
    return header + bgra + bytes(mask)


def write_ico(path, images):
    entries, blobs = [], []
    offset = 6 + 16 * len(images)
    for img in images:
        s = img.shape[0]
        blob = png_bytes(img) if s >= 64 else bmp_icon_bytes(img)
        entries.append(struct.pack("<BBBBHHII", s % 256, s % 256, 0, 0, 1, 32, len(blob), offset))
        blobs.append(blob)
        offset += len(blob)
    Path(path).write_bytes(struct.pack("<HHH", 0, 1, len(images)) + b"".join(entries) + b"".join(blobs))


def contact_sheet(images, path):
    """Every size at 1x, plus small sizes pixel-zoomed, on dark / light / accent backgrounds."""
    pad, zoom = 16, 192
    imgs = [Image.fromarray(i) for i in images]
    small = [i for i in imgs if i.width <= 48]
    band_h = 256 + zoom + 3 * pad
    width = max(pad + sum(i.width + pad for i in imgs), pad + len(small) * (zoom + pad))
    sheet = Image.new("RGB", (width, 3 * band_h), (0, 0, 0))
    for k, bg in enumerate([(32, 32, 32), (243, 243, 243), (88, 180, 240)]):
        top = k * band_h
        sheet.paste(Image.new("RGB", (width, band_h), bg), (0, top))
        xx = pad
        for i in imgs:
            sheet.paste(i, (xx, top + pad + 256 - i.height), i)
            xx += i.width + pad
        xx = pad
        for i in small:
            z = i.resize((zoom, zoom), Image.NEAREST)
            sheet.paste(z, (xx, top + 2 * pad + 256), z)
            xx += zoom + pad
    sheet.save(path)


def main():
    root = Path(__file__).resolve().parent.parent
    assets = root / "assets"
    assets.mkdir(exist_ok=True)
    ico = [render(s) for s in ICO_SIZES]
    write_ico(assets / "mbrowser.ico", ico)
    for s in PNG_SIZES:
        Image.fromarray(render(s)).save(assets / f"mbrowser-{s}.png", optimize=True)
    print("wrote", assets / "mbrowser.ico", "and", ", ".join(f"mbrowser-{s}.png" for s in PNG_SIZES))
    if "--preview" in sys.argv:
        out = Path(sys.argv[sys.argv.index("--preview") + 1])
        out.mkdir(parents=True, exist_ok=True)
        contact_sheet([ico[i] for i in range(len(ICO_SIZES)) if ICO_SIZES[i] in (16, 20, 24, 32, 48, 64, 256)],
                      out / "sheet.png")
        print("preview", out / "sheet.png")


if __name__ == "__main__":
    main()
