"""TMR's icon: a DDR5 UDIMM with its heat spreader off, as SVG, traces lit red like neon.

The module, as a single-rank x8 DDR5 UDIMM has it: two 32-bit subchannels of 4 DRAM packages each, left and right of the centre; the
command/address bus rising from the fingers beside the key and running fly-by along the top on an inner layer, a via and a stub at
each chip, each line ending at its last chip; 8 data lanes from each chip to the fingers under it, on every other finger (the rest
are ground) where there's room; between the groups the PMIC with its three inductors, bulk and output capacitors, and the SPD hub;
decoupling capacitors between the chips; 144 gold fingers a side at 0.85 mm, the radiused key notch off centre, the retention
notches at the ends. Traces run vertical with 45-degree bends, as a board's are routed. The chips' laser markings run vertically, as
a DIMM's chips are turned; the last chip's reads TMR.

The artwork (`module`, vector) is the module at its real 133.35 x 31.25 mm. The icon is a pair of modules, the one behind darker
(layout B): vector at 256 px; on the pixel grid (`pixel_module`) at 96 px and below, whole pixels and one-pixel lines so nothing
blurs: 4 + 4 chips and the PMIC at 48 px and up, 2 + 2 below, a bus line over the chips at every size, and a single module at 16 px,
where a pair has no room. Checks: no two lit traces of a vector module run closer than half a stroke apart, edge to edge; on the grid,
every lane ends on a finger, none in the key, and no part shares another's column.

Usage: dimm.py DIR. Writes into DIR: dimm-burn.svg (the artwork), icon<px>.svg for each icon size, preview.html (the artwork and the
icons at their sizes, on dark and light) and pixels.html (each pixel-grid icon, 96 px and below, rasterised at its size, then
magnified). pack.py turns them into tmr.ico (tools/icon/README.md)."""
import math
import os
import sys


PCB = ("#0b0c0f", "#1b1e24")
ART_EDGE, ICON_EDGE = "#4a505b", "#6a7079"
LANE, CORE = "#ff1e1e", "#ffd2d2"  # neon red: a red tube with a pale core, its glow red
MARK = "#3f4248"  # a chip's laser marking
ETCH = "#9da1a9"  # the last chip's TMR
VIA = "#a9acb2"


def defs(uid, glow_px):
    glow = ""
    if glow_px:
        glow = (f'<filter id="glow{uid}" x="-10%" y="-30%" width="120%" height="160%">'
                f'<feGaussianBlur in="SourceGraphic" stdDeviation="{glow_px:.2f}" result="b1"/>'
                f'<feGaussianBlur in="SourceGraphic" stdDeviation="{glow_px / 3:.2f}" result="b2"/>'
                f'<feMerge><feMergeNode in="b1"/><feMergeNode in="b1"/><feMergeNode in="b2"/><feMergeNode in="SourceGraphic"/>'
                f'</feMerge></filter>'
                f'<filter id="dim{uid}" x="-10%" y="-30%" width="120%" height="160%">'
                f'<feGaussianBlur in="SourceGraphic" stdDeviation="{glow_px / 2:.2f}" result="b1"/>'
                f'<feMerge><feMergeNode in="b1"/><feMergeNode in="SourceGraphic"/></feMerge></filter>')
    return (f'<defs><linearGradient id="pcb{uid}" x1="0" y1="0" x2="0" y2="1"><stop offset="0" stop-color="{PCB[1]}"/>'
            f'<stop offset="1" stop-color="{PCB[0]}"/></linearGradient>'
            f'<linearGradient id="gold{uid}" x1="0" y1="0" x2="0" y2="1"><stop offset="0" stop-color="#fff2b0"/>'
            f'<stop offset="0.35" stop-color="#f0c65a"/><stop offset="1" stop-color="#9a7220"/></linearGradient>'
            f'<linearGradient id="chip{uid}" x1="0" y1="0" x2="0" y2="1"><stop offset="0" stop-color="#303237"/>'
            f'<stop offset="0.08" stop-color="#1f2024"/><stop offset="1" stop-color="#121316"/></linearGradient>'
            f'<linearGradient id="pmic{uid}" x1="0" y1="0" x2="0" y2="1"><stop offset="0" stop-color="#26282c"/>'
            f'<stop offset="1" stop-color="#17181b"/></linearGradient>'
            f'<linearGradient id="ind{uid}" x1="0" y1="0" x2="0" y2="1"><stop offset="0" stop-color="#4a4d53"/>'
            f'<stop offset="0.15" stop-color="#34363b"/><stop offset="1" stop-color="#2a2c30"/></linearGradient>{glow}</defs>')


def chip_marking(x, y, cw, ch, u, last):
    """A chip's laser marking: the pin-1 dot (lower left, where a turned package's ball A1 is, beside the start of its text), and
    lines of turned text, each starting at the chip's lower edge (0.84 of its height) and running a different length; the last chip's
    reads TMR, upward from that edge, with a code line beside it."""
    lw = max(0.5 * u, 0.6)
    base = y + ch * 0.84
    out = [f'<circle cx="{x + 1.1 * u:.1f}" cy="{y + ch - 1.1 * u:.1f}" r="{0.32 * u:.2f}" fill="{MARK}"/>']
    if last:
        fs = ch * 0.3
        tx = x + cw * 0.44
        out.append(f'<text x="{tx:.1f}" y="{base:.1f}" transform="rotate(-90 {tx:.1f} {base:.1f})" text-anchor="start" '
                   f'dominant-baseline="central" '
                   f'font-family="Segoe UI, Arial, sans-serif" font-weight="700" font-size="{fs:.1f}" letter-spacing="{fs * 0.06:.1f}" '
                   f'fill="{ETCH}">TMR</text>')
        out.append(f'<rect x="{x + cw * 0.76:.1f}" y="{base - ch * 0.44:.1f}" width="{lw:.2f}" height="{ch * 0.44:.1f}" '
                   f'rx="{lw / 2:.2f}" fill="{MARK}"/>')
        return "".join(out)
    for fx, fh in ((0.34, 0.6), (0.52, 0.46), (0.7, 0.54)):
        out.append(f'<rect x="{x + cw * fx - lw / 2:.1f}" y="{base - ch * fh:.1f}" width="{lw:.2f}" height="{ch * fh:.1f}" '
                   f'rx="{lw / 2:.2f}" fill="{MARK}"/>')
    return "".join(out)


