import { createSheet } from './draw.js?v=19';

const $ = (id) => document.getElementById(id);
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

// Device token from the one-time pairing; the server also keeps it as cookie.
let token = localStorage.getItem('deviceToken') || '';

const COLORS = ['#111111', '#e5322d', '#1f6fe5', '#1e9e4a', '#f2b705', '#ffffff'];
const SIZES = { pen: [2, 4, 8], highlighter: [14, 24, 40], eraser: [16, 40, 90] };
const svg = (b) => `<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.9" stroke-linecap="round" stroke-linejoin="round">${b}</svg>`;
const ICONS = {
  pen: svg('<path d="M15.2 5.2l3.6 3.6"/><path d="M4 20l1-4.6L16.6 3.8a2.5 2.5 0 0 1 3.6 3.6L8.6 19 4 20z"/>'),
  highlighter: svg('<path d="m9 11-6 6v3h9l3-3"/><path d="m22 12-4.6 4.6a2 2 0 0 1-2.8 0l-5.2-5.2a2 2 0 0 1 0-2.8L14 4"/>'),
  eraser: svg('<path d="m7 21-4.3-4.3c-1-1-1-2.5 0-3.4l9.6-9.6c1-1 2.5-1 3.4 0l5.6 5.6c1 1 1 2.5 0 3.4L13 21"/><path d="M22 21H7"/><path d="m5 11 9 9"/>'),
  lasso: svg('<path d="M7 22a5 5 0 0 1-2-4"/><path d="M3.3 14A6.8 6.8 0 0 1 2 10c0-4.4 4.5-8 10-8s10 3.6 10 8-4.5 8-10 8a12 12 0 0 1-5-1"/><path d="M5 18a2 2 0 1 0 0-4 2 2 0 0 0 0 4z"/>'),
  undo: svg('<path d="M9 14 4 9l5-5"/><path d="M4 9h10.5a5.5 5.5 0 0 1 0 11H11"/>'),
  redo: svg('<path d="m15 14 5-5-5-5"/><path d="M20 9H9.5a5.5 5.5 0 0 0 0 11H13"/>'),
  more: svg('<circle cx="12" cy="12" r="1"/><circle cx="19" cy="12" r="1"/><circle cx="5" cy="12" r="1"/>'),
  filePlus: svg('<path d="M15 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V7Z"/><path d="M14 2v4a2 2 0 0 0 2 2h4"/><path d="M9 15h6"/><path d="M12 18v-6"/>'),
  fit: svg('<path d="M3 7V5a2 2 0 0 1 2-2h2"/><path d="M17 3h2a2 2 0 0 1 2 2v2"/><path d="M21 17v2a2 2 0 0 1-2 2h-2"/><path d="M7 21H5a2 2 0 0 1-2-2v-2"/>'),
  hand: svg('<path d="M18 11V6a2 2 0 0 0-4 0v5"/><path d="M14 10V4a2 2 0 0 0-4 0v2"/><path d="M10 10.5V6a2 2 0 0 0-4 0v8"/><path d="M18 8a2 2 0 1 1 4 0v6a8 8 0 0 1-8 8h-2c-2.8 0-4.5-.86-5.99-2.34l-3.6-3.6a2 2 0 0 1 2.83-2.82L7 15"/>'),
  trash: svg('<path d="M10 11v6"/><path d="M14 11v6"/><path d="M19 6v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6"/><path d="M3 6h18"/><path d="M8 6V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2"/>'),
  send: svg('<path d="M12 19V5"/><path d="m5 12 7-7 7 7"/>'),
};

let sheet = null;
let page = null;          // { id, width, height } (id null = local blank page)
let latestId = null;
let dismissedId = null;
let submittedRev = -1;   // revision of the drawing last sent to the PC
const prefs = Object.assign(
  { tool: 'pen', size: { pen: 2, highlighter: 14, eraser: 16 }, finger: false },
  JSON.parse(localStorage.getItem('prefs2') || '{}'),
);
// Pen and highlighter each keep their own colour
prefs.colors = { pen: prefs.color || COLORS[0], highlighter: '#f2b705', ...prefs.colors };
delete prefs.color;
const INKS = ['pen', 'highlighter'];
const savePrefs = () => localStorage.setItem('prefs2', JSON.stringify(prefs));

// Connection to the PC: shown under "Send to PC", which is greyed out offline.
let online = false;
function setConnected(on) {
  online = on;
  $('submitConn').textContent = on ? 'Connected' : 'Disconnected';
  if (!sending) $('submit').disabled = !sheet || !online;
}
let sending = false;

function api(path, init = {}) {
  const headers = { ...(init.headers || {}), ...(token ? { 'X-Token': token } : {}) };
  return fetch(path, { ...init, headers, cache: 'no-store' });
}

