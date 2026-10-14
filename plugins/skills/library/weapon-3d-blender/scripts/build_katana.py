"""build_katana.py - parametric realistic katana (weapon-3d skill).
blender -b --factory-startup -P build_katana.py -- --out katana_v01.blend --stage 1
Conventions: length +Z (tip up), omote face -> -Y, edge (ha) -> -X, mune -> +X,
origin = centre of the grip (tsuka). Units: metres."""
import bpy, sys, os, math, importlib
import numpy as np
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import wlib; importlib.reload(wlib)
from wlib import *

argv = sys.argv[sys.argv.index('--') + 1:] if '--' in sys.argv else []
def arg(name, default):
    return argv[argv.index(name) + 1] if name in argv else default
OUT = arg('--out', os.path.join(os.path.dirname(os.path.abspath(__file__)), '..', 'katana', 'katana_v01.blend'))
STAGE = int(arg('--stage', '1'))
mm = 0.001

# ------------------------------------------------------------------ TZ numbers
TSUKA_L = 270 * mm          # fuchi top -> kashira end
NAGASA = 720 * mm           # munemachi -> tip, along the mune
SORI = 18 * mm              # sagitta of the mune arc
W0, W1 = 32.0 * mm, 22.0 * mm   # motohaba, sakihaba (at yokote)
T0, T1 = 7.5 * mm, 5.2 * mm     # motokasane, sakikasane
KISS = 36 * mm              # kissaki length
FS = 0.30                   # shinogi-ji share of width (from mune)
MH = 1.3 * mm               # iori-mune height
PH, PK = 1.75, 1.30         # hira / kissaki niku exponents
NH = 8                      # hira samples
Z_T0 = -TSUKA_L / 2         # kashira end
Z_T1 = TSUKA_L / 2          # fuchi top
SEPPA_T, TSUBA_T = 2.0 * mm, 5.0 * mm
Z_MACHI = Z_T1 + 2 * SEPPA_T + TSUBA_T
HABAKI_L = 33 * mm
R_ARC = NAGASA ** 2 / (8 * SORI)
S_Y = NAGASA - KISS
X_M0 = W0 / 2               # mune at +X, edge at -X, blade centred on grip axis at the machi

sc = reset_scene()
C = coll('katana')

# ------------------------------------------------------------------ blade
def frame(s):
    if s < 0:
        return np.array([X_M0, 0, Z_MACHI + s]), np.array([0, 0, 1.0]), np.array([-1.0, 0, 0])
    a = s / R_ARC
    M = np.array([X_M0 + R_ARC * (1 - math.cos(a)), 0, Z_MACHI + R_ARC * math.sin(a)])
    t = np.array([math.sin(a), 0, math.cos(a)])
    n = np.array([-math.cos(a), 0, math.sin(a)])
    return M, t, n

# fukura (kissaki edge) as cubic bezier in (s_rel, w)
def _bez(P, t):
    t = t[:, None]
    return ((1 - t) ** 3) * P[0] + 3 * ((1 - t) ** 2) * t * P[1] + 3 * (1 - t) * t * t * P[2] + t ** 3 * P[3]
_beta = math.radians(38)
_BP = np.array([[0, W1], [0.45 * KISS, W1], [KISS - 0.38 * KISS * math.cos(_beta), 0.38 * KISS * math.sin(_beta)], [KISS, 0]])
_bz = _bez(_BP, np.linspace(0, 1, 400))

def width_at(s):
    if s <= S_Y:
        return W0 + (W1 - W0) * max(s, 0) / S_Y
    return float(np.interp(s - S_Y, _bz[:, 0], _bz[:, 1]))

def thick_at(s):
    if s <= S_Y:
        return T0 + (T1 - T0) * max(s, 0) / S_Y
    q = min((s - S_Y) / KISS, 1)
    return T1 * (1 - q * q) ** 0.7 + 0.15 * mm * (1 - q)

