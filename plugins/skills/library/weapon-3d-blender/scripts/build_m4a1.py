"""build_m4a1.py - realistic M4A1 carbine (KAC M4 RAS, M68 CCO red dot, folding BUIS) - weapon-3d skill.
blender -b --factory-startup -P build_m4a1.py -- --out m4a1_v01.blend --stage 2
Modelled in a 'gun frame' (mm): X forward (muzzle), Y left, Z up, bore axis = X axis, X=0 at upper receiver
front face. Side outlines of stock / grip / magazine traced from the reference (m4a1_outlines.json).
Final orientation (skill convention): muzzle -> +Z, top -> -X, right side -> -Y, origin = pistol grip centre."""
import bpy, sys, os, math, json, importlib
import numpy as np
from mathutils import Matrix
HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import wlib; importlib.reload(wlib)
from wlib import *

argv = sys.argv[sys.argv.index('--') + 1:] if '--' in sys.argv else []
def arg(name, default):
    return argv[argv.index(name) + 1] if name in argv else default
OUT = arg('--out', os.path.join(HERE, '..', 'm4a1', 'm4a1_v01.blend'))
STAGE = int(arg('--stage', '2'))
MM = 0.001
PI = math.pi
OUTL = json.load(open(os.path.join(HERE, 'm4a1_outlines.json')))

sc = reset_scene()
C = coll('m4a1')
MATS = {}

# =============================================================== geometry helpers (mm, gun frame)
def smooth_poly(P, iters=2, keep_deg=38.0):
    P = np.asarray(P, float)
    for _ in range(iters):
        n = len(P); Q = P.copy()
        for i in range(n):
            a, b, c = P[i - 1], P[i], P[(i + 1) % n]
            v1, v2 = b - a, c - b
            d = np.linalg.norm(v1) * np.linalg.norm(v2)
            if d < 1e-9: continue
            ang = math.degrees(math.acos(max(-1.0, min(1.0, float(np.dot(v1, v2) / d)))))
            if ang < keep_deg:
                Q[i] = 0.25 * a + 0.5 * b + 0.25 * c
        P = Q
    return P

def rotx(v, deg):
    a = math.radians(deg); c, s = math.cos(a), math.sin(a)
    v = np.asarray(v, float).copy(); y, z = v[:, 1].copy(), v[:, 2].copy()
    v[:, 1] = y * c - z * s; v[:, 2] = y * s + z * c
    return v
def roty(v, deg, cx=0.0, cz=0.0):
    a = math.radians(deg); c, s = math.cos(a), math.sin(a)
    v = np.asarray(v, float).copy(); x, z = v[:, 0] - cx, v[:, 2] - cz
    v[:, 0] = cx + x * c + z * s; v[:, 2] = cz - x * s + z * c
    return v
def T(vf, d=(0, 0, 0), rx=0.0):
    v, f = vf; v = np.asarray(v, float) + np.array(d, float)
    if rx: v = rotx(v, rx)
    return v, f
def lathe_x(prof_xr, seg=48, y0=0.0, z0=0.0, phase=0.0):
    v, f = lathe([(r, x) for x, r in prof_xr], seg, phase)
    v = v[:, [2, 0, 1]].copy(); v[:, 1] += y0; v[:, 2] += z0
    return v, f
def lathe_y(prof_yr, seg=48, x0=0.0, z0=0.0):
    v, f = lathe([(r, y) for y, r in prof_yr], seg)
    v = v[:, [0, 2, 1]].copy(); v[:, 0] += x0; v[:, 2] += z0
    return v, f
def lathe_z(prof_zr, seg=48, x0=0.0, y0=0.0):
    v, f = lathe([(r, z) for z, r in prof_zr], seg)
    v = v.copy(); v[:, 0] += x0; v[:, 1] += y0
    return v, f
def prism_xz(poly, y0, y1): return prism(poly, y0, y1)
def prism_yz(poly, x0, x1):
    v, f = prism(poly, x0, x1); return v[:, [1, 0, 2]].copy(), f
def prism_xy(poly, z0, z1):
    v, f = prism(poly, z0, z1); return v[:, [0, 2, 1]].copy(), f
def slab(poly_uv, origin, eu, ev, en, h0, h1):
    poly = np.asarray(poly_uv, float); n = len(poly)
    o, eu, ev, en = (np.array(a, float) for a in (origin, eu, ev, en))
    base = o + poly[:, :1] * eu + poly[:, 1:2] * ev
    v = np.vstack([base + h0 * en, base + h1 * en])
    f = [(i, (i + 1) % n, n + (i + 1) % n, n + i) for i in range(n)] + [tuple(range(n))[::-1], tuple(range(n, 2 * n))]
    return v, f
def stadium(L, W, seg=8, c=(0, 0)):
    r = W / 2; cx = max(L / 2 - r, 0)
    pts = [(c[0] + cx + r * math.cos(-PI / 2 + PI * k / seg), c[1] + r * math.sin(-PI / 2 + PI * k / seg)) for k in range(seg + 1)]
    pts += [(c[0] - cx + r * math.cos(PI / 2 + PI * k / seg), c[1] + r * math.sin(PI / 2 + PI * k / seg)) for k in range(seg + 1)]
    return pts
