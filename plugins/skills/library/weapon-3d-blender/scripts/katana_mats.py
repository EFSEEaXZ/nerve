"""katana_mats.py - procedural PBR materials for the katana (Cycles/EEVEE)."""
import bpy, math
from wlib import new_mat, principled, N, L, mix_col, mix_f, math_n, maprange

PI2 = 2 * math.pi

class G:
    """tiny node-graph expression helper"""
    def __init__(s, m):
        s.m = m; s.nt, s.b = principled(m)
    def M(s, op, a, b=None):
        return math_n(s.nt, op, a, b)
    def add(s, a, b): return s.M('ADD', a, b)
    def sub(s, a, b): return s.M('SUBTRACT', a, b)
    def mul(s, a, b): return s.M('MULTIPLY', a, b)
    def div(s, a, b): return s.M('DIVIDE', a, b)
    def sin(s, a): return s.M('SINE', a)
    def pow(s, a, b): return s.M('POWER', a, b)
    def abs(s, a): return s.M('ABSOLUTE', a)
    def uv(s, name):
        n = N(s.nt, 'ShaderNodeUVMap'); n.uv_map = name; return n.outputs['UV']
    def tc(s, out='Object'):
        n = N(s.nt, 'ShaderNodeTexCoord'); return n.outputs[out]
    def sep(s, v):
        n = N(s.nt, 'ShaderNodeSeparateXYZ'); L(s.nt, v, n.inputs[0]); return n.outputs['X'], n.outputs['Y'], n.outputs['Z']
    def comb(s, x, y, z):
        n = N(s.nt, 'ShaderNodeCombineXYZ')
        for i, val in enumerate((x, y, z)):
            if isinstance(val, bpy.types.NodeSocket): L(s.nt, val, n.inputs[i])
            else: n.inputs[i].default_value = val
        return n.outputs[0]
    def mapping(s, v, loc=(0, 0, 0), rot=(0, 0, 0), scale=(1, 1, 1)):
        n = N(s.nt, 'ShaderNodeMapping'); L(s.nt, v, n.inputs['Vector'])
        n.inputs['Location'].default_value = loc; n.inputs['Rotation'].default_value = rot; n.inputs['Scale'].default_value = scale
        return n.outputs['Vector']
    def noise(s, v, scale, detail=4.0, rough=0.5, dist=0.0, out='Fac'):
        n = N(s.nt, 'ShaderNodeTexNoise'); L(s.nt, v, n.inputs['Vector'])
        n.inputs['Scale'].default_value = scale; n.inputs['Detail'].default_value = detail
        n.inputs['Roughness'].default_value = rough; n.inputs['Distortion'].default_value = dist
        return n.outputs[out]
    def voronoi(s, v, scale, feature='F1', rnd=1.0, out='Distance'):
        n = N(s.nt, 'ShaderNodeTexVoronoi'); n.feature = feature; L(s.nt, v, n.inputs['Vector'])
        n.inputs['Scale'].default_value = scale; n.inputs['Randomness'].default_value = rnd
        return n.outputs[out]
    def wave(s, v, scale, dist=0.0, detail=0.0, direction='X', profile='SIN'):
        n = N(s.nt, 'ShaderNodeTexWave'); n.wave_type = 'BANDS'; n.bands_direction = direction; n.wave_profile = profile
        L(s.nt, v, n.inputs['Vector']); n.inputs['Scale'].default_value = scale
        n.inputs['Distortion'].default_value = dist; n.inputs['Detail'].default_value = detail
        return n.outputs['Fac']
    def ao(s, dist):
        n = N(s.nt, 'ShaderNodeAmbientOcclusion'); n.inputs['Distance'].default_value = dist; n.samples = 8
        return n.outputs['AO']
    def bump(s, h, strength, dist, normal=None):
        n = N(s.nt, 'ShaderNodeBump'); L(s.nt, h, n.inputs['Height'])
        n.inputs['Strength'].default_value = strength; n.inputs['Distance'].default_value = dist
        if normal is not None: L(s.nt, normal, n.inputs['Normal'])
        return n.outputs['Normal']
    def mr(s, v, a0, a1, b0=0.0, b1=1.0, smooth=True):
        return maprange(s.nt, v, a0, a1, b0, b1, smooth=smooth)
    def col(s, fac, a, b): return mix_col(s.nt, fac, a, b)
    def flt(s, fac, a, b): return mix_f(s.nt, fac, a, b)
    def out(s, base=None, metal=None, rough=None, normal=None, **kw):
        b = s.b
        for key, val in (('Base Color', base), ('Metallic', metal), ('Roughness', rough), ('Normal', normal)):
            if val is None: continue
            if isinstance(val, bpy.types.NodeSocket): L(s.nt, val, b.inputs[key])
            else: b.inputs[key].default_value = val if key != 'Base Color' else (*val, 1)
        for key, val in kw.items():
            key = key.replace('_', ' ')
            if isinstance(val, bpy.types.NodeSocket): L(s.nt, val, b.inputs[key])
            else: b.inputs[key].default_value = val
        return s.m

