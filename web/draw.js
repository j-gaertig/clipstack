// draw.js – vector ink engine for the iPad web app.
//
// An endless whiteboard. The document is a list of items in image pixel
// Coordinates, drawn in order:
//   strokes – pen, highlighter, eraser: constant width along a B-spline
//   groups  – what the lasso moved; drawn on a layer of their own when they
//             carry erasers, so those only affect the group
// Items are never changed in place: every edit replaces the list, so undo
// And redo are plain snapshots and geometry caches can hang off the items.
// Canvases: bg (paper + image), ink (all items), live + tail (the stroke in
// Progress, the lasso, the selection being dragged, the hover cursor).

const STREAMLINE = { pen: 0.1, other: 0.45 };
const HOLD_TO_STRAIGHTEN_MS = 450;
const HL_ALPHA = 0.35;
const PALM_MS = 300;        // touches starting this soon after the pen are palms
const MAX_UNDO = 300;
const NO_BOX = { x0: Infinity, y0: Infinity, x1: -Infinity, y1: -Infinity };
const union = (a, b) => ({ x0: Math.min(a.x0, b.x0), y0: Math.min(a.y0, b.y0), x1: Math.max(a.x1, b.x1), y1: Math.max(a.y1, b.y1) });
const overlaps = (a, b) => a.x0 <= b.x1 && b.x0 <= a.x1 && a.y0 <= b.y1 && b.y0 <= a.y1;
const grow = (b, d) => ({ x0: b.x0 - d, y0: b.y0 - d, x1: b.x1 + d, y1: b.y1 + d });
const isEmpty = (b) => !(b.x1 >= b.x0);
function ptsBox(pts, from = 0) {
  let x0 = Infinity, y0 = Infinity, x1 = -Infinity, y1 = -Infinity;
  for (let i = Math.max(0, from); i < pts.length; i++) {
    const [x, y] = pts[i];
    if (x < x0) x0 = x; if (x > x1) x1 = x; if (y < y0) y0 = y; if (y > y1) y1 = y;
  }
  return { x0, y0, x1, y1 };
}
let revSeq = 0;
const newRev = () => Date.now() * 1000 + (revSeq++ % 1000);
const coalesced = (e) => { const l = e.getCoalescedEvents?.(); return l && l.length ? l : [e]; };

// ------------------------------------------------------------- stroke shape
// Strokes have a constant width and follow a uniform cubic B-spline with
// The samples as control points (clamped at both ends): round and smooth
// Even when the iPad delivers few, far apart points on fast strokes. Each
// Piece depends on only four neighbouring samples, so a stroke in progress
// Is drawn piece by piece instead of all over again on every move.
// Pieces of a stroke with n samples: 0..n; pieces 0..n-2 are final.
function piece(pts, j) {
  const n = pts.length, q = (k) => pts[Math.min(n - 1, Math.max(0, k - 2))];
  const a = q(j), b = q(j + 1), c = q(j + 2), d = q(j + 3);
  return [
    (a[0] + 4 * b[0] + c[0]) / 6, (a[1] + 4 * b[1] + c[1]) / 6,
    (2 * b[0] + c[0]) / 3, (2 * b[1] + c[1]) / 3,
    (b[0] + 2 * c[0]) / 3, (b[1] + 2 * c[1]) / 3,
    (b[0] + 4 * c[0] + d[0]) / 6, (b[1] + 4 * c[1] + d[1]) / 6,
  ];
}
// Path of pieces `from..to` (inclusive).
function piecesPath(pts, from, to, path = new Path2D()) {
  const f = piece(pts, from);
  path.moveTo(f[0], f[1]);
  for (let j = from; j <= to; j++) { const c = piece(pts, j); path.bezierCurveTo(c[2], c[3], c[4], c[5], c[6], c[7]); }
  return path;
}
// Straight segments through all points: lines (2 points) and the shapes
// Made by holding the pen (closed polygons, ellipses as fine polygons).
function straightPath(pts, closed = false) {
  const p = new Path2D(), a = pts[0], b = pts[pts.length - 1];
  p.moveTo(a[0], a[1]);
  if (pts.length < 2 || (pts.length === 2 && a[0] === b[0] && a[1] === b[1])) { p.lineTo(a[0] + 0.01, a[1]); return p; }
  for (let i = 1; i < pts.length; i++) p.lineTo(pts[i][0], pts[i][1]);
  if (closed) p.closePath();
  return p;
}