def shinogi_at(s):
    if s <= S_Y:
        return FS * width_at(s)
    q = min((s - S_Y) / KISS, 1)
    return FS * W1 * (1 - q) ** 0.9

def ring2d(s, kiss=None):
    W, T, ws = width_at(s), thick_at(s), shinogi_at(s)
    if kiss is None:
        kiss = s > S_Y
    p = PK if kiss else PH
    if kiss:
        q = min((s - S_Y) / KISS, 1)
        mh = MH * (W1 / W0) * (1 - q) + 0.05 * mm
    else:
        mh = MH * (width_at(s) / W0)
    Tm = 0.70 * T
    te = 0.05 * mm
    plus = [(mh, Tm / 2), ((mh + ws) / 2, (Tm / 2 + T / 2) / 2), (ws, T / 2)]
    for k in range(1, NH):
        a = 1 - k / NH
        plus.append((W - a * (W - ws), T / 2 * (1 - (1 - a) ** p)))
    plus.append((W, te))
    ring = [(0.0, 0.0)] + plus + [(w, -t) for (w, t) in reversed(plus)]
    return ring, W

def build_blade():
    ss = list(np.arange(-12 * mm, S_Y, 3 * mm)) + [S_Y]
    kiss_s = list(S_Y + KISS * np.linspace(0, 1, 46)[1:-1] ** 1.0)
    rings, uvs, kinds = [], [], []
    for s in ss:
        r, W = ring2d(s, False); rings.append((s, r, W)); kinds.append(0)
    r, W = ring2d(S_Y + 0.3 * mm, True); rings.append((S_Y + 0.3 * mm, r, W)); kinds.append(1)
    for s in kiss_s:
        if s <= S_Y + 0.6 * mm:
            continue
        r, W = ring2d(s, True); rings.append((s, r, W)); kinds.append(1)
    V, UV, EUV = [], [], []
    for s, r, W in rings:
        M, t, n = frame(s)
        for (w, tt) in r:
            V.append(M + n * w + np.array([0, 1.0, 0]) * tt)
            UV.append((max(s, 0) / NAGASA, 1 - w / max(W, 1e-6)))
            EUV.append((s, max(W - w, 0.0)))
    Nn = len(rings[0][1])
    R = len(rings)
    V = np.array(V).reshape(R, Nn, 3)
    tipM, _, _ = frame(NAGASA)
    v, f = loft_tip(V, tipM, closed=True, cap0=True)
    UV.append((1.0, 0.5)); EUV.append((NAGASA, 0.0))
    Lp = (Nn - 1) // 2
    sharp_j = [0, 1, 3, Lp, Lp + 1, 2 * Lp - 2, 2 * Lp]
    pairs = []
    for i in range(R - 1):
        for j in sharp_j:
            pairs.append((i * Nn + j, (i + 1) * Nn + j))
    iy = len(ss) - 1  # yokote ring
    for j in list(range(3, Lp)) + list(range(Lp + 1, 2 * Lp - 2)):
        pairs.append((iy * Nn + j, iy * Nn + j + 1))
    ob = mk('blade', v, f, C, uv=np.array(UV), smooth=True, sharp_edges=pairs)
    set_uv_per_vertex(ob, np.array(EUV), 'EdgeUV')
    return ob

blade = build_blade()