def _rgba(c): return (*c, 1.0)

# ----------------------------------------------------------------------------- blade steel with hamon
def mat_blade(s_yokote, fs=0.30):
    m = new_mat('steel_blade'); g = G(m)
    s_m, e_m, _ = g.sep(g.uv('EdgeUV'))          # s along blade (m), e = distance from edge (m)
    _, v, _ = g.sep(g.uv('UVMap'))               # v: 0 edge .. 1 mune
    # --- hamon line height H(s): notare (slow wave) + IRREGULAR gunome/choji + noise
    a1 = g.mul(g.sin(g.mul(s_m, PI2 / 0.031)), 0.9)
    notare = g.mul(g.sin(g.add(g.mul(s_m, PI2 / 0.093), a1)), 0.0015)
    ph = g.mul(g.noise(g.comb(g.mul(s_m, 16.0), 0.0, 7.7), 1.0, 2.0, 0.5), 7.0)        # phase wander
    amp = g.mr(g.noise(g.comb(g.mul(s_m, 26.0), 0.0, 1.3), 1.0, 2.0, 0.5), 0.30, 0.70, 0.0003, 0.0017)
    gun = g.mul(g.pow(g.abs(g.sin(g.add(g.mul(s_m, math.pi / 0.0102), ph))), 0.65), amp)
    cho = g.mul(g.pow(g.abs(g.sin(g.add(g.mul(s_m, math.pi / 0.0041), g.mul(ph, 1.9)))), 1.6), g.mul(amp, 0.40))
    nz = g.mul(g.sub(g.noise(g.comb(g.mul(s_m, 45.0), 0.0, 3.1), 1.0, 3.0, 0.55), 0.5), 0.0020)
    H = g.add(g.add(g.add(g.add(notare, gun), cho), nz), 0.0062)
    # yakidashi near the machi, boshi (ko-maru) in the kissaki
    yak = g.mr(s_m, 0.0, 0.035, 0.55, 1.0)
    H = g.mul(H, yak)
    kis = g.mr(s_m, s_yokote - 0.012, s_yokote + 0.004)
    H = g.flt(kis, H, g.add(g.mul(nz, 0.4), 0.0040))
    # soft, misty nioi-guchi: jitter the boundary with fine noise
    fj = g.mul(g.sub(g.noise(g.comb(g.mul(s_m, 900.0), g.mul(e_m, 900.0), 0.5), 1.0, 4.0, 0.6), 0.5), 0.0009)
    d = g.sub(g.add(e_m, fj), H)
    ha = g.mr(d, -0.0009, 0.0006, 1.0, 0.0)
    nioi = g.M('EXPONENT', g.mul(g.pow(g.div(d, 0.0009), 2.0), -1.0))
    # nie: tiny bright crystals scattered along the boundary
    nie_d = g.voronoi(g.comb(g.mul(s_m, 3200.0), g.mul(e_m, 3200.0), 0.0), 1.0, 'F1', 1.0)
    nie = g.mul(g.mr(nie_d, 0.0, 0.16, 1.0, 0.0), g.mr(g.abs(d), 0.0006, 0.0022, 1.0, 0.0))
    # ashi: short streaks toward the edge (irregular)
    ashi_w = g.mr(g.sin(g.add(g.mul(s_m, PI2 / 0.0056), g.mul(ph, 3.0))), 0.88, 0.97)
    ashi = g.mul(g.mul(ashi_w, g.mr(d, -0.0030, -0.0008)), g.mr(d, -0.0008, 0.0, 1.0, 0.0))
    # hada (itame/masame grain) in the ji
    hv = g.comb(g.mul(s_m, 110.0), g.mul(e_m, 1400.0), 0.0)
    hada = g.noise(hv, 1.0, 8.0, 0.62, 1.2)
    hada_c = g.mr(hada, 0.35, 0.65, -1.0, 1.0)
    sj = g.mr(v, 1 - fs - 0.006, 1 - fs + 0.006)           # shinogi-ji + mune (burnished)
    # colours (neutral steel; ha = frosty white, ji = dark mirror)
    ji_c = g.col(g.mr(hada_c, -1, 1), _rgba((0.50, 0.51, 0.53)), _rgba((0.60, 0.61, 0.63)))
    c = g.col(ha, ji_c, _rgba((0.74, 0.745, 0.75)))
    c = g.col(g.mul(nioi, 0.55), c, _rgba((0.86, 0.865, 0.87)))
    c = g.col(g.mul(ashi, 0.45), c, _rgba((0.80, 0.80, 0.80)))
    c = g.col(g.mul(nie, 0.8), c, _rgba((0.95, 0.95, 0.95)))
    c = g.col(sj, c, _rgba((0.58, 0.59, 0.61)))
    r = g.add(0.085, g.mul(hada_c, 0.02))
    r = g.flt(ha, r, 0.36)
    r = g.flt(g.mul(nioi, 0.6), r, 0.26)
    r = g.flt(g.mul(nie, 0.9), r, 0.12)
    r = g.flt(sj, r, 0.05)
    met = g.flt(ha, 1.0, 0.85)
    # fine polishing scratches (along the blade) -> bump
    scr = g.noise(g.comb(g.mul(s_m, 25.0), g.mul(e_m, 9000.0), 0.0), 1.0, 2.0, 0.5)
    nrm = g.bump(scr, 0.035, 0.00002)
    return g.out(c, met, r, nrm)

