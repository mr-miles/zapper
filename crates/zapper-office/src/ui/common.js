'use strict';
// zapper office UI.
//
// Everything shown here comes from workers that may be hostile: mail,
// transcripts, published files, request reasons, even names. The page is
// built with createElement and textContent only; no HTML is ever parsed
// from data. The only URLs taken from data are http(s) links (shown with
// their target) and files of the shared drive, built from our own API path.

// ───────────────────────────── helpers ─────────────────────────────

const $ = (id) => document.getElementById(id);
const SVGNS = 'http://www.w3.org/2000/svg';
const REDUCED = window.matchMedia('(prefers-reduced-motion: reduce)');

// el('div', {class: 'x', onclick: fn, title: 't'}, 'text', child, [more])
// A worker's name as people read it: "barbara" is Barbara. Names stay
// lowercase as addresses (mail, tools); only what is shown is capitalised.
const cap = (n) => (typeof n === 'string' && /^[a-z]/.test(n) ? n[0].toUpperCase() + n.slice(1) : n);
const caps = (list) => list.map((n) => (n === BOARD ? boardName() : cap(n))).join(', ');
function el(tag, props, ...kids) {
  const e = document.createElement(tag);
  setProps(e, props);
  append(e, kids);
  return e;
}
function sv(tag, props, ...kids) {
  const e = document.createElementNS(SVGNS, tag);
  setProps(e, props);
  append(e, kids);
  return e;
}
function setProps(e, props) {
  if (!props) return;
  for (const [k, v] of Object.entries(props)) {
    if (v == null || v === false) continue;
    if (k === 'class') e.setAttribute('class', v);
    else if (k.startsWith('on') && typeof v === 'function') e.addEventListener(k.slice(2), v);
    else if (k === 'href' || k === 'src') throw new Error('set ' + k + ' explicitly'); // URLs only through safe helpers
    else if (k === 'value' || k === 'checked' || k === 'disabled' || k === 'selected') e[k] = v;
    else e.setAttribute(k, v === true ? '' : String(v));
  }
}
function append(e, kids) {
  for (const k of kids.flat(Infinity)) {
    if (k == null || k === false) continue;
    e.append(k instanceof Node ? k : document.createTextNode(String(k)));
  }
}
const str = (v) => (v == null ? '' : typeof v === 'string' ? v : String(v));
const clip = (s, n) => { s = str(s); return s.length > n ? s.slice(0, n - 1) + '…' : s; };
const firstLine = (s) => str(s).split('\n').find((l) => l.trim()) || '';
const nowSec = () => Date.now() / 1000;
const pad2 = (n) => String(n).padStart(2, '0');
function fmtTime(t) { if (!t) return ''; const d = new Date(t * 1000); return pad2(d.getHours()) + ':' + pad2(d.getMinutes()) + ':' + pad2(d.getSeconds()); }
function fmtDate(t) { if (!t) return ''; const d = new Date(t * 1000); return d.toLocaleDateString(undefined, { month: 'short', day: 'numeric' }) + ' ' + pad2(d.getHours()) + ':' + pad2(d.getMinutes()); }
function fmtDur(s) {
  if (s == null || !isFinite(s)) return '–';
  s = Math.max(0, s);
  if (s < 60) return Math.round(s) + 's';
  if (s < 3600) return Math.round(s / 60) + 'm';
  if (s < 86400) return (s / 3600).toFixed(s < 36000 ? 1 : 0) + 'h';
  return (s / 86400).toFixed(1) + 'd';
}
function ago(t) { return t ? fmtDur(T() - t) + ' ago' : ''; }
function fmtNum(n) { n = Number(n) || 0; return n >= 1e6 ? (n / 1e6).toFixed(1) + 'M' : n >= 1e4 ? (n / 1e3).toFixed(0) + 'k' : n >= 1e3 ? (n / 1e3).toFixed(1) + 'k' : String(Math.round(n)); }
function fmtBytes(n) { n = Number(n) || 0; return n < 1024 ? n + ' B' : n < 1048576 ? (n / 1024).toFixed(1) + ' KB' : (n / 1048576).toFixed(1) + ' MB'; }
function median(xs) { if (!xs.length) return null; const a = xs.slice().sort((x, y) => x - y); const m = a.length >> 1; return a.length % 2 ? a[m] : (a[m - 1] + a[m]) / 2; }
const pill = (cls, text) => el('span', { class: 'pill ' + (cls || '') }, text ?? cls);
const arr = (v) => (Array.isArray(v) ? v : v == null ? [] : [v]);
function opt(value, label, selected) { return el('option', { value, selected: !!selected }, label ?? value); }
function selectOf(options, value, props) {
  const s = el('select', props);
  for (const o of options) { const [v, l] = Array.isArray(o) ? o : [o, o]; s.append(opt(v, l, v === value)); }
  return s;
}
let toastTimer = null;
function toast(msg, bad) {
  const t = $('toast');
  t.textContent = str(msg);
  t.className = 'toast' + (bad ? ' bad' : '');
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => t.classList.add('hidden'), bad ? 7000 : 3000);
}
// A colour from config: only #rgb/#rrggbb(aa) is used, anything else falls back.
const safeColor = (c) => (typeof c === 'string' && /^#[0-9a-fA-F]{3,8}$/.test(c) ? c : '#D97757');

// ─────────────────────────────── api ───────────────────────────────

async function api(path, opts) {
  const r = await fetch('/api' + path, opts);
  const ct = r.headers.get('content-type') || '';
  let body = null;
  try { body = ct.includes('application/json') ? await r.json() : await r.text(); } catch (e) { body = null; }
  if (!r.ok) throw new Error((body && typeof body === 'object' && body.error) || (typeof body === 'string' && body.slice(0, 200)) || ('HTTP ' + r.status));
  return body;
}
function write(method, path, body) {
  return api(path, {
    method,
    headers: { 'X-Zapper-UI': '1', 'content-type': 'application/json' },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
}
const enc = encodeURIComponent;
const F = () => '/firms/' + enc(S.firm);
const runPath = (run) => F() + '/runs/' + enc(run || S.run);
// A file of the shared drive. Every part comes from the journal and is
// encoded segment by segment; '.' and '..' segments are refused.
function driveUrl(artifact, version, path) {
  const segs = str(path).split('/');
  if (!/^[\w.-]+$/.test(str(artifact)) || !/^\d+$/.test(str(version)) || segs.some((s) => !s || s === '.' || s === '..')) return null;
  return '/api' + runPath() + '/drive/' + enc(artifact) + '/' + enc(version) + '/' + segs.map(enc).join('/');
}
// Parses an ndjson body; bad lines are skipped.
function parseLines(text) {
  const out = [];
  for (const line of str(text).split('\n')) {
    if (!line.trim()) continue;
    try { const v = JSON.parse(line); if (v && typeof v === 'object') out.push(v); } catch (e) { /* partial or bad line */ }
  }
  return out;
}
// The bytes of complete lines in a text, to follow a file from there.
function completeBytes(text) {
  const i = text.lastIndexOf('\n');
  return i < 0 ? { text: '', bytes: 0 } : { text: text.slice(0, i + 1), bytes: new TextEncoder().encode(text.slice(0, i + 1)).length };
}
