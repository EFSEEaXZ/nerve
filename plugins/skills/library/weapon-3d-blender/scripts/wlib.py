"""wlib.py - small helper library for procedural weapon modelling in Blender 4.x/5.x.
Geometry is built as numpy arrays (verts/faces) -> mesh objects; booleans/bevels via modifiers
applied through the evaluated depsgraph (no bpy.ops, works in background mode)."""
import bpy, bmesh, math, os
import numpy as np
from mathutils import Vector, Matrix

# ---------------------------------------------------------------- scene
def reset_scene():
    for o in list(bpy.data.objects):
        bpy.data.objects.remove(o, do_unlink=True)
    for c in list(bpy.data.collections):
        bpy.data.collections.remove(c)
    for blk in (bpy.data.meshes, bpy.data.materials, bpy.data.curves, bpy.data.lights,
                bpy.data.cameras, bpy.data.node_groups, bpy.data.worlds):
        for x in list(blk):
            blk.remove(x)
    sc = bpy.context.scene
    sc.unit_settings.system = 'METRIC'
    sc.unit_settings.scale_length = 1.0
    sc.unit_settings.length_unit = 'CENTIMETERS'
    return sc


def coll(name, parent=None):
    c = bpy.data.collections.get(name) or bpy.data.collections.new(name)
    p = parent or bpy.context.scene.collection
    if c.name not in [x.name for x in p.children]:
        p.children.link(c)
    return c

# ---------------------------------------------------------------- mesh building
def mk(name, verts, faces, col, mat=None, uv=None, smooth=True, sharp=None, sharp_edges=None, fix=True):
    """verts (N,3), faces list of index tuples. uv: per-vertex (N,2) array.
    smooth=True -> smooth faces; sharp=deg -> sharp edges by angle; sharp_edges: list of (a,b) vertex pairs."""
    verts = np.asarray(verts, dtype=np.float64)
    me = bpy.data.meshes.new(name)
    me.from_pydata(verts.tolist(), [], [tuple(int(i) for i in f) for f in faces])
    me.update()
    ob = bpy.data.objects.new(name, me)
    col.objects.link(ob)
    if fix:
        fix_normals(ob)
    if uv is not None:
        set_uv_per_vertex(ob, uv)
    if smooth:
        shade(ob, True, sharp)
    if sharp_edges:
        mark_sharp(ob, sharp_edges)
    if mat is not None:
        assign(ob, mat)
    return ob


def fix_normals(ob):
    bm = bmesh.new()
    bm.from_mesh(ob.data)
    bmesh.ops.recalc_face_normals(bm, faces=bm.faces)
    bm.to_mesh(ob.data)
    bm.free()
    ob.data.update()


def merge_close(ob, dist=1e-6):
    bm = bmesh.new()
    bm.from_mesh(ob.data)
    bmesh.ops.remove_doubles(bm, verts=bm.verts, dist=dist)
    bm.to_mesh(ob.data)
    bm.free()


def shade(ob, smooth=True, sharp_deg=None):
    me = ob.data
    n = len(me.polygons)
    me.polygons.foreach_set('use_smooth', [smooth] * n)
    if sharp_deg is not None:
        try:
            me.set_sharp_from_angle(angle=math.radians(sharp_deg))
        except Exception:
            pass
    me.update()


def mark_sharp(ob, pairs):
    me = ob.data
    key = {}
    ev = np.empty(len(me.edges) * 2, np.int32)
    me.edges.foreach_get('vertices', ev)
    ev = ev.reshape(-1, 2)
    for i, (a, b) in enumerate(ev):
        key[(min(a, b), max(a, b))] = i
    flags = np.zeros(len(me.edges), bool)
    try:
        cur = np.empty(len(me.edges), bool)
        me.edges.foreach_get('use_edge_sharp', cur)
        flags |= cur
    except Exception:
        pass
    for a, b in pairs:
        i = key.get((min(a, b), max(a, b)))
        if i is not None:
            flags[i] = True
    me.edges.foreach_set('use_edge_sharp', flags.tolist())
    me.update()


