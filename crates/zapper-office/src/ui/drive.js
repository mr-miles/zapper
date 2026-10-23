'use strict';
// The office page: the Shared drive tab.

// ──────────────────────────── shared drive ────────────────────────────

const DRV = { sel: null, sig: null, mainSig: null, viewKey: null };

function mountDrive() {
  const root = $('tab-drive');
  const fk = selectOf([['', 'any kind'], 'document', 'report', 'dataset', 'deck', 'board-pack'], '', { id: 'df-kind', onchange: () => { DRV.sig = null; updateDrive(); } });
  const fa = el('select', { id: 'df-author', onchange: () => { DRV.sig = null; updateDrive(); } });
  const fq = el('input', { type: 'search', id: 'df-q', placeholder: 'Search titles', oninput: () => { DRV.sig = null; updateDrive(); } });
  root.replaceChildren(el('div', { class: 'split' },
    el('div', {}, el('div', { class: 'bar' }, fk, fa, fq), el('div', { class: 'list', id: 'drv-list' })),
    el('div', { id: 'drv-main' })));
  DRV.sig = null; DRV.mainSig = null; DRV.viewKey = null;
}
function openDrive(artifact, version, path) {
  DRV.sel = { artifact: str(artifact), version: version ? Number(version) : null, path: path || null };
  DRV.mainSig = null; DRV.sig = null;
  if (S.tab !== 'drive') showTab('drive'); else updateDrive();
}
function updateDrive() {
  const M = S.M;
  if (!M) return;
  const arts = [...M.artifacts.values()];
  const authors = [...new Set(arts.map((a) => a.author))].sort();
  const fa = $('df-author');
  if (fa.dataset.sig !== authors.join(',')) { const cur = fa.value; fa.dataset.sig = authors.join(','); fa.replaceChildren(opt('', 'any author'), ...authors.map((x) => opt(x, x, x === cur))); }
  const sig = M.published.length + '|' + (DRV.sel && DRV.sel.artifact) + '|' + Math.floor(T() / 60);
  if (sig !== DRV.sig) {
    DRV.sig = sig;
    const k = $('df-kind').value, au = fa.value, q = $('df-q').value.trim().toLowerCase();
    const list = arts.filter((a) => (!k || a.kind === k) && (!au || a.author === au) && (!q || (a.title + ' ' + a.id).toLowerCase().includes(q))).sort((a, b) => b.last - a.last);
    $('drv-list').replaceChildren(...(list.length ? list.map((a) => {
      const it = artifactItem(a);
      if (DRV.sel && DRV.sel.artifact === a.id) it.classList.add('sel');
      return it;
    }) : [el('div', { class: 'empty' }, arts.length ? 'Nothing matches.' : 'Nothing published yet.')]));
  }
  renderDriveMain(M);
}
function renderDriveMain(M) {
  const main = $('drv-main');
  const sel = DRV.sel;
  const a = sel && M.artifacts.get(sel.artifact);
  const sig = sel ? sel.artifact + '|' + sel.version + '|' + sel.path + '|' + (a ? a.versions.length : -1) : 'none';
  if (sig === DRV.mainSig) return;
  DRV.mainSig = sig;
  if (!sel) { main.replaceChildren(el('div', { class: 'empty card' }, 'Choose an artifact.')); DRV.viewKey = null; return; }
  if (!a) { main.replaceChildren(el('div', { class: 'empty card' }, sel.artifact + (S.mode === 'replay' ? ' isn\'t published yet at this point of the run.' : ' isn\'t in this run.'))); DRV.viewKey = null; return; }
  const ver = a.versions.find((v) => v.version === sel.version) || a.versions[a.versions.length - 1];
  const files = ver.files;
  const isMd = (p) => /\.(md|markdown)$/i.test(p);
  const path = (sel.path && files.some((f) => f.path === sel.path) && sel.path) || (files.find((f) => isMd(f.path)) || files[0] || {}).path || null;
  const t = a.thread && M.threads.get(a.thread);
  const viewer = el('div', { class: 'viewer' });
  main.replaceChildren(el('div', { class: 'card' },
    el('div', { style: 'display:flex;gap:10px;align-items:center;flex-wrap:wrap' }, el('h2', { class: 'sec', style: 'margin:0' }, a.title || a.id), pill(a.kind, a.kind)),
    el('div', { class: 'muted small', style: 'margin:4px 0 8px' }, a.id + ' · by ' + a.author + ' · v' + ver.version + ' published ' + fmtDate(ver.ts) + (a.space ? ' · in ' + a.space : '')),
    ver.summary ? el('p', {}, ver.summary) : '',
    // Every version with its note of what changed, newest first.
    el('div', { class: 'vhist' }, [...a.versions].reverse().map((v) => el('div', { class: 'vrow' + (v === ver ? ' sel' : ''), onclick: () => openDrive(a.id, v.version, null), title: 'Open v' + v.version },
      el('span', { class: 'vnum' }, 'v' + v.version),
      el('span', { class: 'vnote' }, v.changes || (v.version === 1 ? 'First version.' : 'No note of what changed.')),
      el('span', { class: 'muted small' }, fmtDate(v.ts))))),
    t ? el('div', { class: 'small' }, 'For: ', threadLink(t.id)) : '',
    el('div', { class: 'files' }, files.map((f) => el('button', { class: 'chip' + (f.path === path ? ' sel' : ''), type: 'button', style: f.path === path ? 'border-color:var(--accent)' : '', onclick: () => openDrive(a.id, ver.version, f.path) }, f.path + ' · ' + fmtBytes(f.size))))),
  el('div', { style: 'margin-top:14px' }, viewer));
  if (path) showFile(viewer, a, ver, path);
}
// The symbols of an SVG sprite, each as a standalone image: the sprite
// itself with a <use> of the symbol, shown through <img> (no scripts, no
// outside resources).
function spriteIcons(text, css) {
  const doc = new DOMParser().parseFromString(text, 'image/svg+xml');
  const root = doc.documentElement;
  if (!root || root.nodeName !== 'svg' || doc.querySelector('parsererror')) return [];
  const syms = [...doc.querySelectorAll('symbol[id]')].slice(0, 200);
  if (!syms.length || root.querySelector(':scope > :not(defs):not(symbol):not(style):not(title):not(desc)')) return [];
  const NS = 'http://www.w3.org/2000/svg';
  return syms.map((sym) => {
    const svg = root.cloneNode(true);
    svg.removeAttribute('style');
    if (css) { const st = doc.createElementNS(NS, 'style'); st.textContent = css; svg.insertBefore(st, svg.firstChild); }
    svg.setAttribute('viewBox', sym.getAttribute('viewBox') || '0 0 24 24');
    svg.setAttribute('width', '64'); svg.setAttribute('height', '64');
    const use = doc.createElementNS(NS, 'use');
    use.setAttribute('href', '#' + sym.id);
    svg.appendChild(use);
    const blob = new Blob([new XMLSerializer().serializeToString(svg)], { type: 'image/svg+xml' });
    return { id: sym.id, url: URL.createObjectURL(blob) };
  });
}
const IMG_EXT = ['png', 'jpg', 'jpeg', 'gif', 'webp', 'svg', 'avif', 'bmp', 'ico'];
async function showFile(box, a, ver, path) {
  const key = S.run + '|' + a.id + '|' + ver.version + '|' + path;
  DRV.viewKey = key;
  const url = driveUrl(a.id, ver.version, path);
  if (!url) { box.replaceChildren(el('div', { class: 'empty card' }, 'This file has an unusual path and isn\'t shown.')); return; }
  const f = ver.files.find((x) => x.path === path) || {};
  const ext = (path.split('.').pop() || '').toLowerCase();
  const name = path.split('/').pop();
  const dl = () => { const x = el('a', { class: 'btn small', download: name }, 'Download ' + name); x.href = url; return x; };
  for (const u of DRV.blobs || []) URL.revokeObjectURL(u);
  DRV.blobs = [];
  if (ext === 'svg' && (f.size || 0) < 1024 * 1024) {
    // A sprite (symbols only) draws nothing as an image: show its symbols.
    try {
      const r = await fetch(url);
      // The artifact's own stylesheets style the symbols (classes, CSS
      // variables), as on the page they were made for.
      const css = [];
      for (const c of ver.files.filter((x) => /\.css$/i.test(x.path) && x.size < 512 * 1024).slice(0, 4)) {
        const cu = driveUrl(a.id, ver.version, c.path);
        const cr = cu ? await fetch(cu) : null;
        if (cr && cr.ok) css.push(await cr.text());
      }
      const icons = r.ok ? spriteIcons(await r.text(), css.join('\n')) : [];
      if (DRV.viewKey !== key) return;
      if (icons.length) {
        DRV.blobs = icons.map((i) => i.url);
        box.replaceChildren(el('div', { class: 'card' },
          el('div', { class: 'muted small', style: 'margin-bottom:8px' }, 'A sprite of ' + icons.length + ' symbols, each shown with its id' + (css.length ? ', styled by this version\'s CSS.' : '.')),
          el('div', { class: 'sprite' }, icons.map((i) => { const img = el('img', { alt: i.id, title: i.id }); img.src = i.url; return el('figure', {}, img, el('figcaption', {}, i.id)); }))),
          el('div', { style: 'margin-top:8px' }, dl()));
        return;
      }
    } catch (e) { /* shown as an image below */ }
  }
  if (IMG_EXT.includes(ext)) { const img = el('img', { alt: path, style: 'max-width:100%;border-radius:10px;background:var(--card)' }); img.src = url; box.replaceChildren(el('div', { class: 'card' }, img), el('div', { style: 'margin-top:8px' }, dl())); return; }
  if (f.size > 3 * 1024 * 1024) { box.replaceChildren(el('div', { class: 'card' }, el('p', { class: 'muted' }, 'This file is ' + fmtBytes(f.size) + '; download it to read it.'), dl())); return; }
  box.replaceChildren(el('div', { class: 'muted' }, 'Loading…'));
  let text;
  try {
    const r = await fetch(url);
    if (!r.ok) throw new Error('HTTP ' + r.status);
    text = await r.text();
  } catch (e) { if (DRV.viewKey === key) box.replaceChildren(el('div', { class: 'empty card' }, 'Could not load the file: ' + e.message)); return; }
  if (DRV.viewKey !== key) return;
  let view;
  if (text.slice(0, 8000).includes('\u0000')) view = el('div', { class: 'card' }, el('p', { class: 'muted' }, 'A binary file.'), dl());
  else if (/^(md|markdown)$/.test(ext)) {
    const dir = path.includes('/') ? path.slice(0, path.lastIndexOf('/') + 1) : '';
    const ctx = { artifact: a.id, version: ver.version, files: new Set(ver.files.map((x) => x.path)), base: dir };
    view = a.kind === 'deck' || a.kind === 'board-pack' ? slideViewer(text, ctx) : el('div', { class: 'card' }, renderMarkdown(text, ctx));
  } else if (ext === 'csv' || ext === 'tsv') view = el('div', { class: 'card csv' }, csvTable(text, ext === 'tsv' ? '\t' : ','));
  else if (ext === 'json') {
    let pretty = text;
    try { if (text.length < 1e6) pretty = JSON.stringify(JSON.parse(text), null, 2); } catch (e) { /* show as is */ }
    view = el('pre', { class: 'file' }, pretty);
  } else view = el('pre', { class: 'file' }, text);
  box.replaceChildren(view, el('div', { style: 'margin-top:8px' }, dl()));
  if (view.classList.contains('slides')) view.focus({ preventScroll: true });
}
function parseCsv(text, sep, maxRows) {
  const rows = [];
  let row = [], cell = '', q = false, i = 0;
  const n = text.length;
  while (i < n && rows.length < maxRows) {
    const c = text[i];
    if (q) {
      if (c === '"') { if (text[i + 1] === '"') { cell += '"'; i += 2; continue; } q = false; i++; continue; }
      cell += c; i++; continue;
    }
    if (c === '"' && cell === '') { q = true; i++; continue; }
    if (c === sep) { row.push(cell); cell = ''; i++; continue; }
    if (c === '\n' || c === '\r') { row.push(cell); rows.push(row); row = []; cell = ''; if (c === '\r' && text[i + 1] === '\n') i++; i++; continue; }
    cell += c; i++;
  }
  if ((cell || row.length) && rows.length < maxRows) { row.push(cell); rows.push(row); }
  return { rows, more: i < n };
}
function csvTable(text, sep) {
  const { rows, more } = parseCsv(text, sep, 501);
  if (!rows.length) return el('div', { class: 'muted' }, 'Empty.');
  const [head, ...body] = rows;
  return el('div', {},
    el('table', {}, el('thead', {}, el('tr', {}, head.slice(0, 200).map((h) => el('th', {}, clip(h, 200))))),
      el('tbody', {}, body.slice(0, 500).map((r) => el('tr', {}, head.slice(0, 200).map((_, k) => el('td', {}, clip(r[k] || '', 300))))))),
    more || body.length > 500 ? el('div', { class: 'muted small', style: 'margin-top:6px' }, 'Showing the first 500 rows.') : '');
}