const dirty = () => !!sheet && (sheet.busy || (sheet.strokeCount > 0 && sheet.revision !== submittedRev));

let toastTimer;
function toast(text) {
  $('toast').textContent = text;
  $('toast').hidden = false;
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => { $('toast').hidden = true; }, 1400);
}

// ---------------------------------------------------------------- drafts
// The current page survives reloads (iPadOS discards background web apps).
const db = new Promise((res) => {
  const req = indexedDB.open('draw', 2);
  req.onupgradeneeded = () => {
    for (const name of ['draft', 'edits']) if (!req.result.objectStoreNames.contains(name)) req.result.createObjectStore(name);
  };
  req.onsuccess = () => res(req.result);
  req.onerror = () => res(null);
});
async function idb(mode, fn, store = 'draft') {
  const d = await db;
  if (!d) return null;
  return new Promise((res) => {
    const tx = d.transaction(store, mode);
    const r = fn(tx.objectStore(store));
    tx.oncomplete = () => res(r && r.result);
    tx.onerror = () => res(null);
  });
}
let saveTimer = 0;
function writeDraft() {
  clearTimeout(saveTimer);
  saveTimer = 0;
  if (!sheet || !page) return;
  idb('readwrite', (s) => s.put({ ...page, strokes: sheet.strokes(), board: sheet.board(), rev: sheet.revision, submittedRev }, 'meta'));
}
function saveDraft() {
  clearTimeout(saveTimer);
  saveTimer = setTimeout(writeDraft, 400);
}
// IPadOS may kill the app any time it is in the background
const flushDraft = () => { if (saveTimer) writeDraft(); };
const saveBackground = (blob) => idb('readwrite', (s) => s.put(blob, 'image'));

// ---------------------------------------------------------------- toolbar
function buildToolbar() {
  for (const b of $('tools').children) {
    b.innerHTML = ICONS[b.dataset.tool];
    b.addEventListener('click', () => setTool(b.dataset.tool));
  }
  for (const c of COLORS) {
    const b = document.createElement('button');
    b.className = 'swatch';
    b.dataset.color = c;
    b.innerHTML = `<span style="background:${c}"></span>`;
    b.addEventListener('click', () => setColor(c));
    $('colors').appendChild(b);
  }
  const custom = document.createElement('label');
  custom.className = 'swatch custom';
  custom.innerHTML = '<span></span><input type="color">';
  custom.querySelector('input').addEventListener('input', (e) => setColor(e.target.value));
  $('colors').appendChild(custom);
  for (let i = 0; i < 3; i++) {
    const b = document.createElement('button');
    b.className = 'size';
    b.dataset.index = i;
    b.addEventListener('click', () => {
      if (!SIZES[prefs.tool]) return;
      prefs.size[prefs.tool] = SIZES[prefs.tool][i];
      sheet?.setSize(prefs.size[prefs.tool], prefs.tool);
      sync();
    });
    $('sizes').appendChild(b);
  }
  $('undo').innerHTML = ICONS.undo;
  $('redo').innerHTML = ICONS.redo;
  $('more').innerHTML = ICONS.more;
  $('submit').innerHTML = `${ICONS.send}<span class="label"><span id="submitLabel">Send to PC</span><span id="submitConn" class="conn">Disconnected</span></span>`;
  $('undo').addEventListener('click', () => sheet?.undo());
  $('redo').addEventListener('click', () => sheet?.redo());
  $('more').addEventListener('click', (e) => { e.stopPropagation(); $('menu').hidden = !$('menu').hidden; });
  document.addEventListener('pointerdown', (e) => { if (!$('menu').contains(e.target) && e.target !== $('more')) $('menu').hidden = true; });
  for (const i of $('menu').querySelectorAll('[data-icon]')) i.innerHTML = ICONS[i.dataset.icon];
  $('fit').addEventListener('click', () => { sheet?.fit(); $('menu').hidden = true; });
  $('finger').addEventListener('click', () => { prefs.finger = !prefs.finger; sheet?.setFingerDraw(prefs.finger); sync(); });
  $('clear').addEventListener('click', () => {
    $('menu').hidden = true;
    if (!sheet?.strokeCount) return;
    sheet.clear();
    toast('Drawing cleared. Undo brings it back.');
  });
  const blank = () => {
    $('menu').hidden = true;
    if (dirty() && !confirm('Discard the unsent drawing?')) return;
    openBlank();
  };
  $('blank').addEventListener('click', blank);
  $('emptyBlank').addEventListener('click', blank);
  $('submit').addEventListener('click', submit);
  sync();
}

