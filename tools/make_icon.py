"""Eddy's app icon: Pacifico's E, a tapered wave under it with leaning crests,
a whirlpool at its right end and a download arrow dropping into it, fitted
inside Android's 66dp safe circle.

    python tools/make_icon.py ui/fonts/Pacifico-Regular.ttf <out dir>

writes ic_launcher_foreground.xml, ic_launcher_monochrome.xml and logo.svg
(needs fontTools)."""
import math, sys
from fontTools.ttLib import TTFont
from fontTools.pens.svgPathPen import SVGPathPen
from fontTools.pens.transformPen import TransformPen
from fontTools.pens.boundsPen import BoundsPen

FONT, OUT = sys.argv[1], sys.argv[2]
font = TTFont(FONT); gs = font.getGlyphSet(); E = gs[font.getBestCmap()[ord('E')]]

# --- Layout in a free frame (y down), then fitted. Units: font units / 10.
# The E: its box is (31,-5)-(812,848) in font units.
def e_path(scale, dx, dy):
    pen = SVGPathPen(gs)
    # font (x, y up) -> icon (x*s+dx, -y*s+dy)
    E.draw(TransformPen(pen, (scale, 0, 0, -scale, dx, dy)))
    return pen.getCommands()

# Wave centreline in the free frame (E spans x 3..81, y -85..0 at scale 0.1).
def wave_line():
    pts = []
    x0, x1 = -6, 84
    for i in range(121):
        t = i / 120
        x = x0 + (x1 - x0) * t
        # Two crests rising towards the whirlpool, leaning right (below).
        y = 15 - 6.5 * math.sin(t * 2.3 * math.pi - 0.7) * (0.5 + 0.5 * t)
        pts.append((x, y))
    # Lean: higher points shift right.
    low = max(y for _, y in pts)
    pts = [(x + 0.9 * (low - y), y) for x, y in pts]
    return pts

R, TURNS = 19.0, 1.7          # the whirlpool's outer radius and turns
W = 8.5                       # the wave's thickness

def whirlpool(start, heading):
    """A spiral from `start`, carrying on the wave's heading, rolling up over
    the top and in to its eye. Its radius shrinks evenly, so the turns are
    evenly spaced (an Archimedean spiral)."""
    hx, hy = heading
    n = math.hypot(hx, hy); hx, hy = hx / n, hy / n
    # The centre is on the wave's left (above it), R away: the wave meets
    # the spiral's outer edge going the way it already goes.
    cx, cy = start[0] + hy * R, start[1] - hx * R
    a0 = math.atan2(start[1] - cy, start[0] - cx)
    return (cx, cy), a0

def spiral_points(centre, a0, t0, t1, offset=0.0, n=260):
    cx, cy = centre
    pts = []
    for i in range(n + 1):
        t = t0 + (t1 - t0) * i / n
        a = a0 + offset - t * TURNS * 2 * math.pi   # y down: decreasing = anticlockwise
        r = R * (1 - t) + 0.4 * t
        pts.append((cx + r * math.cos(a), cy + r * math.sin(a)))
    return pts

def ribbon_of(pts, width_at):
    """A filled outline around `pts`, `width_at(i)` wide at each."""
    top, bot = [], []
    for i, (x, y) in enumerate(pts):
        a = pts[max(i - 1, 0)]; b = pts[min(i + 1, len(pts) - 1)]
        dx, dy = b[0] - a[0], b[1] - a[1]; n = math.hypot(dx, dy) or 1
        nx, ny = -dy / n, dx / n
        w = width_at(i) / 2
        top.append((x + nx * w, y + ny * w)); bot.append((x - nx * w, y - ny * w))
    return top + bot[::-1]

line = wave_line()
hx = line[-1][0] - line[-2][0]; hy = line[-1][1] - line[-2][1]
centre, a0 = whirlpool(line[-1], (hx, hy))
main_spiral = spiral_points(centre, a0, 0, 1)[1:]
centreline = line + main_spiral
n_wave = len(line)
# Arm spacing per turn, and so the widest a turn can be and still show a gap.
GAP = R / TURNS
def main_width(i):
    if i < n_wave:
        t = i / (n_wave - 1)
        return W * min(1, (t / 0.35)) ** 0.6       # thin start, then full
    u = (i - n_wave) / (len(main_spiral) - 1)
    # Down from the wave's width to fit between the turns within a quarter
    # turn, then tapering steadily into the eye.
    arm = GAP * 0.42 * (1 - u) + 0.25
    if u < 0.15:
        k = u / 0.15
        return W * (1 - k) + (GAP * 0.42 * (1 - 0.15) + 0.25) * k
    return arm
ribbon = ribbon_of(centreline, main_width)

# The second arm: half a turn round, so it runs between the main arm's
# turns; it fades in, then tapers into the eye with it.
second = spiral_points(centre, a0, 0.5 / TURNS, 1, offset=math.pi, n=200)
def second_width(i):
    u = i / 200
    return (GAP * 0.30) * math.sin(math.pi * min(1, u / 0.35) / 2) * (1 - u) ** 0.9 + 0.1
inner = ribbon_of(second, second_width)

# A download arrow dropping into the whirlpool from above: its shaft and
# head, as a stroked line (round ends) over the spiral's top.
ARROW_W = 7.0
ax = centre[0] + 1.5
a_top, a_tip = centre[1] - R - 30, centre[1] - R - 6
arrow = [(ax, a_top), (ax, a_tip)]
head = [(ax - 9, a_tip - 9), (ax, a_tip), (ax + 9, a_tip - 9)]

