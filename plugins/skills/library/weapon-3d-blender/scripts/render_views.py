"""render_views.py - multi-view check renders for weapon models (weapon-3d skill).

Usage:
  blender -b file.blend -P render_views.py -- --out renders --res 1024
Options:
  --collection NAME   render only this collection (recursive)
  --views a,b,...     front side back top three_quarter three_quarter_back
                      closeup_top closeup_mid closeup_bottom  (default: skill set)
  --res N             long side in px (default 1024)
  --samples N         Cycles samples (default 64)
  --engine CYCLES|EEVEE
  --focus SPEC        extra 85mm close-ups: 'o1,o2' or 'name=o1,o2@az,el;name2=o3' -> focus_name.png
                      '@az,el,h' = horizontal close-up (long axis left-right, top=-X; az toward tip, el toward top)
  --hdri X            'neutral' (default, procedural grey studio) | Blender studiolight name (studio, interior, city...) | path
  --clay              override all materials with clay (judge form only)
  --sheet 0|1         also write contact sheet  sheet.jpg (default 1)
  --keep-lights       keep scene world/lights instead of the review studio
Conventions checked: length along +Z, face toward -Y, origin at grip centre.
Never saves or modifies the .blend.
"""
import bpy, sys, os, math, argparse, time
import numpy as np
from mathutils import Vector, Matrix

DEFAULT_VIEWS = 'front,side,back,top,three_quarter,three_quarter_back,closeup_top,closeup_bottom'
GEOM = {'MESH', 'CURVE', 'SURFACE', 'META', 'FONT'}


def parse():
    argv = sys.argv[sys.argv.index('--') + 1:] if '--' in sys.argv else []
    ap = argparse.ArgumentParser()
    ap.add_argument('--out', default='renders')
    ap.add_argument('--res', type=int, default=1024)
    ap.add_argument('--collection', default=None)
    ap.add_argument('--views', default=DEFAULT_VIEWS)
    ap.add_argument('--samples', type=int, default=64)
    ap.add_argument('--engine', default='CYCLES')
    ap.add_argument('--focus', default=None)
    ap.add_argument('--hdri', default=None)
    ap.add_argument('--clay', action='store_true')
    ap.add_argument('--sheet', type=int, default=1)
    ap.add_argument('--keep-lights', action='store_true')
    ap.add_argument('--bg', type=float, default=0.20)
    ap.add_argument('--lens', type=float, default=50.0)
    ap.add_argument('--stats-only', action='store_true')
    ap.add_argument('--exposure', type=float, default=0.0, help='film exposure (EV); dark guns: -0.5..-1 with higher --bg')
    ap.add_argument('--world-up', default='z', choices=['z', 'view'],
                    help="'view' = rotate the neutral studio so its bright top follows the camera up vector (horizontal beauty shots of rifles)")
    ap.add_argument('--softbox', type=float, default=0.0,
                    help='big gradient emission card at/above the camera (reflects in polished metal); strength')
    return ap.parse_args(argv)


def collect(scene, coll_name):
    if coll_name:
        c = bpy.data.collections.get(coll_name)
        if c is None:
            raise SystemExit('collection %r not found; have %s' % (coll_name, [x.name for x in bpy.data.collections]))
        objs = [o for o in c.all_objects if o.type in GEOM]
    else:
        objs = [o for o in scene.objects if o.type in GEOM and not o.hide_render]
    return objs


def stats(objs):
    dg = bpy.context.evaluated_depsgraph_get()
    rows, allv = [], []
    for o in objs:
        oe = o.evaluated_get(dg)
        try:
            me = oe.to_mesh()
        except Exception:
            me = None
        if me is None or len(me.vertices) == 0:
            if me is not None:
                oe.to_mesh_clear()
            continue
        n = len(me.vertices)
        co = np.empty(n * 3, dtype=np.float64)
        me.vertices.foreach_get('co', co)
        M = np.array(o.matrix_world)
        w = co.reshape(-1, 3) @ M[:3, :3].T + M[:3, 3]
        me.calc_loop_triangles()
        rows.append((o.name, w.min(0), w.max(0), len(me.polygons), len(me.loop_triangles),
                     tuple(o.matrix_world.translation), len(me.materials)))
        allv.append(w.astype(np.float32))
        oe.to_mesh_clear()
    P = np.concatenate(allv) if allv else np.zeros((0, 3), np.float32)
    return rows, P


