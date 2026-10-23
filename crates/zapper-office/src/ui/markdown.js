'use strict';
// The office page: markdown and slides from untrusted text, built with createElement only.

// ───────────────────────────── markdown ─────────────────────────────
//
// A small, safe subset. It only ever makes nodes with createElement and
// text with createTextNode: raw HTML shows as text. Links are made only for
// http(s) (shown with their target) and drive: references; images only for
// files of the same artifact version. Size and nesting are bounded.
//
// ctx: { artifact, version, files: Set of paths, base: 'dir/' } for images;
// breaks: true turns single newlines into line breaks (mail).

const MD_MAX = 200 * 1024, MD_DEPTH = 6, MD_WINDOW = 4000;
const RE_FENCE = /^ {0,3}(`{3,}|~{3,})\s*([^`\s]*)[^`]*$/;
const RE_HEAD = /^ {0,3}(#{1,6})(?:[ \t]+(.*?))?(?:[ \t]+#+)?[ \t]*$/;
const RE_HR = /^ {0,3}(?:(?:\*[ \t]*){3,}|(?:-[ \t]*){3,}|(?:_[ \t]*){3,})$/;
const RE_QUOTE = /^ {0,3}> ?(.*)$/;
const RE_LIST = /^( {0,12})([-*+]|\d{1,9}[.)])[ \t]+(\S.*)$/;
const RE_TSEP = /^ {0,3}\|?[ \t]*:?-+:?[ \t]*(?:\|[ \t]*:?-+:?[ \t]*)*\|?[ \t]*$/;
const RE_DRIVE = /^drive:([A-Za-z0-9_-]+)(?:@v?(\d+))?(?:\/(.+))?$/;

// A leading YAML front matter block (Marp's `marp: true`, a document's
// title) is metadata, not content.
function stripFrontMatter(src) {
  const m = /^\uFEFF?---[ \t]*\r?\n[\s\S]*?\r?\n(?:---|\.\.\.)[ \t]*(?:\r?\n|$)/.exec(src);
  return m ? src.slice(m[0].length) : src;
}
function renderMarkdown(text, ctx) {
  ctx = Object.assign({}, ctx || {});
  let src = stripFrontMatter(str(text));
  let truncated = false;
  if (src.length > MD_MAX) { src = src.slice(0, MD_MAX); truncated = true; }
  src = src.replace(/\r\n?/g, '\n').replace(/\t/g, '    ');
  const root = el('div', { class: 'md' });
  append(root, mdBlocks(src.split('\n'), ctx, 0));
  if (truncated) root.append(el('p', { class: 'muted' }, '[truncated: longer than 200 KB]'));
  return root;
}
function startsBlock(l) {
  return RE_FENCE.test(l) || RE_HEAD.test(l) || RE_HR.test(l) || RE_QUOTE.test(l) || RE_LIST.test(l);
}
function mdBlocks(lines, ctx, depth) {
  const out = [];
  const n = lines.length;
  let i = 0, m;
  while (i < n) {
    const line = lines[i];
    if (!line.trim()) { i++; continue; }
    if ((m = RE_FENCE.exec(line))) {
      const ch = m[1][0], len = m[1].length, body = [];
      i++;
      while (i < n) {
        const t = lines[i].trim();
        if (t.length >= len && t[0] === ch && /^(`+|~+)$/.test(t)) { i++; break; }
        body.push(lines[i]); i++;
      }
      out.push(el('pre', {}, el('code', {}, body.join('\n'))));
    } else if ((m = RE_HEAD.exec(line))) {
      out.push(el('h' + m[1].length, {}, inline(m[2] || '', ctx, 0)));
      i++;
    } else if (RE_HR.test(line)) {
      out.push(el('hr')); i++;
    } else if (RE_QUOTE.test(line)) {
      const q = [];
      while (i < n && (m = RE_QUOTE.exec(lines[i]))) { q.push(m[1]); i++; }
      out.push(el('blockquote', {}, depth >= MD_DEPTH ? el('pre', {}, q.join('\n')) : mdBlocks(q, ctx, depth + 1)));
    } else if (line.includes('|') && i + 1 < n && lines[i + 1].includes('|') && lines[i + 1].includes('-') && RE_TSEP.test(lines[i + 1])) {
      const [t, next] = mdTable(lines, i, ctx);
      out.push(t); i = next;
    } else if (RE_LIST.test(line)) {
      const [list, next] = mdList(lines, i, ctx, depth);
      out.push(list); i = next;
    } else {
      const p = [line.trim()];
      i++;
      while (i < n && lines[i].trim() && !startsBlock(lines[i])) { p.push(lines[i].trim()); i++; }
      out.push(el('p', {}, inline(p.join('\n'), ctx, 0)));
    }
  }
  return out;
}
function splitRow(l) {
  let s = l.trim();
  if (s.startsWith('|')) s = s.slice(1);
  if (s.endsWith('|') && !s.endsWith('\\|')) s = s.slice(0, -1);
  const cells = [];
  let cur = '';
  for (let k = 0; k < s.length; k++) {
    if (s[k] === '\\' && s[k + 1] === '|') { cur += '|'; k++; } else if (s[k] === '|') { cells.push(cur.trim()); cur = ''; } else cur += s[k];
  }
  cells.push(cur.trim());
  return cells.slice(0, 50);
}
function mdTable(lines, i, ctx) {
  const head = splitRow(lines[i]);
  const aligns = splitRow(lines[i + 1]).map((c) => (/^:-+:$/.test(c) ? 'center' : /^-+:$/.test(c) ? 'right' : null));
  const cell = (tag, text, k) => { const c = el(tag, {}, inline(text, ctx, 0)); if (aligns[k]) c.style.textAlign = aligns[k]; return c; };
  const thead = el('thead', {}, el('tr', {}, head.map((h, k) => cell('th', h, k))));
  const tbody = el('tbody');
  i += 2;
  let rows = 0;
  while (i < lines.length && lines[i].trim() && lines[i].includes('|')) {
    if (rows++ < 1000) { const r = splitRow(lines[i]); tbody.append(el('tr', {}, head.map((_, k) => cell('td', r[k] || '', k)))); }
    i++;
  }
  return [el('table', {}, thead, tbody), i];
}
function mdList(lines, i, ctx, depth) {
  const first = RE_LIST.exec(lines[i]);
  const ordered = /\d/.test(first[2]);
  const base = first[1].length;
  const list = el(ordered ? 'ol' : 'ul');
  if (ordered) { const s = parseInt(first[2], 10); if (s !== 1 && s < 1e6) list.setAttribute('start', String(s)); }
  const items = [];
  let cur = null, loose = false;
  const sibling = (mm) => mm && mm[1].length <= base + 1 && /\d/.test(mm[2]) === ordered;
  while (i < lines.length) {
    const l = lines[i];
    const m = RE_LIST.exec(l);
    if (sibling(m)) { cur = { lines: [m[3]], indent: m[1].length + m[2].length + 1 }; items.push(cur); i++; continue; }
    if (!l.trim()) {
      let j = i + 1;
      while (j < lines.length && !lines[j].trim()) j++;
      if (j < lines.length && cur) {
        const nl = lines[j], ind = nl.length - nl.trimStart().length;
        if (sibling(RE_LIST.exec(nl)) || ind >= cur.indent) { cur.lines.push(''); loose = true; i++; continue; }
      }
      break;
    }
    const ind = l.length - l.trimStart().length;
    if (cur && ind >= Math.min(cur.indent, base + 2)) { cur.lines.push(l.slice(Math.min(ind, cur.indent))); i++; continue; }
    if (cur && cur.lines[cur.lines.length - 1] !== '' && !startsBlock(l)) { cur.lines.push(l.trim()); i++; continue; }
    break;
  }
  for (const it of items) {
    const li = el('li');
    if (depth >= MD_DEPTH) li.append(it.lines.join('\n'));
    else {
      const blocks = mdBlocks(it.lines, ctx, depth + 1);
      if (!loose && blocks.length && blocks[0].tagName === 'P') li.append(...Array.from(blocks.shift().childNodes));
      li.append(...blocks);
    }
    list.append(li);
  }
  return [list, i];
}
const isWordChar = (c) => !!c && /[\p{L}\p{N}]/u.test(c);
function findClose(s, from, mark) {
  const win = s.slice(from, from + MD_WINDOW);
  let k = 0;
  while ((k = win.indexOf(mark, k)) > -1) {
    if (k > 0 && !/\s/.test(win[k - 1])) {
      if (mark.length === 1 && win[k + 1] === mark) { k += 2; continue; }
      if (mark[0] === '_' && isWordChar(win[k + mark.length])) { k++; continue; }
      return from + k;
    }
    k++;
  }
  return -1;
}
// [text](url) at s[i] === '['; returns {text, url, end} or null.
function parseLinkAt(s, i) {
  let depth = 0, j = i;
  const lim = Math.min(s.length, i + 1000);
  for (; j < lim; j++) {
    if (s[j] === '\\') { j++; continue; }
    if (s[j] === '[') depth++;
    else if (s[j] === ']') { depth--; if (depth === 0) break; }
  }
  if (j >= lim || s[j] !== ']' || s[j + 1] !== '(') return null;
  let k = j + 2, pd = 1;
  const lim2 = Math.min(s.length, k + 2100);
  for (; k < lim2; k++) {
    if (s[k] === '\\') { k++; continue; }
    if (s[k] === '(') pd++;
    else if (s[k] === ')') { pd--; if (pd === 0) break; }
  }
  if (k >= lim2 || s[k] !== ')') return null;
  let inner = s.slice(j + 2, k).trim(), url;
  if (inner.startsWith('<')) { const e = inner.indexOf('>'); url = e > 0 ? inner.slice(1, e) : inner; } else url = inner.split(/\s+/)[0] || '';
  return { text: s.slice(i + 1, j), url, end: k + 1 };
}
function inline(s, ctx, depth) {
  s = str(s);
  if (depth > 8) return [document.createTextNode(s)];
  const out = [];
  let buf = '', i = 0;
  const flush = () => { if (buf) { out.push(document.createTextNode(buf)); buf = ''; } };
  while (i < s.length) {
    const c = s[i];
    if (c === '\\' && i + 1 < s.length && /[!-/:-@[-`{-~]/.test(s[i + 1])) { buf += s[i + 1]; i += 2; continue; }
    if (c === '\n') { if (ctx.breaks) { flush(); out.push(el('br')); } else buf += ' '; i++; continue; }
    if (c === '`') {
      let k = i;
      while (s[k] === '`') k++;
      const ticks = s.slice(i, k);
      const rel = s.slice(k, k + MD_WINDOW).indexOf(ticks);
      if (rel > -1) {
        flush();
        let code = s.slice(k, k + rel).replace(/\n/g, ' ');
        if (code.length > 2 && code[0] === ' ' && code[code.length - 1] === ' ') code = code.slice(1, -1);
        out.push(el('code', {}, code));
        i = k + rel + ticks.length;
        continue;
      }
      buf += ticks; i = k; continue;
    }
    if (c === '!' && s[i + 1] === '[') {
      const r = parseLinkAt(s, i + 1);
      if (r) { flush(); out.push(mdImage(r.text, r.url, ctx)); i = r.end; continue; }
    }
    if (c === '[' && !ctx.inLink) {
      const r = parseLinkAt(s, i);
      if (r) { flush(); out.push(mdLink(r.url, () => inline(r.text, Object.assign({}, ctx, { inLink: true }), depth + 1), r.text)); i = r.end; continue; }
    }
    if (c === '<' && !ctx.inLink) {
      const m = /^<(https?:\/\/[^\s<>]+)>/i.exec(s.slice(i, i + 2100));
      if (m) { flush(); out.push(mdLink(m[1], () => [document.createTextNode(m[1])], null, true)); i += m[0].length; continue; }
    }
    if ((c === 'h' || c === 'H') && !ctx.inLink && (i === 0 || /[\s(]/.test(s[i - 1]))) {
      const m = /^https?:\/\/[^\s<>()[\]]+/i.exec(s.slice(i, i + 2100));
      if (m) {
        const u = m[0].replace(/[.,;:!?'"*_]+$/, '');
        flush(); out.push(mdLink(u, () => [document.createTextNode(u)], null, true)); i += u.length; continue;
      }
    }
    if (c === '*' || c === '_' || c === '~') {
      const two = s[i + 1] === c;
      if (c === '~' && !two) { buf += c; i++; continue; }
      const mark = two ? c + c : c;
      if (c === '_' && isWordChar(s[i - 1])) { buf += mark; i += mark.length; continue; }
      const startC = s[i + mark.length];
      if (startC && !/\s/.test(startC)) {
        const end = findClose(s, i + mark.length, mark);
        if (end > -1) {
          flush();
          out.push(el(c === '~' ? 'del' : two ? 'strong' : 'em', {}, inline(s.slice(i + mark.length, end), ctx, depth + 1)));
          i = end + mark.length;
          continue;
        }
      }
      buf += mark; i += mark.length; continue;
    }
    buf += c; i++;
  }
  flush();
  return out;
}
function parseDriveRef(url) {
  const m = RE_DRIVE.exec(str(url).trim());
  return m ? { artifact: m[1], version: m[2] ? Number(m[2]) : null, path: m[3] || null } : null;
}
// A link: http(s) opens in a new tab and shows its target, drive: opens the
// Shared drive tab, anything else stays text.
function mdLink(url, kids, rawText, auto) {
  url = str(url).trim();
  const d = parseDriveRef(url);
  if (d) {
    return el('span', {}, el('button', { class: 'linkish', type: 'button', title: url, onclick: (ev) => { ev.preventDefault(); openDrive(d.artifact, d.version, d.path); } }, kids()),
      el('span', { class: 'url' }, '(' + url + ')'));
  }
  if (/^https?:\/\//i.test(url)) {
    let u = null;
    try { u = new URL(url); } catch (e) { u = null; }
    if (u && (u.protocol === 'http:' || u.protocol === 'https:')) {
      const a = el('a', { target: '_blank', rel: 'noopener noreferrer', title: u.href }, kids());
      a.href = u.href;
      const label = a.textContent;
      return el('span', {}, a, auto || label === url || label === u.href ? '' : el('span', { class: 'url' }, '(' + clip(u.href, 140) + ')'));
    }
  }
  return document.createTextNode(rawText != null ? '[' + rawText + '](' + url + ')' : url);
}
function resolvePath(base, rel) {
  let p = rel;
  try { p = decodeURIComponent(rel); } catch (e) { p = rel; }
  const out = [];
  for (const seg of (base + p).split('/')) {
    if (!seg || seg === '.') continue;
    if (seg === '..') { if (!out.length) return null; out.pop(); } else out.push(seg);
  }
  return out.join('/');
}
// An image: only a relative path to a file of the same artifact version.
function mdImage(alt, src, ctx) {
  src = str(src).trim();
  if (ctx.artifact && ctx.files && src && !/^[a-zA-Z][a-zA-Z0-9+.-]*:/.test(src) && !src.startsWith('/') && !src.startsWith('\\')) {
    const path = resolvePath(ctx.base || '', src.split(/[?#]/)[0]);
    if (path && ctx.files.has(path)) {
      const url = driveUrl(ctx.artifact, ctx.version, path);
      if (url) { const img = el('img', { alt: alt, loading: 'lazy' }); img.src = url; return img; }
    }
  }
  return el('span', { class: 'imgtext' }, '[image: ' + alt + (src ? ' → ' + src : '') + ']');
}

// ─────────────────────────── slide viewer ───────────────────────────

// Slides are separated by `---` lines, as in Marp; its front matter and
// its directives and speaker notes (HTML comments) aren't shown.
function splitSlides(text) {
  const lines = stripFrontMatter(str(text).slice(0, MD_MAX).replace(/\r\n?/g, '\n')).split('\n');
  const slides = [[]];
  let fence = null, comment = false;
  for (const l of lines) {
    if (comment) { if (l.includes('-->')) comment = false; continue; }
    if (fence) { const t = l.trim(); if (t.length >= fence.len && t[0] === fence.ch && /^(`+|~+)$/.test(t)) fence = null; }
    else {
      const m = RE_FENCE.exec(l);
      if (m) fence = { ch: m[1][0], len: m[1].length };
      else if (/^---\s*$/.test(l)) { slides.push([]); continue; }
      else if (/^\s*<!--/.test(l)) { if (!/-->\s*$/.test(l)) comment = true; continue; }
    }
    slides[slides.length - 1].push(l);
  }
  return slides.map((s) => s.join('\n')).filter((s) => s.trim()).slice(0, 500);
}
function slideViewer(text, ctx) {
  const slides = splitSlides(text);
  if (!slides.length) slides.push('');
  let i = 0;
  const stage = el('div', { class: 'slide md' });
  const n = el('span', { class: 'n' });
  const prev = el('button', { class: 'btn small', type: 'button', onclick: () => go(i - 1) }, '← Previous');
  const next = el('button', { class: 'btn small', type: 'button', onclick: () => go(i + 1) }, 'Next →');
  const box = el('div', { class: 'slides', tabindex: 0, 'aria-label': 'Slides; use the arrow keys',
    onkeydown: (ev) => {
      const k = ev.key;
      if (k === 'ArrowRight' || k === 'PageDown' || k === ' ') go(i + 1);
      else if (k === 'ArrowLeft' || k === 'PageUp') go(i - 1);
      else if (k === 'Home') go(0);
      else if (k === 'End') go(slides.length - 1);
      else return;
      ev.preventDefault();
    },
    onclick: (ev) => { if (!ev.target.closest('a,button')) box.focus({ preventScroll: true }); } },
  stage, el('div', { class: 'sctl' }, prev, n, next));
  function go(k) {
    i = Math.max(0, Math.min(slides.length - 1, k));
    stage.replaceChildren(...Array.from(renderMarkdown(slides[i], ctx).childNodes));
    n.textContent = 'Slide ' + (i + 1) + ' of ' + slides.length;
    prev.disabled = i === 0; next.disabled = i === slides.length - 1;
  }
  go(0);
  return box;
}