let prevTool = 'pen';
function setTool(t) {
  if (prefs.tool === t && t !== 'eraser') return;
  if (t === 'eraser' && prefs.tool !== 'eraser') prevTool = prefs.tool;
  prefs.tool = t;
  if (INKS.includes(t)) sheet?.setColor(prefs.colors[t]);
  sheet?.setTool(t);
  sync();
}
function setColor(c) {
  if (!INKS.includes(prefs.tool)) setTool(INKS.includes(prevTool) ? prevTool : 'pen');
  prefs.colors[prefs.tool] = c;
  sheet?.setColor(c);
  sync();
}

function sync() {
  for (const b of $('tools').children) b.classList.toggle('active', b.dataset.tool === prefs.tool);
  const ink = INKS.includes(prefs.tool), color = prefs.colors[prefs.tool];
  for (const b of $('colors').children) {
    if (b.dataset.color) b.classList.toggle('active', ink && b.dataset.color === color);
    else b.classList.toggle('active', ink && !COLORS.includes(color));
  }
  // The lasso has no colour or size: those controls are greyed out
  const lasso = prefs.tool === 'lasso';
  $('colors').classList.toggle('off', lasso);
  $('sizes').classList.toggle('off', lasso);
  const sizes = SIZES[prefs.tool];
  [...$('sizes').children].forEach((b, i) => {
    const d = [6, 11, 17][i];
    b.innerHTML = `<i style="width:${d}px;height:${d}px"></i>`;
    b.classList.toggle('active', !!sizes && sizes[i] === prefs.size[prefs.tool]);
  });
  $('undo').disabled = !sheet?.canUndo;
  $('redo').disabled = !sheet?.canRedo;
  if (!sending) $('submit').disabled = !sheet || !online;
  $('finger').classList.toggle('on', prefs.finger);
  $('finger').setAttribute('aria-checked', String(prefs.finger));
  document.body.classList.toggle('no-sheet', !sheet);
  savePrefs();
}

// ---------------------------------------------------------------- pages
// Exported drawings are remembered by a hash of their PNG, so when the PC
// Sends one back it reopens with its original background and editable ink.
async function hashBlob(blob) {
  const bytes = new Uint8Array(await blob.arrayBuffer());
  let h = 0x811c9dc5, g = 0x01000193;
  for (let i = 0; i < bytes.length; i++) { h = Math.imul(h ^ bytes[i], 16777619); g = Math.imul(g ^ bytes[i], 2246822519); }
  return `${bytes.length}-${(h >>> 0).toString(16)}-${(g >>> 0).toString(16)}`;
}
let background = null; // blob of the current page's background image

function open(image, width, height, id, strokes = null, board = null, rev = 0) {
  sheet?.destroy();
  page = { id, width, height };
  submittedRev = -1;
  $('empty').hidden = true;
  $('banner').hidden = true;
  sheet = createSheet($('sheet'), {
    image, width, height, icons: ICONS,
    onChange: () => { sync(); saveDraft(); },
    // The eraser is a one-shot tool: back to the previous tool after use
    onStrokeEnd: (st) => { if (st.tool === 'eraser') setTool(prevTool); },
  });
  for (const t of Object.keys(prefs.size)) sheet.setSize(prefs.size[t], t);
  sheet.setColor(prefs.colors[INKS.includes(prefs.tool) ? prefs.tool : INKS.includes(prevTool) ? prevTool : 'pen']);
  sheet.setTool(prefs.tool);
  sheet.setFingerDraw(prefs.finger);
  if (strokes) sheet.load(strokes, board, rev);
  sync();
}

async function openFromPc(state) {
  try {
    const res = await api(`api/image/${state.id}`);
    if (!res.ok) throw new Error();
    const blob = await res.blob();
    const key = await hashBlob(blob);
    const known = await idb('readonly', (s) => s.get(key), 'edits');
    if (known) {
      const img = known.bg ? await createImageBitmap(known.bg) : null;
      background = known.bg;
      open(img, known.width, known.height, state.id, known.strokes);
      submittedRev = sheet.revision;  // exactly what the PC has: nothing unsent
      saveBackground(known.bg);
    } else {
      const img = await createImageBitmap(blob);
      background = blob;
      open(img, state.width, state.height, state.id);
      saveBackground(blob);
    }
    saveDraft();
  } catch {
    toast('Could not load the image');
  }
}

function openBlank() {
  const r = $('stage').getBoundingClientRect();
  const ratio = Math.max(0.5, Math.min(2, r.width / Math.max(1, r.height)));
  const width = 2048, height = Math.round(width / ratio);
  background = null;
  open(null, width, height, null);
  saveBackground(null);
  saveDraft();
}

async function restoreDraft() {
  const meta = await idb('readonly', (s) => s.get('meta'));
  if (!meta || !meta.width) return;
  const blob = await idb('readonly', (s) => s.get('image'));
  const img = blob ? await createImageBitmap(blob).catch(() => null) : null;
  if (blob && !img) return;
  background = blob || null;
  open(img, meta.width, meta.height, meta.id, meta.strokes, meta.board, meta.rev);
  submittedRev = meta.submittedRev ?? -1;
  latestId = meta.id;
}