def print_stats(rows, P):
    print('\n=== OBJECTS (evaluated, world space, mm) ===')
    print('%-26s %9s %9s %9s %8s %8s  origin(mm)' % ('name', 'X', 'Y', 'Z', 'polys', 'tris'))
    tp = tt = 0
    for name, mn, mx, polys, tris, org, nm in sorted(rows, key=lambda r: -r[2][2]):
        d = (mx - mn) * 1000
        tp += polys; tt += tris
        print('%-26s %9.1f %9.1f %9.1f %8d %8d  (%.0f,%.0f,%.0f)%s' % (
            name[:26], d[0], d[1], d[2], polys, tris, org[0] * 1000, org[1] * 1000, org[2] * 1000,
            '' if nm else '  [NO MATERIAL]'))
    if len(P):
        mn, mx = P.min(0), P.max(0)
        d = (mx - mn) * 1000
        print('--- TOTAL bbox X %.1f  Y %.1f  Z %.1f mm | min %s max %s (mm)' % (
            d[0], d[1], d[2], np.round(mn * 1000, 1).tolist(), np.round(mx * 1000, 1).tolist()))
        print('--- TOTAL polys %d  tris %d  objects %d' % (tp, tt, len(rows)))
        long_axis = 'XYZ'[int(np.argmax(d))]
        print('--- long axis: %s %s' % (long_axis, '(ok, +Z)' if long_axis == 'Z' else '(WARNING: convention is length along +Z)'))


def look(pos, target, up):
    f = (target - pos).normalized()
    r = f.cross(up)
    if r.length < 1e-8:
        r = f.cross(Vector((0, 1, 0)))
    r.normalize()
    u = r.cross(f)
    M = Matrix((r, u, -f)).transposed().to_4x4()
    M.translation = pos
    return M, r, u


def dir_from(az, el):
    a, e = math.radians(az), math.radians(el)
    return Vector((math.sin(a) * math.cos(e), -math.cos(a) * math.cos(e), math.sin(e)))


VIEWS = {
    # horizontal "display" shots: tsuka left, edge up (camera up = -X), slight perspective
    'beauty': dict(d=Vector((-0.28, -1.0, 0.10)).normalized(), up=Vector((-1, 0, 0)), ortho=False, lens=70),
    'beauty_back': dict(d=Vector((-0.28, 1.0, -0.10)).normalized(), up=Vector((-1, 0, 0)), ortho=False, lens=70),
    # horizontal 3/4 'hero' shots: camera 30 deg toward the tip/muzzle and 18 deg above the spine/top (right / left side)
    'hero': dict(d=Vector((-0.309, -0.824, 0.476)).normalized(), up=Vector((-1, 0, 0)), ortho=False, lens=60),
    'hero_back': dict(d=Vector((-0.309, 0.824, -0.476)).normalized(), up=Vector((-1, 0, 0)), ortho=False, lens=60),
    'front': dict(d=Vector((0, -1, 0)), up=Vector((0, 0, 1)), ortho=True),
    'back': dict(d=Vector((0, 1, 0)), up=Vector((0, 0, 1)), ortho=True),
    'side': dict(d=Vector((1, 0, 0)), up=Vector((0, 0, 1)), ortho=True),
    'top': dict(d=Vector((0, 0, 1)), up=Vector((0, 1, 0)), ortho=True),
    'three_quarter': dict(d=dir_from(35, 20), up=Vector((0, 0, 1)), ortho=False),
    'three_quarter_back': dict(d=dir_from(145, 20), up=Vector((0, 0, 1)), ortho=False),
    'closeup_top': dict(d=dir_from(30, 15), up=Vector((0, 0, 1)), ortho=False, band=(0.70, 1.0), lens=85),
    'closeup_mid': dict(d=dir_from(30, 15), up=Vector((0, 0, 1)), ortho=False, band=(0.35, 0.68), lens=85),
    'closeup_bottom': dict(d=dir_from(30, 15), up=Vector((0, 0, 1)), ortho=False, band=(0.0, 0.30), lens=85),
}


