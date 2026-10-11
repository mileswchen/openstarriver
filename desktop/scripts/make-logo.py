#!/usr/bin/env python3
"""Draw the Starriver app icon: a four-pointed star above a river of starlight.

Design goals (2026-10 rework):
  - true-black background (旧的深海军蓝底被要求改为黑色)
  - bold shapes that stay legible from 16px tray icons up to 1024px dmg art
    (≤64px renders use a simplified "small" variant: thicker star/river,
     no micro-sparkles — 这是旧图标小尺寸发糊的根因修复)
  - brand colors from promo/: cyan #67e8f9 → indigo #818cf8 river,
    warm white star with amber glow

Pure-PIL vector-style rendering, supersampled 4x. Exports to
desktop/electron/assets/:
  icon.png   1024x1024        linux (electron-builder) + mac fallback
  icon.ico   16..256 PNG entries  Windows (tray→explorer 全覆盖)
  icon.icns  16..1024 iconset   macOS (via iconutil, skipped off-mac)
  icon.svg   vector reference master (same geometry, for future edits)

Deterministic (fixed seeds) so rebuilds are reproducible byte-for-byte.
"""
import io
import math
import os
import random
import struct
import subprocess
import sys
import tempfile

from PIL import Image, ImageDraw, ImageFilter

HERE = os.path.dirname(os.path.abspath(__file__))
ASSETS = os.path.join(HERE, "..", "electron", "assets")

S_LARGE = 4096   # supersampled master for ≥128px targets
S_SMALL = 2048   # simplified master for <128px targets
OUT = 1024       # logical canvas all geometry is specified against

 # palette (promo/ brand colors + pure black background)
BG_TOP = (11, 11, 16)        # #0B0B10 — reads black, keeps a hint of night sky
BG_BOTTOM = (0, 0, 0)        # pure black
STAR_TIP = (255, 225, 158)   # warm gold tips
STAR_CORE = (255, 255, 255)
ACCENT_STAR = (255, 243, 214)
GLOW_AMBER = (255, 199, 92)
RIVER_L = (103, 232, 249)    # #67e8f9 brand cyan
RIVER_R = (129, 140, 248)    # #818cf8 brand indigo
STREAK = (240, 253, 255)
SKY_DOT = (208, 226, 255)


def lerp(a, b, t):
    return tuple(int(a[i] + (b[i] - a[i]) * t) for i in range(3))


def v_gradient(size, top, bottom):
    w, h = size
    g = Image.new("RGBA", (w, h))
    px = g.load()
    for y in range(h):
        row = lerp(top, bottom, y / (h - 1)) + (255,)
        for x in range(w):
            px[x, y] = row
    return g


def h_gradient(size, left, right):
    w, h = size
    g = Image.new("RGBA", (w, h))
    px = g.load()
    for x in range(w):
        col = lerp(left, right, x / (w - 1)) + (255,)
        for y in range(h):
            px[x, y] = col
    return g


def bezier(p0, p1, p2, p3, t):
    mt = 1 - t
    x = mt**3 * p0[0] + 3 * mt**2 * t * p1[0] + 3 * mt * t**2 * p2[0] + t**3 * p3[0]
    y = mt**3 * p0[1] + 3 * mt**2 * t * p1[1] + 3 * mt * t**2 * p2[1] + t**3 * p3[1]
    return x, y


def bezier_d(p0, p1, p2, p3, t):
    mt = 1 - t
    dx = 3 * mt**2 * (p1[0] - p0[0]) + 6 * mt * t * (p2[0] - p1[0]) + 3 * t**2 * (p3[0] - p2[0])
    dy = 3 * mt**2 * (p1[1] - p0[1]) + 6 * mt * t * (p2[1] - p1[1]) + 3 * t**2 * (p3[1] - p2[1])
    return dx, dy


RIVER_PTS = [(-0.06, 0.60), (0.30, 0.46), (0.62, 0.92), (1.06, 0.58)]  # unit coords — pronounced S