// ------------------------------------------------------- shape recognition
// A closed stroke held still at the end becomes a clean triangle, a
// Quadrilateral (snapped to a rectangle or square when it is nearly one) or
// An ellipse (snapped to a circle when nearly round). Returns the vertices
// Or null when the stroke isn't closed or doesn't look like any of them.
const dist = (a, b) => Math.hypot(a[0] - b[0], a[1] - b[1]);
function resample(pts, n) {
  const loop = pts.concat([pts[0]]);
  let len = 0;
  for (let i = 1; i < loop.length; i++) len += dist(loop[i], loop[i - 1]);
  const step = len / n, out = [loop[0]];
  let acc = 0;
  for (let i = 1; i < loop.length && out.length < n; i++) {
    let a = loop[i - 1];
    const b = loop[i];
    let d = dist(a, b);
    while (acc + d >= step && out.length < n) {
      const t = (step - acc) / d;
      a = [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t];
      out.push(a);
      d = dist(a, b);
      acc = 0;
    }
    acc += d;
  }
  return out;
}
// Douglas-Peucker on pts[i..j] (open), returns the kept indices between.
function simplify(pts, i, j, eps, out) {
  const a = pts[i], b = pts[j], l = dist(a, b) || 1e-9;
  let best = -1, far = eps;
  for (let k = i + 1; k < j; k++) {
    const d = Math.abs((b[0] - a[0]) * (a[1] - pts[k][1]) - (a[0] - pts[k][0]) * (b[1] - a[1])) / l;
    if (d > far) { far = d; best = k; }
  }
  if (best < 0) return;
  simplify(pts, i, best, eps, out);
  out.push(pts[best]);
  simplify(pts, best, j, eps, out);
}
function cornerAngle(p, a, b) {
  const ux = a[0] - p[0], uy = a[1] - p[1], vx = b[0] - p[0], vy = b[1] - p[1];
  return Math.acos(Math.max(-1, Math.min(1, (ux * vx + uy * vy) / (Math.hypot(ux, uy) * Math.hypot(vx, vy) || 1))));
}
const DEG = Math.PI / 180;
const snapAngle = (t, tol) => { const q = Math.round(t / (Math.PI / 2)) * (Math.PI / 2); return Math.abs(t - q) < tol ? q : t; };
function ellipsePoints(cx, cy, a, b, t) {
  const out = [], c = Math.cos(t), s = Math.sin(t), n = 96;
  for (let i = 0; i < n; i++) {
    const u = Math.cos((i / n) * 2 * Math.PI) * a, v = Math.sin((i / n) * 2 * Math.PI) * b;
    out.push([cx + u * c - v * s, cy + u * s + v * c, 0.5]);
  }
  return out;
}
function recognize(raw) {
  if (raw.length < 8) return null;
  const box = ptsBox(raw), size = Math.max(box.x1 - box.x0, box.y1 - box.y0);
  if (dist(raw[0], raw[raw.length - 1]) > 0.25 * size) return null;
  const pts = resample(raw, 96), n = pts.length;
  // Ellipse through the principal axes
  let cx = 0, cy = 0;
  for (const p of pts) { cx += p[0] / n; cy += p[1] / n; }
  let xx = 0, yy = 0, xy = 0;
  for (const [x, y] of pts) { xx += (x - cx) ** 2; yy += (y - cy) ** 2; xy += (x - cx) * (y - cy); }
  const th = 0.5 * Math.atan2(2 * xy, xx - yy), c = Math.cos(th), s = Math.sin(th);
  let u0 = Infinity, u1 = -Infinity, v0 = Infinity, v1 = -Infinity;
  for (const [x, y] of pts) {
    const u = (x - cx) * c + (y - cy) * s, v = -(x - cx) * s + (y - cy) * c;
    u0 = Math.min(u0, u); u1 = Math.max(u1, u); v0 = Math.min(v0, v); v1 = Math.max(v1, v);
  }
  // A stroke going back and forth on itself has no inside
  if (v1 - v0 < 0.1 * (u1 - u0)) return null;
  const ea = (u1 - u0) / 2, eb = (v1 - v0) / 2, um = (u0 + u1) / 2, vm = (v0 + v1) / 2;
  const ecx = cx + um * c - vm * s, ecy = cy + um * s + vm * c;
  let err = 0;
  for (const [x, y] of pts) {
    const u = (x - ecx) * c + (y - ecy) * s, v = -(x - ecx) * s + (y - ecy) * c;
    err += Math.abs(Math.hypot(u / (ea || 1), v / (eb || 1)) - 1) / n;
  }
  // Corners of the closed polygon
  let i0 = 0;
  for (let i = 1; i < n; i++) if (dist(pts[i], [cx, cy]) > dist(pts[i0], [cx, cy])) i0 = i;
  const ring = pts.slice(i0).concat(pts.slice(0, i0));
  let i1 = 1;
  for (let i = 2; i < n; i++) if (dist(ring[i], ring[0]) > dist(ring[i1], ring[0])) i1 = i;
  const eps = 0.06 * Math.hypot(box.x1 - box.x0, box.y1 - box.y0), loop = ring.concat([ring[0]]);
  let poly = [ring[0]];
  simplify(loop, 0, i1, eps, poly);
  poly.push(ring[i1]);
  simplify(loop, i1, n, eps, poly);
  // Drop nearly straight corners and corners right next to each other
  for (let changed = true; changed && poly.length > 3;) {
    changed = false;
    let worst = -1, wa = 150 * DEG;
    for (let i = 0; i < poly.length; i++) {
      const a = poly[(i + poly.length - 1) % poly.length], b = poly[(i + 1) % poly.length];
      const ang = dist(poly[i], b) < 0.12 * size ? Math.PI : cornerAngle(poly[i], a, b);
      if (ang > wa) { wa = ang; worst = i; }
    }
    if (worst >= 0) { poly.splice(worst, 1); changed = true; }
  }
  const k = poly.length;
  if (err < 0.07 || (k > 4 && err < 0.16)) {
    let a = ea, b = eb, t = snapAngle(th, 8 * DEG);
    if (Math.abs(a - b) < 0.12 * Math.max(a, b)) a = b = (a + b) / 2;
    return { closed: true, points: ellipsePoints(ecx, ecy, a, b, t) };
  }
  if (k === 3) return { closed: true, points: poly.map((p) => [p[0], p[1], 0.5]) };
  if (k !== 4) return null;
  // Nearly right angles everywhere: a rectangle, nearly equal sides: a square
  const right = poly.every((p, i) => Math.abs(cornerAngle(p, poly[(i + 3) % 4], poly[(i + 1) % 4]) - Math.PI / 2) < 14 * DEG);
  if (!right) return { closed: true, points: poly.map((p) => [p[0], p[1], 0.5]) };
  let sx = 0, sy = 0;
  for (let i = 0; i < 4; i++) {
    const a = poly[i], b = poly[(i + 1) % 4], t = Math.atan2(b[1] - a[1], b[0] - a[0]) * 4, w = dist(a, b);
    sx += Math.cos(t) * w; sy += Math.sin(t) * w;
  }
  const t = snapAngle(Math.atan2(sy, sx) / 4, 7 * DEG), rc = Math.cos(t), rs = Math.sin(t);
  const pc = poly.reduce((m, p) => [m[0] + p[0] / 4, m[1] + p[1] / 4], [0, 0]);
  const us = poly.map((p) => (p[0] - pc[0]) * rc + (p[1] - pc[1]) * rs).sort((a, b) => a - b);
  const vs = poly.map((p) => -(p[0] - pc[0]) * rs + (p[1] - pc[1]) * rc).sort((a, b) => a - b);
  let w = (us[2] + us[3] - us[0] - us[1]) / 4, h = (vs[2] + vs[3] - vs[0] - vs[1]) / 4;
  if (Math.abs(w - h) < 0.1 * Math.max(w, h)) w = h = (w + h) / 2;
  return {
    closed: true,
    points: [[-w, -h], [w, -h], [w, h], [-w, h]].map(([u, v]) => [pc[0] + u * rc - v * rs, pc[1] + u * rs + v * rc, 0.5]),
  };
}

// Geometry cache: for strokes the whole centre line as one path (pen), the
// Same curve cut into gently bending pieces (highlighter, eraser) and the
// Bounding box; for groups the box of what they show and whether they need
// A layer of their own. Safari's stroker leaves hairline gaps and even holes
// Where a wide stroke doubles back over itself; pieces that turn less than
// ~35° can't do that, and their overlaps merge cleanly.
const MAX_TURN = 0.6;
const geoms = new WeakMap();
function geom(it) {
  let g = geoms.get(it);
  if (g) return g;
  if (it.tool === 'group') {
    let box = NO_BOX, hasEraser = false;
    for (const c of it.items) {
      const cg = geom(c);
      if (c.tool === 'eraser') hasEraser = true;
      else { box = union(box, cg.box); hasEraser ||= !!cg.hasEraser; }
    }
    g = { box, hasEraser };
  } else {
    const pts = it.points, n = pts.length;
    let path, chunks = [];
    if (it.straight || n < 2) {
      path = straightPath(pts, it.closed);
      chunks = [path];
    } else {
      path = piecesPath(pts, 0, n);
      if (it.tool !== 'pen') {
        let start = 0, turn = 0, dir = null;
        for (let j = 0; j <= n; j++) {
          const c = piece(pts, j);
          const dx = c[6] - c[0], dy = c[7] - c[1];
          if (!dx && !dy) continue;
          const d = Math.atan2(dy, dx);
          if (dir !== null) {
            let a = Math.abs(d - dir);
            if (a > Math.PI) a = 2 * Math.PI - a;
            turn += a;
            if (turn > MAX_TURN) { chunks.push(piecesPath(pts, start, j - 1)); start = j; turn = a; }
          }
          dir = d;
        }
        chunks.push(piecesPath(pts, start, n));
      }
    }
    g = { path, chunks, box: grow(ptsBox(pts), it.size / 2 + 2) };
  }
  geoms.set(it, g);
  return g;
}

function translate(it, dx, dy) {
  if (it.tool === 'group') return { tool: 'group', items: it.items.map((c) => translate(c, dx, dy)) };
  return { ...it, points: it.points.map((p) => [p[0] + dx, p[1] + dy, p[2]]) };
}