def setup_render(sc, a):
    if a.engine.upper().startswith('EEVEE'):
        sc.render.engine = 'BLENDER_EEVEE'
        try:
            sc.eevee.taa_render_samples = a.samples
        except Exception:
            pass
    else:
        sc.render.engine = 'CYCLES'
        try:
            prefs = bpy.context.preferences.addons['cycles'].preferences
            ok = False
            for t in ('OPTIX', 'CUDA'):
                try:
                    prefs.compute_device_type = t
                    prefs.get_devices()
                    devs = [d for d in prefs.devices if d.type == t]
                    if devs:
                        for d in prefs.devices:
                            d.use = (d.type == t)
                        ok = True
                        break
                except Exception:
                    pass
            sc.cycles.device = 'GPU' if ok else 'CPU'
        except Exception as ex:
            print('GPU setup failed', ex)
        sc.cycles.samples = a.samples
        sc.cycles.use_adaptive_sampling = True
        sc.cycles.use_denoising = True
        try:
            sc.cycles.denoiser = 'OPENIMAGEDENOISE'
        except Exception:
            pass
        sc.cycles.max_bounces = 8
        sc.cycles.glossy_bounces = 4
    sc.render.film_transparent = False
    sc.render.resolution_percentage = 100
    sc.render.image_settings.file_format = 'PNG'
    sc.render.image_settings.color_mode = 'RGB'
    try:
        sc.view_settings.exposure = getattr(a, 'exposure', 0.0)
    except Exception:
        pass
    try:
        sc.view_settings.view_transform = 'AgX'
        for lk in ('AgX - Medium High Contrast', 'Medium High Contrast'):
            try:
                sc.view_settings.look = lk
                break
            except Exception:
                pass
    except Exception:
        pass


def studio_world(sc, a):
    w = bpy.data.worlds.new('review_world')
    sc.world = w
    if w.node_tree is None:
        w.use_nodes = True
    nt = w.node_tree
    nt.nodes.clear()
    out = nt.nodes.new('ShaderNodeOutputWorld')
    mix = nt.nodes.new('ShaderNodeMixShader')
    lp = nt.nodes.new('ShaderNodeLightPath')
    bg_light = nt.nodes.new('ShaderNodeBackground')
    bg_cam = nt.nodes.new('ShaderNodeBackground')
    bg_cam.inputs['Color'].default_value = (a.bg, a.bg, a.bg, 1)
    bg_cam.inputs['Strength'].default_value = 1.0
    path = a.hdri or 'neutral'
    if path == 'neutral':
        # procedural neutral studio: grey gradient (ground .06 -> horizon .35 -> top .9) + overhead softbox band.
        # Neutral on purpose: warm HDRIs (interior.exr) tint polished steel yellow.
        tc = nt.nodes.new('ShaderNodeTexCoord')
        mp = nt.nodes.new('ShaderNodeMapping'); mp.vector_type = 'VECTOR'; mp.name = 'rv_world_map'
        nt.links.new(tc.outputs['Generated'], mp.inputs['Vector'])
        sp = nt.nodes.new('ShaderNodeSeparateXYZ'); nt.links.new(mp.outputs['Vector'], sp.inputs[0])
        r1 = nt.nodes.new('ShaderNodeMapRange'); r1.interpolation_type = 'SMOOTHSTEP'
        nt.links.new(sp.outputs['Z'], r1.inputs['Value'])
        r1.inputs['From Min'].default_value = -0.25; r1.inputs['From Max'].default_value = 0.85
        r1.inputs['To Min'].default_value = 0.12; r1.inputs['To Max'].default_value = 1.0
        r2 = nt.nodes.new('ShaderNodeMapRange'); r2.interpolation_type = 'SMOOTHSTEP'   # softbox band
        nt.links.new(sp.outputs['Z'], r2.inputs['Value'])
        r2.inputs['From Min'].default_value = 0.55; r2.inputs['From Max'].default_value = 0.75
        r2.inputs['To Min'].default_value = 0.0; r2.inputs['To Max'].default_value = 1.6
        ad = nt.nodes.new('ShaderNodeMath'); ad.operation = 'ADD'
        nt.links.new(r1.outputs['Result'], ad.inputs[0]); nt.links.new(r2.outputs['Result'], ad.inputs[1])
        nt.links.new(ad.outputs['Value'], bg_light.inputs['Color'])
        bg_light.inputs['Strength'].default_value = 0.8
        print('WORLD: neutral procedural studio')
        path = None
    elif not os.path.exists(path):
        d = bpy.utils.system_resource('DATAFILES', path='studiolights/world')
        cands = [f for f in sorted(os.listdir(d)) if f.lower().endswith(('.exr', '.hdr'))] if d and os.path.isdir(d) else []
        pick = next((f for f in cands if path in f), None)
        path = os.path.join(d, pick) if pick else None
    if path and os.path.exists(path):
        env = nt.nodes.new('ShaderNodeTexEnvironment')
        env.image = bpy.data.images.load(path, check_existing=True)
        nt.links.new(env.outputs['Color'], bg_light.inputs['Color'])
        bg_light.inputs['Strength'].default_value = 1.0
        print('HDRI:', path)
    elif a.hdri not in (None, 'neutral'):
        bg_light.inputs['Color'].default_value = (0.5, 0.5, 0.5, 1)
    nt.links.new(lp.outputs['Is Camera Ray'], mix.inputs['Fac'])
    nt.links.new(bg_light.outputs['Background'], mix.inputs[1])
    nt.links.new(bg_cam.outputs['Background'], mix.inputs[2])
    nt.links.new(mix.outputs['Shader'], out.inputs['Surface'])