// ---------------------------------------------------------------- server
function showPairing(show) {
  $('pair').hidden = !show;
  if (show) setConnected(false);
}

async function checkState() {
  const res = await api('api/state');
  if (res.status === 401) { showPairing(true); return false; }
  showPairing(false);
  const state = await res.json();
  setConnected(true);
  if (!state.id || state.id === latestId) return true;
  latestId = state.id;
  if (page && page.id === state.id) return true;
  if (dirty() && dismissedId !== state.id) {
    $('banner').hidden = false;
    $('bannerLoad').onclick = () => openFromPc(state);
    $('bannerLater').onclick = () => { dismissedId = state.id; $('banner').hidden = true; };
  } else if (!dirty()) {
    openFromPc(state);
  }
  return true;
}

// Long poll: new images show up immediately.
async function eventLoop() {
  let seq = -1;
  for (;;) {
    try {
      if (!(await checkState())) { await sleep(2000); continue; }
      for (;;) {
        const res = await api(`api/events?since=${seq}`);
        if (res.status === 401) break;
        const data = await res.json();
        seq = data.seq;
        if (data.events.includes('image')) await checkState();
      }
    } catch {
      setConnected(false);
      await sleep(2000);
    }
  }
}

// Only the most recent sent drawings stay re-editable.
async function pruneEdits(keep = 40) {
  const d = await db;
  if (!d) return;
  const s = d.transaction('edits', 'readwrite').objectStore('edits');
  const list = [];
  const req = s.openCursor();
  req.onsuccess = () => {
    const c = req.result;
    if (c) { list.push([c.key, c.value.t || 0]); c.continue(); return; }
    list.sort((a, b) => b[1] - a[1]);
    for (const [k] of list.slice(keep)) s.delete(k);
  };
}

async function submit() {
  if (!sheet || !page || sheet.busy) return;
  const btn = $('submit'), label = $('submitLabel');
  btn.disabled = true;
  sending = true;
  label.textContent = 'Sending …';
  try {
    const blob = await sheet.exportBlob();
    const res = await api(page.id == null ? 'api/submit?blank=1' : 'api/submit', { method: 'POST', headers: { 'Content-Type': 'image/png' }, body: blob });
    if (!res.ok) throw new Error(await res.text());
    const record = { bg: background, width: page.width, height: page.height, strokes: sheet.strokes(), t: Date.now() };
    hashBlob(blob).then((key) => idb('readwrite', (s) => s.put(record, key), 'edits')).then(() => pruneEdits());
    submittedRev = sheet.revision;
    saveDraft();
    label.textContent = 'Sent';
    toast('Sent to the PC. It is on the clipboard.');
  } catch (e) {
    label.textContent = 'Try again';
    toast(`Sending failed: ${e.message || e}`);
  }
  setTimeout(() => { label.textContent = 'Send to PC'; sending = false; btn.disabled = !sheet || !online; }, 1600);
}

// ---------------------------------------------------------------- wiring
$('pair').addEventListener('submit', async (e) => {
  e.preventDefault();
  $('pairError').textContent = '';
  try {
    const res = await fetch('api/pair', { method: 'POST', body: $('pairCode').value.replace(/\D/g, ''), cache: 'no-store' });
    if (!res.ok) throw new Error(await res.text());
    token = (await res.json()).token;
    localStorage.setItem('deviceToken', token);
    $('pairCode').value = '';
    showPairing(false);
  } catch (err) {
    $('pairError').textContent = err.message || String(err);
  }
});
window.addEventListener('keydown', (e) => {
  if (e.target instanceof HTMLInputElement) return;
  const mod = e.metaKey || e.ctrlKey;
  if (mod && e.key.toLowerCase() === 'z') { e.preventDefault(); if (e.shiftKey) sheet?.redo(); else sheet?.undo(); }
  else if ((e.key === 'Delete' || e.key === 'Backspace') && sheet?.hasSelection) { e.preventDefault(); sheet.deleteSelection(); }
  else if (e.key === 'Escape') { sheet?.clearSelection(); $('menu').hidden = true; }
});
new ResizeObserver(() => {
  document.documentElement.style.setProperty('--bar', `${$('toolbar').offsetHeight}px`);
}).observe($('toolbar'));
document.addEventListener('visibilitychange', () => { if (document.hidden) flushDraft(); else checkState().catch(() => {}); });
window.addEventListener('pagehide', flushDraft);
document.addEventListener('gesturestart', (e) => e.preventDefault());

buildToolbar();
restoreDraft().finally(eventLoop);