def river_polygon(s, half_base):
    """S-curve river band, bled off both edges; mild mid-stream swell."""
    p0, p1, p2, p3 = [(x * s, y * s) for x, y in RIVER_PTS]
    n = 180
    top, bot = [], []
    for i in range(n + 1):
        t = i / n
        x, y = bezier(p0, p1, p2, p3, t)
        dx, dy = bezier_d(p0, p1, p2, p3, t)
        ln = math.hypot(dx, dy) or 1.0
        nx, ny = -dy / ln, dx / ln
        half = half_base * s * (0.78 + 0.22 * math.sin(math.pi * min(max(t, 0.02), 0.98)))
        top.append((x + nx * half, y + ny * half))
        bot.append((x - nx * half, y - ny * half))
    return top + bot[::-1]


def star4_points(cx, cy, r, inner_ratio, rot=0.0):
    pts = []
    for i in range(8):
        ang = rot + i * math.pi / 4 - math.pi / 2
        rad = r if i % 2 == 0 else r * inner_ratio
        pts.append((cx + rad * math.cos(ang), cy + rad * math.sin(ang)))
    return pts


def sky_dots(seed, count, r_lo, r_hi, a_lo, a_hi):
    """Deterministic scatter of distant stars, kept clear of the main star."""
    rng = random.Random(seed)
    dots = []
    while len(dots) < count:
        x, y = rng.uniform(0.05, 0.95), rng.uniform(0.05, 0.52)
        if math.hypot(x - 0.5, y - 0.36) < 0.30:  # main-star exclusion zone
            continue
        dots.append((x, y, rng.uniform(r_lo, r_hi), rng.randint(a_lo, a_hi)))
    return dots


def river_sparkles(seed, count):
    rng = random.Random(seed)
    p0, p1, p2, p3 = RIVER_PTS
    out = []
    for _ in range(count):
        t = rng.uniform(0.08, 0.92)
        x, y = bezier(p0, p1, p2, p3, t)
        out.append((x + rng.uniform(-0.04, 0.04), y + rng.uniform(-0.026, 0.026),
                    rng.uniform(0.0035, 0.008), rng.randint(150, 255)))
    return out


def layout(small):
    """Tuned parameters: small keeps only what survives at 16-64px."""
    if small:
        return dict(
            star=(0.5, 0.34, 0.220, 0.47),
            glow_scale=1.28, glow_alpha=115, glow_blur=0.030,
            accent=None, dots=None, sparkles=None, glints=None,
            river_half=0.120, streak=(0.028, -0.022, 235, 0.002),
            river_stars=[(0.5, 0.055)],
        )
    # star + river fill more of the canvas than the pre-rounded design —
    # the dock renders the masked squircle, so every % of canvas is visible
    return dict(
        star=(0.5, 0.35, 0.185, 0.44),
        glow_scale=1.30, glow_alpha=150, glow_blur=0.028,
        accent=(0.795, 0.170, 0.044),
        dots=sky_dots(20261008, 10, 0.005, 0.011, 130, 220),
        sparkles=river_sparkles(9527, 14),
        glints=[(0.30, 190), (0.68, 190)],
        river_half=0.095,
        streak=(0.020, -0.018, 225, 0.003),
 # stars riding the S-curve — the "stars scattered across the river" motif
        river_stars=[(0.22, 0.030), (0.52, 0.042), (0.82, 0.030)],
    )