# --- Fit everything (E box corners + ribbon) inside a circle of radius 30.5
# around (54, 54), the 66dp safe zone with a margin.
E_SCALE = 0.1
# The E sits left of the whirlpool.
E_DX = -12
e_box = [(31 * E_SCALE + E_DX, -848 * E_SCALE), (812 * E_SCALE + E_DX, -848 * E_SCALE), (31 * E_SCALE + E_DX, 5 * E_SCALE), (812 * E_SCALE + E_DX, 5 * E_SCALE)]
allpts = ribbon + e_box + inner + arrow + head + [(ax - 12, a_tip - 12), (ax + 12, a_tip - 12), (ax, a_top - 3)]
minx = min(p[0] for p in allpts); maxx = max(p[0] for p in allpts)
miny = min(p[1] for p in allpts); maxy = max(p[1] for p in allpts)
cx0, cy0 = (minx + maxx) / 2, (miny + maxy) / 2
def fit(k):
    return [((x - cx0) * k + 54, (y - cy0) * k + 54) for x, y in allpts]
k = 1.0
while max(math.dist(p, (54, 54)) for p in fit(k)) < 30.5:
    k *= 1.01
while max(math.dist(p, (54, 54)) for p in fit(k)) > 30.5:
    k *= 0.995
tx = lambda x: (x - cx0) * k + 54
ty = lambda y: (y - cy0) * k + 54
ribbon_d = "M" + " L".join(f"{tx(x):.2f} {ty(y):.2f}" for x, y in ribbon) + " Z" \
    + " M" + " L".join(f"{tx(x):.2f} {ty(y):.2f}" for x, y in inner) + " Z"
e_d = e_path(E_SCALE * k, tx(E_DX), ty(0))
arrow_d = (f"M{tx(arrow[0][0]):.2f} {ty(arrow[0][1]):.2f} L{tx(arrow[1][0]):.2f} {ty(arrow[1][1]):.2f} "
           f"M{tx(head[0][0]):.2f} {ty(head[0][1]):.2f} L{tx(head[1][0]):.2f} {ty(head[1][1]):.2f} L{tx(head[2][0]):.2f} {ty(head[2][1]):.2f}")
arrow_w = ARROW_W * k

grad = ('<aapt:attr name="android:fillColor">\n'
        '                <gradient android:type="linear" android:startX="{x0:.1f}" android:startY="{y0:.1f}" android:endX="{x1:.1f}" android:endY="{y1:.1f}">\n'
        '                    <item android:offset="0" android:color="#FFFFFFFF" />\n'
        '                    <item android:offset="1" android:color="#FFB8DCFF" />\n'
        '                </gradient>\n'
        '            </aapt:attr>')
rx = [tx(x) for x, _ in ribbon]; ry = [ty(y) for _, y in ribbon]
g = grad.format(x0=min(rx), y0=min(ry), x1=max(rx), y1=max(ry))

def vector(comment, ribbon_fill, mono):
    ns = ' xmlns:aapt="http://schemas.android.com/aapt"' if not mono else ''
    rib = (f'<path android:pathData="{ribbon_d}">\n            {ribbon_fill}\n        </path>' if not mono
           else f'<path android:pathData="{ribbon_d}" android:fillColor="#FFFFFFFF" />')
    return f'''<?xml version="1.0" encoding="utf-8"?>
<!-- {comment} Generated by tools/make_icon.py (Pacifico's E, SIL OFL); keep within the 66dp safe zone. -->
<vector xmlns:android="http://schemas.android.com/apk/res/android"{ns}
    android:width="108dp"
    android:height="108dp"
    android:viewportWidth="108"
    android:viewportHeight="108">

    <path android:pathData="{e_d}" android:fillColor="#FFFFFFFF" />
    {rib}
    <path
        android:pathData="{arrow_d}"
        android:strokeWidth="{arrow_w:.2f}"
        android:strokeColor="#FFFFFFFF"
        android:strokeLineCap="round"
        android:strokeLineJoin="round" />
</vector>
'''
open(f"{OUT}/ic_launcher_foreground.xml", "w").write(vector("Launcher icon: Eddy's script E over a wave that ends in a whirlpool, a download arrow dropping into it.", g, False))
open(f"{OUT}/ic_launcher_monochrome.xml", "w").write(vector("Themed (monochrome) launcher icon; the system picks the colour.", None, True))
svg = f'''<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 108 108" width="108" height="108">
  <!-- Eddy's icon: Pacifico's E (SIL OFL) over a wave ending in a whirlpool.
       Generated with the launcher icon (same shapes). -->
  <defs>
    <linearGradient id="bg-grad" x1="0%" y1="0%" x2="100%" y2="100%">
      <stop offset="0%" stop-color="#4F8DF7" />
      <stop offset="100%" stop-color="#2447C9" />
    </linearGradient>
    <linearGradient id="wave-grad" gradientUnits="userSpaceOnUse" x1="{min(rx):.1f}" y1="{min(ry):.1f}" x2="{max(rx):.1f}" y2="{max(ry):.1f}">
      <stop offset="0" stop-color="#FFFFFF" />
      <stop offset="1" stop-color="#B8DCFF" />
    </linearGradient>
  </defs>
  <rect width="108" height="108" rx="24" fill="url(#bg-grad)" />
  <g transform="translate(54 54) scale(1.45) translate(-54 -54)">
    <path d="{e_d}" fill="#FFFFFF" />
    <path d="{ribbon_d}" fill="url(#wave-grad)" />
    <path d="{arrow_d}" fill="none" stroke="#FFFFFF" stroke-width="{arrow_w:.2f}" stroke-linecap="round" stroke-linejoin="round" />
  </g>
</svg>
'''
open(f"{OUT}/logo.svg", "w").write(svg)
print("scale", round(k, 3), "whirlpool centre", round(tx(centre[0]), 1), round(ty(centre[1]), 1))