def set_uv_per_vertex(ob, uv, name='UVMap'):
    me = ob.data
    uvl = me.uv_layers.get(name) or me.uv_layers.new(name=name)
    vi = np.empty(len(me.loops), np.int32)
    me.loops.foreach_get('vertex_index', vi)
    uv = np.asarray(uv, dtype=np.float32)[vi]
    uvl.data.foreach_set('uv', uv.ravel())


def box_uv(ob, scale=1.0, name='UVMap'):
    me = ob.data
    if len(me.polygons) == 0:
        return
    uvl = me.uv_layers.get(name) or me.uv_layers.new(name=name)
    vi = np.empty(len(me.loops), np.int32); me.loops.foreach_get('vertex_index', vi)
    co = np.empty(len(me.vertices) * 3); me.vertices.foreach_get('co', co); co = co.reshape(-1, 3)
    pn = np.empty(len(me.polygons) * 3); me.polygons.foreach_get('normal', pn); pn = pn.reshape(-1, 3)
    lt = np.empty(len(me.polygons), np.int32); me.polygons.foreach_get('loop_total', lt)
    fol = np.repeat(np.arange(len(me.polygons)), lt)
    ax = np.abs(pn).argmax(1)[fol]
    p = co[vi]
    u = np.where(ax == 0, p[:, 1], p[:, 0])
    v = np.where(ax == 2, p[:, 1], p[:, 2])
    uvl.data.foreach_set('uv', (np.stack([u, v], 1) * scale).astype(np.float32).ravel())


def loft(rings, closed=True, cap0=False, cap1=False):
    """rings: (R,N,3). Returns verts, faces (quads)."""
    rings = np.asarray(rings, dtype=np.float64)
    R, N, _ = rings.shape
    faces = []
    M = N if closed else N - 1
    for i in range(R - 1):
        b0, b1 = i * N, (i + 1) * N
        for j in range(M):
            j2 = (j + 1) % N
            faces.append((b0 + j, b0 + j2, b1 + j2, b1 + j))
    if cap0:
        faces.append(tuple(range(N))[::-1])
    if cap1:
        faces.append(tuple(range((R - 1) * N, R * N)))
    return rings.reshape(-1, 3), faces


def loft_tip(rings, tip, closed=True, cap0=False):
    """loft and close the last ring with a fan to a tip point."""
    v, f = loft(rings, closed, cap0, False)
    R, N = len(rings), len(rings[0])
    ti = len(v)
    v = np.vstack([v, np.asarray(tip, float)[None]])
    base = (R - 1) * N
    for j in range(N if closed else N - 1):
        f.append((base + j, base + (j + 1) % N, ti))
    return v, f


def lathe(profile, seg=48, phase=0.0, cap0=True, cap1=True):
    """profile: list of (r, z) bottom->top. r==0 at ends -> pole. Axis = Z."""
    prof = [(float(r), float(z)) for r, z in profile]
    pole0 = prof[0][0] < 1e-9
    pole1 = prof[-1][0] < 1e-9
    body = prof[(1 if pole0 else 0):(len(prof) - 1 if pole1 else len(prof))]
    ang = phase + np.arange(seg) * 2 * np.pi / seg
    ca, sa = np.cos(ang), np.sin(ang)
    rings = np.array([[(r * c, r * s, z) for c, s in zip(ca, sa)] for r, z in body])
    v, f = loft(rings, True, cap0 and not pole0, cap1 and not pole1)
    R = len(body)
    if pole0:
        pi = len(v); v = np.vstack([v, [[0, 0, prof[0][1]]]])
        for j in range(seg):
            f.append(((j + 1) % seg, j, pi))
    if pole1:
        pi = len(v); v = np.vstack([v, [[0, 0, prof[-1][1]]]])
        base = (R - 1) * seg
        for j in range(seg):
            f.append((base + j, base + (j + 1) % seg, pi))
    return v, f


