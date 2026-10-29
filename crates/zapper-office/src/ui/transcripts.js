'use strict';
// The office page: a worker's transcript, followed live or replayed.

// A turn that writes this much to the cache is flagged: the cost of
// caching the whole conversation again.
const RECACHE_TOKENS = 20000;

// ──────────────────────────── transcripts ────────────────────────────

function closeTranscript() {
  if (PANEL.tr) { PANEL.tr.closed = true; if (PANEL.tr.es) PANEL.tr.es.close(); }
  PANEL.tr = null;
}
async function openTranscript(name) {
  const tr = { name, events: [], seen: new Set(), es: null, all: false, closed: false, shown: -1, gen: S.gen };
  PANEL.tr = tr;
  PANEL.feed.replaceChildren(el('div', { class: 'muted' }, 'Loading…'));
  const path = runPath() + '/transcripts/' + enc(name);
  let bytes = 0;
  try {
    const text = await api(path);
    const c = completeBytes(typeof text === 'string' ? text : '');
    bytes = c.bytes;
    for (const e of parseLines(c.text)) addTr(tr, e);
  } catch (e) { /* none yet */ }
  if (tr.closed) return;
  renderFeed();
  if (live()) {
    tr.es = new EventSource('/api' + path + '?follow=true&from=' + bytes);
    tr.es.onmessage = (ev) => {
      if (tr.closed) return;
      let e; try { e = JSON.parse(ev.data); } catch (x) { return; }
      if (addTr(tr, e)) appendFeed(e);
    };
  }
}
function addTr(tr, e) {
  if (!e || typeof e !== 'object') return false;
  if (e.seq != null) { if (tr.seen.has(e.seq)) return false; tr.seen.add(e.seq); }
  tr.events.push(e);
  if (tr.events.length > 5000) tr.events.splice(0, tr.events.length - 5000);
  return true;
}
function visibleTr(tr) {
  let evs = tr.events;
  if (S.mode === 'replay') { const pos = T(); evs = evs.filter((e) => !e.ts || e.ts <= pos); }
  if (!tr.all) {
    let i = evs.length - 1;
    while (i > 0 && evs[i].type !== 'wake') i--;
    evs = evs.slice(Math.max(0, i));
  }
  return evs.slice(-1500);
}
function renderFeed() {
  const tr = PANEL.tr;
  if (!tr) return;
  const evs = visibleTr(tr);
  const key = evs.length + ':' + (evs.length ? evs[evs.length - 1].seq : '') + ':' + tr.all;
  if (key === tr.shown) return;
  tr.shown = key;
  PANEL.earlier.textContent = tr.all ? 'Current turn only' : 'Show earlier turns';
  // Newest first.
  const nodes = [];
  for (let i = evs.length - 1; i >= 0; i--) nodes.push(...feedNodes(evs[i]).reverse());
  PANEL.feed.replaceChildren(...(nodes.length ? nodes : [el('div', { class: 'muted' }, S.mode === 'replay' ? 'No turn yet at this point.' : 'No turn yet.')]));
  PANEL.feed.scrollTop = 0;
}
function appendFeed(e) {
  const tr = PANEL.tr;
  if (!tr) return;
  if (e.type === 'wake' && !tr.all) { tr.shown = -1; renderFeed(); return; }
  // New events go on top; someone reading further down stays where they are.
  const top = PANEL.feed.scrollTop < 60;
  const before = PANEL.feed.scrollHeight;
  if (PANEL.feed.firstChild && PANEL.feed.firstChild.classList && PANEL.feed.firstChild.classList.contains('muted')) PANEL.feed.replaceChildren();
  PANEL.feed.prepend(...feedNodes(e).reverse());
  while (PANEL.feed.childElementCount > 2000) PANEL.feed.lastChild.remove();
  if (top) PANEL.feed.scrollTop = 0;
  else PANEL.feed.scrollTop += PANEL.feed.scrollHeight - before;
}
function toolLine(name, input) {
  const short = str(name).replace(/^mcp__office__/, 'office.').replace(/^mcp__([^_]+)__/, '$1.');
  const i = input && typeof input === 'object' ? input : {};
  for (const k of ['command', 'file_path', 'path', 'paths', 'pattern', 'url', 'query', 'to', 'subject', 'title', 'artifact', 'host', 'description', 'prompt', 'content', 'body']) {
    if (i[k] == null) continue;
    const v = Array.isArray(i[k]) ? i[k].map(str).join(', ') : typeof i[k] === 'object' ? JSON.stringify(i[k]) : str(i[k]);
    return [short, clip(firstLine(v), 140)];
  }
  const s = Object.values(i).find((v) => typeof v === 'string');
  return [short, s ? clip(firstLine(s), 140) : ''];
}
function resultText(c) {
  if (typeof c === 'string') return c;
  if (Array.isArray(c)) return c.map((x) => (x && typeof x === 'object' ? (x.type === 'text' ? str(x.text) : '[' + str(x.type) + ']') : str(x))).join('\n');
  return c == null ? '' : JSON.stringify(c);
}
function feedNodes(e) {
  const out = feedEntries(e);
  if (e.ts) {
    for (const n of out) {
      const t = el('span', { class: 'when' }, fmtTime(e.ts));
      const at = n.tagName === 'DETAILS' ? n.querySelector('summary') : n;
      at.prepend(t);
    }
  }
  return out;
}
function feedEntries(e) {
  const out = [];
  switch (e.type) {
    case 'wake': {
      out.push(el('details', { class: 'tx turn' }, el('summary', {}, 'Woken: ', clip(firstLine(e.text), 100)), el('pre', {}, clip(e.text, 20000))));
      break;
    }
    case 'system':
      if (e.subtype === 'init') out.push(el('div', { class: 'tx res' }, 'Claude started' + (e.model ? ' (' + str(e.model) + ')' : '')));
      break;
    case 'assistant': {
      const content = e.message && Array.isArray(e.message.content) ? e.message.content : [];
      for (const c of content) {
        if (!c || typeof c !== 'object') continue;
        if (c.type === 'text' && str(c.text).trim()) out.push(el('div', { class: 'tx text' }, renderMarkdown(clip(c.text, 30000), {})));
        else if (c.type === 'tool_use') {
          const [n, arg] = toolLine(c.name, c.input);
          out.push(el('details', { class: 'tx tool' }, el('summary', {}, '▸ ', el('b', {}, n), arg ? ' ' + arg : ''), el('pre', {}, clip(JSON.stringify(c.input, null, 2), 20000))));
        } else if (c.type === 'thinking') out.push(el('div', { class: 'tx res' }, 'thinking…'));
      }
      break;
    }
    case 'user': {
      const content = e.message && Array.isArray(e.message.content) ? e.message.content : [];
      for (const c of content) {
        if (!c || c.type !== 'tool_result') continue;
        const text = resultText(c.content);
        out.push(el('details', { class: 'tx res' + (c.is_error ? ' err' : '') }, el('summary', {}, c.is_error ? 'error: ' + clip(firstLine(text), 120) : 'result (' + fmtNum(text.length) + ' chars)'), el('pre', {}, clip(text, 20000))));
      }
      break;
    }
    case 'result': {
      // What the budget is charged: input, output and cache writes (the
      // harness's count); cache reads are shown, not charged.
      const u = e.usage || {}, n = (k) => Number(u[k]) || 0;
      const input = n('input_tokens'), output = n('output_tokens'), writes = n('cache_creation_input_tokens'), reads = n('cache_read_input_tokens');
      const tok = input + output + writes;
      const bits = ['Turn finished'];
      if (e.num_turns) bits.push(e.num_turns + ' steps');
      if (tok) bits.push(fmtNum(tok) + ' tokens');
      if (e.duration_ms) bits.push(fmtDur(e.duration_ms / 1000));
      const split = 'charged: ' + fmtNum(input) + ' input, ' + fmtNum(output) + ' output, ' + fmtNum(writes) + ' cache writes; not charged: ' + fmtNum(reads) + ' cache reads';
      out.push(el('div', { class: 'tx turn' + (e.is_error ? ' err' : ''), title: split }, bits.join(' · '), e.is_error && e.result ? el('pre', {}, clip(str(e.result), 4000)) : ''));
      if (writes >= RECACHE_TOKENS) out.push(el('div', { class: 'tx warn' }, 'Re-cached ' + fmtTok(writes) + ' tokens of context, charged to the budget. Claude keeps a conversation cached for about five minutes unused; after a longer pause, a fresh session or a compaction, it is cached again.'));
      break;
    }
    // pi's events (agent.kind: pi).
    case 'tool_execution_start': {
      const [n, arg] = toolLine(str(e.toolName), e.args || {});
      out.push(el('details', { class: 'tx tool' }, el('summary', {}, '▸ ', el('b', {}, n), arg ? ' ' + arg : ''), el('pre', {}, clip(JSON.stringify(e.args || {}, null, 2), 20000))));
      break;
    }
    case 'tool_execution_end': {
      const text = resultText(e.result && e.result.content);
      out.push(el('details', { class: 'tx res' + (e.isError ? ' err' : '') }, el('summary', {}, e.isError ? 'error: ' + clip(firstLine(text), 120) : 'result (' + fmtNum(text.length) + ' chars)'), el('pre', {}, clip(text, 20000))));
      break;
    }
    case 'message_end': {
      const m = e.message || {};
      if (m.role !== 'assistant') break;
      for (const c of Array.isArray(m.content) ? m.content : []) {
        if (!c || typeof c !== 'object') continue;
        if (c.type === 'text' && str(c.text).trim()) out.push(el('div', { class: 'tx text' }, renderMarkdown(clip(c.text, 30000), {})));
        else if (c.type === 'thinking') out.push(el('div', { class: 'tx res' }, 'thinking…'));
      }
      if (m.stopReason === 'error') out.push(el('div', { class: 'tx turn err' }, 'Model request failed', el('pre', {}, clip(str(m.errorMessage), 4000))));
      break;
    }
    case 'agent_settled': out.push(el('div', { class: 'tx turn' + (e.aborted ? ' err' : '') }, e.aborted ? 'Turn aborted' : 'Turn finished')); break;
    case 'stderr': out.push(el('details', { class: 'tx err' }, el('summary', {}, 'stderr: ' + clip(firstLine(e.text), 120)), el('pre', {}, clip(e.text, 8000)))); break;
    case 'raw': out.push(el('details', { class: 'tx res' }, el('summary', {}, 'output: ' + clip(firstLine(e.text), 120)), el('pre', {}, clip(e.text, 8000)))); break;
    default: break;
  }
  return out;
}