# ------------------------------------------------------------------ habaki
def habaki_poly(s, d):
    r, W = ring2d(max(s, 0), False)
    plus = [p for p in r[1:(len(r) + 1) // 2]]  # mune shoulder .. edge (+t side)
    tb = [(0.0, 0.0)] + plus
    pts = []
    t0 = tb[1][1] + d
    for k in range(5, 0, -1):
        ang = math.pi / 2 * k / 5
        pts.append((-d * math.sin(ang), t0 * math.cos(ang)))
    for (w, t) in tb[1:]:
        pts.append((w, t + d))
    te = d
    for k in range(1, 6):
        ang = math.pi / 2 * k / 5
        pts.append((W + d * math.sin(ang), te * math.cos(ang)))
    top = pts
    ring = top + [(w, -t) for (w, t) in reversed(top[1:-1])]
    return ring

def build_habaki():
    ss = np.linspace(-1.5 * mm, HABAKI_L, 14)
    V = []
    for s in ss:
        d = 1.7 * mm + (1.05 * mm - 1.7 * mm) * (max(s, 0) / HABAKI_L)
        M, t, n = frame(s)
        for (w, tt) in habaki_poly(s, d):
            V.append(M + n * w + np.array([0, 1.0, 0]) * tt)
    Nn = len(habaki_poly(0, 1 * mm))
    V = np.array(V).reshape(len(ss), Nn, 3)
    v, f = loft(V, True, True, True)
    ob = mk('habaki', v, f, C, smooth=True, sharp=35)
    bevel(ob, 0.35 * mm, 2, 40)
    wnormal(ob)
    return ob

habaki = build_habaki()

# ------------------------------------------------------------------ seppa, tsuba
import dragonfly; importlib.reload(dragonfly)

def oval_ring(a, b, n, z, seg=96, cx=0.0):
    p = superellipse(a, b, n, seg)
    return np.column_stack([p[:, 0] + cx, p[:, 1], np.full(seg, z)])

def build_seppa(name, z0, z1, kiku=0):
    seg = 240 if kiku else 96
    rings = []
    for z in (z0, z1):
        p = superellipse(19.6 * mm, 14.6 * mm, 2.3, seg)
        if kiku:
            t = np.arange(seg) * 2 * np.pi / seg
            sc_ = 1.0 + 0.022 * (np.abs(np.sin(kiku * t / 2)) ** 0.6 - 0.6)
            p = p * sc_[:, None]
        rings.append(np.column_stack([p, np.full(seg, z)]))
    v, f = loft(np.array(rings), True, True, True)
    ob = mk(name, v, f, C, smooth=True, sharp=40)
    bevel(ob, 0.35 * mm, 2, 40)
    wnormal(ob)
    return ob

seppa_back = build_seppa('seppa_back_kiku', Z_T1, Z_T1 + SEPPA_T, kiku=64 if STAGE >= 2 else 0)
seppa_front = build_seppa('seppa_front', Z_T1 + SEPPA_T + TSUBA_T, Z_MACHI)

def prism_xy(poly2d, z0, z1, c=(0, 0), e1=(1, 0), e2=(0, 1)):
    """2D polygon in a local (u,w) frame placed in the XY plane (centre c, axes e1,e2), extruded along Z."""
    p = np.asarray(poly2d, float)
    c, e1, e2 = np.array(c, float), np.array(e1, float), np.array(e2, float)
    xy = c[None] + p[:, :1] * e1[None] + p[:, 1:2] * e2[None]
    n = len(xy)
    v = np.vstack([np.column_stack([xy, np.full(n, z0)]), np.column_stack([xy, np.full(n, z1)])])
    f = [(i, (i + 1) % n, n + (i + 1) % n, n + i) for i in range(n)]
    f.append(tuple(range(n))[::-1]); f.append(tuple(range(n, 2 * n)))
    return v, f

TSUBA_R = 38.5 * mm
def build_tsuba():
    zc = Z_T1 + SEPPA_T + TSUBA_T / 2
    h, hr = 2.2 * mm, 2.75 * mm
    R = TSUBA_R
    prof = [(0, -h), (R - 3.4 * mm, -h), (R - 2.9 * mm, -hr * 0.97), (R - 1.6 * mm, -hr), (R - 0.6 * mm, -hr * 0.82),
            (R, -hr * 0.35), (R, hr * 0.35), (R - 0.6 * mm, hr * 0.82), (R - 1.6 * mm, hr), (R - 2.9 * mm, hr * 0.97),
            (R - 3.4 * mm, h), (0, h)]
    v, f = lathe(prof, seg=192)
    v[:, 2] += zc
    ob = mk('guard_tsuba', v, f, C, smooth=True, sharp=50)
    if STAGE < 2:
        return ob, None
    z0, z1 = zc - 6 * mm, zc + 6 * mm
    # hitsu-ana: kozuka (half-moon) at -Y, kogai (waisted oval) at +Y; long axis along X
    t = np.linspace(0, np.pi, 40)
    koz = np.vstack([np.column_stack([6.4 * mm * np.cos(t), 5.0 * mm * np.sin(t)]),
                     np.column_stack([np.linspace(-6.4, 6.4, 12)[1:-1] * mm, np.zeros(10)])])
    tt = np.arange(64) * 2 * np.pi / 64
    kog = np.column_stack([6.6 * mm * np.cos(tt), 2.7 * mm * np.sin(tt) * (1 - 0.32 * np.cos(2 * tt))])
    cut_k = mk('cut_kozuka', *prism_xy(koz, z0, z1, c=(0, -16.8 * mm), e1=(1, 0), e2=(0, -1)), C, smooth=False)
    cut_g = mk('cut_kogai', *prism_xy(kog, z0, z1, c=(0, 21.0 * mm)), C, smooth=False)
    boolean(ob, cut_k); boolean(ob, cut_g)
    # copper plug (hitsu-ume) in the kogai-hitsu
    kog2 = kog * 0.985
    plug = mk('tsuba_hitsu_plug', *prism_xy(kog2, zc - h + 0.25 * mm, zc + h - 0.25 * mm, c=(0, 21.0 * mm)), C, smooth=False)
    bevel(plug, 0.25 * mm, 2, 40); wnormal(plug)
    # kage-sukashi dragonfly (negative silhouette), spread wings
    ang = math.radians(38)
    cpos = np.array([math.cos(ang), -math.sin(ang)]) * 26.5 * mm
    e2 = np.array([-math.sin(ang), -math.cos(ang)])   # body axis tangential
    e1 = np.array([e2[1], -e2[0]])
    sc_ = 0.50 * mm
    for k, poly in enumerate(dragonfly.dragonfly_silhouette('spread')):
        cv, cf = prism_xy(np.asarray(poly) * sc_, z0, z1, c=cpos, e1=e1, e2=e2)
        boolean(ob, mk('cut_df%d' % k, cv, cf, C, smooth=False))
    shade(ob, True, 50)
    bevel(ob, 0.3 * mm, 2, 35)
    wnormal(ob)
    return ob, plug

tsuba, tsuba_plug = build_tsuba()

# ------------------------------------------------------------------ tsuka core (samegawa)
CORE = np.array([  # z, a(X), b(Y)  (mm)
    [-112, 16.0, 11.7], [-80, 15.7, 11.5], [-20, 15.3, 11.2], [60, 15.6, 11.4], [100, 16.0, 11.7], [124, 16.3, 11.9]])
SE_N = 2.4

def core_ab(z):
    zz = np.asarray(z) / mm
    return np.interp(zz, CORE[:, 0], CORE[:, 1]) * mm, np.interp(zz, CORE[:, 0], CORE[:, 2]) * mm

def build_core(name, off, z0, z1, nz=90, seg=128, n=SE_N):
    rings = []
    for z in np.linspace(z0, z1, nz):
        a, b = core_ab(z)
        rings.append(oval_ring(a + off, b + off, n, z, seg))
    v, f = loft(np.array(rings), True, True, True)
    return mk(name, v, f, C, smooth=True)

core = build_core('grip_core_samegawa', 0.0, -112 * mm, 124 * mm)
Z_W0 = Z_T0 + 28.5 * mm      # kashira top
Z_W1 = Z_T1 - 13.0 * mm      # fuchi bottom

# superellipse surface param by arc fraction f (0 = +X mune, .25 = +Y ura, .5 = -X ha, .75 = -Y omote)
_TH = np.linspace(0, 2 * np.pi, 8193)
def _se_xy(th, a, b):
    c_, s_ = np.cos(th), np.sin(th)
    return a * np.sign(c_) * np.abs(c_) ** (2 / SE_N), b * np.sign(s_) * np.abs(s_) ** (2 / SE_N)
def _se_nrm(th, a, b):
    x, y = _se_xy(th, a, b)
    gx = np.sign(x) * np.abs(x / a) ** (SE_N - 1) / a
    gy = np.sign(y) * np.abs(y / b) ** (SE_N - 1) / b
    l = np.hypot(gx, gy) + 1e-12
    return gx / l, gy / l
_xa, _ya = _se_xy(_TH, 15.6, 11.4)
_arc = np.concatenate([[0], np.cumsum(np.hypot(np.diff(_xa), np.diff(_ya)))])
LP0 = _arc[-1] * mm
_frac = _arc / _arc[-1]
def theta_of(f):
    return np.interp(np.mod(f, 1.0), _frac, _TH)
def surf_pt(f, z, h):
    th = theta_of(f)
    a, b = core_ab(z)
    x, y = _se_xy(th, a, b)
    nx, ny = _se_nrm(th, a, b)
    return np.stack([x + nx * h, y + ny * h, np.asarray(z) + 0 * x], -1)

# ------------------------------------------------------------------ tsuka-ito (hineri-maki), stage >= 2
ITO_P = 15.2 * mm     # crossing pitch on one face (= one turn of one strand)
ITO_TB = 1.25 * mm    # band thickness
ITO_OV = 0.30 * mm    # overlap of neighbouring bands on the edges
ITO_K = 2.4 * mm      # knot half-length (window tip distance from crossing)
ITO_XV = 9.6 * mm     # window half-width (arc mm from face centre)
ITO_XK = 1.6 * mm     # knot transition half-width
C_REF = Z_W1 - 4.0 * mm   # an omote (front) crossing just under the fuchi
MENUKI = [  # face fraction, z centre, scale
    (0.75, C_REF - 21 * ITO_P / 2, 0.88),   # omote menuki toward kashira
    (0.25, C_REF - 10 * ITO_P / 2, 0.88)]   # ura menuki toward fuchi
MEN_H = 3.5 * mm
MEKUGI_Z = C_REF - 3 * ITO_P / 2           # omote window, ~40 mm below fuchi top

def _smooth(x):
    x = np.clip(x, 0, 1); return x * x * (3 - 2 * x)

def menuki_lift(f, z):
    h = np.zeros_like(z)
    for ff, zm, scl in MENUKI:
        x = (np.mod(f - ff + 0.5, 1.0) - 0.5) * LP0
        q = 1 - (x / (8.6 * mm * scl / 0.88)) ** 2 - ((z - zm) / (16.8 * mm * scl / 0.88)) ** 2
        h += MEN_H * np.clip(q, 0, None) ** 0.5
    return h

def ito_edges(tau):
    half = LP0 / 2
    j = np.floor(tau / half); d1 = tau - j * half; d2 = half - d1
    cj = C_REF + j * ITO_P / 2
    full = ITO_P / 2 + ITO_OV / 2
    def ramp(d):
        return np.minimum(full, ITO_K + (full - ITO_K) * np.clip((d - ITO_XK) / (ITO_XV - ITO_XK), 0, 1))
    lo = np.maximum(cj - ITO_OV / 2, cj + ITO_P / 2 - ramp(d2))
    up = np.minimum(cj + ramp(d1), cj + ITO_P / 2 + ITO_OV / 2)
    # knot transitions
    k1 = d1 < ITO_XK                      # just after crossing j
    sm = _smooth((d1 + ITO_XK) / (2 * ITO_XK))
    lo = np.where(k1, (cj - ITO_K) * (1 - sm) + (cj - ITO_OV / 2) * sm, lo)
    up = np.where(k1, (cj + ITO_OV / 2) * (1 - sm) + (cj + ITO_K) * sm, up)
    k2 = d2 < ITO_XK                      # just before crossing j+1
    cn = cj + ITO_P / 2
    sm2 = _smooth((ITO_XK - d2) / (2 * ITO_XK))
    lo = np.where(k2, (cn - ITO_K) * (1 - sm2) + (cn - ITO_OV / 2) * sm2, lo)
    up = np.where(k2, (cn + ITO_OV / 2) * (1 - sm2) + (cn + ITO_K) * sm2, up)
    # window-edge raise weights (hishigami)
    w_up = 1 - _smooth((d1 - ITO_XV * 0.85) / (ITO_XV * 0.45))
    w_lo = 1 - _smooth((d2 - ITO_XV * 0.85) / (ITO_XV * 0.45))
    # distance to nearest crossing (signed) and its index
    jn = np.where(d1 <= d2, j, j + 1)
    dn = np.where(d1 <= d2, d1, -d2)
    return lo, up, w_up, w_lo, jn.astype(int), dn

VT = np.array([0, 0.03, 0.10, 0.22, 0.36, 0.5, 0.64, 0.78, 0.90, 0.97, 1.0])
def build_strand(name, sign, a_on_top):
    half = LP0 / 2
    jmin = math.floor((Z_W0 - 10 * mm - C_REF) / (ITO_P / 2))
    tau = np.arange(jmin * half, 2.2 * half, 0.38 * mm)
    lo, up, w_up, w_lo, jn, dn = ito_edges(tau)
    keep = (up > Z_W0 - 3.5 * mm) & (lo < Z_W1 + 3.5 * mm)
    idx = np.where(keep)[0]
    tau, lo, up, w_up, w_lo, jn, dn = [x[idx] for x in (tau, lo, up, w_up, w_lo, jn, dn)]
    f = np.mod(0.75 + sign * tau / LP0, 1.0)
    zc = (lo + up) / 2
    top = ((jn // 2) % 2 == 0) == a_on_top
    lift = np.where(top, ITO_TB * 1.02 * (1 - _smooth((np.abs(dn) - 1.9 * mm) / (2.6 * mm))), 0.0)
    lift *= _smooth((Z_W1 + 0.5 * mm - zc) / (5 * mm)) * _smooth((zc - Z_W0 + 0.5 * mm) / (5 * mm))
    V, UV = [], []
    for k in range(len(tau)):
        zs = lo[k] + VT * (up[k] - lo[k])
        base = lift[k] + menuki_lift(np.full(len(VT), f[k]), zs)
        ht = ITO_TB * (0.32 + 0.68 * (4 * VT * (1 - VT)) ** 0.5)
        ht = ht + 0.45 * mm * (w_up[k] * VT ** 4 + w_lo[k] * (1 - VT) ** 4)
        hh = np.concatenate([base + ht, [base[-1] - 0.25 * mm, base[0] - 0.25 * mm]])
        zz = np.concatenate([zs, [zs[-1], zs[0]]])
        V.append(surf_pt(np.full(len(zz), f[k]), zz, hh))
        wv = np.concatenate([VT, [1.0, 0.0]]) * (up[k] - lo[k])
        UV.append(np.column_stack([np.full(len(zz), tau[k]), wv]))
    V = np.array(V); UV = np.concatenate(UV)
    v, fc = loft(V, True, True, True)
    ob = mk(name, v, fc, C, uv=UV, smooth=True)
    return ob

if STAGE >= 2:
    ito_a = build_strand('wrap_ito_a', +1, True)
    ito_b = build_strand('wrap_ito_b', -1, False)
else:
    ito_a = build_core('wrap_ito', 1.45 * mm, Z_W0, Z_W1); ito_b = None

# ------------------------------------------------------------------ menuki (gold dragonflies under the ito)
def place_on_face(v_mm, ff, zc, scl):
    """local dragonfly coords (x lateral, y height, z along) mm -> tsuka surface."""
    x, y, z = v_mm[:, 0] * scl * mm, v_mm[:, 1] * scl * mm, v_mm[:, 2] * scl * mm
    f = ff + x / LP0 * (1 if ff > 0.5 else -1)
    return surf_pt(f, zc + z, y - 0.25 * mm)

menuki = []
if STAGE >= 2:
    for i, (ff, zm, scl) in enumerate(MENUKI):
        parts = dragonfly.dragonfly_mesh('swept', body_lift=1.2)
        v, fcs = combine(*parts)
        v = place_on_face(np.asarray(v), ff, zm, scl)
        ob = mk('menuki_%s' % ('omote' if ff > 0.5 else 'ura'), v, fcs, C, smooth=True, sharp=60)
        menuki.append(ob)
    # mekugi (bamboo peg) in an omote window
    pv, pf = lathe([(0, -0.3 * mm), (2.75 * mm, -0.3 * mm), (2.75 * mm, 0.15 * mm), (2.55 * mm, 0.42 * mm), (1.6 * mm, 0.55 * mm), (0, 0.58 * mm)], seg=40)
    bpt = surf_pt(np.array([0.75]), np.array([MEKUGI_Z]), np.array([0.0]))[0]
    pv2 = np.column_stack([pv[:, 0], -pv[:, 2], pv[:, 1]]) + bpt   # axis -> -Y
    mekugi = mk('mekugi_peg', pv2, pf, C, smooth=True, sharp=50)

def build_fuchi():
    z0, z1 = Z_T1 - 13.0 * mm, Z_T1
    rings = []
    for z, a, b in ((z0, 17.75, 13.35), (z0 + 0.6 * mm, 17.85, 13.45), (z1 - 1.0 * mm, 18.05, 13.65), (z1, 18.1, 13.7)):
        rings.append(oval_ring(a * mm, b * mm, 2.35, z, 128))
    v, f = loft(np.array(rings), True, True, True)
    ob = mk('fuchi', v, f, C, smooth=True, sharp=40)
    bevel(ob, 0.5 * mm, 3, 40)
    wnormal(ob)
    return ob

fuchi = build_fuchi()

def build_kashira():
    zt = Z_T0 + 28.5 * mm
    prof = [(zt, 17.55, 13.25), (zt - 0.6 * mm, 17.6, 13.3), (Z_T0 + 10 * mm, 17.1, 12.8), (Z_T0 + 4.5 * mm, 16.6, 12.3)]
    rr = 3.8
    for k in range(1, 9):
        al = math.pi / 2 * k / 8
        prof.append((Z_T0 + 4.5 * mm - rr * mm * math.sin(al) * (3.8 / 3.8), 12.8 + rr * math.cos(al), 8.5 + rr * math.cos(al)))
    for t in (0.8, 0.6, 0.4, 0.2, 0.06):
        prof.append((Z_T0 + 0.7 * mm - 0.7 * mm * (1 - t * t), 12.8 * t, 8.5 * t))
    rings = [oval_ring(a * mm, b * mm, 2.35, z, 128) for z, a, b in prof]
    v, f = loft(np.array(rings), True, True, True)
    ob = mk('pommel_kashira', v, f, C, smooth=True, sharp=50)
    bevel(ob, 0.4 * mm, 2, 45)
    wnormal(ob)
    return ob

kashira = build_kashira()

def kashira_relief(side):
    """gold dragonfly in relief on the kashira side wall (side=-1 omote/-Y, +1 ura/+Y)."""
    parts = dragonfly.dragonfly_mesh('half', body_lift=0.9)
    v, fcs = combine(*parts)
    v = np.asarray(v)
    scl = 0.62
    zc = Z_T0 + 15.5 * mm
    # local z (body) -> world X (mune +X), local x (wings) -> world Z, height -> outward normal
    X = v[:, 2] * scl * mm * (-side)
    Zw = zc + v[:, 0] * scl * mm
    hgt = v[:, 1] * scl * mm - 0.2 * mm
    # kashira side surface: superellipse ring, interpolate a,b by z
    zz = np.array([Z_T0 + 4.5 * mm, Z_T0 + 10 * mm, zt := Z_T0 + 28.5 * mm])
    aa = np.array([16.6, 17.1, 17.55]) * mm; bb = np.array([12.3, 12.8, 13.25]) * mm
    a = np.interp(Zw, zz, aa); b = np.interp(Zw, zz, bb)
    # find theta for given X on the +/-Y half: x = a*sgn(c)|c|^(2/n)
    cth = np.sign(X) * np.abs(np.clip(X / a, -0.999, 0.999)) ** (SE_N / 2 * 2.35 / SE_N)
    th = np.arccos(np.clip(cth, -1, 1)) * (1 if side > 0 else -1)
    c_, s_ = np.cos(th), np.sin(th)
    n_ = 2.35
    x0 = a * np.sign(c_) * np.abs(c_) ** (2 / n_); y0 = b * np.sign(s_) * np.abs(s_) ** (2 / n_)
    gx = np.sign(x0) * np.abs(x0 / a) ** (n_ - 1) / a; gy = np.sign(y0) * np.abs(y0 / b) ** (n_ - 1) / b
    l = np.hypot(gx, gy) + 1e-12
    P = np.column_stack([x0 + gx / l * hgt, y0 + gy / l * hgt, Zw])
    return mk('kashira_tombo_%s' % ('ura' if side > 0 else 'omote'), P, fcs, C, smooth=True, sharp=60)

reliefs = [kashira_relief(-1), kashira_relief(+1)] if STAGE >= 2 else []

# ------------------------------------------------------------------ materials
if STAGE >= 2:
    import katana_mats as KM; importlib.reload(KM)
    M_STEEL = KM.mat_blade(S_Y, FS)
    M_GOLD = KM.mat_habaki()
    M_SEPPA = KM.mat_seppa()
    M_IRON = KM.mat_iron()
    M_SHAKUDO = KM.mat_shakudo()
    M_RELIEF = KM.mat_gold_relief()
    M_SAME = KM.mat_samegawa()
    M_ITO = KM.mat_ito()
    M_COPPER = KM.mat_simple('copper_plug', (0.62, 0.33, 0.24), 0.9, 0.38, 1200.0, 0.08)
    M_BAMBOO = KM.mat_simple('bamboo_mekugi', (0.40, 0.29, 0.15), 0.0, 0.5, 900.0, 0.15)
else:
    M_STEEL = simple_mat('steel_blade', (0.60, 0.61, 0.63), 1.0, 0.16)
    M_GOLD = M_SEPPA = M_RELIEF = simple_mat('gold_habaki', (1.0, 0.77, 0.34), 1.0, 0.28)
    M_IRON = simple_mat('iron_tsuba', (0.10, 0.085, 0.075), 1.0, 0.55)
    M_SHAKUDO = simple_mat('shakudo', (0.075, 0.07, 0.08), 1.0, 0.32)
    M_ITO = simple_mat('silk_ito', (0.018, 0.022, 0.04), 0.0, 0.62)
    M_SAME = simple_mat('samegawa', (0.80, 0.77, 0.68), 0.0, 0.45)
    M_COPPER = M_BAMBOO = M_GOLD

for ob, m in ((blade, M_STEEL), (habaki, M_GOLD), (tsuba, M_IRON), (fuchi, M_SHAKUDO), (kashira, M_SHAKUDO),
              (seppa_back, M_SEPPA), (seppa_front, M_SEPPA), (core, M_SAME), (ito_a, M_ITO)):
    assign(ob, m)
if ito_b: assign(ito_b, M_ITO)
if tsuba_plug: assign(tsuba_plug, M_COPPER)
for ob in menuki + reliefs: assign(ob, M_RELIEF)
if STAGE >= 2: assign(mekugi, M_BAMBOO)

for ob in C.objects:
    if ob.name not in ('blade', 'wrap_ito_a', 'wrap_ito_b'):
        box_uv(ob, 1.0)

print('R_ARC %.3f m, tip offset %.1f mm, LP0 %.1f mm' % (R_ARC, (frame(NAGASA)[0][0] - X_M0) / mm, LP0 / mm))
report(C)
save(os.path.abspath(OUT))