def build_master(small, s, only=None):
    """Render one variant at supersample factor s (canvas s×s, logical OUT).

    only=None draws everything; "bg"/"river"/"stars" draw a single layer
    (used by --layers to export Icon Composer layer PNGs).
    """
    L = layout(small)
    k = float(s)  # unit coords are canvas fractions — scale straight to px
    in_bg = only in (None, "bg")
    in_river = only in (None, "river")
    in_stars = only in (None, "stars")

    # background: FULL-BLEED black square. macOS 26+ masks app icons to its
    # squircle itself — artwork that ships pre-rounded with alpha instead
 # gets shrunk onto a glass tile (light ring + inset artwork = the "white edge"
 # the user sees). Edge-to-edge is the documented canvas for the new design.
    icon = Image.new("RGBA", (s, s), BG_BOTTOM + (255,))
    icon.paste(v_gradient((s, s), BG_TOP, BG_BOTTOM), (0, 0))
    draw = ImageDraw.Draw(icon)

    if L["dots"] and in_stars:
        for (x, y, r, a) in L["dots"]:
            rr = r * k
            draw.ellipse([x * k - rr, y * k - rr, x * k + rr, y * k + rr], fill=SKY_DOT + (a,))

    # river: soft indigo under-glow, then horizontal brand-gradient band
    poly = river_polygon(s, L["river_half"])
    river_draw = ImageDraw.Draw(icon) if in_river else None
    if in_river:
        glow_layer = Image.new("RGBA", (s, s), (0, 0, 0, 0))
        ImageDraw.Draw(glow_layer).polygon(poly, fill=RIVER_R + (100,))
        glow_layer = glow_layer.filter(ImageFilter.GaussianBlur(0.016 * s))
        icon.alpha_composite(glow_layer)

        river_grad = h_gradient((s, s), RIVER_L, RIVER_R)
        band_mask = Image.new("L", (s, s), 0)
        ImageDraw.Draw(band_mask).polygon(poly, fill=255)
        icon.paste(river_grad, (0, 0), band_mask)

    # bright centerline streak — the "starlight" in the river
    w_st, y_off, a_st, blur_st = L["streak"]
    p0, p1, p2, p3 = [(x * s, (y + y_off) * s) for x, y in RIVER_PTS]
    pts = [bezier(p0, p1, p2, p3, i / 120) for i in range(121)]
    if in_river:
        streak = Image.new("RGBA", (s, s), (0, 0, 0, 0))
        ImageDraw.Draw(streak).line(pts, fill=STREAK + (a_st,), width=int(w_st * k), joint="curve")
        icon.alpha_composite(streak.filter(ImageFilter.GaussianBlur(blur_st * s)))

    if L["sparkles"] and in_river:
        sp = Image.new("RGBA", (s, s), (0, 0, 0, 0))
        dsp = ImageDraw.Draw(sp)
        for (x, y, r, a) in L["sparkles"]:
            rr = r * k
            dsp.ellipse([x * k - rr, y * k - rr, x * k + rr, y * k + rr], fill=STREAK + (a,))
        icon.alpha_composite(sp)
        gl = Image.new("RGBA", (s, s), (0, 0, 0, 0))
        dgl = ImageDraw.Draw(gl)
        for (t, a) in L["glints"]:
            x, y = bezier(*[(px * s, py * s) for px, py in RIVER_PTS], t)
            dgl.ellipse([x - 0.024 * s, y - 0.005 * s, x + 0.024 * s, y + 0.005 * s],
                        fill=STREAK + (a,))
        icon.alpha_composite(gl.filter(ImageFilter.GaussianBlur(0.002 * s)))

    # stars riding the S-curve — warm sparkles sitting on the water
    if L["river_stars"] and in_stars:
        rsl = Image.new("RGBA", (s, s), (0, 0, 0, 0))
        drs = ImageDraw.Draw(rsl)
        rp = [(x * s, y * s) for x, y in RIVER_PTS]
        for (t, r) in L["river_stars"]:
            x, y = bezier(*rp, t)
            g = Image.new("RGBA", (s, s), (0, 0, 0, 0))
            ImageDraw.Draw(g).polygon(star4_points(x, y, r * k * 1.6, 0.42),
                                      fill=(255, 240, 200, 130))
            rsl.alpha_composite(g.filter(ImageFilter.GaussianBlur(0.008 * s)))
            drs.polygon(star4_points(x, y, r * k, 0.42), fill=(255, 248, 228, 250))
        icon.alpha_composite(rsl)

    # accent star (large variant only)
    if L["accent"] and in_stars:
        ax, ay, ar = L["accent"]
        glow = Image.new("RGBA", (s, s), (0, 0, 0, 0))
        ImageDraw.Draw(glow).polygon(
            star4_points(ax * k, ay * k, ar * k * 1.5, 0.40), fill=ACCENT_STAR + (110,))
        icon.alpha_composite(glow.filter(ImageFilter.GaussianBlur(0.012 * s)))
        draw.polygon(star4_points(ax * k, ay * k, ar * k, 0.40), fill=ACCENT_STAR + (235,))

    # main star: amber glow behind, gold-tipped white star on top
    cx, cy, r, inner = L["star"]
    cx, cy, r = cx * k, cy * k, r * k
    if in_stars:
        glow = Image.new("RGBA", (s, s), (0, 0, 0, 0))
        ImageDraw.Draw(glow).polygon(
            star4_points(cx, cy, r * L["glow_scale"], inner), fill=GLOW_AMBER + (L["glow_alpha"],))
        icon.alpha_composite(glow.filter(ImageFilter.GaussianBlur(L["glow_blur"] * s)))

        star_mask = Image.new("L", (s, s), 0)
        ImageDraw.Draw(star_mask).polygon(star4_points(cx, cy, r, inner), fill=255)
        icon.paste(v_gradient((s, s), STAR_CORE, STAR_TIP), (0, 0), star_mask)
    return icon