# ----------------------------------------------------------------------------- habaki: gold with neko-gaki file marks
def mat_habaki():
    """gold-foil habaki: neko-gaki = irregular diagonal file scratches, subtle (strong regular wave = knurl look)"""
    m = new_mat('gold_habaki'); g = G(m)
    co = g.mapping(g.tc('Object'), rot=(0, math.radians(45), 0))
    w = g.wave(co, 380.0, 6.0, 2.0)
    nz = g.noise(g.tc('Object'), 700.0, 4.0)
    scr = g.noise(g.mapping(co, scale=(6000.0, 120.0, 120.0)), 1.0, 2.0, 0.5)
    nrm = g.bump(g.add(g.add(g.mul(w, 0.5), g.mul(nz, 0.5)), g.mul(scr, 0.6)), 0.06, 0.00008)
    base = g.col(g.mr(nz, 0.3, 0.7), _rgba((0.98, 0.74, 0.36)), _rgba((1.0, 0.80, 0.45)))
    r = g.add(0.20, g.mul(nz, 0.12))
    return g.out(base, 1.0, r, nrm)

def mat_seppa():
    m = new_mat('gilt_copper_seppa'); g = G(m)
    nz = g.noise(g.tc('Object'), 1500.0, 2.0)
    nrm = g.bump(nz, 0.05, 0.00005)
    return g.out((0.97, 0.70, 0.42), 1.0, g.add(0.20, g.mul(nz, 0.08)), nrm)

# ----------------------------------------------------------------------------- iron (tsuba): tsuchime + patina
def mat_iron():
    m = new_mat('iron_tsuba'); g = G(m)
    co = g.tc('Object')
    dent = g.voronoi(co, 260.0, 'SMOOTH_F1', 1.0)
    fine = g.noise(co, 1800.0, 6.0, 0.6)
    rust = g.noise(co, 55.0, 8.0, 0.65, 0.4)
    rr = g.mr(rust, 0.48, 0.70)
    base = g.col(rr, _rgba((0.040, 0.035, 0.031)), _rgba((0.105, 0.058, 0.032)))
    base = g.col(g.mr(fine, 0.3, 0.7), base, _rgba((0.030, 0.028, 0.027)))
    rough = g.add(0.42, g.mul(rr, 0.22))
    h = g.add(g.mul(dent, 1.0), g.mul(fine, 0.25))
    nrm = g.bump(h, 0.32, 0.0005)
    return g.out(base, g.flt(rr, 0.62, 0.25), rough, nrm)

# ----------------------------------------------------------------------------- shakudo with nanako
def mat_shakudo():
    """shakudo (Cu-Au alloy, blue-black patina) with fine nanako; keep bump subtle (strong = black carbon mesh)"""
    m = new_mat('shakudo_nanako'); g = G(m)
    co = g.tc('Object')
    dots = g.voronoi(co, 1900.0, 'F1', 0.15)
    dome = g.mr(dots, 0.18, 0.42, 1.0, 0.0)
    nz = g.noise(co, 300.0, 4.0)
    wear = g.mr(g.noise(co, 90.0, 5.0, 0.6), 0.60, 0.75)
    nrm = g.bump(g.add(dome, g.mul(nz, 0.3)), 0.14, 0.0001)
    base = g.col(g.mr(nz, 0.3, 0.7), _rgba((0.030, 0.030, 0.038)), _rgba((0.045, 0.040, 0.050)))
    base = g.col(g.mul(wear, 0.35), base, _rgba((0.20, 0.10, 0.06)))
    return g.out(base, 0.8, g.add(0.30, g.mul(nz, 0.1)), nrm)