// Points along what an item shows, for the lasso hit test.
function samples(it, out = []) {
  if (it.tool === 'group') { for (const c of it.items) if (c.tool !== 'eraser') samples(c, out); return out; }
  const p = it.points, a = p[0], b = p[p.length - 1];
  if (it.straight || p.length < 3) {
    const v = it.closed ? p.concat([a]) : p.length < 2 ? [a, a] : p;
    for (let j = 1; j < v.length; j++) {
      for (let i = 0; i < 8; i++) out.push([v[j - 1][0] + ((v[j][0] - v[j - 1][0]) * i) / 8, v[j - 1][1] + ((v[j][1] - v[j - 1][1]) * i) / 8]);
    }
    out.push(v[v.length - 1]);
    return out;
  }
  const step = Math.max(1, Math.floor(p.length / 64));
  for (let i = 0; i < p.length; i += step) out.push(p[i]);
  out.push(b);
  return out;
}
function inPoly(x, y, poly) {
  let c = false;
  for (let i = 0, j = poly.length - 1; i < poly.length; j = i++) {
    const [xi, yi] = poly[i], [xj, yj] = poly[j];
    if ((yi > y) !== (yj > y) && x < ((xj - xi) * (y - yi)) / (yj - yi) + xi) c = !c;
  }
  return c;
}

function style(ctx, st) {
  ctx.strokeStyle = st.tool === 'eraser' ? '#000' : st.color;
  ctx.lineWidth = st.size;
  ctx.lineCap = 'round';
  ctx.lineJoin = 'round';
}

// Draws `draw` on a scratch canvas covering `box` (doc coordinates under
// Ctx's transform) and lays the result down with `alpha`. One scratch
// Canvas per nesting depth, from `pool`.
function withLayer(ctx, box, pool, depth, alpha, draw) {
  const m = ctx.getTransform(), cw = ctx.canvas.width, ch = ctx.canvas.height;
  const px0 = Math.max(0, Math.floor(box.x0 * m.a + m.e)), py0 = Math.max(0, Math.floor(box.y0 * m.d + m.f));
  const px1 = Math.min(cw, Math.ceil(box.x1 * m.a + m.e)), py1 = Math.min(ch, Math.ceil(box.y1 * m.d + m.f));
  if (px1 <= px0 || py1 <= py0) return;
  const w = px1 - px0, h = py1 - py0;
  const c = pool[depth] || (pool[depth] = document.createElement('canvas'));
  if (c.width < w || c.height < h) { c.width = Math.max(c.width, w); c.height = Math.max(c.height, h); }
  const l = c.getContext('2d');
  l.setTransform(1, 0, 0, 1, 0, 0);
  l.clearRect(0, 0, w, h);
  l.setTransform(m.a, m.b, m.c, m.d, m.e - px0, m.f - py0);
  draw(l);
  ctx.save();
  ctx.setTransform(1, 0, 0, 1, 0, 0);
  ctx.globalAlpha = alpha;
  ctx.drawImage(c, 0, 0, w, h, px0, py0, w, h);
  ctx.restore();
}

function drawItem(ctx, it, pool, depth = 0) {
  const g = geom(it);
  if (it.tool === 'group') {
    if (!g.hasEraser) { for (const c of it.items) drawItem(ctx, c, pool, depth); return; }
    withLayer(ctx, g.box, pool, depth, 1, (l) => { for (const c of it.items) drawItem(l, c, pool, depth + 1); });
    return;
  }
  if (it.tool === 'pen') { style(ctx, it); ctx.stroke(g.path); return; }
  if (it.tool === 'eraser') {
    style(ctx, it);
    ctx.globalCompositeOperation = 'destination-out';
    for (const c of g.chunks) ctx.stroke(c);
    ctx.globalCompositeOperation = 'source-over';
    return;
  }
  // Highlighter: a stroke that turns so little that it can't overlap itself
  // Is simply drawn translucently; others as opaque pieces on a scratch
  // Layer, which is then laid down translucently in one go (its own
  // Overlaps don't get darker).
  if (g.chunks.length === 1) {
    style(ctx, it);
    ctx.globalAlpha = HL_ALPHA;
    ctx.stroke(g.chunks[0]);
    ctx.globalAlpha = 1;
    return;
  }
  withLayer(ctx, g.box, pool, depth, HL_ALPHA, (l) => { style(l, it); for (const c of g.chunks) l.stroke(c); });
}

