"""dragonfly.py - tombo (dragonfly) motif generator, reusable for reliefs / cut-outs.
Local frame: x lateral, y height (out of the base surface), z along body (head +z).
Units: mm at nominal length 34 mm, use scale=L/34*0.001 to place in metres."""
import numpy as np, math
from wlib import loft, lathe

def _ellipsoid(c, r, nu=20, nv=12):
    cx, cy, cz = c; rx, ry, rz = r
    prof = []
    for i in range(nv + 1):
        a = -math.pi / 2 + math.pi * i / nv
        prof.append((math.cos(a), math.sin(a)))
    rings = []
    for (cr, sz) in prof[1:-1]:
        ang = np.arange(nu) * 2 * np.pi / nu
        rings.append(np.column_stack([cx + rx * cr * np.cos(ang), cy + ry * cr * np.sin(ang), np.full(nu, cz + rz * sz)]))
    v, f = loft(np.array(rings), True)
    n0 = len(v)
    v = np.vstack([v, [[cx, cy, cz - rz]], [[cx, cy, cz + rz]]])
    R = len(rings)
    for j in range(nu):
        f.append(((j + 1) % nu, j, n0))
        f.append(((R - 1) * nu + j, (R - 1) * nu + (j + 1) % nu, n0 + 1))
    return v, f

def abdomen_profile(n=60):
    zs = np.linspace(6.5, -17.0, n)
    r = np.interp(zs, [-17.0, -16.2, -14.0, -8.0, 2.0, 5.0, 6.5],
                  [0.25, 0.75, 0.85, 0.95, 1.10, 1.30, 1.15])
    seg = 0.13 * np.maximum(0, np.cos(2 * np.pi * (zs - 4.0) / 2.6)) ** 6
    return zs, r + seg * (zs < 4.5) * (zs > -15.5)

def wing_outline(Lw, W, n=28):
    u = np.linspace(0, 1, n)
    front = 0.30 * W * np.sin(np.pi * np.clip(u, 0, 1)) ** 0.75
    back = -0.70 * W * np.sin(np.pi * np.clip(u * 0.97 + 0.015, 0, 1)) ** 0.55
    up = np.column_stack([u * Lw, front])
    lo = np.column_stack([u * Lw, back])[::-1]
    return np.vstack([up, lo[1:-1]])   # closed polygon (u along wing, v across; +v = leading edge)

def wing_dirs(style):
    # angle from +z (head) axis, degrees
    return {'spread': (78.0, 100.0), 'swept': (150.0, 162.0), 'half': (110.0, 128.0)}[style]

def _wing_frame(side, phi_deg, root):
    p = math.radians(phi_deg)
    d = np.array([side * math.sin(p), 0.0, math.cos(p)])          # along wing
    lead = np.array([-d[2], 0.0, d[0]])
    if lead[2] < 0:
        lead = -lead                                               # leading edge faces the head (+z)
    return np.array(root, float), d, lead

def wing_mesh(side, phi, root, Lw, W, thick=0.45, droop=0.06, nu=24, nv=7):
    o, d, lead = _wing_frame(side, phi, root)
    us = np.linspace(0, 1, nu)
    rings = []
    for u in us:
        fr = 0.30 * W * math.sin(math.pi * u) ** 0.75 + 0.05
        bk = -0.70 * W * math.sin(math.pi * min(1, u * 0.97 + 0.015)) ** 0.55 - 0.05
        vs = np.linspace(bk, fr, nv)
        t = thick * (0.55 + 0.45 * (vs - bk) / max(fr - bk, 1e-6)) * (1 - 0.5 * u)
        y0 = -droop * (u * Lw) ** 1.3 * 0.12
        top = [o + d * (u * Lw) + lead * v + np.array([0, y0 + tt / 2, 0]) for v, tt in zip(vs, t)]
        bot = [o + d * (u * Lw) + lead * v + np.array([0, y0 - tt / 2, 0]) for v, tt in zip(vs[::-1], t[::-1])]
        rings.append(np.array(top + bot))
    v, f = loft(np.array(rings), True, True, True)
    return v, f

def dragonfly_mesh(style='spread', body_lift=1.4):
    parts = []
    # eyes, head, thorax
    parts.append(_ellipsoid((1.55, body_lift + 0.55, 15.0), (1.75, 1.65, 1.7)))
    parts.append(_ellipsoid((-1.55, body_lift + 0.55, 15.0), (1.75, 1.65, 1.7)))
    parts.append(_ellipsoid((0, body_lift + 0.2, 13.6), (1.9, 1.5, 1.5)))
    parts.append(_ellipsoid((0, body_lift + 0.5, 10.0), (2.5, 2.0, 3.7), 24, 14))
    # abdomen (tube along z)
    zs, r = abdomen_profile()
    nu = 16
    ang = np.arange(nu) * 2 * np.pi / nu
    rings = [np.column_stack([rr * np.cos(ang), body_lift + rr * 0.9 * np.sin(ang), np.full(nu, z)]) for z, rr in zip(zs, r)]
    v, f = loft(np.array(rings), True, True, True)
    parts.append((v, f))
    fa, ha = wing_dirs(style)
    for side in (1, -1):
        parts.append(wing_mesh(side, fa, (side * 1.2, body_lift + 1.6, 11.0), 15.5, 4.1))
        parts.append(wing_mesh(side, ha, (side * 1.2, body_lift + 1.5, 8.9), 14.5, 4.7))
    return parts

def dragonfly_silhouette(style='spread'):
    """list of closed 2D polygons (x, z) whose union is the dragonfly outline."""
    polys = []
    def ell(cx, cz, rx, rz, n=28):
        a = np.arange(n) * 2 * np.pi / n
        return np.column_stack([cx + rx * np.cos(a), cz + rz * np.sin(a)])
    polys.append(ell(0, 14.6, 3.2, 2.0))      # head + eyes
    polys.append(ell(0, 10.0, 2.5, 3.9))      # thorax
    zs, r = abdomen_profile(40)
    polys.append(np.vstack([np.column_stack([r, zs]), np.column_stack([-r, zs])[::-1]]))
    fa, ha = wing_dirs(style)
    for side in (1, -1):
        for phi, root, Lw, W in ((fa, (side * 0.8, 11.0), 15.5, 4.1), (ha, (side * 0.8, 8.9), 14.5, 4.7)):
            o, d, lead = _wing_frame(side, phi, (root[0], 0, root[1]))
            ol = wing_outline(Lw, W)
            pts = [o + d * u + lead * vv for u, vv in ol]
            polys.append(np.array([(p[0], p[2]) for p in pts]))
    return polys
