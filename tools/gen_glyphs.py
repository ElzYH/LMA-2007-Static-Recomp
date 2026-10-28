# gen_glyphs.py <outdir> [--set ps5|keyboard] (lma-rs/assets/glyphs/<set>): DualSense or keyboard glyphs as 64x64 RGBA PNGs for the
# texture-override table (#694), named by the
# hash of the 360 glyph they replace (LMA_TEX_LOG on the hub). Drawn here, no copied artwork. The 360 glyphs sit
# in the top-left 34x34 of the texture (the stick icon spans the width), so these do too.
import math, os, struct, sys, zlib

W = H = 64

def png(path, px):
    raw = b''.join(b'\x00' + bytes(px[y * W * 4:(y + 1) * W * 4]) for y in range(H))
    def chunk(t, b): return struct.pack('>I', len(b)) + t + b + struct.pack('>I', zlib.crc32(t + b) & 0xffffffff)
    open(path, 'wb').write(b'\x89PNG\r\n\x1a\n' + chunk(b'IHDR', struct.pack('>IIBBBBB', W, H, 8, 6, 0, 0, 0)) + chunk(b'IDAT', zlib.compress(raw, 9)) + chunk(b'IEND', b''))

def blend(px, x, y, rgb, a):
    if not (0 <= x < W and 0 <= y < H) or a <= 0: return
    i = (y * W + x) * 4
    a = min(a, 1.0); da = px[i + 3] / 255
    oa = a + da * (1 - a)
    for k in range(3):
        px[i + k] = int(round((rgb[k] * a + px[i + k] * da * (1 - a)) / oa)) if oa > 0 else 0
    px[i + 3] = int(round(oa * 255))

def disc(px, cx, cy, r, rgb, ring=None):
    for y in range(H):
        for x in range(W):
            d = math.hypot(x + 0.5 - cx, y + 0.5 - cy)
            blend(px, x, y, rgb, r - d + 0.5)
            if ring:
                blend(px, x, y, ring[1], min(r - d + 0.5, d - (r - ring[0]) + 0.5))

def rrect(px, x0, y0, x1, y1, rad, rgb):
    for y in range(H):
        for x in range(W):
            qx = max(x0 + rad - (x + 0.5), 0, (x + 0.5) - (x1 - rad)); qy = max(y0 + rad - (y + 0.5), 0, (y + 0.5) - (y1 - rad))
            blend(px, x, y, rgb, rad - math.hypot(qx, qy) + 0.5)

def stroke(px, x0, y0, x1, y1, wd, rgb):
    for y in range(H):
        for x in range(W):
            dx, dy = x1 - x0, y1 - y0; L = dx * dx + dy * dy
            t = max(0, min(1, ((x + 0.5 - x0) * dx + (y + 0.5 - y0) * dy) / L)) if L else 0
            d = math.hypot(x + 0.5 - (x0 + t * dx), y + 0.5 - (y0 + t * dy))
            blend(px, x, y, rgb, wd / 2 - d + 0.5)

def ring(px, cx, cy, r, wd, rgb):
    for y in range(H):
        for x in range(W):
            d = math.hypot(x + 0.5 - cx, y + 0.5 - cy)
            blend(px, x, y, rgb, wd / 2 - abs(d - r) + 0.5)

FONT = {'L': ['#   ', '#   ', '#   ', '#   ', '####'], 'R': ['### ', '#  #', '### ', '# # ', '#  #'], '1': [' # ', '## ', ' # ', ' # ', '###'],
        '2': ['### ', '   #', ' ## ', '#   ', '####'], '3': ['### ', '   #', ' ## ', '   #', '### '],
        'E': ['###', '#  ', '## ', '#  ', '###'], 'S': [' ##', '#  ', ' # ', '  #', '## '], 'C': [' ##', '#  ', '#  ', '#  ', ' ##'],
        'Q': [' ## ', '#  #', '#  #', '# ##', ' ###'], 'F': ['###', '#  ', '## ', '#  ', '#  ']}

