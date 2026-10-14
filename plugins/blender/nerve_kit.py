"""nerve kit: build stylized (anime / cel-shaded) characters in Blender from code.

Loaded into every bridge session as `kit` (exec namespace; also `import nerve_kit`) and reloaded
automatically when this file changes. Conventions: Z up, the character faces -Y (Blender's Front
view), +X is the character's left, meters (a 1.6 m character has a ~0.22 m head). Brushes work in
object-local coordinates.

build   mesh, ring, spline, resample, loft, sweep, join, mod, subsurf, mirror, symmetrize, clear
sdf     smin, smax, sd_capsule, sd_ellipsoid, sdf_sample, surface_nets, sdf_mesh, Field (baked grid:
        lookups, grad, offset, drape, ray)
hair    lock (lens-section clump with sharp tip lying on a surface)
sculpt  grab, inflate, pinch, flatten, relax, scale_region  (falloff brushes, X-symmetric, numpy)
look    hexc, toon_scene, toon_material, outline, smooth_normals, assign, ref
decal   Paint (procedural 2D art: ellipse/poly/stroke/gradient), image, decal_material, decal
review  sheet (turnaround contact sheet: render | clay | wire), topo (topology report), help
"""

import math
import os
import tempfile
import time

import bmesh
import bpy
import numpy as np
from mathutils import Matrix, Quaternion, Vector
from mathutils.kdtree import KDTree

# ------------------------------------------------------------------ small helpers


def obj(name_or_obj=None):
    """Object by name (or pass-through); None -> active object."""
    if name_or_obj is None:
        return bpy.context.view_layer.objects.active
    if isinstance(name_or_obj, str):
        o = bpy.data.objects.get(name_or_obj)
        if o is None:
            raise KeyError(f"no object named {name_or_obj!r}")
        return o
    return name_or_obj


def _collection(name):
    if not name:
        return bpy.context.scene.collection
    c = bpy.data.collections.get(name)
    if c is None:
        c = bpy.data.collections.new(name)
        bpy.context.scene.collection.children.link(c)
    return c


def clear(prefix):
    """Delete objects whose name starts with `prefix` (and their orphaned meshes). Returns the count."""
    n = 0
    for o in [o for o in bpy.data.objects if o.name.startswith(prefix)]:
        data = o.data
        bpy.data.objects.remove(o, do_unlink=True)
        if data is not None and data.users == 0 and isinstance(data, bpy.types.Mesh):
            bpy.data.meshes.remove(data)
        n += 1
    return n


def hexc(c, alpha=1.0):
    """'#ffcc99' / (r,g,b) sRGB 0..1 -> linear RGBA tuple (what node colors expect)."""
    if isinstance(c, str):
        c = c.lstrip("#")
        c = [int(c[i:i + 2], 16) / 255 for i in (0, 2, 4)]
    if len(c) == 4:
        alpha = c[3]
    lin = [x / 12.92 if x <= 0.04045 else ((x + 0.055) / 1.055) ** 2.4 for x in c[:3]]
    return (lin[0], lin[1], lin[2], alpha)


# ------------------------------------------------------------------ build


def mesh(name, verts, faces, smooth=True, collection=None):
    """Create an object from verts/faces, or replace the mesh of an existing object with that name
    (keeps its materials, modifiers and transform)."""
    me = bpy.data.meshes.new(name)
    me.from_pydata([tuple(v) for v in verts], [], [tuple(f) for f in faces])
    me.validate(clean_customdata=False)
    return _attach(name, me, collection, smooth)


def _attach(name, me, collection, smooth):
    o = bpy.data.objects.get(name)
    if o is not None and o.type == "MESH":
        old = o.data
        for m in old.materials:
            me.materials.append(m)
        o.data = me
        if old.users == 0:
            bpy.data.meshes.remove(old)
        me.name = name
    else:
        o = bpy.data.objects.new(name, me)
        _collection(collection).objects.link(o)
    if smooth:
        try:
            me.shade_smooth()
        except AttributeError:
            me.polygons.foreach_set("use_smooth", [True] * len(me.polygons))
    me.update()
    return o


def ring(n, rx, ry=None, center=(0, 0, 0), frame=None, power=2.0, start=0.0, shape=None):
    """`n` points on a superellipse |x/rx|^p + |y/ry|^p = 1, counter-clockwise around the frame normal.
    frame: 3x3 Matrix whose columns are (x axis, y axis, normal); default = world XY (normal +Z).
    power: 2 ellipse, >2 boxier, <2 diamond/lens. start: angle offset in degrees.
    shape: optional f(angle_radians) -> radius multiplier (flat backs, pointed chins...)."""
    ry = rx if ry is None else ry
    fr = frame if frame is not None else Matrix.Identity(3)
    ax, ay = fr.col[0], fr.col[1]
    c = Vector(center)
    e = 2.0 / power
    pts = []
    for i in range(n):
        a = math.radians(start) + 2 * math.pi * i / n
        ca, sa = math.cos(a), math.sin(a)
        x = rx * math.copysign(abs(ca) ** e, ca)
        y = ry * math.copysign(abs(sa) ** e, sa)
        if shape:
            k = shape(a)
            x, y = x * k, y * k
        pts.append(c + ax * x + ay * y)
    return pts


def spline(points, per_seg=8, closed=False, alpha=0.5):
    """Centripetal Catmull-Rom curve through the control points -> dense list of Vectors."""
    P = [Vector(p) for p in points]
    if len(P) < 2:
        return P
    if closed:
        P = [P[-1]] + P + [P[0], P[1]]
    else:
        P = [P[0] + (P[0] - P[1])] + P + [P[-1] + (P[-1] - P[-2])]
    out = []
    for i in range(1, len(P) - 2):
        p0, p1, p2, p3 = P[i - 1], P[i], P[i + 1], P[i + 2]

        def tj(ti, a, b):
            return ti + max((b - a).length, 1e-9) ** alpha

        t0 = 0.0
        t1 = tj(t0, p0, p1)
        t2 = tj(t1, p1, p2)
        t3 = tj(t2, p2, p3)
        for s in range(per_seg):
            t = t1 + (t2 - t1) * s / per_seg
            a1 = (t1 - t) / (t1 - t0) * p0 + (t - t0) / (t1 - t0) * p1
            a2 = (t2 - t) / (t2 - t1) * p1 + (t - t1) / (t2 - t1) * p2
            a3 = (t3 - t) / (t3 - t2) * p2 + (t - t2) / (t3 - t2) * p3
            b1 = (t2 - t) / (t2 - t0) * a1 + (t - t0) / (t2 - t0) * a2
            b2 = (t3 - t) / (t3 - t1) * a2 + (t - t1) / (t3 - t1) * a3
            out.append((t2 - t) / (t2 - t1) * b1 + (t - t1) / (t2 - t1) * b2)
    if not closed:
        out.append(Vector(points[-1]))
    return out


def resample(points, n):
    """Re-space a polyline to `n` points evenly along its length."""
    P = [Vector(p) for p in points]
    d = [0.0]
    for a, b in zip(P, P[1:]):
        d.append(d[-1] + (b - a).length)
    L = d[-1] or 1.0
    out, j = [], 0
    for i in range(n):
        s = L * i / (n - 1)
        while j < len(d) - 2 and d[j + 1] < s:
            j += 1
        seg = (d[j + 1] - d[j]) or 1.0
        out.append(P[j].lerp(P[j + 1], (s - d[j]) / seg))
    return out