def prism(poly, y0, y1):
    """2D polygon (x,z) in the XZ plane extruded along Y from y0 to y1."""
    poly = np.asarray(poly, dtype=np.float64)
    n = len(poly)
    a = np.column_stack([poly[:, 0], np.full(n, y0), poly[:, 1]])
    b = np.column_stack([poly[:, 0], np.full(n, y1), poly[:, 1]])
    v = np.vstack([a, b])
    f = [(i, (i + 1) % n, n + (i + 1) % n, n + i) for i in range(n)]
    f.append(tuple(range(n))[::-1])
    f.append(tuple(range(n, 2 * n)))
    return v, f


def box_vf(c, s):
    cx, cy, cz = c; sx, sy, sz = [x / 2 for x in s]
    v = np.array([[cx + dx * sx, cy + dy * sy, cz + dz * sz] for dz in (-1, 1) for dy in (-1, 1) for dx in (-1, 1)])
    f = [(0, 2, 3, 1), (4, 5, 7, 6), (0, 1, 5, 4), (2, 6, 7, 3), (0, 4, 6, 2), (1, 3, 7, 5)]
    return v, f


def cyl_vf(r, z0, z1, seg=32):
    return lathe([(r, z0), (r, z1)], seg)


def xform(v, M):
    v = np.asarray(v, dtype=np.float64)
    M = np.array(M)
    return v @ M[:3, :3].T + M[:3, 3]


def combine(*parts):
    """combine (v,f) tuples into one."""
    V, F, off = [], [], 0
    for v, f in parts:
        V.append(np.asarray(v, float)); F += [tuple(i + off for i in fc) for fc in f]; off += len(v)
    return np.vstack(V), F


def superellipse(a, b, n, seg, phase=0.0):
    """points (x,y) of |x/a|^n + |y/b|^n = 1, starting at angle phase."""
    t = phase + np.arange(seg) * 2 * np.pi / seg
    c, s = np.cos(t), np.sin(t)
    x = a * np.sign(c) * np.abs(c) ** (2.0 / n)
    y = b * np.sign(s) * np.abs(s) ** (2.0 / n)
    return np.column_stack([x, y])


def offset_poly(pts, d):
    """offset a closed CCW 2D polygon outward by d (miter, small d)."""
    p = np.asarray(pts, float)
    n = len(p)
    out = []
    for i in range(n):
        a, b, c = p[i - 1], p[i], p[(i + 1) % n]
        e1 = b - a; e2 = c - b
        n1 = np.array([e1[1], -e1[0]]); n1 /= (np.linalg.norm(n1) + 1e-12)
        n2 = np.array([e2[1], -e2[0]]); n2 /= (np.linalg.norm(n2) + 1e-12)
        m = n1 + n2; m /= (np.linalg.norm(m) + 1e-12)
        cosh = max(0.3, float(np.dot(m, n1)))
        out.append(b + m * d / cosh)
    return np.array(out)

# ---------------------------------------------------------------- modifiers
def bevel(ob, width, seg=2, angle=30, harden=False, profile=0.5, limit='ANGLE', clamp=True, name='Bevel'):
    m = ob.modifiers.new(name, 'BEVEL')
    m.width = width
    m.segments = seg
    m.limit_method = limit
    if limit == 'ANGLE':
        m.angle_limit = math.radians(angle)
    m.profile = profile
    m.use_clamp_overlap = clamp
    m.harden_normals = harden
    try:
        m.miter_outer = 'MITER_ARC'
    except Exception:
        pass
    return m


def wnormal(ob):
    m = ob.modifiers.new('WeightedNormal', 'WEIGHTED_NORMAL')
    m.keep_sharp = True
    m.mode = 'FACE_AREA'
    return m