def mlcc(x, y, w, h, u):
    """A ceramic capacitor: a tan body with tinned end bands across its long side (standing when it's taller than wide)."""
    band = 0.2 * u
    ends = ((f'<rect x="{x:.1f}" y="{y:.1f}" width="{w:.1f}" height="{band:.2f}" fill="{VIA}"/>'
             f'<rect x="{x:.1f}" y="{y + h - band:.1f}" width="{w:.1f}" height="{band:.2f}" fill="{VIA}"/>') if h > w else
            (f'<rect x="{x:.1f}" y="{y:.1f}" width="{band:.2f}" height="{h:.1f}" fill="{VIA}"/>'
             f'<rect x="{x + w - band:.1f}" y="{y:.1f}" width="{band:.2f}" height="{h:.1f}" fill="{VIA}"/>'))
    return f'<rect x="{x:.1f}" y="{y:.1f}" width="{w:.1f}" height="{h:.1f}" rx="{0.08 * u:.2f}" fill="#c4b089"/>' + ends


def check_parts(risers, lit, vias, rects, stroke, label):
    """No bus riser touches a part (a chip, the PMIC, a capacitor, an inductor: a stroke's clearance from its edge; the stubs and
    lanes that end on their own chip are meant to), and no lit trace passes a via but its own (a ring's radius and a stroke from the
    via's centre)."""
    for p in risers:
        pts = polyline_points(p)
        segs = list(zip(pts, pts[1:]))
        for x, y, w, h in rects:
            corners = [(x, y), (x + w, y), (x + w, y + h), (x, y + h)]
            edges = list(zip(corners, corners[1:] + corners[:1]))
            inside = any(x < px_ < x + w and y < py_ < y + h for px_, py_ in pts)
            gap = 0.0 if inside else min(segment_gap(a, b, c, d) for a, b in segs for c, d in edges)
            assert gap >= stroke, (label, "a trace against a part", round(gap, 2), p, (x, y, w, h))
    for p in lit:
        pts = polyline_points(p)
        segs = list(zip(pts, pts[1:]))
        for vx, vy, ring in vias:
            if any(abs(px_ - vx) < 0.02 and abs(py_ - vy) < 0.02 for px_, py_ in pts):
                continue
            gap = min(segment_gap(a, b, (vx, vy), (vx, vy)) for a, b in segs)
            assert gap >= ring + stroke, (label, "a trace through another's via", round(gap, 2), p, (vx, vy))


def polyline_points(d):
    """The vertices of an "M x,y V y L x,y H x" path, for the clearance check."""
    pts, x, y, cmd = [], 0.0, 0.0, None
    tokens = d.replace(",", " ").split()
    i = 0
    while i < len(tokens):
        t = tokens[i]
        if t in "MLVH":
            cmd = t
            i += 1
            continue
        if cmd in "ML":
            x, y = float(tokens[i]), float(tokens[i + 1])
            i += 2
        elif cmd == "V":
            y = float(t)
            i += 1
        elif cmd == "H":
            x = float(t)
            i += 1
        pts.append((x, y))
    return pts


def segment_gap(a, b, c, d):
    """The least distance between segments ab and cd."""
    def point_seg(p, q, r):
        qx, qy, rx, ry = q[0], q[1], r[0], r[1]
        dx, dy = rx - qx, ry - qy
        t = 0.0 if dx == dy == 0 else max(0.0, min(1.0, ((p[0] - qx) * dx + (p[1] - qy) * dy) / (dx * dx + dy * dy)))
        return math.hypot(p[0] - qx - t * dx, p[1] - qy - t * dy)

    def cross(o, p, q):
        return (p[0] - o[0]) * (q[1] - o[1]) - (p[1] - o[1]) * (q[0] - o[0])

    if (cross(a, b, c) * cross(a, b, d) < 0) and (cross(c, d, a) * cross(c, d, b) < 0):
        return 0.0
    return min(point_seg(a, c, d), point_seg(b, c, d), point_seg(c, a, b), point_seg(d, a, b))


def check_clearance(paths, stroke, label):
    """No two lit traces closer than half a stroke apart, edge to edge (1.5 strokes centre to centre): lanes on neighbouring fingers
    sit a finger pitch apart by design, and touching traces read as a short."""
    polys = [polyline_points(p) for p in paths]
    worst = None
    for i in range(len(polys)):
        for j in range(i + 1, len(polys)):
            for a, b in zip(polys[i], polys[i][1:]):
                for c, d in zip(polys[j], polys[j][1:]):
                    g = segment_gap(a, b, c, d)
                    if worst is None or g < worst[0]:
                        worst = (g, i, j)
    assert worst is None or worst[0] >= 1.5 * stroke, (label, "traces too close", worst, paths[worst[1]], paths[worst[2]])
    return worst[0] if worst else None


def jog(fx, y_from, rx, y_turn, top, c):
    """A trace from (fx, y_from) up to `top` at x `rx`: straight when they line up, else up to `y_turn`, a 45-degree chamfer, across,
    another chamfer, and up."""
    d = rx - fx
    if abs(d) < 0.01:
        return f"M {fx:.2f},{y_from:.2f} V {top:.2f}"
    s = 1 if d > 0 else -1
    c = min(c, abs(d) / 2)
    return (f"M {fx:.2f},{y_from:.2f} V {y_turn + c:.2f} L {fx + s * c:.2f},{y_turn:.2f} H {rx - s * c:.2f} L {rx:.2f},{y_turn - c:.2f} "
            f"V {top:.2f}")