def softbox_card(strength):
    """large emission plane with vertical gradient; invisible to camera & shadows.
    Polished steel reflects what is around the camera - without it blades render black."""
    ob = bpy.data.objects.get('rv_softbox')
    if ob is None:
        me = bpy.data.meshes.new('rv_softbox')
        me.from_pydata([(-.5, -.5, 0), (.5, -.5, 0), (.5, .5, 0), (-.5, .5, 0)], [], [(0, 1, 2, 3)])
        ob = bpy.data.objects.new('rv_softbox', me)
        bpy.context.scene.collection.objects.link(ob)
        m = bpy.data.materials.new('rv_softbox_mat')
        m.use_nodes = True
        nt = m.node_tree; nt.nodes.clear()
        out = nt.nodes.new('ShaderNodeOutputMaterial'); em = nt.nodes.new('ShaderNodeEmission')
        tc = nt.nodes.new('ShaderNodeTexCoord'); sp = nt.nodes.new('ShaderNodeSeparateXYZ')
        mr = nt.nodes.new('ShaderNodeMapRange'); mr.interpolation_type = 'SMOOTHSTEP'
        nt.links.new(tc.outputs['Generated'], sp.inputs[0]); nt.links.new(sp.outputs['Y'], mr.inputs['Value'])
        mr.inputs['To Min'].default_value = 0.2 * strength; mr.inputs['To Max'].default_value = 1.2 * strength
        nt.links.new(mr.outputs['Result'], em.inputs['Strength'])
        em.inputs['Color'].default_value = (1, 1, 1, 1)
        nt.links.new(em.outputs['Emission'], out.inputs['Surface'])
        me.materials.append(m)
        for attr in ('visible_camera', 'visible_shadow'):
            if hasattr(ob, attr): setattr(ob, attr, False)
    return ob


def make_light(name, kind='AREA'):
    ld = bpy.data.lights.new(name, kind)
    ob = bpy.data.objects.new(name, ld)
    bpy.context.scene.collection.objects.link(ob)
    return ob


def place_lights(lights, c, d, r, u, S):
    key, fill, rim = lights
    for ob, off, en, size in (
            (key, (-r * 0.9 + u * 1.0 + d * 0.8), 45.0, 0.7),
            (fill, (r * 1.1 - u * 0.2 + d * 0.9), 12.0, 1.0),
            (rim, (r * 0.6 + u * 0.9 - d * 1.3), 55.0, 0.5)):
        pos = c + off * S
        M, _, _ = look(pos, c, Vector((0, 0, 1)) if abs(d.z) < 0.9 else Vector((0, 1, 0)))
        ob.matrix_world = M
        ob.data.energy = en * S * S
        ob.data.size = size * S