def subsurf(ob, levels=1):
    m = ob.modifiers.new('Subsurf', 'SUBSURF')
    m.levels = levels
    m.render_levels = levels
    return m


def apply_mods(ob):
    bpy.context.view_layer.update()
    dg = bpy.context.evaluated_depsgraph_get()
    oe = ob.evaluated_get(dg)
    me = bpy.data.meshes.new_from_object(oe, preserve_all_data_layers=True, depsgraph=dg)
    old = ob.data
    ob.modifiers.clear()
    ob.data = me
    if old.users == 0:
        bpy.data.meshes.remove(old)
    return ob


def boolean(ob, cutter, op='DIFFERENCE', solver='EXACT', apply=True, keep_cutter=False, self_inter=False):
    m = ob.modifiers.new('Bool', 'BOOLEAN')
    m.operation = op
    try:
        m.solver = solver
    except Exception:
        m.solver = 'EXACT'
    m.object = cutter
    if self_inter and m.solver == 'EXACT':
        m.use_self = True
    cutter.hide_render = True
    cutter.hide_viewport = True
    if apply:
        # move to first position so later bevel modifiers stay
        while ob.modifiers[0] != m:
            bpy.context.view_layer.update()
            idx = list(ob.modifiers).index(m)
            ob.modifiers.move(idx, idx - 1)
        apply_mods_first(ob)
        if not keep_cutter:
            me = cutter.data
            bpy.data.objects.remove(cutter, do_unlink=True)
            if me and me.users == 0:
                bpy.data.meshes.remove(me)
    return ob


def apply_mods_first(ob):
    """apply only the first modifier (others are kept)."""
    rest = [(m.name, m.type) for m in ob.modifiers][1:]
    if not rest:
        return apply_mods(ob)
    # temporarily disable others
    states = []
    for m in list(ob.modifiers)[1:]:
        states.append((m.name, m.show_viewport, m.show_render))
        m.show_viewport = False; m.show_render = False
    first = ob.modifiers[0].name
    bpy.context.view_layer.update()
    dg = bpy.context.evaluated_depsgraph_get()
    oe = ob.evaluated_get(dg)
    me = bpy.data.meshes.new_from_object(oe, preserve_all_data_layers=True, depsgraph=dg)
    old = ob.data
    ob.modifiers.remove(ob.modifiers[first])
    ob.data = me
    for name, sv, sr in states:
        mm = ob.modifiers.get(name)
        if mm:
            mm.show_viewport = sv; mm.show_render = sr
    if old.users == 0:
        bpy.data.meshes.remove(old)
    return ob


def cutter(name, v, f, col):
    ob = mk(name, v, f, col, smooth=False)
    return ob

# ---------------------------------------------------------------- materials
def assign(ob, mat, replace=True):
    """Assign material. replace=True (default): object gets exactly this one material.
    NB: after boolean ops a mesh may carry an EMPTY slot 0 -> appending would leave the faces
    on the empty slot (renders white). So we clear slots and reset material_index."""
    me = ob.data
    if replace:
        me.materials.clear()
        me.materials.append(mat)
        if len(me.polygons):
            me.polygons.foreach_set('material_index', [0] * len(me.polygons))
        return
    for i, m in enumerate(me.materials):
        if m is None:
            me.materials[i] = mat
            return
        if m.name == mat.name:
            return
    me.materials.append(mat)


def new_mat(name):
    m = bpy.data.materials.get(name)
    if m:
        bpy.data.materials.remove(m)
    m = bpy.data.materials.new(name)
    try:
        m.use_nodes = True
    except Exception:
        pass
    return m


def N(nt, t, x=0, y=0, **kw):
    n = nt.nodes.new(t)
    n.location = (x, y)
    for k, v in kw.items():
        if k in n.inputs.keys():
            n.inputs[k].default_value = v
        else:
            setattr(n, k, v)
    return n


def L(nt, a, b):
    nt.links.new(a, b)