def downscale(master, n):
    return master.resize((n, n), Image.Resampling.LANCZOS)


def png_bytes(img):
    buf = io.BytesIO()
    img.save(buf, "PNG")
    return buf.getvalue()


def write_ico(masters, path):
    """Multi-size ICO with PNG-compressed entries (Vista+)."""
    sizes = [16, 24, 32, 48, 64, 128, 256]
    blobs = []
    for n in sizes:
        m = masters["small"] if n < 128 else masters["large"]
        blobs.append((n, png_bytes(downscale(m, n))))
    header = struct.pack("<HHH", 0, 1, len(blobs))
    offset = 6 + 16 * len(blobs)
    entries = b""
    data = b""
    for n, blob in blobs:
        b = 0 if n >= 256 else n
        entries += struct.pack("<BBBBHHII", b, b, 0, 0, 1, 32, len(blob), offset)
        data += blob
        offset += len(blob)
    with open(path, "wb") as f:
        f.write(header + entries + data)
    print(f"  wrote {path} ({len(sizes)} sizes: {sizes[0]}..{sizes[-1]})")


ICNS_SPEC = [  # (filename, target px, uses small variant)
    ("icon_16x16.png", 16, True), ("icon_16x16@2x.png", 32, True),
    ("icon_32x32.png", 32, True), ("icon_32x32@2x.png", 64, True),
    ("icon_128x128.png", 128, False), ("icon_128x128@2x.png", 256, False),
    ("icon_256x256.png", 256, False), ("icon_256x256@2x.png", 512, False),
    ("icon_512x512.png", 512, False), ("icon_512x512@2x.png", 1024, False),
]


def write_icns(masters, path):
    if sys.platform != "darwin":
        print("  skipped icon.icns (iconutil only on macOS)")
        return
    with tempfile.TemporaryDirectory() as td:
        iconset = os.path.join(td, "icon.iconset")
        os.makedirs(iconset)
        for name, n, small in ICNS_SPEC:
            m = masters["small"] if small else masters["large"]
            downscale(m, n).save(os.path.join(iconset, name))
        subprocess.run(["iconutil", "-c", "icns", iconset, "-o", path], check=True)
    print(f"  wrote {path} ({len(ICNS_SPEC)} representations)")