def circle(r, n=32, c=(0, 0), a0=0.0, a1=2 * PI, closed=True):
    m = n if closed else n + 1
    return [(c[0] + r * math.cos(a0 + (a1 - a0) * k / n), c[1] + r * math.sin(a0 + (a1 - a0) * k / n)) for k in range(m)]
def rrect(x0, x1, y0, y1, r, seg=4):
    pts = []
    for cx, cy, a in ((x1 - r, y0 + r, -PI / 2), (x1 - r, y1 - r, 0), (x0 + r, y1 - r, PI / 2), (x0 + r, y0 + r, PI)):
        for k in range(seg + 1):
            t = a + (PI / 2) * k / seg; pts.append((cx + r * math.cos(t), cy + r * math.sin(t)))
    return pts
def star(r0, r1, n, c=(0, 0)):
    pts = []
    for k in range(2 * n):
        r = r0 if k % 2 == 0 else r1; a = PI * k / n
        pts.append((c[0] + r * math.cos(a), c[1] + r * math.sin(a)))
    return pts
def knurl_ring(r_out, depth, n, w_frac=0.5, sub=2):
    """flat-topped ribs (gear-like) for knobs: list of (u,v) around origin"""
    pts = []
    for k in range(n):
        a0 = 2 * PI * k / n; da = 2 * PI / n
        for j, (fa, r) in enumerate(((0.0, r_out - depth), (0.12, r_out), (w_frac - 0.12, r_out), (w_frac, r_out - depth))):
            a = a0 + fa * da; pts.append((r * math.cos(a), r * math.sin(a)))
    return pts

def make(name, parts, mat, bev=0.0, seg=2, angle=30, sharp=30, wn=True, smooth=True):
    v, f = combine(*parts) if isinstance(parts, list) else parts
    ob = mk(name, np.asarray(v, float) * MM, f, C, smooth=smooth, sharp=sharp)
    if bev > 0:
        bevel(ob, bev * MM, seg, angle)
        if wn: wnormal(ob)
    MATS[name] = mat
    return ob
def cut(ob, parts, op='DIFFERENCE', self_inter=True):
    # cutter pieces are just concatenated -> they may overlap -> EXACT solver needs use_self
    v, f = combine(*parts) if isinstance(parts, list) else parts
    c = mk(ob.name + '_cut', np.asarray(v, float) * MM, f, C, smooth=False)
    boolean(ob, c, op, self_inter=self_inter)
    return ob