def sock(node, ident, out=False):
    for s in (node.outputs if out else node.inputs):
        if s.identifier == ident or s.name == ident:
            if getattr(s, 'enabled', True):
                return s
    raise KeyError(ident)


def mix_col(nt, fac, a, b, x=0, y=0, blend='MIX'):
    """color mix; a/b may be sockets or RGBA tuples; fac socket or float. returns output socket"""
    n = nt.nodes.new('ShaderNodeMix'); n.location = (x, y)
    n.data_type = 'RGBA'
    n.blend_type = blend
    fi = sock(n, 'Factor_Float'); ai = sock(n, 'A_Color'); bi = sock(n, 'B_Color')
    for s, val in ((fi, fac), (ai, a), (bi, b)):
        if isinstance(val, bpy.types.NodeSocket):
            nt.links.new(val, s)
        else:
            s.default_value = val
    return sock(n, 'Result_Color', out=True)


def mix_f(nt, fac, a, b, x=0, y=0):
    n = nt.nodes.new('ShaderNodeMix'); n.location = (x, y)
    n.data_type = 'FLOAT'
    fi = sock(n, 'Factor_Float'); ai = sock(n, 'A_Float'); bi = sock(n, 'B_Float')
    for s, val in ((fi, fac), (ai, a), (bi, b)):
        if isinstance(val, bpy.types.NodeSocket):
            nt.links.new(val, s)
        else:
            s.default_value = val
    return sock(n, 'Result_Float', out=True)


def math_n(nt, op, a, b=None, x=0, y=0, clamp=False):
    n = nt.nodes.new('ShaderNodeMath'); n.location = (x, y)
    n.operation = op
    n.use_clamp = clamp
    for i, val in enumerate((a, b)):
        if val is None:
            continue
        if isinstance(val, bpy.types.NodeSocket):
            nt.links.new(val, n.inputs[i])
        else:
            n.inputs[i].default_value = val
    return n.outputs[0]


def maprange(nt, val, a0, a1, b0, b1, x=0, y=0, smooth=False, clamp=True):
    n = nt.nodes.new('ShaderNodeMapRange'); n.location = (x, y)
    if smooth:
        n.interpolation_type = 'SMOOTHSTEP'
    n.clamp = clamp
    nt.links.new(val, n.inputs['Value'])
    n.inputs['From Min'].default_value = a0
    n.inputs['From Max'].default_value = a1
    n.inputs['To Min'].default_value = b0
    n.inputs['To Max'].default_value = b1
    return n.outputs['Result']


def principled(m):
    nt = m.node_tree
    b = nt.nodes.get('Principled BSDF')
    if b is None:
        b = nt.nodes.new('ShaderNodeBsdfPrincipled')
        out = nt.nodes.get('Material Output') or nt.nodes.new('ShaderNodeOutputMaterial')
        nt.links.new(b.outputs[0], out.inputs['Surface'])
    return nt, b


def simple_mat(name, color, metallic=0.0, rough=0.5, coat=0.0, coat_rough=0.05, sheen=0.0, spec=0.5):
    m = new_mat(name)
    nt, b = principled(m)
    b.inputs['Base Color'].default_value = (*color, 1)
    b.inputs['Metallic'].default_value = metallic
    b.inputs['Roughness'].default_value = rough
    b.inputs['Coat Weight'].default_value = coat
    b.inputs['Coat Roughness'].default_value = coat_rough
    b.inputs['Sheen Weight'].default_value = sheen
    b.inputs['Specular IOR Level'].default_value = spec
    m.diffuse_color = (*color, 1)
    return m

# ---------------------------------------------------------------- misc
def save(path):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    bpy.ops.wm.save_as_mainfile(filepath=path, compress=True)
    print('SAVED', path)


def report(col):
    for o in col.all_objects:
        if o.type == 'MESH':
            d = o.dimensions
            print('  %-24s %7.1f %7.1f %7.1f mm  faces %d' % (o.name, d.x * 1000, d.y * 1000, d.z * 1000, len(o.data.polygons)))


