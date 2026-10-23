import math, re, sys
# Lowest ink of "Edd" every 40 units (envelope.py), font units, y up.
env = dict(map(int, l.split()) for l in open(sys.argv[1]).read().strip().splitlines() if 'None' not in l)
PAD, DEPTH, DROP = 110, 1.4, 130   # gap under the letters, curve depth, the wave's drop
LEAN = 0.55                        # crests shift right by this much of their height
THICK = 95                         # widest, mid-wave
# Centreline (y down from the baseline), following the letters' bottoms.
xs = [x for x in sorted(env) if 120 <= x <= 1620]
pts = [(x + 20.0, PAD - env[x] * DEPTH + DROP) for x in xs]
# Catmull-Rom densified, so the lean and offsets stay smooth.
def dense(p, n=8):
    out = []
    for i in range(len(p) - 1):
        p0 = p[max(i - 1, 0)]; p1 = p[i]; p2 = p[i + 1]; p3 = p[min(i + 2, len(p) - 1)]
        for k in range(n):
            t = k / n; t2 = t * t; t3 = t2 * t
            out.append(tuple(0.5 * ((2 * p1[j]) + (-p0[j] + p2[j]) * t + (2 * p0[j] - 5 * p1[j] + 4 * p2[j] - p3[j]) * t2 + (-p0[j] + 3 * p1[j] - 3 * p2[j] + p3[j]) * t3) for j in (0, 1)))
    out.append(p[-1])
    return out
line = dense(pts)
# Lean: higher points (smaller y) move right, so each crest's back is gentle
# and its front steep.
low = max(y for _, y in line)
line = [(x + LEAN * (low - y), y) for x, y in line]
# Curl: the last crest rolls up and over to the left, tightening.
ex, ey = line[-1]
cx, cy, r0 = ex - 10, ey - 80, 82
for k in range(1, 40):
    a = math.pi / 2 - k / 39 * 1.45 * math.pi   # from below the centre, up the right, over the top
    r = r0 * (1 - 0.45 * k / 39)
    line.append((cx + r * math.cos(a), cy + r * math.sin(a)))
# Arc length along it, for the taper.
L = [0.0]
for a, b in zip(line, line[1:]):
    L.append(L[-1] + math.dist(a, b))
total = L[-1]
def width(s):
    u = s / total
    # Thin at the start, widest past the middle, holding into the curl and
    # thinning to its lip.
    return THICK * math.sin(math.pi * u ** 0.75) ** 0.7
top, bottom = [], []
for i, (x, y) in enumerate(line):
    a = line[max(i - 1, 0)]; b = line[min(i + 1, len(line) - 1)]
    dx, dy = b[0] - a[0], b[1] - a[1]; n = math.hypot(dx, dy) or 1
    nx, ny = -dy / n, dx / n
    w = width(L[i]) / 2
    top.append((x + nx * w, y + ny * w)); bottom.append((x - nx * w, y - ny * w))
outline = top + bottom[::-1]
# The light on its crests: the top edge down to just above the middle.
mid = []
for i, (x, y) in enumerate(line):
    a = line[max(i - 1, 0)]; b = line[min(i + 1, len(line) - 1)]
    dx, dy = b[0] - a[0], b[1] - a[1]; n = math.hypot(dx, dy) or 1
    nx, ny = -dy / n, dx / n
    w = width(L[i]) * 0.12
    mid.append((x + nx * w, y + ny * w))
crest = top + mid[::-1]
def path(points):
    return "M " + " L ".join(f"{x:.0f} {y:.0f}" for x, y in points) + " Z"
p = sys.argv[2]; s = open(p).read()
s = re.sub(r'property <string> wave: "[^"]*";', 'property <string> wave: "' + path(outline) + '";', s)
s = re.sub(r'property <string> crest: "[^"]*";', 'property <string> crest: "' + path(crest) + '";', s)
open(p, 'w').write(s)
print(len(outline), 'points; x', round(min(x for x, _ in outline)), round(max(x for x, _ in outline)), 'y', round(min(y for _, y in outline)), round(max(y for _, y in outline)))
