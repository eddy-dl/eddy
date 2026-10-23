import sys
from fontTools.ttLib import TTFont
from fontTools.pens.basePen import BasePen
class Flat(BasePen):
    def __init__(s, gs, dx): super().__init__(gs); s.pts=[]; s.dx=dx; s.cur=None
    def _moveTo(s,p): s.cur=p; s.pts.append((p[0]+s.dx,p[1]))
    def _lineTo(s,p):
        a=s.cur
        for t in range(1,11): s.pts.append((a[0]+(p[0]-a[0])*t/10+s.dx, a[1]+(p[1]-a[1])*t/10))
        s.cur=p
    def _curveToOne(s,p1,p2,p3):
        a=s.cur
        for t in range(1,21):
            t/=20; u=1-t
            x=u**3*a[0]+3*u*u*t*p1[0]+3*u*t*t*p2[0]+t**3*p3[0]
            y=u**3*a[1]+3*u*u*t*p1[1]+3*u*t*t*p2[1]+t**3*p3[1]
            s.pts.append((x+s.dx,y))
        s.cur=p3
    def _qCurveToOne(s,p1,p2):
        a=s.cur
        for t in range(1,21):
            t/=20; u=1-t
            s.pts.append((u*u*a[0]+2*u*t*p1[0]+t*t*p2[0]+s.dx, u*u*a[1]+2*u*t*p1[1]+t*t*p2[1]))
        s.cur=p2
f=TTFont(sys.argv[1]); gs=f.getGlyphSet(); cmap=f.getBestCmap()
x=0; pts=[]
for ch in "Edd":
    g=gs[cmap[ord(ch)]]; pen=Flat(gs,x); g.draw(pen); pts+=pen.pts; x+=g.width
step=40
for bx in range(0, 1760, step):
    ys=[p[1] for p in pts if bx<=p[0]<bx+step]
    print(bx, round(min(ys)) if ys else None)