# ------------------------------------------------------------------ 2D outline tools (traced silhouettes)
def _poly_area(P):
    P = np.asarray(P, float)
    return 0.5 * float(np.sum(P[:, 0] * np.roll(P[:, 1], -1) - np.roll(P[:, 0], -1) * P[:, 1]))


def rdp_closed(P, eps):
    """Ramer-Douglas-Peucker for a closed polygon (removes pixel noise of traced contours)."""
    P = np.asarray(P, float)
    def rdp(Q):
        if len(Q) < 3:
            return Q
        a, b = Q[0], Q[-1]; ab = b - a; L = np.linalg.norm(ab)
        if L < 1e-12:
            d = np.linalg.norm(Q - a, axis=1)
        else:
            d = np.abs(ab[0] * (Q[:, 1] - a[1]) - ab[1] * (Q[:, 0] - a[0])) / L
        i = int(np.argmax(d))
        if d[i] > eps:
            return np.vstack([rdp(Q[:i + 1])[:-1], rdp(Q[i:])])
        return np.vstack([a, b])
    i0 = 0; i1 = int(np.argmax(np.linalg.norm(P - P[0], axis=1)))
    A = rdp(np.vstack([P[i0:i1 + 1]])); B = rdp(np.vstack([P[i1:], P[:1]]))
    return np.vstack([A[:-1], B[:-1]])


def clean_outline(P, eps=0.5, corner_deg=55.0, iters=3):
    """traced outline -> clean CAD-like curve: RDP simplify, then Chaikin smoothing that keeps sharp corners
    (turning angle > corner_deg). Fixes the 'zipper' jaggies that bevel modifiers make on noisy contours."""
    P = rdp_closed(P, eps)
    for _ in range(iters):
        n = len(P); sharp = np.zeros(n, bool)
        for i in range(n):
            v1 = P[i] - P[i - 1]; v2 = P[(i + 1) % n] - P[i]
            d = np.linalg.norm(v1) * np.linalg.norm(v2)
            if d > 1e-12:
                sharp[i] = math.degrees(math.acos(max(-1.0, min(1.0, float(np.dot(v1, v2) / d))))) > corner_deg
        out = []
        for i in range(n):
            a, b = P[i], P[(i + 1) % n]
            out.append(a if sharp[i] else 0.75 * a + 0.25 * b)
            if not sharp[(i + 1) % n]:
                out.append(0.25 * a + 0.75 * b)
        P = np.asarray(out)
    return P


def resample_closed(P, step):
    P = np.asarray(P, float); Q = np.vstack([P, P[:1]])
    seg = np.linalg.norm(np.diff(Q, axis=0), axis=1); Lc = np.concatenate([[0], np.cumsum(seg)])
    n = max(8, int(round(Lc[-1] / step)))
    s = np.linspace(0, Lc[-1], n, endpoint=False)
    return np.stack([np.interp(s, Lc, Q[:, 0]), np.interp(s, Lc, Q[:, 1])], 1)


def poly_dist(Q, P):
    """distance of points Q (N,2) to the closed polygon P (M,2) boundary"""
    Q = np.asarray(Q, float); A = np.asarray(P, float); B = np.roll(A, -1, 0); AB = B - A
    AQ = Q[:, None, :] - A[None]
    t = np.clip((AQ * AB[None]).sum(-1) / np.maximum((AB * AB).sum(-1), 1e-12)[None], 0, 1)
    return np.linalg.norm(AQ - t[..., None] * AB[None], axis=-1).min(1)