def mat_gold_relief(name='gold_relief'):
    m = new_mat(name); g = G(m)
    ao = g.ao(0.0015)
    occ = g.mr(ao, 0.4, 1.0, 1.0, 0.0)
    base = g.col(occ, _rgba((1.0, 0.76, 0.33)), _rgba((0.32, 0.19, 0.06)))
    return g.out(base, 1.0, g.add(0.2, g.mul(occ, 0.25)))

# ----------------------------------------------------------------------------- samegawa (ray skin)
def mat_samegawa():
    """round pearly nodules, densely packed: 2 offset Voronoi layers of round domes (F1 distance),
    max() of both -> beads fill each other's gaps; crevices darker cream. (distance-to-edge domes
    looked like cracked mud - polygonal cells - avoid)"""
    m = new_mat('samegawa'); g = G(m)
    co = g.tc('Object')
    def domes(vec, scale, r):
        d = g.voronoi(vec, scale, 'F1', 0.85)
        return g.pow(g.mr(d, 0.0, r, 1.0, 0.0), 0.6)            # 1 at centre -> 0 at radius r (cell units)
    co2 = g.mapping(co, loc=(0.00041, 0.00023, 0.00037))
    small = g.M('MAXIMUM', domes(co, 1100.0, 0.56), g.mul(domes(co2, 1100.0, 0.48), 0.85))
    big = g.M('MAXIMUM', domes(co, 470.0, 0.56), g.mul(domes(co2, 470.0, 0.48), 0.85))
    patch = g.mr(g.noise(co, 40.0, 2.0, 0.5), 0.53, 0.60)        # patches of larger nodules
    h = g.flt(patch, small, big)
    crev = g.mr(h, 0.0, 0.35, 1.0, 0.0)
    tint = g.noise(co, 900.0, 2.0, 0.5)
    pearl = g.col(g.mr(tint, 0.3, 0.7), _rgba((0.84, 0.83, 0.78)), _rgba((0.94, 0.93, 0.89)))
    base = g.col(crev, pearl, _rgba((0.44, 0.40, 0.32)))
    nrm = g.bump(h, 0.6, 0.00035)
    return g.out(base, 0.0, g.add(0.32, g.mul(crev, 0.30)), nrm, Coat_Weight=0.5, Coat_Roughness=0.15)

# ----------------------------------------------------------------------------- silk tsuka-ito
def mat_ito():
    """dark navy hira-ito: fine braid ribs + fibre streaks, low sheen (too much sheen = denim look)"""
    m = new_mat('silk_ito'); g = G(m)
    u, v, _ = g.sep(g.uv('UVMap'))               # u along band (m), v across band (m)
    tw = g.sin(g.add(g.mul(u, PI2 / 0.0011), g.mul(v, PI2 / 0.0008)))
    rib = g.sin(g.mul(v, PI2 / 0.00045))
    fib = g.noise(g.comb(g.mul(u, 2500.0), g.mul(v, 16000.0), 0.0), 1.0, 3.0, 0.6)
    nz = g.noise(g.tc('Object'), 2500.0, 3.0)
    h = g.add(g.add(g.mul(tw, 0.45), g.mul(rib, 0.20)), g.mul(fib, 0.45))
    nrm = g.bump(h, 0.22, 0.00015)
    base = g.col(g.mr(g.add(g.mul(nz, 0.6), g.mul(fib, 0.4)), 0.3, 0.7),
                 _rgba((0.0065, 0.0072, 0.012)), _rgba((0.013, 0.014, 0.023)))
    return g.out(base, 0.0, g.add(0.56, g.mul(fib, 0.10)), nrm,
                 Specular_IOR_Level=0.35, Sheen_Weight=0.10, Sheen_Roughness=0.40,
                 Sheen_Tint=(0.30, 0.33, 0.45, 1.0))

def mat_simple(name, color, metal, rough, bump_scale=None, bump_strength=0.1):
    m = new_mat(name); g = G(m)
    nrm = None
    if bump_scale:
        nrm = g.bump(g.noise(g.tc('Object'), bump_scale, 4.0), bump_strength, 0.0002)
    return g.out(color, metal, rough, nrm)