def fit(P, c, d, up, ortho, lens, res):
    M, r, u = look(c + d, c, up)
    rel = P - np.array(c)
    x = rel @ np.array(r)
    y = rel @ np.array(u)
    z = rel @ np.array(d)
    ex, ey = float(x.max() - x.min()), float(y.max() - y.min())
    cx, cy = float(x.max() + x.min()) / 2, float(y.max() + y.min()) / 2
    asp = max(ex, 1e-6) / max(ey, 1e-6)
    asp = min(max(asp, 0.08 if ortho else 0.35), 12 if ortho else 2.5)
    if asp >= 1:
        rx, ry = res, max(48, int(round(res / asp / 2)) * 2)
    else:
        rx, ry = max(48, int(round(res * asp / 2)) * 2), res
    center = c + r * cx + u * cy
    size = float(np.linalg.norm(P.max(0) - P.min(0)))
    if ortho:
        dist = size * 2 + 1.0
        scale = max(ex, ey * rx / ry) * 1.06 if rx >= ry else max(ey, ex * ry / rx) * 1.06
        return dict(pos=center + d * dist, center=center, r=r, u=u, ortho=True, scale=scale, rx=rx, ry=ry, S=size)
    half = 18.0 / lens
    tx, ty = (half, half * ry / rx) if rx >= ry else (half * rx / ry, half)
    xs, ys = x - cx, y - cy
    D = float(np.max(z + np.maximum(np.abs(xs) / tx, np.abs(ys) / ty))) * 1.06
    return dict(pos=center + d * D, center=center, r=r, u=u, ortho=False, lens=lens, rx=rx, ry=ry, S=size)


def compose_sheet(paths, out_path, row_h=900, max_w=2600):
    imgs = []
    for p in paths:
        im = bpy.data.images.load(p, check_existing=False)
        w, h = im.size
        nw = max(8, int(w * row_h / h))
        im.scale(nw, row_h)
        a = np.empty(nw * row_h * 4, dtype=np.float32)
        im.pixels.foreach_get(a)
        imgs.append(a.reshape(row_h, nw, 4))
        bpy.data.images.remove(im)
    rows, cur, cw = [], [], 0
    for a in imgs:
        if cur and cw + a.shape[1] + 8 > max_w:
            rows.append(cur); cur, cw = [], 0
        cur.append(a); cw += a.shape[1] + 8
    if cur:
        rows.append(cur)
    W = max(sum(x.shape[1] + 8 for x in rw) for rw in rows)
    H = len(rows) * (row_h + 8)
    sheet = np.ones((H, W, 4), dtype=np.float32) * 0.1
    sheet[..., 3] = 1
    for i, rw in enumerate(rows):
        y0 = H - (i + 1) * (row_h + 8)  # pixels are bottom-up
        x0 = 0
        for a in rw:
            sheet[y0:y0 + row_h, x0:x0 + a.shape[1]] = a
            x0 += a.shape[1] + 8
    img = bpy.data.images.new('sheet', W, H, alpha=False)
    img.pixels.foreach_set(sheet.ravel())
    img.filepath_raw = out_path
    img.file_format = 'JPEG'
    bpy.context.scene.render.image_settings.quality = 88
    img.save()
    print('SHEET', out_path, W, H)