def poly_inside(Q, P):
    Q = np.asarray(Q, float); A = np.asarray(P, float); B = np.roll(A, -1, 0)
    y = Q[:, 1][:, None]; x = Q[:, 0][:, None]
    ya, yb, xa, xb = A[:, 1][None], B[:, 1][None], A[:, 0][None], B[:, 0][None]
    den = np.where(np.abs(yb - ya) < 1e-12, 1e-12, yb - ya)
    hit = ((ya > y) != (yb > y)) & (x < xa + (y - ya) * (xb - xa) / den)
    return hit.sum(1) % 2 == 1


def pillow(poly, half, r_edge, step=1.5, rings=7, grid=None):
    """Rounded solid from a closed 2D outline (x,z): thickness 2*half along Y, edge rounded with an elliptic
    profile (in-plane radius r_edge). For organic parts (pistol grips, stocks, handles) instead of prism+bevel.
    Constrained Delaunay of boundary + offset rings + interior grid (mathutils.geometry.delaunay_2d_cdt)."""
    from mathutils import Vector
    from mathutils.geometry import delaunay_2d_cdt
    P = np.asarray(poly, float)
    if _poly_area(P) < 0:
        P = P[::-1]
    B = resample_closed(P, step); nb = len(B)
    Tg = np.roll(B, -1, 0) - np.roll(B, 1, 0); Tg /= np.linalg.norm(Tg, axis=1, keepdims=True) + 1e-12
    Nin = np.stack([-Tg[:, 1], Tg[:, 0]], 1)                      # CCW -> left normal points inside
    cand = []
    for k in range(1, rings + 1):
        dk = r_edge * (1 - math.cos((math.pi / 2) * k / rings))
        R = B + Nin * dk
        cand.append((R, dk))
    g = grid or step * 1.4
    xmin, zmin = B.min(0); xmax, zmax = B.max(0)
    G_ = [(x + (g / 2 if j % 2 else 0), z) for j, z in enumerate(np.arange(zmin, zmax, g * 0.866)) for x in np.arange(xmin, xmax, g)]
    cand.append((np.asarray(G_, float), None))
    pts = [B]; keep_min = step * 0.45
    acc = B.copy()
    for R, dk in cand:
        if not len(R):
            continue
        ins = poly_inside(R, B); d = poly_dist(R, B)
        ok = ins & ((np.abs(d - dk) < 0.2 * dk + 0.05) if dk is not None else (d > r_edge + 0.5 * g))
        R = R[ok]
        if not len(R):
            continue
        # drop points too close to already accepted ones (avoid slivers)
        sel = []
        for p in R:
            if np.min(np.sum((acc - p) ** 2, axis=1)) > keep_min ** 2:
                sel.append(p); acc = np.vstack([acc, p])
        if sel:
            pts.append(np.asarray(sel))
    V2 = np.vstack(pts)
    edges = [(i, (i + 1) % nb) for i in range(nb)]
    res = delaunay_2d_cdt([Vector((float(p[0]), float(p[1]))) for p in V2], edges, [list(range(nb))], 1, 1e-6)
    vo = np.array([(v.x, v.y) for v in res[0]], float); fo = res[2]
    d = poly_dist(vo, B)
    u = np.clip(d / r_edge, 0, 1)
    h = half * np.sqrt(np.clip(1 - (1 - u) ** 2, 0, 1))
    n = len(vo); bnd = h < 1e-7
    idx_bot = np.arange(n); idx_top = np.where(bnd, idx_bot, n + np.cumsum(~bnd) - 1)
    top_extra = vo[~bnd]
    V = np.vstack([np.column_stack([vo[:, 0], -h, vo[:, 1]]),
                   np.column_stack([top_extra[:, 0], h[~bnd], top_extra[:, 1]])])
    F = []
    for f in fo:
        f = list(f)
        a2 = _poly_area(vo[f])
        ccw = f if a2 > 0 else f[::-1]
        F.append(tuple(idx_bot[i] for i in ccw))            # CCW in (x,z) -> normal -Y (bottom side)
        F.append(tuple(idx_top[i] for i in ccw[::-1]))      # +Y side
    return V, F