def module(uid, x0, y0, w, h, art, edge, left_notch=True, omit=None):
    """The module in the box (x0, y0, w, h): the artwork (`art`, the real board) or the 256 px icon's (a 2:1 board, fewer of
    everything). `left_notch`: the left retention notch (off for a module behind another, where it would peek out). `omit`: an x
    range whose fingers aren't drawn (a module behind another, where one would show against the front's edge). Returns the SVG, the
    outline, the chips' lower edge (from y0), every lane's x, and the fingers' centres with their width."""
    u = h / 31.25  # a millimetre of a real DIMM's height
    r, rr = 0.9 * u, 0.5 * u
    finger_top = y0 + h - 3.6 * u
    notch_x, notch_w, notch_top = x0 + w * 0.462, 2.2 * u, y0 + h - 3.4 * u  # DDR5's key, left of centre
    side = 2.6 * u
    sy = y0 + h * 0.6
    left_side = (f"V {sy + side:.2f} a {side / 2:.2f} {side / 2:.2f} 0 0 0 0,{-side:.2f} " if left_notch else "")
    outline = (f"M {x0 + r:.2f},{y0:.2f} H {x0 + w - r:.2f} Q {x0 + w:.2f},{y0:.2f} {x0 + w:.2f},{y0 + r:.2f} V {sy:.2f} "
               f"a {side / 2:.2f} {side / 2:.2f} 0 0 0 0,{side:.2f} V {y0 + h - r:.2f} "
               f"Q {x0 + w:.2f},{y0 + h:.2f} {x0 + w - r:.2f},{y0 + h:.2f} "
               f"H {notch_x + notch_w:.2f} V {notch_top + rr:.2f} "
               f"Q {notch_x + notch_w:.2f},{notch_top:.2f} {notch_x + notch_w - rr:.2f},{notch_top:.2f} "
               f"H {notch_x + rr:.2f} Q {notch_x:.2f},{notch_top:.2f} {notch_x:.2f},{notch_top + rr:.2f} V {y0 + h:.2f} "
               f"H {x0 + r:.2f} Q {x0:.2f},{y0 + h:.2f} {x0:.2f},{y0 + h - r:.2f} {left_side}V {y0 + r:.2f} "
               f"Q {x0:.2f},{y0:.2f} {x0 + r:.2f},{y0:.2f} Z")
    out = [f'<path d="{outline}" fill="url(#pcb{uid})" stroke="{edge}" stroke-width="{max(0.8, 0.35 * u):.2f}"/>']

    # Fingers: `total` at an even pitch over the row, the notch a gap of its own (no finger dropped)
    total, margin = (144, 4.5 * u) if art else (40, 2.0 * u)
    gap = 0.3 * u
    pitch = (w - 2 * margin - notch_w - 2 * gap) / total
    n_left = round((notch_x - gap - (x0 + margin)) / pitch)
    fingers = [x0 + margin + (i + 0.5) * pitch for i in range(n_left)]
    fingers += [notch_x + notch_w + gap + (i + 0.5) * pitch for i in range(total - n_left)]

    # Chips: portrait packages, two groups about the centre
    cx = x0 + w / 2
    lw = max(0.32 * u, 0.9) if art else max(0.42 * u, 1.0)
    stroke = lw * 1.5
    if art:
        cw, ch, cgap, centre, cy = 9.5 * u, 11.0 * u, 3.5 * u, 18.0 * u, y0 + 7.4 * u
        per_side, lanes, bus, inset, pw = 4, 8, 4, 0.7 * u, 5.0 * u
    else:
        per_side, lanes, bus, centre, cgap, inset, pw = 4, 2, 2, 9.5 * u, 1.2 * u, 1.0 * u, 2.4 * u
        cw = (w / 2 - centre / 2 - 2.5 * u - cgap * (per_side - 1)) / per_side
        ch, cy = cw * 1.45, y0 + 8.6 * u
    xs = ([cx - centre / 2 - cw - i * (cw + cgap) for i in range(per_side)][::-1]
          + [cx + centre / 2 + i * (cw + cgap) for i in range(per_side)])

    # The command/address bus. Each line leaves a finger beside the key (line 0 the nearest) and rises beside its group (line 0 the
    # innermost, a finger pitch apart), jogging across where finger and riser don't line up: the line whose finger leads the jog turns
    # highest, so no jog crosses another. Along the top it runs on an inner layer (line 0 the highest), a via and a stub down at each
    # chip, and it ends at its last chip
    left_bus = [f for f in fingers if f < notch_x][::-1][:bus]
    right_bus = [f for f in fingers if f > notch_x][:bus]
    step = max(0.85 * u, pitch, 2.2 * stroke)
    bus_ys = [y0 + (2.0 + k * 0.85) * u for k in range(bus)]
    turn_low = finger_top - 1.8 * u  # the lowest turn high enough for a stitching via over the finger beside its chamfer
    risers, inner, stubs, vias = [], [], [], []
    rx_of = {}
    for sign, side_fingers, chips in ((-1, left_bus, xs[:per_side]), (1, right_bus, xs[per_side:])):
        # The artwork's bundles sit a little further in, so the smallest jog is a stroke, not a kink, and each clears its chip alike
        side_inset = 1.0 * u if art else inset
        rxs = [cx + sign * (centre / 2 - side_inset - (bus - 1 - k) * step) for k in range(bus)]
        dirs = {k: rxs[k] - side_fingers[k] for k in range(bus)}
        # Rank by finger in the direction the bundle jogs: the leading finger turns highest
        order = sorted(range(bus), key=lambda k: side_fingers[k] * (1 if sum(dirs.values()) > 0 else -1))
        for rank, k in enumerate(order):
            y_turn = turn_low - (bus - 1 - rank) * step
            risers.append(jog(side_fingers[k], finger_top + 0.4 * u, rxs[k], y_turn, bus_ys[k], 1.0 * u))
            rx_of[(sign, k)] = rxs[k]
            taps = [x + cw * (k + 1) / (bus + 1) for x in chips]
            last = min(taps) if sign < 0 else max(taps)
            inner.append(f"M {rxs[k]:.2f},{bus_ys[k]:.2f} H {last:.2f}")
            vias.append((rxs[k], bus_ys[k]))
            for t in taps:
                stubs.append(f"M {t:.2f},{bus_ys[k]:.2f} V {cy:.2f}")
                vias.append((t, bus_ys[k]))
    left_limit = min(left_bus + [rx_of[(-1, k)] for k in range(bus)]) - 0.8 * u
    right_limit = max(right_bus + [rx_of[(1, k)] for k in range(bus)]) + 0.8 * u

    # Data lanes: down from the chip, a 45-degree run, and down into the finger (straight down when the lane is nearly over it). Of
    # the lanes slanting one way, the one leading the slant ends its run highest and each after it a step lower, so each run is its
    # neighbour's parallel, a pitch and a step away (a pitch over the square root of two, otherwise)
    lanes_d, lane_xs = [], []
    y2_low = finger_top - 0.8 * u
    stagger = 0.22 * u
    # Each chip's lanes go to its slot (its width and half the gaps beside it), every other finger where the slot has room (a ground
    # between), centred under it. A side can't keep one parity across its chips: past the bus the right side has exactly as many odd
    # fingers as lanes, and the lanes would drift under the next chip (2026-10-23)
    chip_fingers = {}
    for i, x in enumerate(xs):
        slot = [f for f in fingers if x - cgap / 2 <= f <= x + cw + cgap / 2]
        slot = [f for f in slot if f < left_limit] if i < per_side else [f for f in slot if f > right_limit]
        stride = 2 if len(slot) >= 2 * lanes - 1 else 1
        chip_fingers[i] = slot[(len(slot) - stride * (lanes - 1) - 1) // 2::stride][:lanes]
        assert len(chip_fingers[i]) == lanes, (uid, i, len(slot))
    for i, x in enumerate(xs):
        picks = chip_fingers[i]
        sxs = [x + cw * (j + 0.5) / lanes for j in range(lanes)]
        # A lane whose jog would be narrower than about a trace runs straight down to its finger
        sxs = [fx if abs(fx - sx) < 0.6 * pitch else sx for sx, fx in zip(sxs, picks)]
        leftward = [j for j in range(lanes) if picks[j] < sxs[j]]
        rightward = [j for j in range(lanes) if picks[j] >= sxs[j]]
        rise = {j: (len(leftward) - 1 - n) for n, j in enumerate(leftward)} | {j: n for n, j in enumerate(rightward)}
        for j, fx in enumerate(picks):
            sx = sxs[j]
            d = abs(fx - sx)
            y2 = y2_low - stagger * rise[j]
            assert y2 - d > cy + ch + 0.3 * u, (uid, "no room for a lane's 45-degree run", i, j)
            lanes_d.append(f"M {sx:.2f},{cy + ch:.2f} V {y2 - d:.2f} L {fx:.2f},{y2:.2f} V {finger_top + 0.4 * u:.2f}")
            lane_xs += [sx, fx]
    lit = risers + stubs + lanes_d
    check_clearance(lit, stroke, uid)
    out.append(f'<path d="{" ".join(inner)}" fill="none" stroke="{LANE}" stroke-width="{lw * 1.1:.2f}" stroke-linecap="round" '
               f'opacity="0.5" filter="url(#dim{uid})"/>')
    # The artwork: a stitching via over each finger no signal takes (ground and power), on a short unlit stub, as a bare module shows
    stitch = []
    small = 0.3 * u
    if art:
        signal = set(left_bus + right_bus) | {f for i in chip_fingers for f in chip_fingers[i]}
        lit_segs = [seg for path in lit for pts in [polyline_points(path)] for seg in zip(pts, pts[1:])]
        # Each where it clears every lit trace by a stroke (beside a bus riser's jog, there's no room)
        vy = finger_top - 0.45 * u
        stitch = [(f, vy) for f in fingers
                  if f not in signal and min(segment_gap(a, b, (f, vy), (f, vy)) for a, b in lit_segs) >= small + stroke]
        out.append(f'<path d="{" ".join(f"M {f:.2f},{finger_top + 0.4 * u:.2f} V {vy:.2f}" for f, vy in stitch)}" fill="none" '
                   f'stroke="#2c3037" stroke-width="{lw:.2f}" stroke-linecap="round"/>')
    d = " ".join(lit)
    out.append(f'<path d="{d}" fill="none" stroke="{LANE}" stroke-width="{stroke:.2f}" stroke-linecap="round" stroke-linejoin="round" '
               f'filter="url(#glow{uid})"/>')
    out.append(f'<path d="{d}" fill="none" stroke="{CORE}" stroke-width="{lw * 0.45:.2f}" stroke-linecap="round" stroke-linejoin="round"/>')
    # Vias: a tinned ring with its drill
    ring = max(0.42 * u, 0.7)
    out.append(f'<g fill="{VIA}">' + "".join(f'<circle cx="{vx:.2f}" cy="{vy:.2f}" r="{ring:.2f}"/>' for vx, vy in vias) + "</g>")
    out.append(f'<g fill="{PCB[0]}">' + "".join(f'<circle cx="{vx:.2f}" cy="{vy:.2f}" r="{ring * 0.43:.2f}"/>' for vx, vy in vias) + "</g>")
    if stitch:
        out.append(f'<g fill="#7d8087">' + "".join(f'<circle cx="{vx:.2f}" cy="{vy:.2f}" r="{small:.2f}"/>' for vx, vy in stitch) + "</g>")
        out.append(f'<g fill="{PCB[0]}">' + "".join(f'<circle cx="{vx:.2f}" cy="{vy:.2f}" r="{small * 0.43:.2f}"/>' for vx, vy in stitch)
                   + "</g>")

    # Gold fingers
    fw = pitch * 0.72
    shown = [f for f in fingers if omit is None or not (f + fw / 2 > omit[0] and f - fw / 2 < omit[1])]
    out.append(f'<g fill="url(#gold{uid})">' + "".join(
        f'<rect x="{f - fw / 2:.2f}" y="{finger_top:.2f}" width="{fw:.2f}" height="{y0 + h - finger_top - 0.2 * u:.2f}" '
        f'rx="{min(fw * 0.25, 0.2 * u):.2f}"/>'
        for f in shown) + "</g>")

    # The chips, their markings where they're large enough to read, then (the artwork) a column of decoupling caps in each gap. Every
    # part's box goes in `parts`, for the check that no bus riser touches one
    parts = []
    for i, x in enumerate(xs):
        out.append(f'<rect x="{x:.2f}" y="{cy:.2f}" width="{cw:.2f}" height="{ch:.2f}" rx="{0.4 * u:.2f}" fill="url(#chip{uid})" '
                   f'stroke="#55585f" stroke-width="{max(0.12 * u, 0.8):.2f}"/>')
        parts.append((x, cy, cw, ch))
        if art or ch >= 24:
            out.append(chip_marking(x, cy, cw, ch, u, i == len(xs) - 1))
    if art:
        for i in range(len(xs) - 1):
            if i != per_side - 1:
                gx = xs[i] + cw + (cgap - 1.0 * u) / 2
                for fy in (0.12, 0.36, 0.6, 0.84):
                    out.append(mlcc(gx, cy + ch * fy, 1.0 * u, 0.5 * u, u))
                    parts.append((gx, cy + ch * fy, 1.0 * u, 0.5 * u))

    # Between the groups: the PMIC; its three inductors (charcoal, tinned ends, the third smaller), two output caps under each, a
    # gap between them; the bulk caps standing beside it; and the SPD hub
    py = y0 + 7.6 * u if art else cy + 1.0 * u
    out.append(f'<rect x="{cx - pw / 2:.1f}" y="{py:.1f}" width="{pw:.1f}" height="{pw:.1f}" rx="{0.3 * u:.2f}" fill="url(#pmic{uid})" '
               f'stroke="#55585f" stroke-width="{max(0.12 * u, 0.8):.2f}"/>')
    parts.append((cx - pw / 2, py, pw, pw))
    if not art:
        # Two inductor blocks under the PMIC, large enough at 256 px to read as parts (three tiny ones read as a dash)
        iw, ih = 1.1 * u, 1.0 * u
        ix = cx - iw - 0.15 * u
        for _ in range(2):
            iy = py + pw + 0.7 * u
            out.append(f'<rect x="{ix:.2f}" y="{iy:.2f}" width="{iw:.2f}" height="{ih:.2f}" rx="{0.15 * u:.2f}" fill="url(#ind{uid})"/>')
            parts.append((ix, iy, iw, ih))
            ix += iw + 0.3 * u
    if art:
        out.append(f'<circle cx="{cx - pw / 2 + 0.8 * u:.1f}" cy="{py + 0.8 * u:.1f}" r="{0.28 * u:.2f}" fill="{MARK}"/>')
        sizes = [(2.7 * u, 2.5 * u), (2.7 * u, 2.5 * u), (2.2 * u, 2.0 * u)]
        ix = cx - (sum(s[0] for s in sizes) + 1.0 * u) / 2
        iy = py + pw + 1.0 * u
        for iw, ih in sizes:
            out.append(f'<rect x="{ix:.1f}" y="{iy:.1f}" width="{iw:.1f}" height="{ih:.1f}" rx="{0.3 * u:.2f}" fill="url(#ind{uid})"/>'
                       f'<rect x="{ix:.1f}" y="{iy:.1f}" width="{0.35 * u:.2f}" height="{ih:.1f}" fill="#8a8d93"/>'
                       f'<rect x="{ix + iw - 0.35 * u:.1f}" y="{iy:.1f}" width="{0.35 * u:.2f}" height="{ih:.1f}" fill="#8a8d93"/>'
                       f'<rect x="{ix + iw * 0.35:.1f}" y="{iy + ih * 0.42:.1f}" width="{iw * 0.3:.1f}" height="{0.35 * u:.2f}" '
                       f'rx="{0.15 * u:.2f}" fill="{MARK}"/>')
            parts.append((ix, iy, iw, ih))
            capw = min(1.2 * u, (iw - 0.3 * u) / 2)
            for cx_ in (ix, ix + iw - capw):
                out.append(mlcc(cx_, iy + ih + 0.4 * u, capw, 0.6 * u, u))
                parts.append((cx_, iy + ih + 0.4 * u, capw, 0.6 * u))
            ix += iw + 0.5 * u
        for bx in (cx - pw / 2 - 1.2 * u, cx + pw / 2 + 0.4 * u):
            for by in (py + 0.3 * u, py + 2.6 * u):
                out.append(mlcc(bx, by, 0.8 * u, 1.6 * u, u))
                parts.append((bx, by, 0.8 * u, 1.6 * u))
        hy = iy + 2.5 * u + 1.6 * u
        out.append(f'<rect x="{cx - 1.0 * u:.1f}" y="{hy:.1f}" width="{2.0 * u:.1f}" height="{2.0 * u:.1f}" rx="{0.2 * u:.2f}" '
                   f'fill="url(#pmic{uid})" stroke="#55585f" stroke-width="{0.1 * u:.2f}"/>')
        parts.append((cx - 1.0 * u, hy, 2.0 * u, 2.0 * u))
    check_parts(risers, lit, [(vx, vy, ring) for vx, vy in vias] + [(vx, vy, small) for vx, vy in stitch], parts, stroke, uid)
    return "".join(out), outline, cy + ch - y0, lane_xs, (fingers, fw)


ART_W, ART_H = 1333.5, 312.5


def art_svg():
    body, *_ = module("a", 6, 6, ART_W, ART_H, True, ART_EDGE)
    return (f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {ART_W + 12} {ART_H + 12}" width="{ART_W + 12:.0f}" '
            f'height="{ART_H + 12:.0f}">'
            f'{defs("a", 4.5)}{body}</svg>')


def vector_icon(px):
    """The pair as vector: the one behind shaded through its own outline, without the left notch the front would show a sliver of;
    the front's top meeting the back's chips' lower edge, so no stray row of the back's lanes shows; the back placed so none of its
    lanes runs within 3 px of the front's right edge, and without the finger that would show against it; the two centred in the
    square."""
    w = px * 0.84
    h = w / 2.0
    fx0 = px * 0.01
    _, _, chips_end, _, _ = module("t", 0, 0, w, h, False, ICON_EDGE)
    off = chips_end
    by = (px - h - off) / 2
    edge = fx0 + w
    for nudge in [i * 0.5 for i in range(0, 25)]:
        bx = px - w - px * 0.01 - nudge
        _, _, _, lane_xs, (finger_xs, fw) = module("t", bx, by, w, h, False, ICON_EDGE, left_notch=False)
        # No back lane within 3 px of the front's right edge (a back finger that would show within 2.5 px of the edge isn't drawn)
        if all(abs(x - edge) >= 3 for x in lane_xs):
            break
    else:
        raise AssertionError("no place for the back module clear of the front's edge")
    shift = (px - (bx + w) - fx0) / 2
    bx, fx0 = bx + shift, fx0 + shift
    back, outline, *_ = module(f"b{px}", bx, by, w, h, False, ICON_EDGE, left_notch=False, omit=(fx0 + w, fx0 + w + 0.6 + 2.5))
    front, *_ = module(f"f{px}", fx0, by + off, w, h, False, ICON_EDGE)
    glow = (h / 31.25) * 0.35
    shade = f'<path d="{outline}" fill="#000" opacity="0.4"/>'
    return (f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {px} {px}" width="{px}" height="{px}">{defs(f"b{px}", glow)}'
            f'{defs(f"f{px}", glow)}{back}{shade}{front}</svg>')


# The pixel grid. (board, edge, chip, marking, lane, gold, gold low, PMIC, a lone mark, TMR): the front module, and the one behind
# darker
FRONT = ("#252a32", "#6a7079", "#08090b", "#454a52", "#ff2a2a", "#f0c65a", "#b38a2c", "#5a606b", "#2a2e35", "#9da1a9")
BACK = ("#20242b", "#50555f", "#060708", "#33373e", "#c42222", "#a8873a", "#7a6022", "#40454f", "#1e2126", "#6f7279")


# "TMR" in a 3 x 5 pixel font, each glyph's rows top to bottom, for the last chip at the sizes it fits (turned to read upward)
GLYPHS = {"T": ["###", ".#.", ".#.", ".#.", ".#."], "M": ["#...#", "##.##", "#.#.#", "#...#", "#...#"],
          "R": ["##.", "#.#", "##.", "#.#", "#.#"]}


def text_height(text):
    """The rows `text` takes turned: each glyph's width, a pixel apart."""
    return sum(len(GLYPHS[ch][0]) for ch in text) + len(text) - 1


def turned_text(x0, y0, text):
    """`text` turned to read upward from (x0, y0 + its height): each glyph rotated a quarter turn anticlockwise (5 wide, as tall as
    the glyph is wide), stacked from the bottom a pixel apart. Returns the pixels."""
    pixels = []
    bottom = y0 + text_height(text) - 1
    for ch in text:
        glyph = GLYPHS[ch]
        for r, row in enumerate(glyph):
            for c, on in enumerate(row):
                if on == "#":
                    # (c, r) in the glyph turned anticlockwise: x is r, y counts up from the glyph's bottom by c
                    pixels.append((x0 + r, bottom - c))
        bottom -= len(glyph[0]) + 1
    return pixels


def pixel_module(px0, py0, w, h, per_side, colours, comb, stubs, centre, skip_fingers=(), tmr=False):
    """A module `w` x `h` pixels at (px0, py0), its parts on whole pixels: the edge, down to the bottom; a bus dash over each group of
    chips, a board row below it (and a stub down into each chip when `stubs`); the chips, with vertical marking lines from their lower
    edge part way up, and on the last, when `tmr` and it fits, TMR in place of its lines; the lanes, mirrored about the centre; the gold
    fingers, combed when `comb`, the comb mirrored with the lanes so every lane ends on a finger; the PMIC in the centre gap when it is 4
    pixels wide. Each part of a chip has a column of its own, and the chips sit a pitch of even width apart when combed, so all lanes
    share the comb's parity. Returns the rects and the layout (the key's columns, to cut, among it)."""
    board, edge, chip, mark, lane, gold, gold_low, pmic, faint, etch = colours
    gap_rows = 2 if h >= 20 else 1
    lane_rows = 3 if h >= 20 else 2 if h >= 9 else 1
    finger_rows = 3 if h >= 20 else 2 if h >= 9 else 1
    chip_rows = h - 2 - gap_rows - lane_rows - finger_rows
    for g in (1, 2, 3):
        cw = (w - 4 - centre - 2 * (per_side - 1) * g) // (2 * per_side)
        if not comb or (cw + g) % 2 == 0:
            break
    else:
        raise AssertionError(("no even chip pitch", w, per_side))
    # A chip no taller than a package is: the rows to spare go to the bus stubs and the lanes
    tallest = text_height("TMR") + 2 if cw == 7 else max(3, math.ceil(cw * 1.8))
    if chip_rows > tallest:
        spare = chip_rows - tallest
        chip_rows = tallest
        gap_rows += spare // 2
        lane_rows += spare - spare // 2
    block = 2 * (per_side * cw + (per_side - 1) * g) + centre
    assert (w - block) % 2 == 0, (w, per_side, block)
    rects = []

    def rect(x, y, rw, rh, fill):
        rects.append(f'<rect x="{x}" y="{y}" width="{rw}" height="{rh}" fill="{fill}"/>')

    body = h - finger_rows
    rect(px0, py0, w, body, edge)
    rect(px0 + 1, py0 + 1, w - 2, body - 1, board)
    rect(px0, py0 + body, w, finger_rows, board)
    # The sides run on down the finger rows, as the board's edge does (the end columns are never fingers)
    for ex in (px0, px0 + w - 1):
        rect(ex, py0 + body, 1, finger_rows, edge)
    mirror = lambda x: 2 * px0 + w - 1 - x
    left = [px0 + (w - block) // 2 + i * (cw + g) for i in range(per_side)]
    right = sorted(mirror(x) - cw + 1 for x in left)
    gap_x = left[-1] + cw  # the centre gap's first column
    bus_y, chips_y = py0 + 1, py0 + 2 + gap_rows
    lanes_y, fingers_y = chips_y + chip_rows, py0 + body
    # A left chip's columns (a right chip's are their mirror): lanes on even columns, at most 4, the last clear of the
    # inner edge; marks on the odd columns between, at most 3; the stub on the inner edge
    if cw >= 3:
        lane_cols = list(range(0, cw - 2, 2))[:4]
        mark_cols = list(range(1, cw - 1, 2))[:3]
        stub_col = cw - 1
    else:
        lane_cols, mark_cols, stub_col = [cw - 1], [], cw - 1
    for group in (left, right):
        rect(group[0], bus_y, group[-1] + cw - group[0], 1, lane)
    left_lanes = [x + c for x in left for c in lane_cols]
    lanes = left_lanes + [mirror(x) for x in left_lanes]
    for lx in lanes:
        rect(lx, lanes_y, 1, lane_rows, lane)
    stub_xs = []
    last = right[-1]
    tmr_here = tmr and cw >= 7 and chip_rows >= text_height("TMR") + 2
    for x in left + right:
        rect(x, chips_y, cw, chip_rows, chip)
        sx = x + stub_col if x in left else x + (cw - 1 - stub_col)
        stub_xs.append(sx)
        if stubs:
            rect(sx, bus_y + 1, 1, gap_rows, lane)
        if x == last and tmr_here:
            for tx, ty in turned_text(x + (cw - 5) // 2, chips_y + 1, "TMR"):
                rect(tx, ty, 1, 1, etch)
            continue
        # Turned text: each line from a row above the chip's lower edge, part way up; a chip with one line has it faint, which a
        # full-tone line in a narrow chip turns into a ring (a 3-pixel chip's over its lower half only). The heights run the same
        # left to right on every chip, as one face's packages carry one marking; only a right chip's columns mirror
        bottom = chips_y + chip_rows - 1
        tone = mark if len(mark_cols) > 1 else faint
        for k, c in enumerate(mark_cols):
            j = k if x in left else len(mark_cols) - 1 - k
            run = chip_rows - 1 - chip_rows // 2 if cw == 3 else max(1, round(chip_rows * (0.6, 0.45, 0.55)[j % 3]))
            rect(x + c if x in left else x + cw - 1 - c, bottom - run, 1, run, tone)
    for x, sx in zip(left + right, stub_xs):
        cols = {x + c if x in left else x + cw - 1 - c for c in mark_cols}
        assert sx not in cols and not (cols & set(lanes)), ("a chip's parts share a column", x)
    if centre >= 4:
        rect(gap_x + 1, chips_y + max(0, (chip_rows - 2) // 2), 2, 2, pmic)
    # Fingers: the left half on the lanes' parity, the right half its mirror; the key a gap left of centre, clear of the lanes
    half = px0 + w // 2
    left_cols = [c for c in range(px0 + 1, half) if not comb or c % 2 == left_lanes[0] % 2]
    cols = set(left_cols) | {mirror(c) for c in left_cols}
    notch = {gap_x - 1, gap_x, gap_x + 1} if comb else {gap_x}
    assert not notch & set(lanes), ("a lane in the key", notch, lanes)
    cols -= notch
    assert set(lanes) <= cols, ("a lane off the fingers", sorted(set(lanes) - cols))
    for c in sorted(cols - set(skip_fingers)):
        rect(c, fingers_y, 1, 1, gold)
        if finger_rows > 1:
            rect(c, fingers_y + 1, 1, finger_rows - 1, gold_low)
    side_y = py0 + round(0.6 * h)
    return "".join(rects), dict(notch=sorted(notch), fingers_y=fingers_y, finger_rows=finger_rows, chips_end=chips_y + chip_rows - py0,
                                lanes=lanes, fingers=sorted(cols), side_y=side_y, edge=edge, lanes_y=lanes_y, lane_rows=lane_rows,
                                tmr=tmr_here)


# (px, module w, module h, chips a side, combed fingers, stubs, centre gap): 16 one module, the rest the pair. Sizes Windows asks
# for at 100-200% scaling (taskbar 24/30/36/48, Explorer 48/60/72/80/96, 16/20/32/40/64 elsewhere), each drawn, none scaled
PIXEL = [(16, 16, 10, 2, False, False, 2), (20, 18, 10, 2, False, False, 2), (24, 20, 12, 2, False, False, 2),
         (30, 26, 12, 2, True, False, 2), (32, 28, 13, 2, True, False, 2), (36, 30, 14, 2, True, False, 2),
         (40, 34, 16, 2, True, False, 2), (48, 40, 20, 4, True, True, 4), (60, 52, 25, 4, True, True, 4), (64, 54, 27, 4, True, True, 4),
         (72, 60, 30, 4, True, True, 4), (80, 68, 34, 4, True, True, 4), (96, 76, 38, 4, True, True, 4)]


def pixel_icon(px, w, h, per_side, comb, stubs, centre):
    """The pair on the grid: the front's top meeting the back's chips' lower edge; the back right-aligned, nudged left until two
    columns of board separate its lanes from the front's right edge, and without fingers there if none of its lanes shows above them.
    The key cut through the front's fingers, and from 48 px the retention notches through the edges, taller as the module is (a
    real notch is 2.6 mm of 31.25), outlined inside by the edge colour except beside a lane."""
    holes = []

    def cut_ends(rows, x0, both):
        if h < 20:
            return ""
        tall = 3 if h >= 30 else 2 if h >= 24 else 1
        top = rows["side_y"] - (tall - 1) // 2
        ends = [(x0, x0 + 1, x0 + 2), (x0 + w - 1, x0 + w - 2, x0 + w - 3)] if both else [(x0 + w - 1, x0 + w - 2, x0 + w - 3)]
        lane_rows = range(rows["lanes_y"], rows["lanes_y"] + rows["lane_rows"])
        paint = []
        for edge_x, inner_x, beyond in ends:
            holes.append((edge_x, top, 1, tall))
            beside_lane = beyond in rows["lanes"] and any(top <= y < top + tall for y in lane_rows)
            if not beside_lane:
                paint.append(f'<rect x="{inner_x}" y="{top}" width="1" height="{tall}" fill="{rows["edge"]}"/>')
        return "".join(paint)

    if px == 16:
        y = (px - h) // 2
        body, rows = pixel_module(0, y, w, h, per_side, FRONT, comb, stubs, centre)
        holes += [(x, rows["fingers_y"], 1, rows["finger_rows"]) for x in rows["notch"]]
        parts = [body]
    else:
        _, probe = pixel_module(0, 0, w, h, per_side, BACK, comb, stubs, centre)
        off = probe["chips_end"]
        by = (px - h - off) // 2
        for nudge in range(5):
            bx = px - w - nudge
            _, rows_b = pixel_module(bx, by, w, h, per_side, BACK, comb, stubs, centre)
            if not {w, w + 1} & set(rows_b["lanes"]):
                break
        else:
            raise AssertionError(("no place for the back module", px))
        # Centre the pair, the empty columns split each side
        dx = (px - (bx + w)) // 2
        bx += dx
        _, rows_b = pixel_module(bx, by, w, h, per_side, BACK, comb, stubs, centre)
        beyond = dx + w  # the first column right of the front
        visible = [c for c in rows_b["fingers"] if c >= beyond]
        skip = visible if not any(c >= beyond for c in rows_b["lanes"]) else [c for c in visible if c == beyond]
        back, rows_b = pixel_module(bx, by, w, h, per_side, BACK, comb, stubs, centre, skip_fingers=skip, tmr=True)
        front, rows_f = pixel_module(dx, by + off, w, h, per_side, FRONT, comb, stubs, centre, tmr=True)
        holes += [(x, rows_f["fingers_y"], 1, rows_f["finger_rows"]) for x in rows_f["notch"]]
        parts = [back, cut_ends(rows_b, bx, False), front, cut_ends(rows_f, dx, True)]
    mask = "".join(f'<rect x="{x}" y="{y}" width="{rw}" height="{rh}" fill="#000"/>' for x, y, rw, rh in holes)
    return (f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {px} {px}" width="{px}" height="{px}" shape-rendering="crispEdges">'
            f'<defs><mask id="m{px}"><rect width="{px}" height="{px}" fill="#fff"/>{mask}</mask></defs>'
            f'<g mask="url(#m{px})">{"".join(parts)}</g></svg>')


SIZES = [256] + [p[0] for p in PIXEL][::-1]


def main(out):
    os.makedirs(out, exist_ok=True)
    open(os.path.join(out, "dimm-burn.svg"), "w", encoding="utf-8").write(art_svg())
    open(os.path.join(out, "icon256.svg"), "w", encoding="utf-8").write(vector_icon(256))
    for spec in PIXEL:
        open(os.path.join(out, f"icon{spec[0]}.svg"), "w", encoding="utf-8").write(pixel_icon(*spec))
    icons = "".join(f'<img src="icon{p}.svg" width="{p}" height="{p}">' for p in SIZES)
    page = ("<!doctype html><html><head><style>body{margin:0;padding:14px;background:#2b2b2b;color:#ddd;font:14px Segoe UI}"
            ".name{font-weight:600;margin:10px 0 4px}.bg{display:flex;align-items:center;gap:12px;padding:8px;flex-wrap:wrap}"
            ".dark{background:#1c1c1c}.light{background:#f3f3f3}</style></head><body>"
            '<div class="name">the artwork</div><img src="dimm-burn.svg" width="1300">'
            f'<div class="name">the icon: {", ".join(str(p) for p in SIZES)} px, on dark, then light</div>'
            f'<div class="bg dark">{icons}</div><div class="bg light">{icons}</div></body></html>')
    open(os.path.join(out, "preview.html"), "w", encoding="utf-8").write(page)
    cells = [(p, bg) for p in SIZES[1:] for bg in ("#1c1c1c", "#f3f3f3")]
    script = "".join(f"draw('icon{p}.svg',{p},'{bg}','{p} px');" for p, bg in cells)
    pixels = ("<!doctype html><html><head><style>body{margin:0;padding:10px;background:#2b2b2b;color:#ddd;font:13px Segoe UI}"
              "#out{display:flex;flex-wrap:wrap;gap:10px;width:1580px}figure{margin:0}figcaption{margin-top:2px}</style></head>"
              "<body><div id='out'></div><script>"
              "function draw(src,p,bg,label){const f=document.createElement('figure');document.getElementById('out').appendChild(f);"
              "const img=new Image();img.onload=()=>{const s=document.createElement('canvas');s.width=p;s.height=p;"
              "const c=s.getContext('2d');c.fillStyle=bg;c.fillRect(0,0,p,p);c.drawImage(img,0,0,p,p);"
              "const m=document.createElement('canvas');"
              "const z=Math.max(3,Math.floor(300/p));m.width=p*z;m.height=p*z;const k=m.getContext('2d');k.imageSmoothingEnabled=false;"
              "k.drawImage(s,0,0,p*z,p*z);f.appendChild(m);"
              "const t=document.createElement('figcaption');t.textContent=label;f.appendChild(t);};"
              "img.src=src;}" + script + "</script></body></html>")
    open(os.path.join(out, "pixels.html"), "w", encoding="utf-8").write(pixels)


if __name__ == "__main__":
    if len(sys.argv) != 2:
        sys.exit("usage: dimm.py DIR")
    main(sys.argv[1])