def write_svg(path):
    """Vector reference built from the same geometry constants."""
    k = OUT  # unit → px
    L = layout(False)
    star = L["star"]
    river = "M %.1f %.1f C %.1f %.1f, %.1f %.1f, %.1f %.1f" % tuple(
        v * k for p in RIVER_PTS for v in p)
    pts = " ".join("%.1f,%.1f" % (x * k, y * k)
                   for x, y in star4_points(star[0] * k, star[1] * k, star[2] * k, star[3]))
    acc = L["accent"]
    acc_pts = " ".join("%.1f,%.1f" % (x, y)
                       for x, y in star4_points(acc[0] * k, acc[1] * k, acc[2] * k, 0.40))
    dots = "\n".join(
        f'  <circle cx="{x*k:.0f}" cy="{y*k:.0f}" r="{r*k:.0f}" fill="#d0e2ff" opacity="{a/255:.2f}"/>'
        for (x, y, r, a) in L["dots"])
    rst = "\n".join(
        '<polygon points="%s" fill="#fff8e4" opacity="0.96"/>' % " ".join(
            "%.1f,%.1f" % (px, py) for px, py in star4_points(
                bezier(*[(x*k, y*k) for x, y in RIVER_PTS], t)[0],
                bezier(*[(x*k, y*k) for x, y in RIVER_PTS], t)[1],
                r*k, 0.42))
        for (t, r) in L["river_stars"])
    svg = f'''<svg xmlns="http://www.w3.org/2000/svg" width="{OUT}" height="{OUT}" viewBox="0 0 {OUT} {OUT}">
  <!-- Starriver app icon master. Raster outputs (icon.png/.ico/.icns) are
       generated by make-logo.py from this same geometry — keep in sync. -->
  <defs>
    <linearGradient id="bg" x1="0" y1="0" x2="0" y2="1">
      <stop offset="0" stop-color="#0b0b10"/><stop offset="1" stop-color="#000"/>
    </linearGradient>
    <linearGradient id="river" x1="0" y1="0" x2="1" y2="0">
      <stop offset="0" stop-color="#67e8f9"/><stop offset="1" stop-color="#818cf8"/>
    </linearGradient>
    <linearGradient id="star" x1="0" y1="0" x2="0" y2="1">
      <stop offset="0" stop-color="#ffffff"/><stop offset="1" stop-color="#ffe19e"/>
    </linearGradient>
    <clipPath id="frame"><rect width="{OUT}" height="{OUT}"/></clipPath>
  </defs>
  <g clip-path="url(#frame)">
    <rect width="{OUT}" height="{OUT}" fill="url(#bg)"/>
{dots}
{rst}
    <path d="{river}" fill="none" stroke="url(#river)" stroke-width="{2*L['river_half']*k:.0f}" stroke-linecap="round"/>
    <path d="{river}" fill="none" stroke="#f0fdff" stroke-opacity="0.88"
          stroke-width="{L['streak'][0]*k:.0f}" stroke-linecap="round" transform="translate(0,{L['streak'][1]*k:.0f})"/>
    <polygon points="{acc_pts}" fill="#fff3d6" opacity="0.92"/>
    <polygon points="{pts}" fill="url(#star)"/>
  </g>
</svg>
'''
    with open(path, "w") as f:
        f.write(svg)
    print(f"  wrote {path}")


def export_layers():
    """Icon Composer layer PNGs (1024, full-canvas): bg / river / stars."""
    out_dir = os.path.join(ASSETS, "icon-layers")
    os.makedirs(out_dir, exist_ok=True)
    for name in ("1-background", "2-river", "3-stars"):
        only = {"1-background": "bg", "2-river": "river", "3-stars": "stars"}[name]
        img = build_master(False, S_LARGE, only=only).resize((OUT, OUT), Image.Resampling.LANCZOS)
        p = os.path.join(out_dir, f"{name}.png")
        img.save(p)
        print(f"  wrote {p}")


def main():
    if "--layers" in sys.argv:
        export_layers()
        return
    os.makedirs(ASSETS, exist_ok=True)
    print("rendering masters (4096px detail / 2048px simplified)...")
    masters = {"large": build_master(False, S_LARGE), "small": build_master(True, S_SMALL)}

    png_path = os.path.join(ASSETS, "icon.png")
    downscale(masters["large"], 1024).save(png_path)
    print(f"  wrote {png_path} (1024x1024)")

    write_ico(masters, os.path.join(ASSETS, "icon.ico"))
    write_icns(masters, os.path.join(ASSETS, "icon.icns"))
    write_svg(os.path.join(ASSETS, "icon.svg"))
    print("done")


if __name__ == "__main__":
    main()