def text(px, s, x, y, scale, rgb):
    for ch in s:
        rows = FONT[ch]
        for j, row in enumerate(rows):
            for i, c in enumerate(row):
                if c == '#':
                    for yy in range(scale):
                        for xx in range(scale):
                            blend(px, x + i * scale + xx, y + j * scale + yy, rgb, 1.0)
        x += (len(rows[0]) + 1) * scale

DARK = (28, 28, 34); LIGHT = (214, 214, 220); SHADE = (120, 120, 130)
CROSS = (120, 170, 255); CIRCLE = (255, 90, 90); SQUARE = (255, 130, 200); TRIANGLE = (90, 230, 120)

def face(symbol):
    px = bytearray(W * H * 4)
    cx, cy, r = 17.5, 19.5, 16.0
    disc(px, cx, cy, r, DARK, ring=(2.0, SHADE))
    if symbol == 'cross':
        stroke(px, cx - 6, cy - 6, cx + 6, cy + 6, 3.2, CROSS); stroke(px, cx + 6, cy - 6, cx - 6, cy + 6, 3.2, CROSS)
    elif symbol == 'circle':
        ring(px, cx, cy, 6.5, 3.0, CIRCLE)
    elif symbol == 'square':
        for (ax, ay, bx, by) in ((-6, -6, 6, -6), (6, -6, 6, 6), (6, 6, -6, 6), (-6, 6, -6, -6)):
            stroke(px, cx + ax, cy + ay, cx + bx, cy + by, 3.0, SQUARE)
    elif symbol == 'triangle':
        for (ax, ay, bx, by) in ((0, -7.5, 7, 5.5), (7, 5.5, -7, 5.5), (-7, 5.5, 0, -7.5)):
            stroke(px, cx + ax, cy + ay, cx + bx, cy + by, 3.0, TRIANGLE)
    return px

def shoulder(label):
    px = bytearray(W * H * 4)
    rrect(px, 1, 6, 35, 34, 6, LIGHT)
    rrect(px, 3, 8, 33, 32, 5, DARK)
    text(px, label, 7 if label[1] == '1' else 7, 13, 3, LIGHT)
    return px

def stick(label):
    px = bytearray(W * H * 4)
    disc(px, 19.5, 19.5, 17.0, (22, 150, 40), ring=(2.5, (10, 80, 20)))
    text(px, label, 10, 13, 3, (240, 255, 240))
    return px

KEY_FACE = (236, 236, 240); KEY_EDGE = (150, 150, 160); KEY_INK = (30, 30, 36)

def keycap(label=None, enter=False, arrow=None):
    """A keyboard key: a light cap with its legend, in the top-left 34x34 as the 360 glyphs."""
    px = bytearray(W * H * 4)
    rrect(px, 1, 2, 35, 36, 6, KEY_EDGE)
    rrect(px, 2.5, 3, 33.5, 32, 5, KEY_FACE)
    if arrow:
        # an arrow key: the shaft and the two sides of the head, pointing (dx, dy)
        dx, dy = {'up': (0, -1), 'down': (0, 1), 'left': (-1, 0), 'right': (1, 0)}[arrow]
        cx, cy = 18.0, 17.5
        tip = (cx + 8 * dx, cy + 8 * dy)
        stroke(px, cx - 8 * dx, cy - 8 * dy, tip[0], tip[1], 3.0, KEY_INK)
        for side in (1, -1):
            stroke(px, tip[0], tip[1], tip[0] - 6 * dx + 6 * side * dy, tip[1] - 6 * dy + 6 * side * dx, 3.0, KEY_INK)
    elif enter:
        # the return arrow: down, then left, with its head
        stroke(px, 25, 9, 25, 22, 3.0, KEY_INK)
        stroke(px, 25, 22, 10, 22, 3.0, KEY_INK)
        stroke(px, 10, 22, 15, 17, 3.0, KEY_INK)
        stroke(px, 10, 22, 15, 27, 3.0, KEY_INK)
    else:
        scale = 3 if len(label) == 1 else 2
        width = sum((len(FONT[c][0]) + 1) * scale for c in label) - scale
        text(px, label, int(18 - width / 2), int(17.5 - 2.5 * scale), scale, KEY_INK)
    return px

