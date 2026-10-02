"""CSS colors (rgb, oklab, oklch, color(srgb …)) to sRGB hex, as Chromium resolves them."""
import json, re, math, sys
def lin2srgb(c):
    c = max(0.0, min(1.0, c))
    return 12.92*c if c <= 0.0031308 else 1.055*c**(1/2.4) - 0.055
def oklab2rgb(L, a, b):
    l_ = L + 0.3963377774*a + 0.2158037573*b
    m_ = L - 0.1055613458*a - 0.0638541728*b
    s_ = L - 0.0894841775*a - 1.2914855480*b
    l, m, s = l_**3, m_**3, s_**3
    r = 4.0767416621*l - 3.3077115913*m + 0.2309699292*s
    g = -1.2684380046*l + 2.6097574011*m - 0.3413193965*s
    bb = -0.0041960863*l - 0.7034186147*m + 1.7076147010*s
    return [lin2srgb(x) for x in (r, g, bb)]
def num(t):
    t = t.strip()
    if t.endswith('%'): return float(t[:-1]) / 100
    if t == 'none': return 0.0
    return float(t)
def parse(c):
    """CSS color string → (r,g,b 0-255 ints, alpha 0-1)"""
    if c is None: return None
    c = c.strip()
    m = re.match(r'(\w+)\((.*)\)$', c)
    if not m: return None
    fn, body = m.group(1), m.group(2)
    alpha = 1.0
    if '/' in body:
        body, a = body.rsplit('/', 1); alpha = num(a)
    parts = body.replace(',', ' ').split()
    if fn in ('rgb', 'rgba'):
        r, g, b = [float(x) for x in parts[:3]]
        if len(parts) == 4: alpha = float(parts[3])
        rgb = [r/255, g/255, b/255]
    elif fn == 'oklab':
        rgb = oklab2rgb(num(parts[0]), float(parts[1]), float(parts[2]))
    elif fn == 'oklch':
        L, C, H = num(parts[0]), float(parts[1]), float(parts[2]) if parts[2] != 'none' else 0
        rgb = oklab2rgb(L, C*math.cos(math.radians(H)), C*math.sin(math.radians(H)))
    elif fn == 'color' and parts[0] == 'srgb':
        rgb = [max(0, min(1, float(x))) for x in parts[1:4]]
    else:
        return None
    return tuple(round(max(0, min(1, x))*255) for x in rgb), round(alpha, 4)
def hexa(c):
    p = parse(c)
    if not p: return None
    (r, g, b), a = p
    return f"#{r:02x}{g:02x}{b:02x}" + ("" if a >= 0.9999 else f"{round(a*255):02x}")
if __name__ == '__main__':
    # colors.py vars.json → web-theme.json: the web theme's colors by role, light and dark.
    d = json.load(open(sys.argv[1] if len(sys.argv) > 1 else 'vars.json'))
    L, D = d['light']['vars'], d['dark']['vars']
    skip = ('--ls-', '--tw', '--appearance', '--stage-', '--color-', '--contrast-', '--lsg-')
    keep = ['--ls-text-3', '--ls-bg', '--ls-glass-edge', '--ls-scrim', '--lsg-1-bg', '--lsg-2-bg', '--lsg-3-bg', '--ls-glass-opaque', '--ls-accent']
    roles = {}
    for k in sorted(L):
        if k.startswith(skip) and k not in keep:
            continue
        a, b = hexa(L[k].get('color')), hexa(D.get(k, {}).get('color'))
        if a and b:
            roles[k[2:]] = {'light': a, 'dark': b}
    out = {'source': 'code/apps/web, default zenith theme, resolved by Chromium (scripts/gpui-parity)', 'colors': roles}
    json.dump(out, open('web-theme.json', 'w'), indent=1)
    print(len(roles), 'colors → web-theme.json')