export function createSheet(host, { image, width, height, icons = {}, onChange = () => {}, onStrokeEnd = () => {} }) {
  const W = width, H = height;
  const ac = new AbortController();
  const listen = (target, type, fn, opts = {}) => target.addEventListener(type, fn, { ...opts, signal: ac.signal });
  const make = (cls, parent = host) => {
    const c = document.createElement('canvas');
    c.className = `sheet-${cls}`;
    parent.appendChild(c);
    return c;
  };
  const bgC = make('bg'), inkC = make('ink');
  // The stroke in progress: finished pieces are added to `live`, the still
  // Changing end (+ predicted points, hover cursor, lasso) is redrawn on
  // `tail`. Both are opaque; the wrapper's opacity makes the highlighter
  // Translucent as a whole, so overlaps don't get darker.
  const liveWrap = document.createElement('div');
  liveWrap.className = 'sheet-livewrap';
  host.appendChild(liveWrap);
  const liveC = make('live', liveWrap), tailC = make('tail', liveWrap);
  const ring = document.createElement('div');
  ring.className = 'eraser-ring';
  host.appendChild(ring);
  // Lasso selection: a dashed frame and a small bar above it
  const selBox = document.createElement('div');
  selBox.className = 'sel-box';
  selBox.hidden = true;
  const selBar = document.createElement('div');
  selBar.className = 'sel-bar';
  selBar.hidden = true;
  selBar.innerHTML = `<button type="button">${icons.trash || ''}<span>Delete</span></button>`;
  host.append(selBox, selBar);
  // Taps on the bar belong to the bar, not to the board below
  for (const t of ['pointerdown', 'pointermove', 'pointerup', 'pointercancel']) listen(selBar, t, (e) => e.stopPropagation());
  // pointerup, not click: iPadOS drops the click of a Pencil tap that moved a little
  const delBtn = selBar.querySelector('button');
  listen(delBtn, 'pointerup', (e) => {
    const r = delBtn.getBoundingClientRect();
    if (e.clientX >= r.left - 8 && e.clientX <= r.right + 8 && e.clientY >= r.top - 8 && e.clientY <= r.bottom + 8) api.deleteSelection();
  });
  listen(delBtn, 'click', () => api.deleteSelection());

  const bg = bgC.getContext('2d');
  const ink = inkC.getContext('2d');
  const live = liveC.getContext('2d', { desynchronized: true });
  const tail = tailC.getContext('2d', { desynchronized: true });
  const inkPool = [], livePool = [];

  let items = [];
  let undoStack = [], redoStack = [];  // {items, rev}
  let rev = 0;
  let tool = 'pen', color = '#111111', sizes = { pen: 4, highlighter: 22, eraser: 40 };
  let fingerDraw = false;
  let view = { s: 1, x: 0, y: 0 };
  let fitted = true, pendingBoard = null;
  let cssW = 0, cssH = 0, dpr = 1;

  function commit(next) {
    undoStack.push({ items, rev });
    if (undoStack.length > MAX_UNDO) undoStack.shift();
    redoStack = [];
    items = next;
    rev = newRev();
  }

  // ---------------------------------------------------------------- geometry
  // The page (image) sits at 0,0..W,H; the view can go anywhere, only the
  // Zoom is limited relative to the page.
  const PAGE = { x0: 0, y0: 0, x1: W, y1: H };
  let hostRect = { left: 0, top: 0 };
  const toDoc = (cx, cy) => [(cx - hostRect.left - view.x) / view.s, (cy - hostRect.top - view.y) / view.s];
  const viewBox = () => { const x0 = -view.x / view.s, y0 = -view.y / view.s; return { x0, y0, x1: x0 + cssW / view.s, y1: y0 + cssH / view.s }; };
  // Shows `r` (doc coordinates) as large as possible with a margin on all sides.
  function fitRect(r) {
    const m = Math.max(16, Math.min(cssW, cssH) * 0.04);
    const w = Math.max(1, r.x1 - r.x0), h = Math.max(1, r.y1 - r.y0);
    view.s = Math.min((cssW - 2 * m) / w, (cssH - 2 * m) / h);
    view.x = (cssW - w * view.s) / 2 - r.x0 * view.s;
    view.y = (cssH - h * view.s) / 2 - r.y0 * view.s;
    clampView();
  }
  const pageScale = () => Math.min(cssW / W, cssH / H);
  function clampView() {
    const k = pageScale(), cx = cssW / 2, cy = cssH / 2;
    const s = Math.min(k * 12, Math.max(k / 12, view.s));
    if (s !== view.s) {
      // Keep the screen centre in place when the zoom is clamped
      view.x = cx - ((cx - view.x) / view.s) * s;
      view.y = cy - ((cy - view.y) / view.s) * s;
      view.s = s;
    }
  }
  // Fit: everything visible (plus margin), at least the page.
  function fitView() {
    fitRect(contentRect(image ? PAGE : null) || PAGE);
    fitted = true;
  }
  function applyBoard(b) {
    view.s = b.z * pageScale();
    view.x = cssW / 2 - b.cx * view.s;
    view.y = cssH / 2 - b.cy * view.s;
    clampView();
    fitted = false;
  }
  // Area of the visible ink. Erased ink doesn't count: with erasers around
  // The ink is rasterised small and scanned.
  let boundsCache = null;
  function inkBounds() {
    if (boundsCache && boundsCache.items === items) return boundsCache.box;
    let box = NO_BOX, erasers = false;
    for (const it of items) {
      if (it.tool === 'eraser') erasers = true;
      else { box = union(box, geom(it).box); erasers ||= !!geom(it).hasEraser; }
    }
    if (erasers && !isEmpty(box)) box = rasterBounds(box);
    boundsCache = { items, box };
    return box;
  }
  function rasterBounds(b) {
    const k = Math.min(1, 600 / Math.max(b.x1 - b.x0, b.y1 - b.y0));
    const w = Math.max(1, Math.ceil((b.x1 - b.x0) * k)), h = Math.max(1, Math.ceil((b.y1 - b.y0) * k));
    const c = document.createElement('canvas');
    c.width = w; c.height = h;
    const x = c.getContext('2d', { willReadFrequently: true });
    x.setTransform(k, 0, 0, k, -b.x0 * k, -b.y0 * k);
    const pool = [];
    for (const it of items) drawItem(x, it, pool);
    const d = x.getImageData(0, 0, w, h).data;
    let x0 = w, y0 = h, x1 = -1, y1 = -1;
    for (let y = 0; y < h; y++) {
      for (let i = y * w * 4 + 3, xx = 0; xx < w; xx++, i += 4) {
        if (d[i] > 6) { if (xx < x0) x0 = xx; if (xx > x1) x1 = xx; if (y < y0) y0 = y; y1 = y; }
      }
    }
    if (x1 < 0) return NO_BOX;
    return { x0: b.x0 + x0 / k, y0: b.y0 + y0 / k, x1: b.x0 + (x1 + 1) / k, y1: b.y0 + (y1 + 1) / k };
  }
  // Union of `base` and the visible ink (with a small margin).
  function contentRect(base) {
    const b = inkBounds();
    if (isEmpty(b)) return base;
    const e = grow(b, Math.max(16, Math.min(W, H) * 0.03));
    return base ? union(base, e) : e;
  }

  function resize() {
    const r = host.getBoundingClientRect();
    hostRect = r;
    if (!r.width || !r.height) return;
    const oldW = cssW, oldH = cssH;
    cssW = r.width; cssH = r.height; dpr = window.devicePixelRatio || 1;
    for (const c of [bgC, inkC, liveC, tailC]) {
      c.width = Math.round(cssW * dpr);
      c.height = Math.round(cssH * dpr);
    }
    tailBox = null;
    if (pendingBoard) { applyBoard(pendingBoard); pendingBoard = null; }
    else if (fitted || !oldW) fitView();
    else { view.x += (cssW - oldW) / 2; view.y += (cssH - oldH) / 2; clampView(); }
    renderNow();
    paintLive();
  }
  const ro = new ResizeObserver(resize);
  ro.observe(host);
  listen(window, 'scroll', () => { hostRect = host.getBoundingClientRect(); }, { capture: true });

  // ---------------------------------------------------------------- rendering
  const setView = (ctx) => ctx.setTransform(dpr * view.s, 0, 0, dpr * view.s, dpr * view.x, dpr * view.y);
  const clearCanvas = (ctx) => { ctx.setTransform(1, 0, 0, 1, 0, 0); ctx.clearRect(0, 0, ctx.canvas.width, ctx.canvas.height); };

  function renderBg() {
    bg.setTransform(1, 0, 0, 1, 0, 0);
    bg.fillStyle = '#fff';
    bg.fillRect(0, 0, bgC.width, bgC.height);
    if (!image) return;
    setView(bg);
    bg.drawImage(image, 0, 0, W, H);
  }
  // Only what is on screen; `skip`: the selection while it is dragged.
  function renderInk(skip) {
    clearCanvas(ink);
    setView(ink);
    const vb = viewBox();
    for (const it of items) {
      if (skip && skip.has(it)) continue;
      if (overlaps(geom(it).box, vb)) drawItem(ink, it, inkPool);
    }
  }
  // While a gesture moves the view, the last sharp frame is only moved and
  // Scaled by the GPU (smooth even with lots of ink); it is re-rendered
  // Sharply when the view rests for a moment, when the gesture ends and,
  // If rendering is cheap, every few frames in between.
  let rendered = { s: 1, x: 0, y: 0 }, previewing = false, settle = 0, renderCost = 0, renderedAt = 0;
  for (const c of [bgC, inkC]) c.style.transformOrigin = '0 0';
  function renderAll() {
    const t = performance.now();
    renderBg();
    renderInk(drag ? sel.items : null);
    if (drag) paintDrag();
    rendered = { ...view };
    if (previewing) { bgC.style.transform = inkC.style.transform = ''; previewing = false; }
    placeSel();
    renderedAt = performance.now();
    renderCost = renderCost ? renderCost * 0.6 + (renderedAt - t) * 0.4 : renderedAt - t;
  }
  function preview() {
    const k = view.s / rendered.s;
    bgC.style.transform = inkC.style.transform = `matrix(${k},0,0,${k},${view.x - rendered.x * k},${view.y - rendered.y * k})`;
    previewing = true;
    placeSel();
    clearTimeout(settle);
    settle = setTimeout(render, 110);
    if (renderCost < 6 && performance.now() - renderedAt > 180) render();
  }
  let frame = 0;
  function render() {
    if (frame) return;
    frame = requestAnimationFrame(() => { frame = 0; renderAll(); });
  }
  function renderNow() { cancelAnimationFrame(frame); frame = 0; clearTimeout(settle); renderAll(); }

  // -------------------------------------------------------------- live layer
  let cur = null;           // stroke in progress
  let curPen = false;
  let lasso = null;         // { points } in doc coordinates
  let drag = null;          // lasso selection being moved
  let curPointer = null, curType = '';
  let predicted = [];
  let lastMove = { x: 0, y: 0 };
  let holdTimer = null;
  let liveFrame = 0;
  let hover = null;
  let liveDone = 0;          // pieces of `cur` already on the live canvas
  let tailBox = null;        // what was drawn on `tail` (CSS px)
  const busy = () => !!(cur || lasso || drag);

  // `tail` is redrawn every frame; only the area drawn last time is cleared
  function clearTail() {
    if (!tailBox) return;
    const b = tailBox;
    tailBox = null;
    tail.setTransform(1, 0, 0, 1, 0, 0);
    tail.clearRect(Math.floor(b.x0 * dpr) - 2, Math.floor(b.y0 * dpr) - 2, Math.ceil((b.x1 - b.x0) * dpr) + 4, Math.ceil((b.y1 - b.y0) * dpr) + 4);
  }
  const markScreen = (b) => { tailBox = tailBox ? union(tailBox, b) : b; };
  const markDoc = (b) => markScreen({ x0: b.x0 * view.s + view.x, y0: b.y0 * view.s + view.y, x1: b.x1 * view.s + view.x, y1: b.y1 * view.s + view.y });

  function paintLive() {
    clearTail();
    if (cur) paintStroke();
    else if (lasso) paintLasso();
    const h = cur ? eraserPos() : hover;
    if (h && tool === 'eraser' && !drag) showRing(h);
    else ring.style.display = 'none';
    if (h && !busy() && (tool === 'pen' || tool === 'highlighter')) drawDot(h);
  }
  function drawLive() {
    if (liveFrame) return;
    liveFrame = requestAnimationFrame(() => { liveFrame = 0; paintLive(); });
  }

  // Draws the stroke in progress: only the new finished pieces plus the
  // Short changing end.
  function paintStroke() {
    const st = cur, pts = st.points, n = pts.length, r = st.size / 2 + 2;
    if (st.straight) {
      setView(tail); style(tail, st); tail.stroke(straightPath(pts, st.closed));
      markDoc(grow(ptsBox(pts), r));
      return;
    }
    const target = st.tool === 'eraser' ? ink : live;
    if (n >= 2 && n - 2 >= liveDone) {
      setView(target); style(target, st);
      if (st.tool === 'eraser') target.globalCompositeOperation = 'destination-out';
      target.stroke(piecesPath(pts, liveDone, n - 2));
      target.globalCompositeOperation = 'source-over';
      liveDone = n - 1;
    }
    if (st.tool !== 'eraser') {
      const all = predicted.length ? pts.concat(predicted) : pts;
      setView(tail); style(tail, st);
      tail.stroke(n < 2 ? straightPath(pts) : piecesPath(all, liveDone, all.length));
      markDoc(grow(ptsBox(all, liveDone - 3), r));
    }
  }
  function paintLasso() {
    const pts = lasso.points;
    if (pts.length < 2) return;
    const p = new Path2D();
    p.moveTo(pts[0][0], pts[0][1]);
    for (let i = 1; i < pts.length; i++) p.lineTo(pts[i][0], pts[i][1]);
    p.closePath();
    setView(tail);
    tail.lineJoin = 'round';
    tail.lineCap = 'round';
    tail.fillStyle = 'rgba(77, 107, 250, 0.07)';
    tail.fill(p);
    tail.strokeStyle = 'rgba(255, 255, 255, 0.9)';
    tail.lineWidth = 3.5 / view.s;
    tail.stroke(p);
    tail.strokeStyle = '#4d6bfa';
    tail.lineWidth = 1.6 / view.s;
    tail.setLineDash([6 / view.s, 5 / view.s]);
    tail.stroke(p);
    tail.setLineDash([]);
    markDoc(grow(ptsBox(pts), 4 / view.s));
  }
  function eraserPos() {
    if (!cur || cur.tool !== 'eraser') return null;
    const p = cur.points[cur.points.length - 1];
    return { x: p[0] * view.s + view.x, y: p[1] * view.s + view.y };
  }
  // A DOM ring with mix-blend-mode: difference takes the inverse colour of
  // Whatever it is over, so it is visible on any background
  function showRing(h) {
    const r = Math.max(2, (sizes.eraser * view.s) / 2);
    ring.style.display = 'block';
    ring.style.width = ring.style.height = `${2 * r}px`;
    ring.style.transform = `translate(${h.x - r}px, ${h.y - r}px)`;
  }
  function drawDot(h) {
    const r = Math.max(1.5, (sizes[tool] * view.s) / 2);
    tail.setTransform(dpr, 0, 0, dpr, 0, 0);
    tail.beginPath();
    tail.arc(h.x, h.y, r, 0, Math.PI * 2);
    tail.fillStyle = color;
    tail.globalAlpha = tool === 'highlighter' ? HL_ALPHA : 0.6;
    tail.fill();
    tail.globalAlpha = 1;
    markScreen(grow({ x0: h.x, y0: h.y, x1: h.x, y1: h.y }, r + 1));
  }

  // ------------------------------------------------------------------ strokes
  function begin(e) {
    const [x, y] = toDoc(e.clientX, e.clientY);
    cur = { tool, color, size: sizes[tool], points: [[x, y, 0.5]] };
    curPen = e.pointerType === 'pen';
    curPointer = e.pointerId; curType = e.pointerType;
    predicted = [];
    liveDone = 0;
    clearCanvas(live);
    liveWrap.style.opacity = tool === 'highlighter' ? String(HL_ALPHA) : '1';
    lastMove = { x: e.clientX, y: e.clientY };
    armHold();
    paintLive();
  }

  function addPoint(ev, exact = false) {
    let [x, y] = toDoc(ev.clientX, ev.clientY);
    const pts = cur.points;
    const lastP = pts[pts.length - 1];
    if (cur.straight) { if (!cur.closed) pts[pts.length - 1] = [x, y, 0.5]; return; }
    if (!exact) {
      const k = curPen ? STREAMLINE.pen : STREAMLINE.other;
      x = lastP[0] + (x - lastP[0]) * (1 - k);
      y = lastP[1] + (y - lastP[1]) * (1 - k);
    }
    if (Math.hypot(x - lastP[0], y - lastP[1]) * view.s < 0.6) return;
    pts.push([x, y, 0.5]);
  }

  function armHold() {
    clearTimeout(holdTimer);
    holdTimer = setTimeout(straighten, HOLD_TO_STRAIGHTEN_MS);
  }

  // Holding the pen still at the end of a stroke turns it into a straight
  // Line, or a clean triangle, quadrilateral or ellipse when it is closed.
  function straighten() {
    if (!cur || cur.straight || cur.tool === 'eraser') return;
    const pts = cur.points;
    if (pts.length < 4) return;
    const a = pts[0], b = pts[pts.length - 1], box = ptsBox(pts);
    const shape = Math.max(box.x1 - box.x0, box.y1 - box.y0) * view.s >= 24 ? recognize(pts) : null;
    if (shape) {
      cur.points = shape.points;
      cur.closed = true;
    } else {
      if (Math.hypot(b[0] - a[0], b[1] - a[1]) * view.s < 30) return;
      // A closed loop that isn't a shape stays as drawn
      if (dist(a, b) < 0.25 * Math.max(box.x1 - box.x0, box.y1 - box.y0)) return;
      cur.points = [a, b];
    }
    cur.straight = true;
    predicted = [];
    clearCanvas(live);
    paintLive();
    if (navigator.vibrate) navigator.vibrate(8);
  }

  function extend(e) {
    for (const ev of coalesced(e)) addPoint(ev);
    const pr = !cur.straight && e.getPredictedEvents ? e.getPredictedEvents() : [];
    predicted = pr.slice(0, 2).map((ev) => [...toDoc(ev.clientX, ev.clientY), 0.5]);
    if (Math.hypot(e.clientX - lastMove.x, e.clientY - lastMove.y) > 3) {
      lastMove = { x: e.clientX, y: e.clientY };
      if (!cur.straight) armHold();
    }
    drawLive();
  }

  function end() {
    clearTimeout(holdTimer);
    if (!cur) return;
    const st = cur;
    cur = null; curPointer = null; predicted = [];
    commit(items.concat([st]));
    if (previewing) renderNow();
    else if (st.tool !== 'eraser') { setView(ink); drawItem(ink, st, inkPool); }
    else {
      // The eraser already worked on the ink while moving: only its end is left
      const n = st.points.length;
      setView(ink); style(ink, st);
      ink.globalCompositeOperation = 'destination-out';
      ink.stroke(n < 2 ? straightPath(st.points) : piecesPath(st.points, Math.max(0, liveDone - 1), n));
      ink.globalCompositeOperation = 'source-over';
    }
    clearCanvas(live);
    clearTail();
    liveWrap.style.opacity = '1';
    paintLive();
    changed();
    onStrokeEnd(st);
  }
  function cancelStroke() {
    clearTimeout(holdTimer);
    if (!cur) return;
    const wasEraser = cur.tool === 'eraser';
    cur = null; curPointer = null; predicted = [];
    clearCanvas(live);
    clearTail();
    liveWrap.style.opacity = '1';
    if (wasEraser) renderNow();
    paintLive();
  }

  // -------------------------------------------------------------------- lasso
  let sel = null;            // { items: Set, box }
  function setSelection(list) {
    let box = NO_BOX;
    for (const it of list) box = union(box, geom(it).box);
    sel = list.length ? { items: new Set(list), box } : null;
    placeSel();
  }
  function clearSelection() {
    if (!sel) return;
    if (drag) cancelDrag();
    sel = null;
    placeSel();
  }
  function onSelection(e) {
    if (!sel) return false;
    const [x, y] = toDoc(e.clientX, e.clientY), b = grow(sel.box, 16 / view.s);
    return x >= b.x0 && x <= b.x1 && y >= b.y0 && y <= b.y1;
  }
  function placeSel() {
    if (!sel) { selBox.hidden = selBar.hidden = true; return; }
    const b = sel.box, ox = drag ? drag.dx : 0, oy = drag ? drag.dy : 0, pad = 8;
    const x0 = (b.x0 + ox) * view.s + view.x - pad, y0 = (b.y0 + oy) * view.s + view.y - pad;
    const w = (b.x1 - b.x0) * view.s + 2 * pad, h = (b.y1 - b.y0) * view.s + 2 * pad;
    selBox.hidden = false;
    selBox.style.width = `${w}px`;
    selBox.style.height = `${h}px`;
    selBox.style.transform = `translate(${x0}px, ${y0}px)`;
    selBar.hidden = !!drag;
    if (drag) return;
    const bw = selBar.offsetWidth, bh = selBar.offsetHeight;
    const bx = Math.min(cssW - bw - 8, Math.max(8, x0 + w / 2 - bw / 2));
    let by = y0 - bh - 10;
    if (by < 8) by = y0 + h + 10 + bh <= cssH - 8 ? y0 + h + 10 : Math.max(8, y0 + 10);
    selBar.style.transform = `translate(${bx}px, ${by}px)`;
  }

  function startLasso(e) {
    lasso = { points: [toDoc(e.clientX, e.clientY)] };
    curPointer = e.pointerId; curType = e.pointerType;
    liveWrap.style.opacity = '1';
    drawLive();
  }
  function extendLasso(e) {
    const pts = lasso.points;
    for (const ev of coalesced(e)) {
      const p = toDoc(ev.clientX, ev.clientY), l = pts[pts.length - 1];
      if (Math.hypot(p[0] - l[0], p[1] - l[1]) * view.s >= 2) pts.push(p);
    }
    drawLive();
  }
  // Selects what lies mostly inside the loop.
  function finishLasso() {
    const poly = lasso.points;
    lasso = null; curPointer = null;
    drawLive();
    let len = 0;
    for (let i = 1; i < poly.length; i++) len += Math.hypot(poly[i][0] - poly[i - 1][0], poly[i][1] - poly[i - 1][1]);
    if (poly.length < 3 || len * view.s < 16) return;
    const pb = ptsBox(poly);
    const chosen = items.filter((it) => {
      if (it.tool === 'eraser' || !overlaps(geom(it).box, pb)) return false;
      const s = samples(it);
      let inside = 0;
      for (const [x, y] of s) if (inPoly(x, y, poly)) inside++;
      return s.length && inside * 2 >= s.length;
    });
    setSelection(chosen);
  }
  function cancelLasso() { lasso = null; curPointer = null; drawLive(); }

  // Moving the selection: it goes to the top together with the erasers that
  // Touched it after it was drawn (as a group, so they keep erasing only it).
  function movePlan() {
    const s = sel.items, first = items.findIndex((it) => s.has(it));
    const sub = items.slice(first).filter((it) => s.has(it) || (it.tool === 'eraser' && overlaps(geom(it).box, sel.box)));
    return { sub, rest: items.filter((it) => !s.has(it)) };
  }
  function startDrag(e) {
    const { sub } = movePlan();
    drag = { group: { tool: 'group', items: sub }, sub, cx: e.clientX, cy: e.clientY, dx: 0, dy: 0 };
    curPointer = e.pointerId; curType = e.pointerType;
    liveWrap.style.opacity = '1';
    renderNow();
    paintLive();
  }
  function paintDrag() {
    clearCanvas(live);
    setView(live);
    drawItem(live, drag.group, livePool);
    liveC.style.transform = `translate(${drag.dx * view.s}px, ${drag.dy * view.s}px)`;
  }
  function moveDrag(e) {
    drag.dx = (e.clientX - drag.cx) / view.s;
    drag.dy = (e.clientY - drag.cy) / view.s;
    liveC.style.transform = `translate(${drag.dx * view.s}px, ${drag.dy * view.s}px)`;
    placeSel();
  }
  function endDrag() {
    const d = drag;
    drag = null; curPointer = null;
    liveC.style.transform = '';
    clearCanvas(live);
    if (Math.hypot(d.dx, d.dy) * view.s >= 1) {
      const { rest } = movePlan();
      const moved = d.sub.map((it) => translate(it, d.dx, d.dy));
      const top = d.sub.some((it) => it.tool === 'eraser') ? [{ tool: 'group', items: moved }] : moved;
      commit(rest.concat(top));
      setSelection(top);
      changed();
    }
    renderNow();
  }
  function cancelDrag() {
    drag = null; curPointer = null;
    liveC.style.transform = '';
    clearCanvas(live);
    renderNow();
  }

  // What the pen (mouse, drawing finger) does when it touches down.
  function primaryDown(e) {
    if (previewing || frame) renderNow();
    hover = null;
    if (sel && onSelection(e)) { startDrag(e); return; }
    clearSelection();
    if (tool === 'lasso') startLasso(e);
    else begin(e);
  }
  function actionMove(e) {
    if (cur) extend(e);
    else if (lasso) extendLasso(e);
    else if (drag) moveDrag(e);
  }
  function finishAction(e) {
    if (cur) { if (e && !cur.straight) addPoint(e, true); end(); }
    else if (lasso) finishLasso();
    else if (drag) endDrag();
  }
  function abortAction() {
    if (cur) cancelStroke();
    else if (lasso) cancelLasso();
    else if (drag) cancelDrag();
  }

  // ----------------------------------------------------------------- gestures
  const touches = new Map();  // pointerId -> {x, y}, fingers that count
  const palms = new Set();    // fingers ignored until they lift
  let gesture = null;         // {origin, startView, startDist, startMid, max, t0, moved}
  let lastPen = 0;
  let penDown = false;
  let pendingTouch = null;    // finger-draw start, delayed to see if it's a pinch
  let panMouse = null;

  function startGesture() {
    const pts = [...touches.values()];
    const mid = pts.reduce((a, p) => [a[0] + p.x / pts.length, a[1] + p.y / pts.length], [0, 0]);
    const dist = pts.length > 1 ? Math.hypot(pts[0].x - pts[1].x, pts[0].y - pts[1].y) : 0;
    gesture = {
      origin: gesture ? gesture.origin : { ...view, fitted },
      startView: { ...view }, startMid: mid, startDist: dist,
      max: Math.max(gesture ? gesture.max : 0, pts.length),
      t0: gesture ? gesture.t0 : performance.now(),
      moved: gesture ? gesture.moved : false,
    };
  }

  function updateGesture() {
    const pts = [...touches.values()];
    if (!pts.length || !gesture || gesture.frozen) return;
    const r = hostRect;
    const mid = pts.reduce((a, p) => [a[0] + p.x / pts.length, a[1] + p.y / pts.length], [0, 0]);
    const g = gesture;
    if (Math.hypot(mid[0] - g.startMid[0], mid[1] - g.startMid[1]) > 10) g.moved = true;
    let s = g.startView.s;
    if (pts.length > 1 && g.startDist > 0) {
      const dist = Math.hypot(pts[0].x - pts[1].x, pts[0].y - pts[1].y);
      if (Math.abs(dist - g.startDist) > 10) g.moved = true;
      s = g.startView.s * (dist / g.startDist);
    }
    const ax = g.startMid[0] - r.left, ay = g.startMid[1] - r.top;
    const docX = (ax - g.startView.x) / g.startView.s, docY = (ay - g.startView.y) / g.startView.s;
    view.s = s;
    clampView();
    view.x = mid[0] - r.left - docX * view.s;
    view.y = mid[1] - r.top - docY * view.s;
    fitted = false;
    preview();
  }

  // The pen came down while fingers touch the screen: those are a resting
  // Palm. Whatever they did (pan, zoom, a finger stroke) is undone.
  function dropPalms() {
    if (pendingTouch) { clearTimeout(pendingTouch.timer); pendingTouch = null; }
    if (busy() && curType === 'touch') abortAction();
    if (!touches.size) return;
    for (const id of touches.keys()) palms.add(id);
    touches.clear();
    if (gesture) {
      const o = gesture.origin;
      gesture = null;
      if (o.s !== view.s || o.x !== view.x || o.y !== view.y) { view = { s: o.s, x: o.x, y: o.y }; fitted = o.fitted; renderNow(); }
    }
  }

  function startPending() {
    const p = pendingTouch;
    clearTimeout(p.timer);
    pendingTouch = null;
    primaryDown(p.e);
    return p;
  }

  function onDown(e) {
    if (e.pointerType === 'touch') { touchDown(e); return; }
    host.setPointerCapture?.(e.pointerId);
    if (e.pointerType === 'pen') {
      lastPen = performance.now();
      penDown = true;
      dropPalms();
      // An action whose pointerup got lost must never block the next one
      if (busy()) finishAction();
      primaryDown(e);
      return;
    }
    if (e.button === 0) { if (!busy()) primaryDown(e); }
    else panMouse = { x: e.clientX, y: e.clientY, v: { ...view } };
  }

  function touchDown(e) {
    if (penDown || performance.now() - lastPen < PALM_MS) { palms.add(e.pointerId); return; }
    host.setPointerCapture?.(e.pointerId);
    touches.set(e.pointerId, { x: e.clientX, y: e.clientY });
    if (touches.size === 1 && !busy()) {
      if (sel && onSelection(e)) { primaryDown(e); return; }
      if (fingerDraw) {
        pendingTouch = { e, id: e.pointerId, timer: setTimeout(() => { if (pendingTouch && touches.size === 1) startPending(); }, 70) };
        return;
      }
    }
    // Another finger: whatever the first one started becomes a pinch
    if (pendingTouch) { clearTimeout(pendingTouch.timer); pendingTouch = null; }
    let aborted = false;
    if (busy() && curType === 'touch') { abortAction(); aborted = true; }
    startGesture();
    if (aborted) gesture.moved = true;
  }

  function onMove(e) {
    const id = e.pointerId;
    if (palms.has(id)) return;
    if (e.pointerType === 'touch') {
      if (!touches.has(id)) return;
      touches.set(id, { x: e.clientX, y: e.clientY });
      if (pendingTouch && pendingTouch.id === id) {
        if (Math.hypot(e.clientX - pendingTouch.e.clientX, e.clientY - pendingTouch.e.clientY) <= 5) return;
        startPending();
      }
      if (busy() && curPointer === id) { actionMove(e); return; }
      updateGesture();
      return;
    }
    if (panMouse) {
      view.x = panMouse.v.x + e.clientX - panMouse.x;
      view.y = panMouse.v.y + e.clientY - panMouse.y;
      fitted = false;
      preview();
      return;
    }
    if (busy() && id === curPointer) { actionMove(e); return; }
    // Pen moving with the tip down but nothing started (missed pointerdown)
    if (e.pointerType === 'pen' && e.buttons & 1 && !busy()) {
      penDown = true; lastPen = performance.now();
      dropPalms();
      primaryDown(e);
      return;
    }
    // A hovering Pencil: the hand is about to rest on the screen
    if (e.pointerType === 'pen') lastPen = performance.now();
    hover = { x: e.clientX - hostRect.left, y: e.clientY - hostRect.top };
    drawLive();
  }

  function onUp(e) {
    const id = e.pointerId;
    if (palms.delete(id)) return;
    if (e.pointerType === 'pen') { lastPen = performance.now(); penDown = false; }
    if (panMouse && e.pointerType === 'mouse') { panMouse = null; render(); return; }
    if (pendingTouch && pendingTouch.id === id) startPending();  // a tap: a dot
    if (busy() && id === curPointer) finishAction(e);
    if (e.pointerType === 'touch' && touches.has(id)) {
      touches.delete(id);
      if (touches.size) {
        // One finger of a pinch lifted: don't let the other one start panning
        if (gesture && gesture.max > 1) { gesture.frozen = true; return; }
        if (gesture) startGesture();
        return;
      }
      const g = gesture;
      gesture = null;
      if (g) {
        // Two-finger tap = undo, three-finger tap = redo
        if (!g.moved && performance.now() - g.t0 < 350) {
          // One-finger tap: leaves the lasso selection
          if (g.max === 1) clearSelection();
          else if (g.max === 2) api.undo();
          else if (g.max === 3) api.redo();
        }
        render();
      }
    }
  }

  function onCancel(e) {
    const id = e.pointerId;
    palms.delete(id);
    touches.delete(id);
    if (e.pointerType === 'pen') { penDown = false; lastPen = performance.now(); }
    if (pendingTouch && pendingTouch.id === id) { clearTimeout(pendingTouch.timer); pendingTouch = null; }
    // IPadOS cancels pen pointers on some fast re-touches: keep what was drawn
    if (busy() && id === curPointer) {
      if (cur && cur.points.length < 2) cancelStroke();
      else if (lasso) cancelLasso();
      else finishAction();
    }
    if (!touches.size && gesture) { gesture = null; render(); }
  }

  function onLeave(e) {
    if (e.pointerType !== 'touch' && !busy()) { hover = null; drawLive(); }
  }

  function onWheel(e) {
    e.preventDefault();
    if (drag) return;
    const r = hostRect;
    const cx = e.clientX - r.left, cy = e.clientY - r.top;
    if (e.ctrlKey || e.metaKey || (Math.abs(e.deltaY) > Math.abs(e.deltaX) * 2 && !e.shiftKey)) {
      const f = Math.exp(-e.deltaY * (e.ctrlKey ? 0.01 : 0.0015));
      const dx = (cx - view.x) / view.s, dy = (cy - view.y) / view.s;
      view.s *= f;
      clampView();
      view.x = cx - dx * view.s;
      view.y = cy - dy * view.s;
    } else {
      view.x -= e.deltaX;
      view.y -= e.deltaY;
    }
    fitted = false;
    preview();
  }

  listen(host, 'pointerdown', onDown);
  listen(host, 'pointermove', onMove);
  listen(host, 'pointerup', onUp);
  listen(host, 'pointercancel', onCancel);
  listen(host, 'pointerleave', onLeave);
  listen(host, 'wheel', onWheel, { passive: false });
  listen(host, 'contextmenu', (e) => e.preventDefault());
  // Without this iPadOS treats quick successive pencil taps as double-tap
  // Zoom / selection gestures and swallows or cancels the pointer events.
  const block = (e) => { if (e.cancelable && !selBar.contains(e.target)) e.preventDefault(); };
  for (const t of ['touchstart', 'touchmove', 'touchend', 'dblclick', 'selectstart']) listen(host, t, block, { passive: false });

  function changed() { onChange(api); }
  function after() { clearSelection(); boundsCache = null; renderNow(); paintLive(); changed(); }

  const api = {
    get tool() { return tool; },
    setTool(t) {
      if (t === tool) return;
      if (busy()) finishAction();
      tool = t;
      if (t !== 'lasso') clearSelection();
      drawLive(); changed();
    },
    get color() { return color; },
    setColor(c) { color = c; drawLive(); changed(); },
    size: (t = tool) => sizes[t],
    setSize(v, t = tool) { sizes[t] = v; drawLive(); changed(); },
    get fingerDraw() { return fingerDraw; },
    setFingerDraw(v) { fingerDraw = !!v; changed(); },
    get strokeCount() { return items.length; },
    get canUndo() { return undoStack.length > 0; },
    get canRedo() { return redoStack.length > 0; },
    get busy() { return busy(); },
    get revision() { return rev; },
    get hasSelection() { return !!sel; },
    get width() { return W; },
    get height() { return H; },
    undo() {
      if (busy() || !undoStack.length) return;
      redoStack.push({ items, rev });
      ({ items, rev } = undoStack.pop());
      after();
    },
    redo() {
      if (busy() || !redoStack.length) return;
      undoStack.push({ items, rev });
      ({ items, rev } = redoStack.pop());
      after();
    },
    clear() {
      if (busy() || !items.length) return;
      commit([]);
      after();
    },
    clearSelection() { clearSelection(); },
    deleteSelection() {
      if (!sel || busy()) return;
      const s = sel.items;
      commit(items.filter((it) => !s.has(it)));
      after();
    },
    fit() { fitView(); renderNow(); paintLive(); },
    strokes: () => items,
    // The view (centre and zoom), so a reload comes back to the same spot.
    board: () => (fitted || !cssW ? null : { cx: (cssW / 2 - view.x) / view.s, cy: (cssH / 2 - view.y) / view.s, z: view.s / pageScale() }),
    load(list, board = null, r = 0) {
      items = (list || []).filter((it) => it && (it.tool === 'group' ? Array.isArray(it.items) : Array.isArray(it.points) && it.points.length));
      undoStack = []; redoStack = [];
      rev = r || 0;
      boundsCache = null;
      sel = null;
      if (board && board.z > 0) { if (cssW) applyBoard(board); else pendingBoard = board; }
      else fitView();
      renderNow(); placeSel(); changed();
    },
    // The area that was worked on: the image (exactly, if all ink is on it)
    // Plus whatever visible ink is around it with a small margin.
    exportRect() {
      const r = contentRect(image ? PAGE : null) || PAGE;
      return { x0: Math.floor(r.x0), y0: Math.floor(r.y0), x1: Math.ceil(r.x1), y1: Math.ceil(r.y1) };
    },
    // Rendered as vectors, at the image's resolution.
    async exportBlob() {
      const { x0, y0, x1, y1 } = api.exportRect();
      const w = x1 - x0, h = y1 - y0;
      const k = Math.min(1, 8192 / Math.max(w, h), Math.sqrt(16e6 / (w * h)));
      const out = document.createElement('canvas');
      out.width = Math.ceil(w * k); out.height = Math.ceil(h * k);
      const o = out.getContext('2d');
      o.fillStyle = '#fff';
      o.fillRect(0, 0, out.width, out.height);
      o.setTransform(k, 0, 0, k, -x0 * k, -y0 * k);
      if (image) o.drawImage(image, 0, 0, W, H);
      const inkLayer = document.createElement('canvas');
      inkLayer.width = out.width; inkLayer.height = out.height;
      const l = inkLayer.getContext('2d');
      l.setTransform(k, 0, 0, k, -x0 * k, -y0 * k);
      const pool = [];
      for (const it of items) drawItem(l, it, pool);
      o.setTransform(1, 0, 0, 1, 0, 0);
      o.drawImage(inkLayer, 0, 0);
      const blob = await new Promise((res) => out.toBlob(res, 'image/png'));
      // Free the big canvases right away (Safari counts canvas memory)
      out.width = out.height = inkLayer.width = inkLayer.height = 0;
      for (const c of pool) c.width = c.height = 0;
      if (!blob) throw new Error('Could not render the drawing');
      return blob;
    },
    destroy() {
      ro.disconnect();
      ac.abort();
      clearTimeout(holdTimer); clearTimeout(settle);
      cancelAnimationFrame(frame); cancelAnimationFrame(liveFrame);
      if (pendingTouch) clearTimeout(pendingTouch.timer);
      for (const c of [bgC, inkC, liveC, tailC, ...inkPool, ...livePool]) c.width = c.height = 0;
      host.replaceChildren();
    },
  };
  resize();
  return api;
}