# lma-app's keyboard map (keys.rs): Enter/Space = A, Esc/Backspace = B, R = Y, Q / E = LB / RB,
# 1 / 3 = LT / RT, C = the right stick
KEYBOARD = {
    '340157ca1854882d': ('A -> Enter', keycap(enter=True)),
    '56487c0a5b537226': ('B -> Esc', keycap('ESC')),
    'e3eefc792d9f2c5e': ('Y -> R', keycap('R')),
    'a9a23b02633dce43': ('X -> F', keycap('F')),
    'e20a3321227cad03': ('LB -> Q', keycap('Q')),
    '3b4e1b35d87a5fbf': ('RB -> E', keycap('E')),
    '3a052683c7fe182c': ('LT -> 1', keycap('1')),
    '1e35308cbd0fcd67': ('RT -> 3', keycap('3')),
    '998de0650aab198b': ('RS -> C', keycap('C')),
    # the d-pad arrows (the glyph atlas's slots after Y; WASD and the arrow keys move)
    '475d718f6a1fcd31': ('d-pad up -> Up', keycap(arrow='up')),
    '4f0adac5b2eed887': ('d-pad down -> Down', keycap(arrow='down')),
    '01d740ffcbd17d3e': ('d-pad left -> Left', keycap(arrow='left')),
    'd9eb1a54fb089eee': ('d-pad right -> Right', keycap(arrow='right')),
}

GLYPHS = {
    '340157ca1854882d': ('A -> Cross', face('cross')),
    '56487c0a5b537226': ('B -> Circle', face('circle')),
    'e3eefc792d9f2c5e': ('Y -> Triangle', face('triangle')),
    'a9a23b02633dce43': ('X -> Square', face('square')),
    'e20a3321227cad03': ('LB -> L1', shoulder('L1')),
    '3b4e1b35d87a5fbf': ('RB -> R1', shoulder('R1')),
    '3a052683c7fe182c': ('LT -> L2', shoulder('L2')),
    '1e35308cbd0fcd67': ('RT -> R2', shoulder('R2')),
    '998de0650aab198b': ('RS -> R3', stick('R3')),
}
out = sys.argv[1]
kind = sys.argv[sys.argv.index('--set') + 1] if '--set' in sys.argv else 'ps5'
os.makedirs(out, exist_ok=True)
if kind == 'keyboard':
    for h, (what, px) in KEYBOARD.items():
        png(os.path.join(out, f'{h}.png'), px)
    with open(os.path.join(out, 'README.md'), 'w', newline='\n') as f:
        f.write('# Keyboard glyphs (#694 step d)\n\nThe 360 button glyphs replaced by the keys lma-app maps to those buttons (`lma-app/src/keys.rs`),\n'
                'keyed by the hash of the guest bytes of the texture they replace (`LMA_TEX_LOG`), 64x64 RGBA: drawn by\n'
                '`tools/gen_glyphs.py --set keyboard` (no copied artwork). lma-app installs them while the keyboard is the\n'
                'last device used.\n\n| file | replaces |\n|---|---|\n' + ''.join(f'| {h}.png | {what} |\n' for h, (what, _) in KEYBOARD.items()))
    print('wrote', len(KEYBOARD), 'glyphs to', out)
    sys.exit(0)
for h, (what, px) in GLYPHS.items():
    png(os.path.join(out, f'{h}.png'), px)
with open(os.path.join(out, 'README.md'), 'w', newline='\n') as f:
    f.write('# DualSense glyphs (#694)\n\nReplacement images for the 360 button glyphs, keyed by the hash of the guest bytes of the\n'
            'texture they replace (`LMA_TEX_LOG`), 64x64 RGBA as the originals: drawn by `tools/gen_glyphs.py`\n'
            '(no copied artwork). START, BACK, LS and the d-pad glyphs are still to be found; Y came from the squad screen, X from the pre-match screen.\n\n'
            '| file | replaces |\n|---|---|\n' + ''.join(f'| {h}.png | {what} |\n' for h, (what, _) in GLYPHS.items()))
print('wrote', len(GLYPHS), 'glyphs to', out)