def pica_rail(x0, x1, pitch=10.01, slot=5.23, depth=3.0):
    """MIL-STD-1913 profile (y,z) from base z=0; returns (rail_vf, [slot cutters])."""
    sec = [(-7.6, 0), (7.6, 0), (7.6, 3.65), (10.6, 6.65), (7.85, 9.4), (-7.85, 9.4), (-10.6, 6.65), (-7.6, 3.65)]
    rail = prism_yz(sec, x0, x1)
    L = x1 - x0; n = int((L - 4.0 + (pitch - slot)) // pitch)
    span = n * pitch - (pitch - slot)
    s0 = x0 + (L - span) / 2
    cuts = [box_vf((s0 + k * pitch + slot / 2, 0, 9.4 - depth + 2.5), (slot, 26, 5.0)) for k in range(n)]
    return rail, cuts

def edge_x(poly, z):
    P = np.asarray(poly, float); xs = []
    for i in range(len(P)):
        a, b = P[i - 1], P[i]
        if (a[1] - z) * (b[1] - z) <= 0 and abs(b[1] - a[1]) > 1e-9:
            t = (z - a[1]) / (b[1] - a[1]); xs.append(a[0] + t * (b[0] - a[0]))
    return (min(xs), max(xs)) if xs else (None, None)

# =============================================================== UPPER RECEIVER (A4 flat-top)
UP_X0, UP_X1 = -187.0, 0.0
up_sec = [(-15, -16), (15, -16), (15, 11), (13.6, 15.5), (10.5, 19.6), (8, 21), (-8, 21), (-10.5, 19.6), (-13.6, 15.5), (-15, 11)]
FA_Y, FA_Z = -13.0, 6.5
upper = make('upper_receiver', prism_yz(up_sec, UP_X0, UP_X1), 'anod', bev=0.8, seg=3)
# forward-assist housing + brass deflector are part of the forging -> UNION first, the live bevel then fillets the junctions
cut(upper, [lathe_x([(-124.5, 0), (-124.5, 8.6), (-101, 8.6), (-95, 6.8), (-92.0, 5.2), (-92.0, 0)], 40, FA_Y, FA_Z),
            prism_xz([(-124.5, -1.0), (-96, -1.0), (-92, 3.0), (-92, 10.0), (-96, 14.0), (-124.5, 14.0)], -15.5, -13.0),
            prism_xz([(-103.5, 1.0), (-91.0, 1.0), (-91.0, 18.5), (-96.5, 18.5), (-103.5, 8.5)], -19.8, -13.5)], op='UNION')
cut(upper, [box_vf((-53, -14.5, 2.25), (74, 5, 20.5))])                          # ejection port (right side)
make('dust_cover', [prism_xz(rrect(-89.4, -16.6, -7.6, 11.9, 1.2), -15.25, -13.6),
                    prism_xz(rrect(-58.5, -50.5, 5.0, 10.5, 1.0), -16.3, -15.0)], 'anod', bev=0.4)
make('dust_cover_rod', lathe_x([(-91.5, 0), (-91.5, 1.3), (-14.5, 1.3), (-14.5, 0)], 16, -14.4, -8.6), 'phos')
make('forward_assist_plunger', lathe_x([(-134.2, 0), (-134.2, 5.6), (-133.6, 6.2), (-131.8, 6.2), (-131.8, 5.6), (-130.6, 5.6), (-130.6, 6.2),
                                        (-128.8, 6.2), (-128.8, 5.6), (-127.6, 5.6), (-127.6, 6.2), (-124.0, 6.2), (-124.0, 0)], 40, FA_Y, FA_Z),
     'phos', bev=0.25)
make('charging_handle', [box_vf((-193.3, 0, 17.0), (9.6, 35, 8.0)), box_vf((-193.3, 18.5, 17.0), (8.0, 6.0, 6.5))], 'anod', bev=1.2, seg=3)
r_up, c_up = pica_rail(UP_X0 + 0.5, UP_X1 - 1.0)
rail_up = make('rail_upper', T(r_up, (0, 0, 21.0)), 'anod', bev=0.3)
cut(rail_up, [T(c, (0, 0, 21.0)) for c in c_up])

# =============================================================== LOWER RECEIVER
LOW = [(-187, 16), (-187, -16), (-3, -16), (0, -17.5), (2.2, -20.5), (3.0, -24.0), (1.0, -26.3), (-3.1, -27.9), (-7.0, -29.4),
       (-10.1, -33.3), (-10.8, -34.8), (-10.8, -60.4), (-8.0, -64.5), (-7.7, -66.6), (-10.1, -66.8), (-13.6, -70.2), (-76.0, -81.3),
       (-80.5, -80.5), (-80.8, -50.0), (-125.5, -50.0), (-127.0, -55.0), (-130.5, -61.5), (-133.5, -61.5), (-136.4, -57.0),
       (-136.6, -49.5), (-172.6, -50.3), (-174.9, -44.1), (-180.3, -37.9), (-184.2, -35.6), (-190.4, -34.1), (-197.4, -34.1),
       (-198.9, -31.0), (-198.9, 10.0), (-197.0, 14.0), (-193.0, 16.0)]
lower = make('lower_receiver', prism_xz(LOW, -11.5, 11.5), 'anod', bev=0.8, seg=3)
MW = [(-10.8, -24.0), (-10.8, -60.4), (-8.0, -64.5), (-7.7, -66.6), (-10.1, -66.8), (-13.6, -70.2), (-76.0, -81.3), (-80.5, -80.5), (-80.8, -24.0)]
LIP = [(-9.2, -62.4), (-8.0, -64.5), (-7.7, -66.6), (-10.1, -66.8), (-13.6, -70.2), (-76.0, -81.3), (-80.5, -80.5), (-80.6, -75.8), (-76.0, -76.4), (-13.2, -65.3)]
cut(lower, [prism_xz(MW, -14.0, 14.0), prism_xz(LIP, -14.9, 14.9)], op='UNION')     # magwell walls + flared lip (one forging)
cut(lower, [prism_xz([(-14.2, -40), (-14.2, -72.0), (-75.8, -83.0), (-77.2, -40)], -11.6, 11.6)])  # mag well opening
# mag catch: lower half-ring fence + button (right side)
MC = (-85.0, -34.0)
fence = [(MC[0] + 8.8 * math.cos(a), MC[1] + 8.8 * math.sin(a)) for a in np.linspace(PI * 1.02, PI * 1.98, 14)] + \
        [(MC[0] + 6.0 * math.cos(a), MC[1] + 6.0 * math.sin(a)) for a in np.linspace(PI * 1.98, PI * 1.02, 14)]
make('mag_catch_fence', prism_xz(fence, -14.6, -11.0), 'anod', bev=0.8, seg=3)
make('mag_catch_button', [lathe_y([(-15.3, 0), (-15.3, 4.6), (-14.9, 5.0), (-11.0, 5.0), (-11.0, 0)], 32, MC[0], MC[1])], 'phos', bev=0.0)
make('bolt_catch', prism_xz([(-90.5, -18.5), (-78.0, -18.5), (-76.0, -22.0), (-77.5, -31.0), (-88.0, -31.0), (-90.5, -27.0)], 11.0, 13.9), 'phos', bev=0.6)
SEL = (-146.0, -33.0)
make('selector_lever', [prism_xz(circle(5.4, 28, SEL), 11.0, 14.6),
                        prism_xz([(SEL[0], SEL[1] + 3.6), (-131.0, SEL[1] + 1.8), (-129.4, SEL[1]), (-131.0, SEL[1] - 1.8), (SEL[0], SEL[1] - 3.6)], 11.0, 14.0)],
     'phos', bev=0.5)
make('selector_right_end', prism_xz(circle(3.8, 24, SEL), -12.3, -11.0), 'phos', bev=0.3)
pins = []
for (px, pz), r_r, r_l in (((-5.5, -22.0), 3.0, 4.5), ((-168.0, -20.6), 3.0, 4.5), ((-97.0, -31.0), 2.2, 2.2), ((-117.0, -38.5), 2.2, 2.2)):
    pins.append(prism_xz(circle(r_r, 24, (px, pz)), -12.1, -11.0))
    pins.append(lathe_y([(11.0, 0), (11.0, r_l), (12.0, r_l), (12.6, r_l * 0.7), (12.8, 0)], 24, px, pz))
make('receiver_pins', pins, 'phos', bev=0.0)
TG = [(-80.4, -66.0), (-80.4, -80.6), (-84.0, -81.6), (-124.0, -81.4), (-133.5, -81.2), (-137.4, -79.4), (-137.2, -72.0), (-135.2, -63.0),
      (-132.0, -62.0), (-131.2, -66.0), (-132.4, -76.6), (-124.0, -77.4), (-86.5, -77.4), (-84.6, -75.5), (-84.6, -66.0)]
make('trigger_guard', prism_xz(TG, -4.5, 4.5), 'anod', bev=0.9, seg=3)
TRG = [(-101.5, -49.0), (-101.8, -55.0), (-103.0, -61.0), (-105.3, -66.5), (-108.6, -70.5), (-110.3, -70.0), (-108.8, -65.8), (-107.3, -60.5), (-106.6, -55.0), (-106.6, -49.0)]
make('trigger', prism_xz(TRG, -2.4, 2.4), 'phos', bev=0.6, seg=2)

# =============================================================== PISTOL GRIP (A2)
GRIP = clean_outline(OUTL['grip'], 0.6, 75, 3)
grip = make('pistol_grip', pillow(GRIP, 14.3, 9.0, 1.3, 7), 'grip', bev=0.0, sharp=85)   # rounded (pillow) not prism+bevel

# =============================================================== MAGAZINE (30 rd, aluminium)
MAG = clean_outline(OUTL['mag'], 0.8, 60, 3)
make('magazine', prism_xz(MAG, -11.3, 11.3), 'mag', bev=1.6, seg=2)
ribs = []
for t in (0.30, 0.64):
    zs = np.linspace(-84.0, -160.0, 16); L_, R_ = [], []
    for z in zs:
        xr, xf = edge_x(MAG, z)
        xc = xr + t * (xf - xr)
        zb = -186.0 + (xc + 50.0) * (16.0 / 56.0) + 13.0
        if z < zb: break
        L_.append((xc - 3.2, z)); R_.append((xc + 3.2, z))
    poly = L_ + R_[::-1]
    for s in (1, -1):
        ribs.append(prism_xz(poly, s * 10.9, s * 12.0))
make('magazine_ribs', ribs, 'mag', bev=0.45, seg=3)
R0, F0 = np.array([-50.6, -186.3]), np.array([8.0, -168.4])
dd = (F0 - R0) / np.linalg.norm(F0 - R0); nn = np.array([-dd[1], dd[0]])
FP = [R0 - 1.4 * dd, F0 + 1.4 * dd, F0 + 1.4 * dd + 3.6 * nn, R0 - 1.4 * dd + 3.6 * nn]
make('magazine_floorplate', prism_xz([tuple(p) for p in FP], -12.4, 12.4), 'mag', bev=0.9, seg=2)

# =============================================================== BARREL, GAS TUBE, FLASH HIDER
make('barrel', lathe_x([(2.0, 0), (2.0, 9.5), (226.0, 9.5), (226.0, 7.8), (261.0, 7.8), (261.8, 7.3), (262.8, 6.3), (281.5, 6.3),
                        (282.5, 7.3), (283.5, 7.5), (333.5, 7.5), (333.5, 0)], 48), 'phos', bev=0.0)
make('gas_tube', lathe_x([(-6.0, 0), (-6.0, 2.4), (188.0, 2.4), (188.0, 0)], 16, 0.0, 13.6), 'phos')
make('crush_washer', lathe_x([(333.4, 0), (333.4, 8.9), (335.8, 9.2), (335.8, 0)], 48), 'phos')
fh = make('flash_hider', lathe_x([(335.8, 0), (335.8, 8.6), (337.6, 8.6), (338.2, 9.2), (339.2, 9.2), (339.6, 8.4), (341.6, 8.4), (342.0, 9.2),
                                  (344.6, 9.2), (345.0, 8.4), (347.0, 8.4), (347.4, 9.2), (376.6, 9.2), (379.3, 8.0), (379.3, 0)], 64), 'phos', bev=0.0)
fh_cuts = [lathe_x([(352.0, 0), (352.0, 6.4), (380.0, 6.4), (380.0, 0)], 32), lathe_x([(330.0, 0), (330.0, 2.9), (381.0, 2.9), (381.0, 0)], 24)]
for k, ang in enumerate((90, 30, -30, 150, 210)):          # 5 slots, closed at the bottom (A2 birdcage)
    a = math.radians(ang); en = (0, math.cos(a), math.sin(a)); ev = (0, -math.sin(a), math.cos(a))
    fh_cuts.append(slab(stadium(19.0, 2.6, 6), (366.4, 0, 0), (1, 0, 0), ev, en, 4.0, 12.0))
cut(fh, fh_cuts)
shade(fh, True, 35)

# =============================================================== FRONT SIGHT BASE (A2)
fsb_parts = [lathe_x([(182.5, 0), (182.5, 10.6), (183.5, 11.4), (225.0, 11.4), (226.2, 10.4), (226.2, 0)], 48),
             prism_xz([(184.0, 0.0), (226.0, 0.0), (226.0, 13.0), (224.8, 19.0), (186.5, 19.0), (184.0, 16.0)], -9.6, 9.6),
             prism_xz([(214.4, -6.0), (215.2, -10.8), (216.7, -13.2), (216.7, -21.7), (223.7, -21.7), (226.0, -18.6), (233.7, -17.8),
                       (233.7, -13.2), (230.0, -11.6), (226.0, -6.0)], -4.6, 4.6)]
EAR_O = [(178.8, 14.0), (180.3, 23.2), (183.4, 27.1), (188.1, 33.3), (193.5, 41.8), (197.4, 47.2), (202.8, 53.4), (205.9, 58.0), (209.4, 62.0),
         (212.8, 63.3), (217.2, 63.0), (219.8, 61.1), (221.0, 57.5), (222.1, 41.8), (222.9, 33.3), (223.7, 24.0), (224.5, 14.0)]
EAR_I = [(190.2, 21.5), (217.6, 21.5), (217.8, 49.0), (216.4, 52.6), (213.4, 53.6), (210.6, 51.4)]
for s in (1, -1):
    fsb_parts.append(prism_xz(smooth_poly(EAR_O, 1, 40), s * 5.6, s * 9.4))
fsb = make('front_sight_base', fsb_parts, 'phos', bev=0.7, seg=2)
cut(fsb, [prism_xz(smooth_poly(EAR_I, 1, 50), -12, 12)])
make('front_sight_post', [lathe_z([(18.0, 0), (18.0, 3.6), (22.5, 3.6), (23.0, 2.6), (23.0, 0)], 24, 211.4, 0.0),
                          prism_xz([(210.3, 22.5), (212.5, 22.5), (212.5, 43.8), (211.4, 45.2), (210.3, 43.8)], -0.95, 0.95)], 'phos', bev=0.2)
make('fsb_taper_pins', [prism_xz(circle(1.7, 16, c), s * 11.4, s * 11.9) for c in ((189.0, 3.0), (220.0, 3.0)) for s in (1, -1)], 'phos')
make('handguard_cap', lathe_x([(175.6, 0), (175.6, 19.5), (177.0, 21.0), (181.0, 21.0), (182.6, 19.0), (182.6, 0)], 48), 'phos', bev=0.0)

# =============================================================== DELTA RING / BARREL NUT
make('barrel_nut', lathe_x([(-1.0, 0), (-1.0, 19.6), (4.0, 19.6), (4.0, 0)], 48), 'phos')
dr = make('delta_ring', lathe_x([(3.2, 0), (3.2, 22.0), (3.6, 24.3), (4.6, 25.2), (6.0, 25.2), (21.5, 22.4), (22.2, 21.6), (22.2, 0)], 72), 'anod', bev=0.0)
cut(dr, [slab([(0, -0.55), (14.5, -0.55), (14.5, 0.55), (0, 0.55)], (6.2, 0, 0), (1, 0, 0), (0, -math.sin(a), math.cos(a)),
              (0, math.cos(a), math.sin(a)), 21.6, 27.0) for a in np.linspace(0, 2 * PI, 48, endpoint=False)])
shade(dr, True, 40)

# =============================================================== KAC M4 RAS (quad rail handguard)
RA, RC = 20.5, 14.2                 # flat half-width, chamfer start
oct_ = [(RA, -RC), (RA, RC), (RC, RA), (-RC, RA), (-RA, RC), (-RA, -RC), (-RC, -RA), (RC, -RA)]
RX0, RX1 = 22.0, 171.0
ras = make('ras_handguard', prism_yz(oct_, RX0, RX1), 'anod', bev=0.8)
cut(ras, [lathe_x([(RX0 - 1, 0), (RX0 - 1, 17.6), (RX1 + 1, 17.6), (RX1 + 1, 0)], 48)])   # bore first (cutters must not overlap)
vent_cuts = []
d45 = (RA + RC) / math.sqrt(2) / 1.0
for sy, sz in ((-1, 1), (-1, -1), (1, 1), (1, -1)):
    en = np.array([0, sy, sz]) / math.sqrt(2); ev = np.array([0, -sz, sy]) / math.sqrt(2)
    xs = [37.3 + 21.2 * k for k in range(7)] if sz > 0 else [36.4 + 18.6 * k for k in range(8)]
    Lv = 15.0 if sz > 0 else 12.0
    for xc in xs:
        vent_cuts.append(slab(stadium(Lv, 6.4, 8), (xc, 0, 0), (1, 0, 0), ev, en, 12.0, 30.0))
cut(ras, vent_cuts)
for nm, ang in (('rail_ras_top', 0), ('rail_ras_right', 90), ('rail_ras_bottom', 180), ('rail_ras_left', -90)):
    rr, rc = pica_rail(RX0 + 0.8, RX1 - 0.8)
    ob = make(nm, T(rr, (0, 0, RA - 0.2), ang), 'anod', bev=0.3)
    cut(ob, [T(c, (0, 0, RA - 0.2), ang) for c in rc])
make('ras_front_ring', lathe_x([(166.0, 0), (166.0, 22.0), (171.5, 22.0), (171.5, 0)], 8, phase=PI / 8), 'anod', bev=0.6)
cover = [box_vf((73.0, 0, -RA - 7.6), (104.0, 24.4, 6.0))] + [box_vf((26.0 + 10.01 * k, 0, -RA - 11.0), (5.0, 22.0, 1.6)) for k in range(10)]
make('ras_rail_cover', cover, 'poly', bev=0.6, seg=2)

# =============================================================== BUFFER TUBE / CASTLE NUT / STOCK
BZ = -2.3
make('buffer_tube', lathe_x([(-370.0, 0), (-370.0, 14.6), (-201.5, 14.6), (-201.5, 0)], 48, 0, BZ), 'anod')
cn = make('castle_nut', lathe_x([(-209.6, 0), (-209.6, 15.6), (-209.0, 16.3), (-202.2, 16.3), (-201.6, 15.6), (-201.6, 0)], 48, 0, BZ), 'phos')
cut(cn, [slab([(0, -1.6), (5, -1.6), (5, 1.6), (0, 1.6)], (-210.5, 0, BZ), (1, 0, 0), (0, -math.sin(a), math.cos(a)),
              (0, math.cos(a), math.sin(a)), 14.6, 18.0) for a in (PI / 4, 3 * PI / 4, 5 * PI / 4, 7 * PI / 4)])
make('receiver_end_plate', lathe_x([(-201.6, 0), (-201.6, 16.9), (-199.0, 16.9), (-199.0, 0)], 48, 0, BZ), 'phos', bev=0.0)
STK = clean_outline(OUTL['stock'], 0.9, 50, 3)
stock = make('stock', prism_xz(STK, -19.0, 19.0), 'poly', bev=3.0, seg=3, angle=25)
scuts = []
for row_z in (-4.8, -12.0):
    for k in range(5):
        x0 = -366.0 + 29.6 * k
        for s in (1, -1):
            scuts.append(slab(stadium(26.6, 4.2, 6), (x0 + 13.3, s * 19.0, row_z), (1, 0, 0), (0, 0, 1), (0, s, 0), -1.6, 3.0))
scuts.append(prism_xz(stadium(31.0, 5.4, 8, (-345.0, -29.5)), -25, 25))                         # sling slot
for s in (1, -1):
    scuts.append(box_vf((-282.0, s * 18.5, -56.0), (78.0, 19.0, 56.0)))                       # narrow lower arm
cut(stock, scuts)
make('stock_tube_hole_ring', lathe_x([(-213.0, 0), (-213.0, 15.4), (-211.4, 15.4), (-211.4, 0)], 48, 0, BZ), 'poly')
make('sling_loop_rear', T(lathe_x([(-1.3, 0)], 4), (0, 0, 0)) if False else
     (lambda: (lambda V, F: (V, F))(*combine(*[T((np.array([[(-349.0 + (7.0 + 1.3 * math.cos(b)) * math.cos(a)), 1.3 * math.sin(b),
                                                              (-103.5 + (7.0 + 1.3 * math.cos(b)) * math.sin(a))] for a in np.linspace(0, 2 * PI, 33)[:-1] for b in np.linspace(0, 2 * PI, 13)[:-1]]),
                                                    [((i * 12 + j), (i * 12 + (j + 1) % 12), (((i + 1) % 32) * 12 + (j + 1) % 12), (((i + 1) % 32) * 12 + j)) for i in range(32) for j in range(12)]))])))(),
     'phos')

# =============================================================== REAR BUIS (folded)
make('buis_base', [prism_xz(rrect(-152.0, -121.0, 30.2, 40.5, 1.5), -10.5, 10.5),
                   prism_xz([(-181.0, 38.4), (-129.0, 38.4), (-126.5, 40.6), (-129.0, 43.4), (-181.0, 43.4)], -7.0, 7.0)], 'anod', bev=0.6)
make('buis_drum', [prism_xz(knurl_ring(8.2, 0.6, 30), -11.5, 11.5)], 'anod', bev=0.25)
bpy.data.objects['buis_drum'].data.transform(Matrix.Translation((-160.0 * MM, 0, 38.6 * MM)))

# =============================================================== M68 CCO (Aimpoint CompM2 type) + QRP mount
OZ = 60.0
make('optic_body', lathe_x([(-118.6, 0), (-118.6, 14.0), (-121.8, 14.0), (-121.8, 16.6), (-121.2, 17.4), (-112.4, 17.4), (-112.0, 19.0),
                            (-64.0, 19.0), (-63.0, 16.0), (-40.6, 16.0), (-40.0, 16.6), (-4.0, 16.6), (-1.0, 15.2), (-1.0, 13.6), (-5.0, 13.6),
                            (-5.0, 0)], 64, 0, OZ), 'optic', bev=0.0)
make('optic_eyepiece_rubber', lathe_x([(-123.0, 0), (-123.0, 16.0), (-122.5, 17.6), (-120.5, 17.8), (-120.0, 17.2), (-118.4, 17.2), (-117.9, 17.8),
                                       (-115.9, 17.8), (-115.4, 17.2), (-113.8, 17.2), (-113.3, 17.8), (-112.2, 17.8), (-112.2, 0)], 64, 0, OZ), 'rubber')
make('optic_lens_rear', lathe_x([(-119.2, 0), (-119.2, 14.1), (-118.4, 14.1), (-118.4, 0)], 48, 0, OZ), 'glass')
make('optic_lens_front', lathe_x([(-5.6, 0), (-5.6, 13.7), (-4.8, 13.7), (-4.8, 0)], 48, 0, OZ), 'glass')
make('optic_adjuster_block', prism_yz(superellipse(17.6, 17.6, 4.0, 48) + np.array([0, OZ]), -62.5, -41.0), 'optic', bev=0.6)
make('optic_turrets', [T(prism_xy(knurl_ring(7.2, 0.5, 24), 0, 5.0), (-51.8, 0, OZ + 16.5)),
                       T(prism_xz(knurl_ring(7.2, 0.5, 24), -5.0, 0), (-51.8, -16.5, OZ))], 'optic', bev=0.2)
BZ2 = OZ + 24.5
make('optic_battery_housing', [lathe_x([(-108.0, 0), (-108.0, 8.4), (-72.0, 8.4), (-72.0, 0)], 40, 0, BZ2),
                               prism_yz(rrect(-6.0, 6.0, OZ + 12.0, BZ2, 2.0), -106.0, -74.0)], 'optic', bev=0.0)
make('optic_knobs', [T(prism_yz(knurl_ring(9.2, 0.7, 28), -121.0, -108.0), (0, 0, BZ2)),
                     T(prism_yz(knurl_ring(9.0, 0.7, 28), -72.0, -62.0), (0, 0, BZ2))], 'optic', bev=0.25)
make('mount_base', [prism_xz([(-108.0, 30.2), (-38.0, 30.2), (-38.0, 45.0), (-44.0, 45.0), (-50.0, 38.5), (-100.0, 38.5), (-108.0, 36.0)], -12.0, 12.0),
                    box_vf((-82.0, 0, 41.0), (16.0, 18.0, 6.0))], 'anod', bev=0.7)
make('mount_ring', lathe_x([(-89.5, 0), (-89.5, 22.2), (-74.5, 22.2), (-74.5, 0)], 64, 0, OZ), 'anod', bev=0.0)
cut(bpy.data.objects['mount_ring'], [lathe_x([(-91, 0), (-91, 19.05), (-73, 19.05), (-73, 0)], 64, 0, OZ)])
lever_c = [(-80.0, 59.0), (-78.6, 70.0), (-75.4, 80.0), (-70.6, 87.0), (-64.0, 90.4), (-60.0, 90.2)]
lc = np.array(lever_c); lo, hi = [], []
for i in range(len(lc)):
    tng = lc[min(i + 1, len(lc) - 1)] - lc[max(i - 1, 0)]; tng /= np.linalg.norm(tng); nrm = np.array([-tng[1], tng[0]])
    w = 3.2 if i < len(lc) - 1 else 2.2
    lo.append(lc[i] - nrm * w); hi.append(lc[i] + nrm * w)
make('mount_lever', [prism_xz([tuple(p) for p in lo + hi[::-1]], -25.2, -22.6), lathe_y([(-27.5, 0), (-27.5, 6.4), (-22.0, 6.4), (-22.0, 0)], 32, -80.0, 59.0)],
     'anod', bev=0.5)
make('mount_crossbolt_nut', lathe_y([(-13.0, 0), (-13.0, 3.6), (-11.8, 3.6), (-11.8, 0)], 24, -62.0, 34.0), 'brass')

# =============================================================== MARKINGS (roll marks left side, white RAS rail numbers right side)
FONT = r'C:\Windows\Fonts\arialbd.ttf'
def text_vf(txt, size, depth=0.08, spacing=1.0):
    cu = bpy.data.curves.new('tx', 'FONT'); cu.body = txt; cu.size = size; cu.extrude = depth / 2
    cu.align_x = 'CENTER'; cu.align_y = 'CENTER'; cu.space_character = spacing
    if os.path.exists(FONT):
        cu.font = bpy.data.fonts.load(FONT, check_existing=True)
    tob = bpy.data.objects.new('tx', cu); bpy.context.scene.collection.objects.link(tob)
    bpy.context.view_layer.update()
    me = bpy.data.meshes.new_from_object(tob.evaluated_get(bpy.context.evaluated_depsgraph_get()))
    V = np.array([v.co[:] for v in me.vertices], float); F = [tuple(p.vertices) for p in me.polygons]
    bpy.data.objects.remove(tob); bpy.data.curves.remove(cu); bpy.data.meshes.remove(me)
    return V, F
def place(vf, origin, ex, ey, en):
    V, F = vf; o, ex, ey, en = (np.array(a, float) for a in (origin, ex, ey, en))
    return o + V[:, :1] * ex + V[:, 1:2] * ey + V[:, 2:3] * en, F
LEFT = dict(ex=(-1, 0, 0), ey=(0, 0, 1), en=(0, 1, 0))          # reads muzzle->stock when seen from the left
marks = []
for i, (txt, sz) in enumerate((('PROPERTY OF U.S. GOVT.', 2.6), ('M4A1 CARBINE', 3.4), ('CAL 5.56 MM', 3.0), ('W1047392', 3.0))):
    marks.append(place(text_vf(txt, sz), (-44.0, 14.0, -33.5 - 6.0 * i), **LEFT))
for txt, (px, pz) in (('SAFE', (-121.5, -33.0)), ('SEMI', (-146.0, -23.6)), ('AUTO', (-168.5, -40.0))):
    marks.append(place(text_vf(txt, 2.6), (px, 11.5, pz), **LEFT))
make('roll_marks', marks, 'mark', bev=0.0, smooth=False)
nums = []
_, rc = pica_rail(RX0 + 0.8, RX1 - 0.8)
teeth = sorted(float(np.mean(np.asarray(c[0])[:, 0])) for c in rc)
YR = -(RA - 0.2 + 9.4) - 0.01
for k, xs in enumerate(teeth[1::2][:8]):
    xt = xs + 10.01 / 2
    nums.append(place(text_vf('R%d' % (14 + 2 * k), 2.3), (xt, YR, 0.0), ex=(0, 0, -1), ey=(1, 0, 0), en=(0, -1, 0)))
make('ras_numbers', nums, 'paint', bev=0.0, smooth=False)

# =============================================================== finish: shading, materials, orientation
for ob in list(C.objects):
    if ob.type == 'MESH' and not ob.name.endswith('_cut'):
        if ob.name not in ('flash_hider', 'delta_ring'):
            shade(ob, True, 30)
        box_uv(ob, 1.0)

if STAGE >= 2:
    import m4_mats as MMAT; importlib.reload(MMAT)
    LIB = {'anod': MMAT.anodized(), 'phos': MMAT.phosphate(), 'poly': MMAT.polymer(), 'grip': MMAT.grip_checker(),
           'mag': MMAT.mag_al(), 'optic': MMAT.optic_al(), 'rubber': MMAT.rubber(), 'glass': MMAT.lens_glass(), 'brass': MMAT.brass(),
           'mark': MMAT.roll_mark(), 'paint': MMAT.paint_white()}
else:
    LIB = {'anod': simple_mat('al_anodized', (0.045, 0.046, 0.05), 0.2, 0.45), 'phos': simple_mat('steel_phosphate', (0.04, 0.042, 0.038), 0.4, 0.6),
           'poly': simple_mat('polymer_black', (0.03, 0.03, 0.032), 0.0, 0.6), 'mag': simple_mat('mag_gi_grey', (0.09, 0.095, 0.09), 0.3, 0.5),
           'optic': simple_mat('optic_black', (0.02, 0.02, 0.022), 0.2, 0.35), 'rubber': simple_mat('rubber', (0.02, 0.02, 0.02), 0.0, 0.85),
           'glass': simple_mat('lens_coated', (0.01, 0.01, 0.015), 0.0, 0.03, coat=1.0), 'brass': simple_mat('brass', (0.78, 0.58, 0.3), 1.0, 0.32)}
    LIB['grip'] = LIB['poly']; LIB['mark'] = LIB['anod']; LIB['paint'] = simple_mat('paint_white', (0.6, 0.6, 0.58), 0.0, 0.6)
for name, key in MATS.items():
    ob = bpy.data.objects.get(name)
    if ob: assign(ob, LIB[key])

GRIP_C = np.mean(np.asarray(GRIP), axis=0)              # pistol grip centre (x, z) in gun frame
R = Matrix(((0, 0, -1, 0), (0, 1, 0, 0), (1, 0, 0, 0), (0, 0, 0, 1)))    # fwd->+Z, top->-X, left->+Y
M = R @ Matrix.Translation((-GRIP_C[0] * MM, 0, -GRIP_C[1] * MM))
for ob in C.objects:
    if ob.type == 'MESH':
        ob.data.transform(M)
        ob.data.update()
print('grip centre (gun frame mm):', GRIP_C.round(1))
report(C)
save(os.path.abspath(OUT))