def _grid_cap(faces, verts, loop, tip=None, offset=None, power=0.8):
    """Fill a loop of 4k vertices with a k x k quad grid (Coons patch; four 3-poles at the corners),
    domed toward `tip`. offset: loop index of the first corner (default n/8, i.e. 45 deg from index 0)."""
    n = len(loop)
    k = n // 4
    o = n // 8 if offset is None else offset
    L = [loop[(o + i) % n] for i in range(n)]
    P = lambda idx: Vector(verts[idx])
    side = [L[s * k:(s + 1) * k + 1] if s < 3 else L[3 * k:] + [L[0]] for s in range(4)]
    G = [[None] * (k + 1) for _ in range(k + 1)]
    for i in range(k + 1):
        G[i][0] = side[0][i]
        G[i][k] = side[2][k - i]
    for j in range(k + 1):
        G[k][j] = side[1][j]
        G[0][j] = side[3][k - j]
    p00, p10, p01, p11 = P(G[0][0]), P(G[k][0]), P(G[0][k]), P(G[k][k])
    pts = {}
    for i in range(1, k):
        for j in range(1, k):
            u, v = i / k, j / k
            pts[i, j] = ((1 - v) * P(G[i][0]) + v * P(G[i][k]) + (1 - u) * P(G[0][j]) + u * P(G[k][j])
                         - ((1 - u) * (1 - v) * p00 + u * (1 - v) * p10 + (1 - u) * v * p01 + u * v * p11))
    if tip is not None and k % 2 == 0:
        lift = Vector(tip) - pts[k // 2, k // 2]
        for (i, j), p in pts.items():
            w = (math.sin(math.pi * i / k) * math.sin(math.pi * j / k)) ** power
            pts[i, j] = p + lift * w
    for (i, j), p in pts.items():
        verts.append(tuple(p))
        G[i][j] = len(verts) - 1
    for i in range(k):
        for j in range(k):
            faces.append([G[i][j], G[i + 1][j], G[i + 1][j + 1], G[i][j + 1]])


def _cap(faces, verts, loop, kind, tip=None, flip=False):
    """Close a boundary loop (list of vertex indices). kind: ngon | fan | quad | grid | point."""
    if not kind:
        return
    if kind == "grid" and len(loop) % 4 == 0:
        _grid_cap(faces, verts, loop, tip)
        return
    lp = list(reversed(loop)) if flip else list(loop)
    if kind == "ngon":
        faces.append(lp)
        return
    c = Vector((0, 0, 0))
    for i in loop:
        c += Vector(verts[i])
    c /= len(loop)
    verts.append(tuple(tip if tip is not None else c))
    ci = len(verts) - 1
    n = len(lp)
    if kind == "quad" and n % 2 == 0:
        for i in range(0, n, 2):
            faces.append([lp[i], lp[(i + 1) % n], lp[(i + 2) % n], ci])
    else:  # fan / point
        for i in range(n):
            faces.append([lp[i], lp[(i + 1) % n], ci])


def loft(name, rings, closed=True, caps=(None, None), tips=(None, None), smooth=True, collection=None):
    """Skin cross-section rings (lists of points, all the same length) into a clean quad mesh.
    closed: each ring is a closed loop (tube) vs an open row (sheet).
    caps: per end None | 'ngon' | 'fan' | 'quad' (quads around a center pole of valence n/2 —
          use n=8 for a perfect 4-pole) | 'grid' (k x k quad patch for n = 4k, domed toward `tips[i]`;
          best for skull tops and fingertips) | 'point' (fan to `tips[i]`, e.g. a hair tip).
    A ring of length 1 at either end is a tip vertex (triangle fan)."""
    verts, faces, idx = [], [], []
    for r in rings:
        idx.append(list(range(len(verts), len(verts) + len(r))))
        verts += [tuple(p) for p in r]
    for a, b in zip(idx, idx[1:]):
        if len(a) == 1 or len(b) == 1:
            tip, lp = (a[0], b) if len(a) == 1 else (b[0], a)
            n = len(lp)
            rng = range(n) if closed else range(n - 1)
            for i in rng:
                if len(a) == 1:
                    faces.append([tip, lp[(i + 1) % n], lp[i]])
                else:
                    faces.append([lp[i], lp[(i + 1) % n], tip])
            continue
        if len(a) != len(b):
            raise ValueError(f"loft: ring sizes differ ({len(a)} vs {len(b)})")
        n = len(a)
        rng = range(n) if closed else range(n - 1)
        for i in rng:
            j = (i + 1) % n
            faces.append([a[i], a[j], b[j], b[i]])
    if closed:
        if len(idx[0]) > 1:
            _cap(faces, verts, idx[0], caps[0], tips[0], flip=True)
        if len(idx[-1]) > 1:
            _cap(faces, verts, idx[-1], caps[1], tips[1])
    o = mesh(name, verts, faces, smooth=smooth, collection=collection)
    _fix_normals(o)
    return o


def _fix_normals(o):
    bm = bmesh.new()
    bm.from_mesh(o.data)
    bmesh.ops.recalc_face_normals(bm, faces=bm.faces)
    bm.to_mesh(o.data)
    bm.free()
    o.data.update()


def _frames(P, up=None):
    """Parallel-transport frames along a polyline: list of (tangent, normal, binormal)."""
    n = len(P)
    T = []
    for i in range(n):
        d = P[min(i + 1, n - 1)] - P[max(i - 1, 0)]
        T.append(d.normalized() if d.length > 1e-12 else Vector((0, 0, 1)))
    u = Vector(up) if up is not None else (Vector((0, 0, 1)) if abs(T[0].z) < 0.9 else Vector((1, 0, 0)))
    N = (u - T[0] * u.dot(T[0]))
    if N.length < 1e-9:
        N = T[0].orthogonal()
    N.normalize()
    out = [(T[0], N, T[0].cross(N))]
    for i in range(1, n):
        q = T[i - 1].rotation_difference(T[i])
        N = (q @ N)
        N = (N - T[i] * N.dot(T[i])).normalized()
        out.append((T[i], N, T[i].cross(N)))
    return out


def _val(v, t, i):
    if callable(v):
        return v(t)
    if isinstance(v, (list, tuple)) and v and isinstance(v[0], (list, tuple)):
        return v[i]
    if isinstance(v, (list, tuple)) and len(v) > 2:
        return v[i]
    return v


def sweep(name, path, radius=0.02, n=8, aspect=1.0, power=2.0, twist=0.0, up=None, smooth_path=0,
          caps=("quad", "quad"), shape=None, smooth=True, collection=None, across=None):
    """Sweep a superellipse cross-section along a path -> quad tube (limbs, fingers, hair clumps, tails).
    path: list of points; smooth_path=k re-samples it as a Catmull-Rom spline with k points.
    radius: number | (rx, ry) | per-point list | f(t 0..1) -> r or (rx, ry). aspect: ry/rx multiplier.
    twist: degrees (number or f(t)). up: section x-axis hint at the start (default world Z).
    caps: per end None | 'ngon' | 'fan' | 'quad' | 'point' (closes to the path end point — tapering tips).
    shape: f(angle, t) -> radius multiplier (e.g. a flat back on a hair clump).
    across: f(point, tangent, t) -> direction of the section's x axis (wide side of a flat hair lock
    = across the scalp); overrides parallel transport."""
    P = [Vector(p) for p in path]
    if smooth_path:
        P = resample(spline(P, per_seg=12), smooth_path)
    F = _frames(P, up)
    if across is not None:
        m0 = len(P)
        G = []
        for i, (T, N, B) in enumerate(F):
            a = Vector(across(P[i], T, i / (m0 - 1) if m0 > 1 else 0.0))
            a = a - T * a.dot(T)
            if a.length < 1e-9:
                a = N
            a.normalize()
            G.append((T, a, T.cross(a)))
        F = G
    rings, m = [], len(P)
    for i, (p, (T, N, B)) in enumerate(zip(P, F)):
        t = i / (m - 1) if m > 1 else 0.0
        r = _val(radius, t, i)
        rx, ry = (r if isinstance(r, (list, tuple)) else (r, r))
        ry *= aspect
        tw = math.radians(_val(twist, t, i) if callable(twist) else twist * t)
        Nq = Quaternion(T, tw) @ N
        fr = Matrix((Nq, T.cross(Nq), T)).transposed()
        sh = (lambda a, _t=t: shape(a, _t)) if shape else None
        rings.append(ring(n, max(rx, 1e-6), max(ry, 1e-6), p, fr, power, shape=sh))
    tips = [None, None]
    caps = list(caps)
    for k, end in ((0, 0), (1, -1)):
        if caps[k] == "point":
            if k == 0:
                rings[0] = [P[0]]
            else:
                rings[-1] = [P[-1]]
            caps[k] = None
    return loft(name, rings, closed=True, caps=tuple(caps), tips=tuple(tips), smooth=smooth, collection=collection)


def join(name, objects, delete=True, smooth=True, collection=None):
    """Merge mesh objects (in their world placement) into one object `name`; material slots are
    unioned and remapped. Sources are removed when delete=True. Returns the new object."""
    objs = [obj(o) for o in objects]
    verts, faces, fmat, mats = [], [], [], []
    for o in objs:
        me = o.data
        mw = o.matrix_world
        base = len(verts)
        verts.extend(mw @ v.co for v in me.vertices)
        slot = []
        for m in me.materials:
            if m is not None and m not in mats:
                mats.append(m)
            slot.append(mats.index(m) if m is not None else 0)
        for p in me.polygons:
            faces.append([base + i for i in p.vertices])
            fmat.append(slot[p.material_index] if slot else 0)
    if delete:
        for o in objs:
            me = o.data
            bpy.data.objects.remove(o)
            if me.users == 0:
                bpy.data.meshes.remove(me)
    o = mesh(name, verts, faces, smooth=smooth, collection=collection)
    o.data.materials.clear()
    for m in mats:
        o.data.materials.append(m)
    o.data.polygons.foreach_set("material_index", fmat)
    o.data.update()
    return o


def mod(o, kind, name=None, **props):
    """Add a modifier (kind = 'SUBSURF', 'MIRROR', 'SOLIDIFY'...) and set its properties."""
    o = obj(o)
    m = o.modifiers.new(name or kind.title(), kind)
    for k, v in props.items():
        setattr(m, k, v)
    return m


def subsurf(o, levels=2, render=None):
    """Subdivision surface (kept before outline modifiers)."""
    o = obj(o)
    m = next((m for m in o.modifiers if m.type == "SUBSURF"), None) or o.modifiers.new("Subsurf", "SUBSURF")
    m.levels = levels
    m.render_levels = levels if render is None else render
    _outline_last(o)
    return m


def mirror(o, axis="X", clip=True, merge=0.0005):
    """Mirror modifier (first in the stack) for symmetric modeling of one half (x >= 0)."""
    o = obj(o)
    m = next((m for m in o.modifiers if m.type == "MIRROR"), None) or o.modifiers.new("Mirror", "MIRROR")
    m.use_axis = [axis == "X", axis == "Y", axis == "Z"]
    m.use_clip = clip
    m.use_mirror_merge = True
    m.merge_threshold = merge
    while o.modifiers[0] != m:
        with bpy.context.temp_override(object=o):
            bpy.ops.object.modifier_move_up(modifier=m.name)
    return m


def symmetrize(o, direction="POSITIVE_X", threshold=1e-4):
    """Copy the +X half onto -X (or as `direction` says) on the base mesh."""
    o = obj(o)
    bm = bmesh.new()
    bm.from_mesh(o.data)
    bmesh.ops.symmetrize(bm, input=bm.verts[:] + bm.edges[:] + bm.faces[:], direction=direction, dist=threshold)
    bm.to_mesh(o.data)
    bm.free()
    o.data.update()
    return o


# ------------------------------------------------------------------ sculpt brushes


def _co(o, shape=None):
    me = o.data
    if shape:
        kb = me.shape_keys.key_blocks[shape]
        a = np.empty(len(kb.data) * 3, np.float64)
        kb.data.foreach_get("co", a)
    else:
        a = np.empty(len(me.vertices) * 3, np.float64)
        me.vertices.foreach_get("co", a)
    return a.reshape(-1, 3)


def _set_co(o, a, shape=None):
    me = o.data
    flat = np.ascontiguousarray(a, dtype=np.float64).ravel()
    if shape:
        me.shape_keys.key_blocks[shape].data.foreach_set("co", flat)
    else:
        me.vertices.foreach_set("co", flat)
    me.update()


def _normals(o, co=None):
    me = o.data
    a = np.empty(len(me.vertices) * 3, np.float64)
    try:
        me.vertex_normals.foreach_get("vector", a)
    except AttributeError:
        me.vertices.foreach_get("normal", a)
    return a.reshape(-1, 3)


_FALLOFF = {
    "smooth": lambda d: 1 - (3 * d ** 2 - 2 * d ** 3),
    "sphere": lambda d: np.sqrt(np.clip(1 - d ** 2, 0, 1)),
    "linear": lambda d: 1 - d,
    "sharp": lambda d: (1 - d) ** 2,
    "root": lambda d: 1 - np.sqrt(d),
    "const": lambda d: np.ones_like(d),
}


def _weights(co, center, radius, falloff, mask):
    r = np.asarray(radius if isinstance(radius, (list, tuple)) else (radius,) * 3, np.float64)
    d = np.linalg.norm((co - np.asarray(center, np.float64)) / r, axis=1)
    w = np.where(d < 1, _FALLOFF[falloff](np.clip(d, 0, 1)), 0.0)
    if mask is not None:
        w = w * (np.array([mask(Vector(v)) for v in co]) if callable(mask) else np.asarray(mask))
    return w


def _brush(o, center, radius, falloff, sym, mask, fn, shape=None):
    """Apply fn(co, w, center, sign) -> displacement per vertex, with X symmetry."""
    o = obj(o)
    co = _co(o, shape)
    c = np.asarray(center, np.float64)
    if not sym:
        w = _weights(co, c, radius, falloff, mask)
        co += fn(co, w, c, 1.0)
    else:
        cm = c * np.array([-1.0, 1, 1])
        side = np.sign(c[0]) if abs(c[0]) > 1e-9 else 1.0
        own = (np.sign(co[:, 0]) == side) | (np.abs(co[:, 0]) < 1e-6)
        w1 = _weights(co, c, radius, falloff, mask)
        w2 = _weights(co, cm, radius, falloff, mask)
        d1 = fn(co, w1, c, 1.0)
        d2 = fn(co, w2, cm, -1.0)
        if abs(c[0]) < 1e-9:
            d = d1
        else:
            d = np.where(own[:, None], d1, d2)
        center_line = np.abs(co[:, 0]) < 1e-6
        d[center_line, 0] = 0.0
        co += d
    _set_co(o, co, shape)
    return int(np.count_nonzero(np.abs(co).sum(1)))


def grab(o, center, radius, delta, falloff="smooth", sym=True, mask=None, shape=None):
    """Move vertices near `center` by `delta` (x mirrored on the other side). radius: r or (rx, ry, rz)."""
    dv = np.asarray(delta, np.float64)

    def fn(co, w, c, s):
        return w[:, None] * (dv * np.array([s, 1, 1]))

    _brush(o, center, radius, falloff, sym, mask, fn, shape)
    return obj(o)


def inflate(o, center, radius, amount, falloff="smooth", sym=True, mask=None, shape=None):
    """Push vertices along their normals (negative = deflate)."""
    o = obj(o)
    nrm = _normals(o)

    def fn(co, w, c, s):
        return w[:, None] * nrm * amount

    _brush(o, center, radius, falloff, sym, mask, fn, shape)
    return o


def pinch(o, center, radius, strength=0.3, falloff="smooth", sym=True, mask=None, axes=(1, 1, 1), shape=None):
    """Pull vertices toward the brush center (strength 0..1; negative pushes away). axes limits the pull."""
    ax = np.asarray(axes, np.float64)

    def fn(co, w, c, s):
        return w[:, None] * (c - co) * strength * ax

    _brush(o, center, radius, falloff, sym, mask, fn, shape)
    return obj(o)


def scale_region(o, center, radius, factors, pivot=None, falloff="smooth", sym=True, mask=None, shape=None):
    """Non-uniform scale (fx, fy, fz) around `pivot` (default: center), blended by the falloff."""
    f = np.asarray(factors if isinstance(factors, (list, tuple)) else (factors,) * 3, np.float64)
    pv = np.asarray(pivot if pivot is not None else center, np.float64)

    def fn(co, w, c, s):
        p = pv * np.array([s, 1, 1])
        return w[:, None] * ((co - p) * (f - 1))

    _brush(o, center, radius, falloff, sym, mask, fn, shape)
    return obj(o)


def flatten(o, center, radius, strength=0.5, normal=None, falloff="smooth", sym=True, mask=None, shape=None):
    """Project vertices toward the plane through their weighted centroid (normal: plane normal; default
    = average vertex normal)."""
    o = obj(o)
    nrm = _normals(o)

    def fn(co, w, c, s):
        if w.sum() < 1e-12:
            return np.zeros_like(co)
        cen = (co * w[:, None]).sum(0) / w.sum()
        nv = np.asarray(normal, np.float64) * np.array([s, 1, 1]) if normal is not None else (nrm * w[:, None]).sum(0)
        nv = nv / (np.linalg.norm(nv) or 1)
        dist = (co - cen) @ nv
        return -w[:, None] * strength * dist[:, None] * nv

    _brush(o, center, radius, falloff, sym, mask, fn, shape)
    return o


def _neighbors(o):
    me = o.data
    e = np.empty(len(me.edges) * 2, np.int64)
    me.edges.foreach_get("vertices", e)
    return e.reshape(-1, 2)


def relax(o, center=None, radius=None, iterations=5, factor=0.5, falloff="smooth", keep_boundary=True, sym=True, shape=None):
    """Laplacian smoothing (whole mesh, or a falloff region). Evens out lumps and spacing."""
    o = obj(o)
    co = _co(o, shape)
    E = _neighbors(o)
    nv = len(co)
    deg = np.bincount(E.ravel(), minlength=nv).astype(np.float64)
    w = np.ones(nv) if center is None else _weights(co, center, radius, falloff, None)
    if center is not None and sym:
        w = np.maximum(w, _weights(co, np.asarray(center) * np.array([-1.0, 1, 1]), radius, falloff, None))
    if keep_boundary:
        bm = bmesh.new()
        bm.from_mesh(o.data)
        bnd = np.array([v.is_boundary for v in bm.verts])
        bm.free()
        w = np.where(bnd, 0.0, w)
    center_line = np.abs(co[:, 0]) < 1e-6
    for _ in range(iterations):
        acc = np.zeros_like(co)
        np.add.at(acc, E[:, 0], co[E[:, 1]])
        np.add.at(acc, E[:, 1], co[E[:, 0]])
        avg = acc / np.maximum(deg, 1)[:, None]
        d = (avg - co) * (w * factor)[:, None]
        d[center_line, 0] = 0.0
        co = co + d
    _set_co(o, co, shape)
    return o


# ------------------------------------------------------------------ look


def _set_engine(sc, engine):
    names = {"EEVEE": ["BLENDER_EEVEE_NEXT", "BLENDER_EEVEE"], "CYCLES": ["CYCLES"], "WORKBENCH": ["BLENDER_WORKBENCH"]}
    for n in names.get(engine.upper(), [engine]):
        try:
            sc.render.engine = n
            return n
        except TypeError:
            continue
    raise RuntimeError(f"unknown engine {engine}")


def _try(fn):
    try:
        fn()
    except Exception:
        pass


def toon_scene(sun_rot=(50, 0, 35), sun_energy=None, ambient="#303040", samples=16):
    """EEVEE + Standard view transform (exact flat colors) + one crisp key sun + dim ambient world.
    sun_rot: XYZ Euler degrees. Returns the sun object (named 'Key')."""
    sc = bpy.context.scene
    _set_engine(sc, "EEVEE")
    vs = sc.view_settings
    _try(lambda: setattr(vs, "view_transform", "Standard"))
    _try(lambda: setattr(vs, "look", "None"))
    vs.exposure, vs.gamma = 0.0, 1.0
    _try(lambda: setattr(sc.eevee, "taa_render_samples", samples))
    _try(lambda: setattr(sc.eevee, "taa_samples", samples))
    w = sc.world or bpy.data.worlds.new("World")
    sc.world = w
    _try(lambda: setattr(w, "use_nodes", True))
    bg = next((n for n in w.node_tree.nodes if n.type == "BACKGROUND"), None)
    if bg:
        bg.inputs["Color"].default_value = hexc(ambient)
        bg.inputs["Strength"].default_value = 1.0
    sun = bpy.data.objects.get("Key")
    if sun is None or sun.type != "LIGHT":
        ld = bpy.data.lights.new("Key", "SUN")
        sun = bpy.data.objects.new("Key", ld)
        sc.collection.objects.link(sun)
    sun.data.energy = sun_energy if sun_energy is not None else math.pi
    _try(lambda: setattr(sun.data, "angle", math.radians(0.5)))
    _try(lambda: setattr(sun.data, "use_shadow", True))
    sun.rotation_euler = [math.radians(a) for a in sun_rot]
    return sun


def _node(nt, kind, loc, **inputs):
    n = nt.nodes.new(kind)
    n.location = loc
    for k, v in inputs.items():
        _inp(n, k).default_value = v
    return n


def _inp(n, name):
    for s in n.inputs:
        if s.identifier in (name, f"{name}_Color", f"{name}_Float") and getattr(s, "enabled", True):
            return s
    for s in n.inputs:
        if s.name == name and getattr(s, "enabled", True):
            return s
    return n.inputs[name]


def _out_sock(n, name):
    for s in n.outputs:
        if s.identifier in (name, f"{name}_Color", f"{name}_Float") and getattr(s, "enabled", True):
            return s
    for s in n.outputs:
        if s.name == name and getattr(s, "enabled", True):
            return s
    return n.outputs[name]


def _mix(nt, loc, blend="MIX"):
    m = nt.nodes.new("ShaderNodeMix")
    m.data_type = "RGBA"
    m.blend_type = blend
    m.location = loc
    return m


def toon_material(name, base, shade=None, deep=None, highlight=None, threshold=0.5, deep_at=0.15,
                  highlight_at=0.92, soft=0.02, rim=None, rim_width=0.35, rim_strength=0.6):
    """Cel material (EEVEE): Diffuse -> Shader to RGB -> stepped ramp -> flat emission.
    base / shade / deep / highlight: colors (hex or sRGB tuples) for lit, shadow, crease-shadow and
    hot-spot bands; shade defaults to a warm darker base. threshold: light level of the terminator
    (0..1), soft: band blur width (0 = razor). rim: color of a fresnel rim on the lit side or None.
    Re-calling with the same name rebuilds the node tree in place."""
    mat = bpy.data.materials.get(name) or bpy.data.materials.new(name)
    _try(lambda: setattr(mat, "use_nodes", True))
    nt = mat.node_tree
    nt.nodes.clear()
    b = hexc(base)
    s = hexc(shade) if shade else (b[0] * 0.55, b[1] * 0.42, b[2] * 0.5, 1)
    mat.diffuse_color = b
    dif = _node(nt, "ShaderNodeBsdfDiffuse", (-900, 0))
    _inp(dif, "Color").default_value = (1, 1, 1, 1)
    s2r = _node(nt, "ShaderNodeShaderToRGB", (-700, 0))
    nt.links.new(dif.outputs[0], s2r.inputs[0])
    bw = _node(nt, "ShaderNodeRGBToBW", (-520, 0))
    nt.links.new(s2r.outputs[0], bw.inputs[0])
    ramp = _node(nt, "ShaderNodeValToRGB", (-340, 0))
    cr = ramp.color_ramp
    # build bands: color i holds from stop i to stop i+1; `soft` widens each step into a short blend
    cr.interpolation = "LINEAR" if soft > 0 else "CONSTANT"
    bands = [(0.0, hexc(deep) if deep else s)]
    if deep:
        bands.append((deep_at, s))
    bands.append((threshold, b))
    if highlight:
        bands.append((highlight_at, hexc(highlight)))
    els = cr.elements
    while len(els) > 1:
        els.remove(els[-1])
    pts = []
    for i, (pos, col) in enumerate(bands):
        if i == 0:
            pts.append((0.0, col))
            continue
        prev = bands[i - 1][1]
        if soft > 0:
            pts.append((max(pos - soft / 2, 0.0), prev))
            pts.append((min(pos + soft / 2, 1.0), col))
        else:
            pts.append((pos, col))
    els[0].position, els[0].color = pts[0]
    for pos, col in pts[1:]:
        e = els.new(pos)
        e.color = col
    nt.links.new(bw.outputs[0], _inp(ramp, "Fac"))
    color_out = ramp.outputs["Color"]
    if rim:
        lw = _node(nt, "ShaderNodeLayerWeight", (-520, -260))
        _inp(lw, "Blend").default_value = rim_width
        rr = _node(nt, "ShaderNodeValToRGB", (-340, -260))
        rr.color_ramp.interpolation = "CONSTANT"
        rr.color_ramp.elements[0].color = (0, 0, 0, 1)
        rr.color_ramp.elements[1].position = 0.5
        rr.color_ramp.elements[1].color = (1, 1, 1, 1)
        nt.links.new(lw.outputs["Facing"], _inp(rr, "Fac"))
        lit = _node(nt, "ShaderNodeMath", (-160, -360))
        lit.operation = "MULTIPLY"
        nt.links.new(rr.outputs["Color"], lit.inputs[0])
        lit.inputs[1].default_value = rim_strength
        m = _mix(nt, (0, 0), "SCREEN")
        nt.links.new(lit.outputs[0], _inp(m, "Factor"))
        nt.links.new(color_out, _inp(m, "A"))
        _inp(m, "B").default_value = hexc(rim)
        color_out = _out_sock(m, "Result")
    em = _node(nt, "ShaderNodeEmission", (200, 0))
    nt.links.new(color_out, _inp(em, "Color"))
    outn = _node(nt, "ShaderNodeOutputMaterial", (400, 0))
    nt.links.new(em.outputs[0], outn.inputs["Surface"])
    return mat


def assign(o, mat, slot=0):
    """Put a material into slot `slot` (creating slots as needed)."""
    o = obj(o)
    m = bpy.data.materials[mat] if isinstance(mat, str) else mat
    while len(o.data.materials) <= slot:
        o.data.materials.append(None)
    o.data.materials[slot] = m
    return o


def _outline_last(o):
    ol = [m for m in o.modifiers if m.type == "SOLIDIFY" and m.name.startswith("Outline")]
    for m in ol:
        while o.modifiers[-1] != m:
            with bpy.context.temp_override(object=o):
                bpy.ops.object.modifier_move_down(modifier=m.name)


def outline(o, width=0.0025, color="#2b1d24", vgroup=None, vgroup_min=0.0):
    """Inverted-hull outline: a back-face-culled, flipped Solidify shell with a flat dark material.
    width in meters (~0.002-0.004 for a 1.6 m character). vgroup scales the width per vertex
    (thin at fingertips/eyelids, thick at silhouette)."""
    o = obj(o)
    key = f"Outline_{color.lstrip('#') if isinstance(color, str) else 'c'}"
    mat = bpy.data.materials.get(key)
    if mat is None:
        mat = bpy.data.materials.new(key)
        _try(lambda: setattr(mat, "use_nodes", True))
        nt = mat.node_tree
        nt.nodes.clear()
        em = _node(nt, "ShaderNodeEmission", (0, 0))
        _inp(em, "Color").default_value = hexc(color)
        outn = _node(nt, "ShaderNodeOutputMaterial", (200, 0))
        nt.links.new(em.outputs[0], outn.inputs["Surface"])
        mat.diffuse_color = hexc(color)
    mat.use_backface_culling = True
    _try(lambda: setattr(mat, "use_backface_culling_shadow", True))
    _try(lambda: setattr(mat, "shadow_method", "NONE"))
    mats = o.data.materials
    if not len(mats):
        mats.append(None)
    k = max(len([m for m in mats if not (m and m.name.startswith("Outline_"))]), 1)
    if mat.name not in [m.name for m in mats if m]:
        # outline slot must be the last one: indices >= k clamp to it
        for i in range(len(mats) - 1, -1, -1):
            if mats[i] and mats[i].name.startswith("Outline_"):
                mats.pop(index=i)
        mats.append(mat)
    m = next((m for m in o.modifiers if m.type == "SOLIDIFY" and m.name.startswith("Outline")), None)
    if m is None:
        m = o.modifiers.new("Outline", "SOLIDIFY")
    m.thickness = width
    m.offset = 1.0
    m.use_flip_normals = True
    m.use_rim = False
    m.use_even_offset = False
    _try(lambda: setattr(m, "use_quality_normals", True))
    m.material_offset = k
    if vgroup:
        m.vertex_group = vgroup
        m.thickness_vertex_group = vgroup_min
    _outline_last(o)
    return m


def smooth_normals(o, center, radii, region=None, region_radius=0.1, falloff="smooth", strength=1.0,
                   name="NormalProxy"):
    """Cel-shading normals (the anime face trick): copy custom normals from a smooth ellipsoid proxy
    (center, radii; object-local) onto `o` with a Data Transfer modifier, so terminator lines fall
    in clean shapes instead of following every bump. The effect is weighted by a vertex group: full
    inside `region` (a point, with region_radius r or (rx, ry, rz)) fading out by `falloff`; the
    whole mesh when region is None. Re-call to update. Returns the (hidden) proxy object."""
    o = obj(o)
    pname = f"{o.name}_{name}"
    bm = bmesh.new()
    try:
        bmesh.ops.create_uvsphere(bm, u_segments=64, v_segments=40, radius=1.0)
    except TypeError:
        bmesh.ops.create_uvsphere(bm, u_segments=64, v_segments=40, diameter=1.0)
    c = Vector(center)
    for v in bm.verts:
        v.co = Vector((v.co.x * radii[0], v.co.y * radii[1], v.co.z * radii[2])) + c
    me = bpy.data.meshes.new(pname)
    bm.to_mesh(me)
    bm.free()
    _try(me.shade_smooth)
    p = bpy.data.objects.get(pname)
    if p is None:
        p = bpy.data.objects.new(pname, me)
        (o.users_collection[0] if o.users_collection else bpy.context.scene.collection).objects.link(p)
    else:
        old = p.data
        p.data = me
        if old.users == 0:
            bpy.data.meshes.remove(old)
    me.name = pname
    p.parent = o
    p.matrix_parent_inverse = Matrix.Identity(4)
    p.matrix_basis = Matrix.Identity(4)
    p.display_type = "WIRE"
    p.hide_render = True
    _try(lambda: p.hide_set(True))
    vg = o.vertex_groups.get(name) or o.vertex_groups.new(name=name)
    co = _co(o)
    w = np.ones(len(co)) if region is None else _weights(co, region, region_radius, falloff, None)
    w = np.clip(w * strength, 0, 1)
    vg.remove(list(range(len(co))))
    for i in np.nonzero(w > 1e-4)[0]:
        vg.add([int(i)], float(w[i]), "REPLACE")
    m = o.modifiers.get(name) or o.modifiers.new(name, "DATA_TRANSFER")
    m.object = p
    m.use_loop_data = True
    m.data_types_loops = {"CUSTOM_NORMAL"}
    m.loop_mapping = "POLYINTERP_NEAREST"
    m.vertex_group = vg.name
    m.mix_mode = "REPLACE"
    m.mix_factor = 1.0
    _outline_last(o)
    return p


def ref(path, view="front", height=1.7, opacity=0.4, name=None, offset=1.5):
    """Reference image on an image empty facing the given view (front | side | back), bottom at z=0."""
    img = bpy.data.images.load(path, check_existing=True)
    e = bpy.data.objects.get(name or f"REF_{view}") or bpy.data.objects.new(name or f"REF_{view}", None)
    if e.name not in bpy.context.scene.collection.objects:
        bpy.context.scene.collection.objects.link(e)
    e.empty_display_type = "IMAGE"
    e.data = img
    e.empty_display_size = height
    e.empty_image_offset = (-0.5, 0.0)
    e.color[3] = opacity
    _try(lambda: setattr(e, "use_empty_image_alpha", True))
    rot = {"front": (90, 0, 0), "side": (90, 0, 90), "back": (90, 0, 180)}[view]
    e.rotation_euler = [math.radians(a) for a in rot]
    e.location = {"front": (0, offset, 0), "side": (-offset, 0, 0), "back": (0, -offset, 0)}[view]
    e.hide_render = True
    return e


# ------------------------------------------------------------------ decals (procedural 2D art on 3D)


class Paint:
    """RGBA canvas for procedural decal art (eyes, brows, mouth, blush, logos). Coordinates are UV:
    u right, v up, both 0..1. Every draw call takes an SDF-ish shape, anti-aliases it over `soft`
    pixels and composites it with straight-alpha 'over'; colors are hex / sRGB tuples or a field
    from p.gradient(). Draw calls return the shape's coverage (H x W, 0..1) so it can be reused as
    `clip=` (e.g. clip the iris to the eye opening) or combined with `*` / `np.maximum`."""

    def __init__(self, w=512, h=None, bg=(0, 0, 0, 0)):
        h = h or w
        self.w, self.h = w, h
        self.a = np.zeros((h, w, 4), np.float32)
        self.a[:] = bg
        ys, xs = np.mgrid[0:h, 0:w]
        self.u = (xs + 0.5) / w
        self.v = 1.0 - (ys + 0.5) / h
        self.px = 1.0 / min(w, h)

    @staticmethod
    def _rgba(c):
        if isinstance(c, str):
            s = c.lstrip("#")
            v = [int(s[i:i + 2], 16) / 255 for i in range(0, len(s), 2)]
            return np.array(v + [1.0] * (4 - len(v)), np.float32)
        c = np.asarray(c, np.float32)
        if c.ndim == 1 and len(c) == 3:
            c = np.append(c, 1.0)
        if c.ndim == 3 and c.shape[2] == 3:
            c = np.concatenate([c, np.ones(c.shape[:2] + (1,), np.float32)], 2)
        return c

    def curve(self, pts, n=12, closed=False):
        """Smooth (centripetal Catmull-Rom) 2D curve through pts -> list of (u, v)."""
        P = spline([Vector((p[0], p[1], 0)) for p in pts], per_seg=n, closed=closed)
        return [(p.x, p.y) for p in P]

    def coverage(self, sdf, soft=1.0):
        return np.clip(0.5 - sdf / (max(soft, 1e-3) * self.px), 0.0, 1.0)

    def over(self, cov, color, opacity=1.0, clip=None):
        """Composite `color` with coverage `cov` (H x W) over the canvas. Returns cov."""
        c = self._rgba(color)
        if clip is not None:
            cov = cov * clip
        if c.ndim == 1:
            rgb, ca = c[:3], c[3]
        else:
            rgb, ca = c[..., :3], c[..., 3]
        sa = cov * ca * opacity
        da = self.a[..., 3]
        oa = sa + da * (1 - sa)
        num = rgb * sa[..., None] + self.a[..., :3] * (da * (1 - sa))[..., None]
        self.a[..., :3] = np.where(oa[..., None] > 1e-6, num / np.maximum(oa, 1e-6)[..., None], self.a[..., :3])
        self.a[..., 3] = oa
        return cov

    # --- shapes (signed distance in uv units, negative inside)
    def sdf_ellipse(self, c, r, rot=0.0):
        x, y = self.u - c[0], self.v - c[1]
        if rot:
            t = math.radians(rot)
            x, y = x * math.cos(t) + y * math.sin(t), -x * math.sin(t) + y * math.cos(t)
        rx, ry = (r, r) if np.isscalar(r) else r
        q = np.sqrt((x / rx) ** 2 + (y / ry) ** 2)
        # first-order distance estimate: (q - 1) / |grad q|
        g = np.sqrt((x / rx ** 2) ** 2 + (y / ry ** 2) ** 2) / np.maximum(q, 1e-6)
        return (q - 1) / np.maximum(g, 1e-6)

    def _seg_dist(self, a, b):
        ax, ay = a
        bx, by = b
        dx, dy = bx - ax, by - ay
        L2 = max(dx * dx + dy * dy, 1e-12)
        t = np.clip(((self.u - ax) * dx + (self.v - ay) * dy) / L2, 0, 1)
        return np.hypot(self.u - ax - t * dx, self.v - ay - t * dy), t

    def sdf_poly(self, pts):
        d = np.full(self.u.shape, 1e9, np.float32)
        inside = np.zeros(self.u.shape, bool)
        n = len(pts)
        for i in range(n):
            a, b = pts[i], pts[(i + 1) % n]
            dist, _ = self._seg_dist(a, b)
            d = np.minimum(d, dist)
            # even-odd crossing test
            cond = (a[1] > self.v) != (b[1] > self.v)
            xint = a[0] + (self.v - a[1]) * (b[0] - a[0]) / ((b[1] - a[1]) or 1e-12)
            inside ^= cond & (self.u < xint)
        return np.where(inside, -d, d)

    def sdf_stroke(self, pts, width):
        """Polyline with per-point (or constant) full width -> tapered brush stroke."""
        n = len(pts)
        ws = [width] * n if np.isscalar(width) else list(width)
        if len(ws) != n:  # resample width list along the polyline
            ws = list(np.interp(np.linspace(0, 1, n), np.linspace(0, 1, len(ws)), ws))
        d = np.full(self.u.shape, 1e9, np.float32)
        for i in range(n - 1):
            dist, t = self._seg_dist(pts[i], pts[i + 1])
            r = (ws[i] + (ws[i + 1] - ws[i]) * t) / 2
            d = np.minimum(d, dist - r)
        return d

    # --- draw calls
    def ellipse(self, c, r, color, rot=0.0, soft=1.0, opacity=1.0, clip=None):
        return self.over(self.coverage(self.sdf_ellipse(c, r, rot), soft), color, opacity, clip)

    def poly(self, pts, color, soft=1.0, opacity=1.0, clip=None, smooth=0):
        if smooth:
            pts = self.curve(pts, smooth, closed=True)
        return self.over(self.coverage(self.sdf_poly(pts), soft), color, opacity, clip)

    def stroke(self, pts, width, color, soft=1.0, opacity=1.0, clip=None, smooth=8):
        """Brush stroke through pts; width: one value or a list (tapers, resampled along the curve)."""
        if smooth and len(pts) > 2:
            pts = self.curve(pts, smooth)
        return self.over(self.coverage(self.sdf_stroke(pts, width), soft), color, opacity, clip)

    def gradient(self, stops, p0=(0.5, 1.0), p1=(0.5, 0.0), radial=False):
        """Color field for fills: stops = [(t, color), ...] along p0->p1 (or radial from p0 with
        radius |p1-p0|)."""
        if radial:
            t = np.hypot(self.u - p0[0], self.v - p0[1]) / max(math.dist(p0, p1), 1e-6)
        else:
            dx, dy = p1[0] - p0[0], p1[1] - p0[1]
            t = ((self.u - p0[0]) * dx + (self.v - p0[1]) * dy) / max(dx * dx + dy * dy, 1e-12)
        t = np.clip(t, 0, 1)
        ts = [s[0] for s in stops]
        cs = np.array([self._rgba(s[1]) for s in stops])
        return np.stack([np.interp(t, ts, cs[:, k]) for k in range(4)], -1).astype(np.float32)

    def image(self, name, path=None):
        """Store as a Blender image (sRGB, alpha) -- saved to `path` (PNG) when given, else packed."""
        return image(name, self.a, path)


def image(name, rgba, path=None):
    """numpy H x W x 4 straight-alpha sRGB array (row 0 = top) -> Blender image `name`."""
    h, w = rgba.shape[:2]
    img = bpy.data.images.get(name)
    if img is not None and (img.size[0] != w or img.size[1] != h):
        bpy.data.images.remove(img)
        img = None
    if img is None:
        img = bpy.data.images.new(name, w, h, alpha=True)
    _try(lambda: setattr(img.colorspace_settings, "name", "sRGB"))
    img.alpha_mode = "STRAIGHT"
    img.pixels.foreach_set(np.ascontiguousarray(rgba[::-1]).ravel().astype(np.float32))
    if path:
        os.makedirs(os.path.dirname(path), exist_ok=True)
        img.filepath_raw = path
        img.file_format = "PNG"
        img.save()
        img.source = "FILE"
        img.filepath = path
        img.reload()
    else:
        _try(img.pack)
    return img


def decal_material(name, img, strength=1.0, interpolation="Linear"):
    """Unlit alpha material for decals (eyes, brows, mouth, blush): image color -> emission,
    image alpha -> transparency."""
    img = bpy.data.images[img] if isinstance(img, str) else img
    mat = bpy.data.materials.get(name) or bpy.data.materials.new(name)
    _try(lambda: setattr(mat, "use_nodes", True))
    nt = mat.node_tree
    nt.nodes.clear()
    tex = _node(nt, "ShaderNodeTexImage", (-500, 0))
    tex.image = img
    tex.interpolation = interpolation
    tex.extension = "CLIP"
    em = _node(nt, "ShaderNodeEmission", (-200, -80))
    _inp(em, "Strength").default_value = strength
    nt.links.new(tex.outputs["Color"], _inp(em, "Color"))
    tr = _node(nt, "ShaderNodeBsdfTransparent", (-200, 80))
    mix = _node(nt, "ShaderNodeMixShader", (0, 0))
    nt.links.new(tex.outputs["Alpha"], mix.inputs[0])
    nt.links.new(tr.outputs[0], mix.inputs[1])
    nt.links.new(em.outputs[0], mix.inputs[2])
    outn = _node(nt, "ShaderNodeOutputMaterial", (200, 0))
    nt.links.new(mix.outputs[0], outn.inputs["Surface"])
    _try(lambda: setattr(mat, "surface_render_method", "BLENDED"))
    _try(lambda: setattr(mat, "blend_method", "BLEND"))
    _try(lambda: setattr(mat, "shadow_method", "NONE"))
    mat.use_backface_culling = True
    return mat


def decal(target, name, material, center, size, view=(0, 1, 0), rotation=0.0, res=(12, 12),
          offset=0.0004, mirror=False):
    """Project a textured grid onto `target`'s evaluated surface (object-local coordinates).
    center: 3D point the decal is aimed at; size: (w, h) in meters on the projection plane;
    view: projection direction (default +Y = seen from the front); rotation: degrees around view.
    mirror=True also builds the X-mirrored twin '<name>.R' (the image mirrors with it).
    The decal is parented to target and offset along the surface normal. Returns the object(s)."""
    from mathutils.bvhtree import BVHTree

    target = obj(target)
    # project onto the skin, not onto the inverted-hull outline shell
    hide = [m for m in target.modifiers if m.type == "SOLIDIFY" and m.name.startswith("Outline") and m.show_viewport]
    for m in hide:
        m.show_viewport = False
    bpy.context.view_layer.update()
    dg = bpy.context.evaluated_depsgraph_get()
    bvh = BVHTree.FromObject(target, dg)
    for m in hide:
        m.show_viewport = True
    d = Vector(view).normalized()
    up0 = Vector((0, 0, 1)) if abs(d.z) < 0.9 else Vector((0, 1, 0))
    rt = d.cross(up0).normalized()
    up = rt.cross(d).normalized()
    if rotation:
        q = Quaternion(d, math.radians(rotation))
        rt, up = q @ rt, q @ up
    c = Vector(center)
    nu, nv = res
    verts, uvs = [], []
    for j in range(nv + 1):
        for i in range(nu + 1):
            s, t = i / nu, j / nv
            p = c + rt * ((s - 0.5) * size[0]) + up * ((t - 0.5) * size[1])
            hit = bvh.ray_cast(p - d * 0.5, d, 1.0)
            if hit[0] is None:
                loc, nrm, _, _ = bvh.find_nearest(p)
            else:
                loc, nrm = hit[0], hit[1]
            verts.append(loc + nrm.normalized() * offset)
            uvs.append((s, t))
    faces = [(j * (nu + 1) + i, j * (nu + 1) + i + 1, (j + 1) * (nu + 1) + i + 1, (j + 1) * (nu + 1) + i)
             for j in range(nv) for i in range(nu)]
    out = []
    for side in ([1, -1] if mirror else [1]):
        vv = [Vector((v.x * side, v.y, v.z)) for v in verts]
        ff = faces if side == 1 else [tuple(reversed(f)) for f in faces]
        nm = name if side == 1 else f"{name}.R"
        o = mesh(nm, vv, ff, smooth=True)
        me = o.data
        uvl = me.uv_layers.get("UVMap") or me.uv_layers.new(name="UVMap")
        for poly in me.polygons:
            for li, vi in zip(poly.loop_indices, poly.vertices):
                uvl.data[li].uv = uvs[vi]
        mat = bpy.data.materials[material] if isinstance(material, str) else material
        me.materials.clear()
        me.materials.append(mat)
        o.parent = target
        o.matrix_parent_inverse = Matrix.Identity(4)
        o.matrix_basis = Matrix.Identity(4)
        _try(lambda o=o: setattr(o, "visible_shadow", False))
        out.append(o)
    return out[0] if len(out) == 1 else out


# ------------------------------------------------------------------ SDF modelling (volume -> mesh)
# Organic forms (heads, hands, hair clumps) are easier to fit to references as signed-distance fields:
# fn(x, y, z) -> distance (negative inside), vectorised over numpy arrays. Combine pieces with
# smin/smax (soft union/intersection, k = blend radius in metres), mesh with sdf_mesh.


def smin(a, b, k):
    """Soft union of two distance fields (polynomial smooth-min, blend radius k)."""
    h = np.clip(0.5 + 0.5 * (b - a) / k, 0.0, 1.0)
    return b * (1 - h) + a * h - k * h * (1 - h)


def smax(a, b, k):
    """Soft intersection / soft subtraction (smax(a, -b, k)) of distance fields."""
    return -smin(-a, -b, k)


def sd_capsule(x, y, z, a, b, ra, rb=None):
    """Distance to a (cone-)capsule from a to b with radii ra -> rb."""
    rb = ra if rb is None else rb
    a = np.asarray(a, float)
    ba = np.asarray(b, float) - a
    px, py, pz = x - a[0], y - a[1], z - a[2]
    t = np.clip((px * ba[0] + py * ba[1] + pz * ba[2]) / float(ba @ ba), 0.0, 1.0)
    qx, qy, qz = px - t * ba[0], py - t * ba[1], pz - t * ba[2]
    return np.sqrt(qx * qx + qy * qy + qz * qz) - (ra + (rb - ra) * t)


def sd_ellipsoid(x, y, z, center, radii):
    """Approximate (gradient-normalised) distance to an axis-aligned ellipsoid."""
    cx, cy, cz = center
    rx, ry, rz = radii
    k0 = np.sqrt(((x - cx) / rx) ** 2 + ((y - cy) / ry) ** 2 + ((z - cz) / rz) ** 2)
    k1 = np.sqrt(((x - cx) / rx**2) ** 2 + ((y - cy) / ry**2) ** 2 + ((z - cz) / rz**2) ** 2)
    return k0 * (k0 - 1.0) / np.maximum(k1, 1e-9)


def sdf_sample(fn, lo, hi, step):
    """Sample fn on a regular grid (z-slice by slice). Returns (V[i,j,k] float32, origin)."""
    xs = np.arange(lo[0], hi[0] + step * 0.5, step)
    ys = np.arange(lo[1], hi[1] + step * 0.5, step)
    zs = np.arange(lo[2], hi[2] + step * 0.5, step)
    V = np.empty((len(xs), len(ys), len(zs)), np.float32)
    X, Y = np.meshgrid(xs, ys, indexing="ij")
    for k, z in enumerate(zs):
        V[:, :, k] = fn(X, Y, np.full_like(X, z))
    return V, np.array(lo, float)


def surface_nets(V, origin, step):
    """Naive surface nets: grid of signed distances -> (verts (N,3), quads (M,4)), outward winding."""
    nx, ny, nz = V.shape
    c = [V[i:nx - 1 + i, j:ny - 1 + j, k:nz - 1 + k] for k in (0, 1) for j in (0, 1) for i in (0, 1)]
    off = np.array([(i, j, k) for k in (0, 1) for j in (0, 1) for i in (0, 1)], float)
    edges = [(0, 1), (2, 3), (4, 5), (6, 7), (0, 2), (1, 3), (4, 6), (5, 7), (0, 4), (1, 5), (2, 6), (3, 7)]
    acc = np.zeros(c[0].shape + (3,), np.float32)
    cnt = np.zeros(c[0].shape, np.float32)
    for a, b in edges:
        va, vb = c[a], c[b]
        m = (va < 0) != (vb < 0)
        t = np.where(m, va / np.where(m, va - vb, 1), 0)
        acc += np.where(m[..., None], off[a] + (off[b] - off[a]) * t[..., None], 0)
        cnt += m
    act = cnt > 0
    idx = -np.ones(act.shape, np.int64)
    idx[act] = np.arange(int(act.sum()))
    verts = origin + step * (np.argwhere(act) + acc[act] / cnt[act][:, None])
    quads = []
    for ax in range(3):
        sl0 = [slice(1, -1)] * 3
        sl1 = [slice(1, -1)] * 3
        sl0[ax] = slice(0, -1)
        sl1[ax] = slice(1, None)
        s0 = V[tuple(sl0)] < 0
        m = s0 != (V[tuple(sl1)] < 0)
        e = np.argwhere(m) + 1
        e[:, ax] -= 1
        i, j, k = e[:, 0], e[:, 1], e[:, 2]
        if ax == 0:
            q = np.stack([idx[i, j - 1, k - 1], idx[i, j, k - 1], idx[i, j, k], idx[i, j - 1, k]], 1)
        elif ax == 1:
            q = np.stack([idx[i - 1, j, k - 1], idx[i - 1, j, k], idx[i, j, k], idx[i, j, k - 1]], 1)
        else:
            q = np.stack([idx[i - 1, j - 1, k], idx[i, j - 1, k], idx[i, j, k], idx[i - 1, j, k]], 1)
        f = s0[m]
        q[f] = q[f][:, ::-1]
        quads.append(q)
    return verts, np.concatenate(quads)


class Field:
    """A distance field baked on a grid: fast trilinear lookups, gradients, offsets and a vertical
    'drape' (hair / cloth falls straight down from the widest part). Callable like any SDF fn, so it
    works with sdf_mesh. Build: Field(fn, lo, hi, step) or Field.from_grid(V, origin, step)."""

    def __init__(self, fn=None, lo=None, hi=None, step=0.002):
        if fn is not None:
            self.V, self.o = sdf_sample(fn, lo, hi, step)
        self.h = float(step)

    @classmethod
    def from_grid(cls, V, origin, step):
        f = cls(step=step)
        f.V, f.o = V.astype(np.float32), np.asarray(origin, float)
        return f

    def __call__(self, x, y, z):
        V = self.V
        g = np.stack([np.asarray(x, float), np.asarray(y, float), np.asarray(z, float)], -1)
        g = np.clip((g - self.o) / self.h, 0, np.array(V.shape) - 1.0001)
        i = np.floor(g).astype(np.int64)
        f = g - i
        i0, i1, i2 = i[..., 0], i[..., 1], i[..., 2]
        fx, fy, fz = f[..., 0], f[..., 1], f[..., 2]
        c00 = V[i0, i1, i2] * (1 - fx) + V[i0 + 1, i1, i2] * fx
        c10 = V[i0, i1 + 1, i2] * (1 - fx) + V[i0 + 1, i1 + 1, i2] * fx
        c01 = V[i0, i1, i2 + 1] * (1 - fx) + V[i0 + 1, i1, i2 + 1] * fx
        c11 = V[i0, i1 + 1, i2 + 1] * (1 - fx) + V[i0 + 1, i1 + 1, i2 + 1] * fx
        return (c00 * (1 - fy) + c10 * fy) * (1 - fz) + (c01 * (1 - fy) + c11 * fy) * fz

    def at(self, p):
        p = np.asarray(p, float)
        return self(p[..., 0], p[..., 1], p[..., 2])

    def grad(self, p, normalize=True):
        p = np.asarray(p, float)
        e = self.h
        g = np.stack([self.at(p + d) - self.at(p - d) for d in np.eye(3) * e], -1) / (2 * e)
        if normalize:
            g = g / np.maximum(np.linalg.norm(g, axis=-1, keepdims=True), 1e-9)
        return g

    def offset(self, d):
        """Field grown outward by d metres (d may be an array shaped like the grid)."""
        return Field.from_grid(self.V - d, self.o, self.h)

    def drape(self, z_from=None):
        """Vertical curtain: every point is inside if anything above it (up to z_from) is inside."""
        V = self.V.copy()
        k0 = V.shape[2] - 1 if z_from is None else int(np.clip((z_from - self.o[2]) / self.h, 0, V.shape[2] - 1))
        V[:, :, :k0 + 1] = np.minimum.accumulate(V[:, :, :k0 + 1][:, :, ::-1], axis=2)[:, :, ::-1]
        return Field.from_grid(V, self.o, self.h)

    def coords(self):
        """Grid point coordinates (X, Y, Z) shaped like V (for building per-voxel offsets)."""
        n = self.V.shape
        ax = [self.o[i] + self.h * np.arange(n[i]) for i in range(3)]
        return np.meshgrid(*ax, indexing="ij")

    def ray(self, center, dirs, level=0.0, rmin=0.0, rmax=0.35):
        """First crossing of field = level going out from center along dirs (N,3); level may be per-ray."""
        c = np.asarray(center, float)
        d = np.asarray(dirs, float)
        d = d / np.linalg.norm(d, axis=-1, keepdims=True)
        lv = np.broadcast_to(np.asarray(level, float), d.shape[:-1])
        steps = np.arange(rmin, rmax, self.h)
        vals = np.stack([self.at(c + d * r) - lv for r in steps], -1)
        out = vals > 0
        k = np.where(out.any(-1), np.argmax(out, -1), len(steps) - 1)
        lo = steps[np.maximum(k - 1, 0)]
        hi = steps[k]
        for _ in range(20):
            mid = 0.5 * (lo + hi)
            o = (self.at(c + d * mid[..., None]) - lv) > 0
            hi = np.where(o, mid, hi)
            lo = np.where(o, lo, mid)
        return c + d * (0.5 * (lo + hi))[..., None]


def lock(name, path, normals, width=0.03, thick=0.5, flat=0.5, belly=0.3, power=1.6, n=10,
         tip_power=0.8, twist=0.0, collection=None):
    """Anime hair clump: a lens-section blade swept along `path`, wide side lying on the surface whose
    outward `normals` are given per point; width tapers to a sharp tip (path end), widest at `belly`.
    thick = outer half-thickness / half-width, flat = inner side factor (0.5 -> flatter underside).
    width may be a number or f(t) -> full width."""
    P = [Vector(p) for p in path]
    Nn = [Vector(v) for v in normals]
    m = len(P)

    def wfun(t):
        w = width(t) if callable(width) else width * (1 - t ** 2.2) ** tip_power * (
            1 + 0.18 * math.exp(-((t - belly) / 0.28) ** 2))
        return max(w * 0.5, 1e-5)

    def across(p, T, t):
        i = min(int(round(t * (m - 1))), m - 1)
        return Nn[i].cross(T)

    def shape(a, t):
        s = math.sin(a)
        return 1.0 - (1.0 - flat) * max(0.0, -s)

    return sweep(name, P, radius=lambda t: (wfun(t), wfun(t) * thick), n=n, power=power, twist=twist,
                 caps=("fan", "point"), shape=shape, collection=collection, across=across)


def _mesh_np(name, verts, faces, collection=None):
    me = bpy.data.meshes.new(name)
    me.vertices.add(len(verts))
    me.vertices.foreach_set("co", np.asarray(verts, np.float32).ravel())
    f = np.asarray(faces, np.int32)
    me.loops.add(f.size)
    me.loops.foreach_set("vertex_index", f.ravel())
    me.polygons.add(len(f))
    me.polygons.foreach_set("loop_start", np.arange(0, f.size, f.shape[1], dtype=np.int32))
    me.update(calc_edges=True)
    me.validate(clean_customdata=False)
    return _attach(name, me, collection, True)


def sdf_mesh(name, fn, lo, hi, step=0.002, relax=2, remesh=None, symmetric=True, collection=None):
    """Mesh a distance field. lo/hi = bounds (keep lo.x = -hi.x for X-symmetric fields), step = voxel
    size. relax = Laplacian passes; remesh = target face count for QuadriFlow (clean quads) or None
    to keep the voxel-grid quads. Reuses an existing object of that name (materials, modifiers kept)."""
    V, origin = sdf_sample(fn, lo, hi, step)
    v, q = surface_nets(V, origin, step)
    o = _mesh_np(name, v, q, collection)
    if relax:
        globals()["relax"](o, iterations=relax, factor=0.5, keep_boundary=False, sym=False)
    if remesh:
        mods = [(m, m.show_viewport) for m in o.modifiers]
        with bpy.context.temp_override(object=o, active_object=o, selected_objects=[o],
                                       selected_editable_objects=[o]):
            try:
                bpy.ops.object.quadriflow_remesh(target_faces=int(remesh), use_mesh_symmetry=symmetric,
                                                 use_preserve_sharp=False, use_preserve_boundary=False,
                                                 seed=0)
            except Exception as e:  # keep the voxel mesh if QuadriFlow fails
                print("quadriflow failed:", e)
    _fix_normals(o)
    return o


# ------------------------------------------------------------------ review: topology


def topo(o=None, sym_tol=1e-4):
    """Topology report of the base mesh: face types, poles, open/non-manifold edges, flipped normals,
    duplicates, X-symmetry, and the triangle count after modifiers."""
    o = obj(o)
    me = o.data
    bm = bmesh.new()
    bm.from_mesh(me)
    bm.verts.ensure_lookup_table()
    tris = sum(1 for f in bm.faces if len(f.verts) == 3)
    quads = sum(1 for f in bm.faces if len(f.verts) == 4)
    ngons = [f for f in bm.faces if len(f.verts) > 4]
    poles = {"3": 0, "5": 0, "6+": 0}
    pole6 = []
    for v in bm.verts:
        if v.is_boundary or not v.link_faces:
            continue
        k = len(v.link_edges)
        if k == 3:
            poles["3"] += 1
        elif k == 5:
            poles["5"] += 1
        elif k >= 6:
            poles["6+"] += 1
            pole6.append([round(x, 3) for x in v.co])
    boundary = sum(1 for e in bm.edges if e.is_boundary)
    nonman = sum(1 for e in bm.edges if len(e.link_faces) > 2)
    wire = sum(1 for e in bm.edges if not e.link_faces)
    loose = sum(1 for v in bm.verts if not v.link_edges)
    flipped = sum(1 for e in bm.edges if len(e.link_faces) == 2 and not e.is_contiguous)
    degenerate = sum(1 for f in bm.faces if f.calc_area() < 1e-12)
    kd = KDTree(len(bm.verts))
    for v in bm.verts:
        kd.insert(v.co, v.index)
    kd.balance()
    dup = sum(1 for v in bm.verts if len(kd.find_range(v.co, 1e-6)) > 1) // 2
    has_mirror = any(m.type == "MIRROR" and m.show_viewport for m in o.modifiers)
    asym, worst = 0, 0.0
    if not has_mirror:
        for v in bm.verts:
            if abs(v.co.x) < sym_tol:
                continue
            _, _, dist = kd.find(Vector((-v.co.x, v.co.y, v.co.z)))
            if dist > sym_tol:
                asym += 1
                worst = max(worst, dist)
    rep = {
        "object": o.name,
        "verts": len(bm.verts), "edges": len(bm.edges), "faces": len(bm.faces),
        "quads": quads, "tris": tris, "ngons": len(ngons),
        "poles": poles,
        "open_edges": boundary, "non_manifold_edges": nonman, "wire_edges": wire, "loose_verts": loose,
        "flipped_normal_edges": flipped, "degenerate_faces": degenerate, "duplicate_verts": dup,
        "symmetry": "mirror modifier" if has_mirror else ({"asymmetric_verts": asym, "worst": round(worst, 5)} if asym else "symmetric"),
    }
    if ngons:
        rep["ngons_at"] = [[round(x, 3) for x in f.calc_center_median()] for f in ngons[:5]]
    if pole6:
        rep["6+poles_at"] = pole6[:5]
    bm.free()
    dg = bpy.context.evaluated_depsgraph_get()
    ev = o.evaluated_get(dg)
    m2 = ev.to_mesh()
    m2.calc_loop_triangles()
    rep["render_tris"] = len(m2.loop_triangles)
    ev.to_mesh_clear()
    rep["dims"] = [round(x, 3) for x in o.dimensions]
    return rep


# ------------------------------------------------------------------ review: contact sheet

_FONT = {
    "A": "010101111101101", "B": "110101110101110", "C": "011100100100011", "D": "110101101101110",
    "E": "111100110100111", "F": "111100110100100", "G": "011100101101011", "H": "101101111101101",
    "I": "111010010010111", "J": "001001001101010", "K": "101101110101101", "L": "100100100100111",
    "M": "101111111101101", "N": "110101101101101", "O": "010101101101010", "P": "110101110100100",
    "Q": "010101101110011", "R": "110101110101101", "S": "011100010001110", "T": "111010010010010",
    "U": "101101101101111", "V": "101101101101010", "W": "101101111111101", "X": "101101010101101",
    "Y": "101101010010010", "Z": "111001010100111", "0": "111101101101111", "1": "010110010010111",
    "2": "110001010100111", "3": "110001010001110", "4": "101101111001001", "5": "111100110001110",
    "6": "011100111101111", "7": "111001010010010", "8": "111101111101111", "9": "111101111001110",
    " ": "000000000000000", "/": "001001010100100", "-": "000000111000000", ".": "000000000000010",
    ":": "000010000010000", "(": "010100100100010", ")": "010001001001010", ",": "000000000010100",
    "_": "000000000000111", "=": "000111000111000", "+": "000010111010000", "%": "101001010100101",
}


def _text(canvas, x, y, s, color=(0.1, 0.1, 0.12), scale=2):
    """Draw text into a top-down RGBA float canvas at pixel (x, y) = top-left."""
    H, W = canvas.shape[:2]
    cx = x
    for ch in s.upper():
        g = _FONT.get(ch, _FONT[" "])
        for r in range(5):
            for c in range(3):
                if g[r * 3 + c] == "1":
                    y0, x0 = y + r * scale, cx + c * scale
                    if 0 <= y0 < H - scale and 0 <= x0 < W - scale:
                        canvas[y0:y0 + scale, x0:x0 + scale, :3] = color
        cx += 4 * scale


_VIEWS = {"front": 0.0, "34": 35.0, "3/4": 35.0, "side": 90.0, "right": 90.0, "back": 180.0, "34back": 145.0, "left": -90.0, "-34": -35.0}


def _mesh_points(objs):
    dg = bpy.context.evaluated_depsgraph_get()
    pts = []
    for o in objs:
        ev = o.evaluated_get(dg)
        if o.type == "MESH":
            m = ev.to_mesh()
            a = np.empty(len(m.vertices) * 3, np.float64)
            m.vertices.foreach_get("co", a)
            ev.to_mesh_clear()
            a = a.reshape(-1, 3)
            if len(a) > 20000:
                a = a[:: len(a) // 20000 + 1]
        else:
            a = np.array([tuple(c) for c in ev.bound_box], np.float64)
        M = np.array(o.matrix_world)
        pts.append(a @ M[:3, :3].T + M[:3, 3])
    return np.concatenate(pts) if pts else np.zeros((1, 3))


def sheet(objects=None, views=("front", "34", "side", "back"), mode="render", size=560, focus=None,
          lens=None, elevation=0.0, bg=(0.78, 0.78, 0.8), samples=16, cols=None, out=None,
          margin=1.1, label=True, clay="#c9c2bb"):
    """Render a model sheet: the same objects from several angles side by side at one shared scale.
    objects: names to show (default: every visible mesh/curve). views: names (front 34 side back 34back
    left) or azimuth degrees. mode: render (scene materials/lights) | clay (matcap clay + cavity, to
    judge form) | wire (subdivision off + cage wireframe, to judge topology). focus: object names /
    {'center':[x,y,z], 'size':s} to frame (e.g. just the head). lens: mm for perspective (default
    orthographic). Returns {path, cells, seconds}."""
    t0 = time.time()
    sc = bpy.context.scene
    if objects:
        targets = [obj(n) for n in objects]
    else:
        targets = [o for o in sc.objects if o.type in ("MESH", "CURVE", "SURFACE", "META", "FONT", "CURVES") and o.visible_get() and not o.hide_render]
    if not targets:
        raise RuntimeError("nothing to render")
    # framing
    if isinstance(focus, dict):
        center = np.asarray(focus["center"], np.float64)
        half = float(focus["size"]) / 2
        pts = np.array([center + half * np.array(s) for s in [(i, j, k) for i in (-1, 1) for j in (-1, 1) for k in (-1, 1)]])
    else:
        fobjs = [obj(n) for n in focus] if focus else targets
        pts = _mesh_points(fobjs)
        center = (pts.min(0) + pts.max(0)) / 2
    az_list = [(_VIEWS[v] if isinstance(v, str) else float(v), v) for v in views]
    el = math.radians(elevation)
    Wm = Hm = 1e-6
    frames = []
    for az, _ in az_list:
        a = math.radians(az)
        back = np.array([math.sin(a) * math.cos(el), -math.cos(a) * math.cos(el), math.sin(el)])  # center -> camera
        f = -back
        r = np.cross(f, [0, 0, 1.0])
        r /= np.linalg.norm(r)
        u = np.cross(r, f)
        rel = pts - center
        Wm = max(Wm, 2 * np.abs(rel @ r).max())
        Hm = max(Hm, 2 * np.abs(rel @ u).max())
        frames.append(back)
    aspect = min(max(Wm / Hm, 0.4), 2.0)
    cw, ch = (int(size * aspect), size) if aspect <= 1 else (size, int(size / aspect))
    cw, ch = cw + cw % 2, ch + ch % 2
    scale = max(Wm, Hm) * margin
    radius = float(np.linalg.norm(pts - center, axis=1).max())
    # save state
    rd = sc.render
    saved = dict(engine=rd.engine, rx=rd.resolution_x, ry=rd.resolution_y, pct=rd.resolution_percentage,
                 fp=rd.filepath, fmt=rd.image_settings.file_format, cm=rd.image_settings.color_mode,
                 film=rd.film_transparent, cam=sc.camera)
    shading = sc.display.shading
    sh_saved = {k: getattr(shading, k) for k in ("light", "color_type", "show_cavity", "single_color", "studio_light") if hasattr(shading, k)}
    hidden, temp, colors, subs = [], [], {}, []
    tmpdir = tempfile.mkdtemp(prefix="nerve-sheet-")
    cells = []
    try:
        cam_data = bpy.data.cameras.new("_sheet_cam")
        cam = bpy.data.objects.new("_sheet_cam", cam_data)
        sc.collection.objects.link(cam)
        temp.append(cam)
        if lens:
            cam_data.type = "PERSP"
            cam_data.lens = lens
            fov = 2 * math.atan(18.0 / lens)
            dist = (scale / 2) / math.tan(fov / 2)
        else:
            cam_data.type = "ORTHO"
            cam_data.ortho_scale = scale
            dist = radius * 3 + 1.0
        cam_data.clip_start = max(dist - radius * 2, 0.001) if lens else 0.01
        cam_data.clip_end = dist + radius * 4 + 10
        sc.camera = cam
        if objects:
            for o in sc.objects:
                if o not in targets and o.type in ("MESH", "CURVE", "SURFACE", "META", "FONT", "CURVES", "GPENCIL", "GREASEPENCIL") and not o.hide_render:
                    o.hide_render = True
                    hidden.append(o)
        rd.resolution_x, rd.resolution_y, rd.resolution_percentage = cw, ch, 100
        rd.image_settings.file_format = "PNG"
        rd.image_settings.color_mode = "RGBA"
        rd.film_transparent = True
        if mode in ("clay", "wire"):
            _set_engine(sc, "WORKBENCH")
            _try(lambda: setattr(sc.display, "render_aa", "8"))
            shading.color_type = "OBJECT"
            if mode == "clay":
                shading.light = "MATCAP"
                _try(lambda: setattr(shading, "studio_light", "clay_studio.exr"))
                shading.show_cavity = True
                _try(lambda: setattr(shading, "cavity_type", "BOTH"))
            else:
                shading.light = "STUDIO"
                shading.show_cavity = False
            cc = hexc(clay)
            for o in targets:
                colors[o.name] = tuple(o.color)
                o.color = (cc[0] ** (1 / 2.2), cc[1] ** (1 / 2.2), cc[2] ** (1 / 2.2), 1)
                for m in o.modifiers:
                    if m.type == "SOLIDIFY" and m.name.startswith("Outline") and m.show_render:
                        m.show_render = False
                        subs.append(m)
                    if mode == "wire" and m.type in ("SUBSURF", "MULTIRES") and m.show_render:
                        m.show_render = False
                        subs.append(m)
            if mode == "wire":
                px = scale / max(cw, ch)
                for o in targets:
                    if o.type != "MESH":
                        continue
                    w = o.copy()
                    w.name = "_sheet_wire"
                    for m in list(w.modifiers):
                        if m.type in ("SUBSURF", "MULTIRES", "SOLIDIFY"):
                            w.modifiers.remove(m)
                    wm = w.modifiers.new("w", "WIREFRAME")
                    wm.thickness = px * 1.3
                    wm.offset = 1.0
                    wm.use_even_offset = True
                    wm.use_replace = True
                    w.color = (0.08, 0.08, 0.1, 1)
                    sc.collection.objects.link(w)
                    temp.append(w)
        else:
            _try(lambda: setattr(sc.eevee, "taa_render_samples", samples))
        for (az, vname), back in zip(az_list, frames):
            pos = Vector(center + back * dist)
            cam.location = pos
            cam.rotation_euler = (Vector(center) - pos).to_track_quat("-Z", "Y").to_euler()
            p = os.path.join(tmpdir, f"cell{len(cells)}.png")
            rd.filepath = p
            bpy.ops.render.render(write_still=True)
            cells.append((p, vname))
    finally:
        for o in temp:
            d = o.data
            bpy.data.objects.remove(o, do_unlink=True)
            if isinstance(d, bpy.types.Camera):
                bpy.data.cameras.remove(d)
        for o in hidden:
            o.hide_render = False
        for n, c in colors.items():
            if n in bpy.data.objects:
                bpy.data.objects[n].color = c
        for m in subs:
            m.show_render = True
        for k, v in sh_saved.items():
            _try(lambda k=k, v=v: setattr(shading, k, v))
        rd.engine = saved["engine"]
        rd.resolution_x, rd.resolution_y, rd.resolution_percentage = saved["rx"], saved["ry"], saved["pct"]
        rd.filepath, rd.image_settings.file_format = saved["fp"], saved["fmt"]
        _try(lambda: setattr(rd.image_settings, "color_mode", saved["cm"]))
        rd.film_transparent = saved["film"]
        sc.camera = saved["cam"]
    # composite (top-down canvas)
    n = len(cells)
    cols = cols or (n if n <= 5 else 4)
    rows = (n + cols - 1) // cols
    gap = 6
    W, H = cols * cw + (cols + 1) * gap, rows * ch + (rows + 1) * gap
    canvas = np.ones((H, W, 4), np.float32)
    canvas[..., :3] = (0.55, 0.55, 0.58)
    for i, (p, vname) in enumerate(cells):
        img = bpy.data.images.load(p, check_existing=False)
        img.colorspace_settings.name = "Non-Color"  # raw sRGB bytes, no linearization
        a = np.empty(img.size[0] * img.size[1] * 4, np.float32)
        img.pixels.foreach_get(a)
        a = a.reshape(img.size[1], img.size[0], 4)[::-1]
        bpy.data.images.remove(img)
        al = a[..., 3:4]
        rgb = a[..., :3] * al + np.asarray(bg, np.float32) * (1 - al)
        r, c = divmod(i, cols)
        y0, x0 = gap + r * (ch + gap), gap + c * (cw + gap)
        canvas[y0:y0 + ch, x0:x0 + cw, :3] = rgb
        if label:
            _text(canvas, x0 + 8, y0 + 8, str(vname) if isinstance(vname, str) else f"{vname:g}")
    if label:
        tris = 0
        dg = bpy.context.evaluated_depsgraph_get()
        for o in targets:
            if o.type == "MESH":
                m = o.evaluated_get(dg).to_mesh()
                m.calc_loop_triangles()
                tris += len(m.loop_triangles)
                o.evaluated_get(dg).to_mesh_clear()
        _text(canvas, gap + 8, H - gap - 18, f"{mode}  {tris} tris  scale {scale:.2f}m", color=(0.25, 0.25, 0.28))
    out = out or os.path.join(os.environ.get("NERVE_BLENDER_OUT") or tempfile.gettempdir(), f"sheet-{time.strftime('%Y%m%d-%H%M%S')}.png")
    os.makedirs(os.path.dirname(out), exist_ok=True)
    im = bpy.data.images.new("_nerve_sheet", W, H, alpha=True)
    im.colorspace_settings.name = "Non-Color"
    im.pixels.foreach_set(canvas[::-1].ravel())
    im.filepath_raw = out
    im.file_format = "PNG"
    im.save()
    bpy.data.images.remove(im)
    for p, _ in cells:
        _try(lambda p=p: os.remove(p))
    _try(lambda: os.rmdir(tmpdir))
    return {"path": out, "cells": [str(v) for _, v in cells], "cell": [cw, ch], "seconds": round(time.time() - t0, 2)}


# ------------------------------------------------------------------ discoverability


def help():
    """Signatures + first doc line of every kit function."""
    import inspect

    out = [__doc__.strip(), ""]
    for name, f in globals().items():
        if name.startswith("_") or getattr(f, "__module__", None) != __name__:
            continue
        if inspect.isclass(f):
            doc = (inspect.getdoc(f) or "").split("\n")[0]
            out.append(f"kit.{name}{inspect.signature(f)}  # {doc}")
            for mn, m in vars(f).items():
                if not mn.startswith("_") and inspect.isfunction(m):
                    out.append(f"  .{mn}{inspect.signature(m)}")
            continue
        if not inspect.isfunction(f):
            continue
        doc = (inspect.getdoc(f) or "").split("\n")[0]
        out.append(f"kit.{name}{inspect.signature(f)}  # {doc}")
    return "\n".join(out)
