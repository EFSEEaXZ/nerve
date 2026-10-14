"""m4_mats.py - procedural PBR materials for the M4A1 (Cycles). Reuses the G node helper from katana_mats.
Key looks: hard-anodised aluminium (dark grey satin), manganese phosphate steel (grey-green, matte),
black polymer (stipple), GI magazine (lighter grey), optic black anodising, coated lens glass.
Edge wear = Bevel-node edge mask x AO (convex edges only) -> lighter bare metal, kept subtle."""
import bpy, math
from wlib import new_mat, N, L
from katana_mats import G, _rgba

def _edge(g, radius=0.0007, lo=0.985, hi=0.90):
    """0 on flat areas -> 1 on (mostly convex) edges"""
    try:
        bv = N(g.nt, 'ShaderNodeBevel'); bv.samples = 8; bv.inputs['Radius'].default_value = radius
        geo = N(g.nt, 'ShaderNodeNewGeometry')
        dp = N(g.nt, 'ShaderNodeVectorMath'); dp.operation = 'DOT_PRODUCT'
        L(g.nt, bv.outputs['Normal'], dp.inputs[0]); L(g.nt, geo.outputs['Normal'], dp.inputs[1])
        e = g.mr(dp.outputs['Value'], lo, hi)
        ao = g.mr(g.ao(0.004), 0.55, 0.95)          # concave corners are occluded -> no wear there
        return g.mul(e, ao)
    except Exception:
        return None

def _wear(g, edge, amount=1.0, scale=900.0):
    nz = g.noise(g.tc('Object'), scale, 6.0, 0.65)
    return g.mul(g.mr(g.mul(edge, g.add(0.35, nz)), 0.35, 0.75), amount)

def finish(name, base, rough, metal=0.0, wear=0.6, wear_col=(0.42, 0.42, 0.43), mottle=0.07,
           bump_scale=2600.0, bump_str=0.05, spec=0.5, coat=0.0):
    m = new_mat(name); g = G(m)
    co = g.tc('Object')
    n1 = g.noise(co, 90.0, 3.0, 0.55)                       # large mottling (finish variation) - keep subtle, else 'concrete'
    n2 = g.noise(co, bump_scale, 3.0, 0.5)                  # fine bead-blast grain
    b0 = tuple(c * (1 - mottle) for c in base); b1 = tuple(min(1, c * (1 + mottle)) for c in base)
    col = g.col(g.mr(n1, 0.3, 0.7), _rgba(b0), _rgba(b1))
    r = g.add(rough - 0.02, g.mul(n1, 0.04))
    met = metal
    if wear > 0:
        e = _edge(g)
        if e is not None:
            w = _wear(g, e, wear)
            col = g.col(w, col, _rgba(wear_col))
            r = g.flt(w, r, 0.30)
            met = g.flt(w, metal, 1.0)
    nrm = g.bump(n2, bump_str, 0.00003)
    return g.out(col, met, r, nrm, Specular_IOR_Level=spec, Coat_Weight=coat, Coat_Roughness=0.25)

def anodized():  return finish('al_anodized', (0.050, 0.051, 0.055), 0.50, 0.15, wear=0.7, wear_col=(0.50, 0.50, 0.51))
def phosphate(): return finish('steel_phosphate', (0.042, 0.044, 0.041), 0.62, 0.30, wear=0.5, wear_col=(0.42, 0.42, 0.41), mottle=0.10)
def polymer():   return finish('polymer_black', (0.034, 0.034, 0.036), 0.70, 0.0, wear=0.25, wear_col=(0.07, 0.07, 0.07), bump_scale=1800.0, bump_str=0.10, spec=0.35)
def roll_mark(): return finish('roll_mark', (0.13, 0.13, 0.135), 0.62, 0.4, wear=0.0, mottle=0.05, bump_str=0.02)   # stamped marks read slightly lighter
def mag_al():    return finish('mag_gi_grey', (0.060, 0.063, 0.064), 0.50, 0.25, wear=0.8, wear_col=(0.45, 0.45, 0.45), mottle=0.08)
def optic_al():  return finish('optic_black', (0.020, 0.020, 0.022), 0.34, 0.2, wear=0.35, wear_col=(0.4, 0.4, 0.41), bump_str=0.03)
def rubber():    return finish('rubber', (0.018, 0.018, 0.019), 0.82, 0.0, wear=0.0, bump_scale=3000.0, bump_str=0.15)

def grip_checker():
    """A2 grip: polymer + diamond checkering on the flat side panels (mask by |N.y| of object normal)."""
    m = new_mat('polymer_grip'); g = G(m)
    co = g.tc('Object')
    x, y, z = g.sep(co)
    p = 0.0016
    a = g.abs(g.sin(g.mul(g.add(x, z), math.pi / p)))
    b = g.abs(g.sin(g.mul(g.sub(x, z), math.pi / p)))
    h = g.M('MINIMUM', a, b)
    gn = N(g.nt, 'ShaderNodeNewGeometry')
    tc = N(g.nt, 'ShaderNodeVectorTransform'); tc.vector_type = 'NORMAL'; tc.convert_from = 'WORLD'; tc.convert_to = 'OBJECT'
    L(g.nt, gn.outputs['True Normal'], tc.inputs[0])
    _, ny, _ = g.sep(tc.outputs[0])
    mask = g.mr(g.abs(ny), 0.93, 0.985)
    nz = g.noise(co, 1800.0, 3.0)
    nrm = g.bump(g.add(g.mul(h, mask), g.mul(nz, 0.15)), 0.5, 0.00025)
    col = g.col(g.mr(nz, 0.3, 0.7), _rgba((0.027, 0.027, 0.029)), _rgba((0.034, 0.034, 0.036)))
    return g.out(col, 0.0, g.add(0.58, g.mul(mask, 0.06)), nrm)

def lens_glass():
    m = new_mat('lens_coated'); g = G(m)
    kw = dict(Specular_IOR_Level=1.0, Coat_Weight=1.0, Coat_Roughness=0.02)
    m2 = g.out((0.010, 0.010, 0.014), 0.0, 0.03, None, **kw)
    try:
        g.b.inputs['Thin Film Thickness'].default_value = 420.0
        g.b.inputs['Thin Film IOR'].default_value = 1.38
    except Exception:
        pass
    return m2

def brass():  return finish('brass', (0.78, 0.58, 0.30), 0.32, 1.0, wear=0.0, mottle=0.1, bump_str=0.02)
def steel():  return finish('steel_bright', (0.55, 0.55, 0.56), 0.30, 1.0, wear=0.0, mottle=0.08, bump_str=0.02)
def paint_white(): return finish('paint_white', (0.62, 0.62, 0.58), 0.65, 0.0, wear=0.0, mottle=0.12, bump_str=0.02)