def main():
    a = parse()
    sc = bpy.context.scene
    objs = collect(sc, a.collection)
    if not objs:
        raise SystemExit('nothing to render')
    rows, P = stats(objs)
    print_stats(rows, P)
    if a.stats_only:
        return
    keep = set(o.name for o in objs)
    for o in sc.objects:
        if o.type in GEOM and o.name not in keep:
            o.hide_render = True
        if o.type == 'LIGHT' and not a.keep_lights:
            o.hide_render = True
    setup_render(sc, a)
    if not a.keep_lights:
        studio_world(sc, a)
    if a.clay:
        m = bpy.data.materials.new('clay_override')
        m.diffuse_color = (0.6, 0.6, 0.6, 1)
        bsdf = m.node_tree.nodes.get('Principled BSDF')
        if bsdf:
            bsdf.inputs['Base Color'].default_value = (0.55, 0.55, 0.55, 1)
            bsdf.inputs['Roughness'].default_value = 0.55
        bpy.context.view_layer.material_override = m
    cam_d = bpy.data.cameras.new('review_cam')
    cam = bpy.data.objects.new('review_cam', cam_d)
    sc.collection.objects.link(cam)
    sc.camera = cam
    lights = [make_light(n) for n in ('rv_key', 'rv_fill', 'rv_rim')] if not a.keep_lights else None
    os.makedirs(a.out, exist_ok=True)
    zmin, zmax = float(P[:, 2].min()), float(P[:, 2].max())
    H = zmax - zmin
    c_all = Vector(((P.max(0) + P.min(0)) / 2).tolist())
    views = [v.strip() for v in a.views.split(',') if v.strip()]
    focus = {}
    if a.focus:
        # syntax: "o1,o2"  or  "name=o1,o2@az,el;name2=o3"  (several close-ups, optional camera angles)
        for k, item in enumerate(x for x in a.focus.split(';') if x.strip()):
            nm, _, rest = item.rpartition('=')
            objs_s, _, ang = rest.partition('@')
            parts = [t.strip() for t in ang.split(',')] if ang else ['30', '15']
            az, el = float(parts[0]), float(parts[1])
            hz = len(parts) > 2 and parts[2].lower().startswith('h')   # '@az,el,h' -> horizontal (long axis left-right, top = -X)
            key = 'focus_' + nm.strip() if nm.strip() else ('focus' if k == 0 else 'focus%d' % k)
            focus[key] = (set(t.strip() for t in objs_s.split(',')), az, el, hz)
            views.append(key)
    done = []
    for v in views:
        if v in focus:
            names, az, el, hz = focus[v]
            fr, fP = stats([o for o in objs if o.name in names])
            if not len(fP):
                print('focus: no objects', names); continue
            if hz:   # az: toward +Z (tip/muzzle), el: toward -X (spine/top); 0,0 = looking at the -Y (right) side
                a_, e_ = math.radians(az), math.radians(el)
                dvec = Vector((-math.sin(e_), -math.cos(a_) * math.cos(e_), math.sin(a_) * math.cos(e_)))
                spec = dict(d=dvec, up=Vector((-1, 0, 0)), ortho=False, lens=85)
            else:
                spec = dict(d=dir_from(az, el), up=Vector((0, 0, 1)), ortho=False, lens=85)
            sub = fP
        else:
            spec = VIEWS.get(v)
            if spec is None:
                print('unknown view', v); continue
            sub = P
            if 'band' in spec:
                lo, hi = spec['band']
                m = (P[:, 2] >= zmin + lo * H) & (P[:, 2] <= zmin + hi * H)
                sub = P[m] if m.sum() > 10 else P
        if len(sub) > 60000:
            sub = sub[np.random.default_rng(1).choice(len(sub), 60000, replace=False)]
        c = Vector(((sub.max(0) + sub.min(0)) / 2).tolist())
        f = fit(sub, c, spec['d'], spec['up'], spec['ortho'], spec.get('lens', a.lens), a.res)
        M, r, u = look(f['pos'], f['center'], spec['up'])
        cam.matrix_world = M
        if f['ortho']:
            cam_d.type = 'ORTHO'
            cam_d.ortho_scale = f['scale']
        else:
            cam_d.type = 'PERSP'
            cam_d.lens = f['lens']
        cam_d.sensor_fit = 'AUTO'
        cam_d.sensor_width = 36
        cam_d.clip_start = 0.002
        cam_d.clip_end = 100
        sc.render.resolution_x, sc.render.resolution_y = f['rx'], f['ry']
        if lights:
            place_lights(lights, f['center'], spec['d'], r, u, max(f['S'], 0.05))
        if a.world_up == 'view' and sc.world and sc.world.node_tree:
            mp = sc.world.node_tree.nodes.get('rv_world_map')
            if mp is not None:   # rotate studio so that the camera's up vector becomes the studio 'top' (+Z)
                q = Vector(spec['up']).normalized().rotation_difference(Vector((0, 0, 1)))
                mp.inputs['Rotation'].default_value = q.to_euler('XYZ')
        if a.softbox > 0:
            sb = softbox_card(a.softbox)
            D = (f['pos'] - f['center']).length
            # centred just below the camera: flat faces seen head-on reflect the area around the camera
            Ms, _, _ = look(f['pos'] - u * (0.05 * D), f['center'], u)
            sb.matrix_world = Ms @ Matrix.Diagonal((2.6 * D, 1.2 * D, 1.0, 1.0))
        path = os.path.abspath(os.path.join(a.out, v + '.png'))
        sc.render.filepath = path
        t = time.time()
        bpy.ops.render.render(write_still=True)
        print('VIEW %-20s %4dx%-4d %.1fs -> %s' % (v, f['rx'], f['ry'], time.time() - t, path))
        done.append(path)
    if a.sheet and done:
        compose_sheet(done, os.path.abspath(os.path.join(a.out, 'sheet.jpg')))
        print('SHEET ORDER:', ', '.join(os.path.splitext(os.path.basename(p))[0] for p in done))


main()
