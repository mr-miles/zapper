'use strict';
// The office page: the Statistics tab.

// ──────────────────────────── statistics ────────────────────────────

const STAT = { sig: null };
const STATE_ORDER = ['working', 'reviewing', 'idle', 'waiting', 'queued', 'booting', 'over-budget', 'error', 'off'];

function mountStats() {
  $('tab-stats').replaceChildren(el('div', { class: 'bar' }, el('span', { class: 'muted small', id: 'st-range' })), el('div', { id: 'st-body' }));
  STAT.sig = null;
}
function statRange(M) {
  const to = live() ? nowSec() : T();
  const from = M.started || to;
  return [from, Math.max(to, from + 1)];
}
function hbars(rows, opts) {
  opts = opts || {};
  const H = rows.length * 24 + 8, W = 720, L = 230, BW = 400;
  const max = Math.max(1, opts.budget || 0, ...rows.map((r) => r.value));
  const s = sv('svg', { class: 'chart bars', viewBox: `0 0 ${W} ${H}`, role: 'img', 'aria-label': opts.label || 'bar chart' });
  rows.forEach((r, i) => {
    const y = 4 + i * 24;
    const lbl = sv('text', { class: 'lbl', x: L - 8, y: y + 15, 'text-anchor': 'end' }); lbl.textContent = clip(r.label, 32);
    const w = (r.value / max) * BW;
    const val = sv('text', { x: L + Math.max(w, 0) + 6, y: y + 15 }); val.textContent = opts.fmt ? opts.fmt(r.value) : String(r.value);
    s.append(lbl, sv('rect', { class: 'track', x: L, y: y + 4, width: BW, height: 14, rx: 4 }), sv('rect', { class: opts.cls || 'barf', x: L, y: y + 4, width: Math.max(w, r.value ? 2 : 0), height: 14, rx: 4 }), val);
  });
  if (opts.budget) { const x = L + (opts.budget / max) * BW; s.append(sv('line', { class: 'budget', x1: x, y1: 0, x2: x, y2: H })); }
  return s;
}
// The team over the run: a lane per person from joining to leaving;
// founders, staff and contractors apart, a contract's negotiation hatched
// before its work.
function teamChart(M, from, to) {
  const people = [...M.workers.values()].filter((w) => w.inChart || w.left || w.hiredAt != null)
    .map((w) => {
      const k = w.contract && M.contracts.get(w.contract);
      const start = w.hiredAt != null ? w.hiredAt : from;
      const end = (w.left && w.left.ts) || (k && k.endedAt) || null;
      const accepted = k && (k.log.find((l) => l.ev === 'accepted') || {}).ts;
      return { w, k, start, end, accepted, kind: k ? 'contract' : w.hiredAt != null ? 'staff' : 'founder' };
    })
    .sort((a, b) => a.start - b.start || a.w.name.localeCompare(b.w.name));
  if (!people.length) return [];
  const span = to - from, W = 900, L = 250, BW = 630, RH = 24;
  const x = (t) => L + ((Math.min(Math.max(t, from), to) - from) / span) * BW;
  const svg = sv('svg', { class: 'chart team', viewBox: `0 0 ${W} ${people.length * RH + 26}`, role: 'img', 'aria-label': 'the team over the run' },
    sv('defs', {}, sv('pattern', { id: 'tm-hatch', width: 6, height: 6, patternUnits: 'userSpaceOnUse', patternTransform: 'rotate(45)' }, sv('rect', { class: 'tm-contract', width: 2.5, height: 6 }))));
  people.forEach((p, i) => {
    const y = i * RH + 4;
    const lbl = sv('text', { class: 'lbl', x: L - 8, y: y + 14, 'text-anchor': 'end' }); lbl.textContent = clip(who(M, p.w.name), 34) + (p.end ? ' ×' : '');
    svg.append(lbl, sv('rect', { class: 'track', x: L, y: y + 3, width: BW, height: 14, rx: 3 }));
    const bar = (a, b, cls, tip) => {
      if (b <= a) return;
      const r = sv('rect', { class: cls, x: x(a), y: y + 3, width: Math.max(1, x(b) - x(a)), height: 14, rx: 3 });
      const t = sv('title'); t.textContent = tip; r.append(t); svg.append(r);
    };
    const end = p.end || to;
    const tail = p.end ? ', left ' + fmtTime(p.end) + (p.w.left && p.w.left.reason ? ': ' + clip(p.w.left.reason, 120) : '') : '';
    if (p.k) {
      const work = p.accepted || null;
      bar(p.start, work || end, 'tm-nego', p.w.name + ': negotiating contract ' + p.k.id + ' (' + p.k.title + ') from ' + fmtTime(p.start) + (p.k.rounds ? ', ' + p.k.rounds + ' counter' + (p.k.rounds > 1 ? 's' : '') : '') + (work ? '' : tail));
      if (work) bar(work, end, 'tm-contract', p.w.name + ': contract ' + p.k.id + ' (' + p.k.title + ') ' + termsText(p.k.agreed) + ', from ' + fmtTime(work) + tail);
    } else {
      bar(p.start, end, 'tm-' + p.kind, who(M, p.w.name) + ': ' + (p.kind === 'founder' ? 'founder' : 'hired by ' + p.w.hiredBy + ' ' + fmtTime(p.start) + ' with ' + fmtTok(p.w.hireBudget)) + tail);
    }
  });
  const yAxis = people.length * RH + 20;
  for (let k = 0; k <= 4; k++) {
    const t = sv('text', { x: L + (k / 4) * BW, y: yAxis, 'text-anchor': k === 0 ? 'start' : k === 4 ? 'end' : 'middle' });
    t.textContent = span > 86400 ? fmtDate(from + (k / 4) * span) : fmtTime(from + (k / 4) * span);
    svg.append(t);
  }
  const now = people.filter((p) => !p.end);
  const sw = (cls) => sv('svg', { width: 10, height: 10 }, sv('rect', { class: cls, width: 10, height: 10, rx: 3 }));
  return [el('h2', { class: 'sec' }, 'Team'),
    el('div', { class: 'cards' },
      kpi('Headcount', String(now.filter((p) => p.kind !== 'contract').length), now.filter((p) => p.kind === 'contract').length + ' contractor' + (now.filter((p) => p.kind === 'contract').length === 1 ? '' : 's') + ' engaged'),
      kpi('Hired', String(people.filter((p) => p.kind === 'staff').length), people.filter((p) => p.kind === 'staff' && p.end).length + ' left since'),
      kpi('Contracts', String(people.filter((p) => p.kind === 'contract').length), people.filter((p) => p.k && p.k.state === 'completed').length + ' delivered')),
    el('div', { class: 'card', style: 'margin-top:12px' }, svg,
      el('div', { class: 'legend' }, el('span', {}, sw('tm-founder'), ' founder'), el('span', {}, sw('tm-staff'), ' staff'), el('span', {}, sw('tm-nego'), ' negotiating a contract'), el('span', {}, sw('tm-contract'), ' contractor'), el('span', {}, '× left')))];
}
function kpi(k, v, sub) { return el('div', { class: 'card kpi' }, el('div', { class: 'k' }, k), el('div', { class: 'v' }, v), sub ? el('div', { class: 'muted small' }, sub) : ''); }
// Terms in a line: "2.0M tokens, due Oct 17 18:00".
const termsText = (t) => (t ? [t.budget != null ? fmtTok(t.budget) + ' tokens' : 'budget open', t.due ? 'due ' + fmtDate(t.due) : ''].filter(Boolean).join(', ') : '');
// A worker's grants, for their panel: what they may use, and until when.
function grantsLine(M, w) {
  const m = M.grants.get(w.name);
  const gs = m ? [...m.entries()].filter(([, g]) => g.expires == null || g.expires > T()) : [];
  const asked = [...M.credReqs.values()].filter((q) => q.worker === w.name && (q.state === 'pending' || q.state === 'escalated'));
  return el('div', { class: 'small', style: 'margin-top:4px' }, el('b', {}, 'Access: '),
    gs.length ? gs.map(([r, g]) => r + ' ' + g.level + (g.expires ? ' (' + fmtDur(Math.max(0, g.expires - T())) + ' left)' : '')).join(' · ') : 'none',
    asked.length ? el('span', { class: 'muted' }, ' · asked: ' + asked.map((q) => q.resource + ' ' + q.level).join(', ')) : '');
}
// A contractor's contract, for their panel.
function contractLine(M, w) {
  const k = w.contract && M.contracts.get(w.contract);
  if (!k) return '';
  const t = k.agreed || k.quote || k.offer;
  return el('div', { class: 'small', style: 'margin-top:4px' },
    el('b', {}, 'Contractor'), ' for ' + k.by + ', contract ' + k.id + ' (' + k.title + '): ' + k.state + (t ? ' · ' + termsText(t) : '') + (k.rounds ? ' · ' + k.rounds + ' counter' + (k.rounds > 1 ? 's' : '') : '') + ' · ',
    threadLink(k.thread));
}
// "Barbara · CEO": a name with the worker's title, for tables and charts.
function who(M, n) {
  if (n === BOARD) return boardName() + ' · board';
  const w = M.workers.get(n);
  const t = w && (w.title || w.role);
  return t ? cap(n) + ' · ' + t : cap(n);
}
function updateStats() {
  const M = S.M;
  if (!M) return;
  const [from, to] = statRange(M);
  const sig = M.count + '|' + Math.floor(to / 15);
  if (sig === STAT.sig) return;
  STAT.sig = sig;
  $('st-range').textContent = fmtDate(from) + ' – ' + fmtDate(to);
  const inR = (ts) => ts >= from && ts <= to;
  // Everyone who worked in the run, those let go included.
  const ws = [...M.workers.values()].filter((w) => w.inChart || w.left || w.segs.length).sort((a, b) => (a.hiredAt ?? 0) - (b.hiredAt ?? 0));
  const body = [];
  if (!M.started) { $('st-body').replaceChildren(el('div', { class: 'empty card' }, 'No run yet.')); return; }

  // Activity.
  body.push(el('h2', { class: 'sec' }, 'Activity'));
  const span = to - from, W = 900, L = 250, BW = 630, RH = 24;
  const act = sv('svg', { class: 'chart', viewBox: `0 0 ${W} ${ws.length * RH + 26}`, role: 'img', 'aria-label': 'worker states over time' });
  const shares = [];
  ws.forEach((w, i) => {
    const y = i * RH + 4;
    const lbl = sv('text', { class: 'lbl', x: L - 8, y: y + 14, 'text-anchor': 'end' }); lbl.textContent = clip(who(M, w.name) + (w.left ? ' (left)' : ''), 34);
    act.append(lbl, sv('rect', { class: 's-off', x: L, y: y + 3, width: BW, height: 14, rx: 3 }));
    const tot = {};
    for (const s of w.segs) {
      const a = Math.max(from, s.from), b = Math.min(to, s.to == null ? to : s.to);
      if (b <= a) continue;
      tot[s.state] = (tot[s.state] || 0) + (b - a);
      const r = sv('rect', { class: 's-' + s.state, x: L + ((a - from) / span) * BW, y: y + 3, width: Math.max(0.5, ((b - a) / span) * BW), height: 14 });
      const tt = sv('title'); tt.textContent = who(M, w.name) + ': ' + s.state + ' ' + fmtTime(a) + '–' + fmtTime(b) + ' (' + fmtDur(b - a) + ')';
      r.append(tt); act.append(r);
    }
    shares.push([w.name, tot]);
  });
  const yAxis = ws.length * RH + 20;
  for (let k = 0; k <= 4; k++) {
    const x = L + (k / 4) * BW;
    const t = sv('text', { x, y: yAxis, 'text-anchor': k === 0 ? 'start' : k === 4 ? 'end' : 'middle' });
    t.textContent = span > 86400 ? fmtDate(from + (k / 4) * span) : fmtTime(from + (k / 4) * span);
    act.append(t);
  }
  const used = STATE_ORDER.filter((s) => shares.some(([, t]) => t[s]));
  body.push(el('div', { class: 'card' }, act,
    el('div', { class: 'legend' }, used.map((s) => el('span', {}, sv('svg', { width: 10, height: 10 }, sv('rect', { class: 's-' + s, width: 10, height: 10, rx: 3 })), ' ', s))),
    el('table', { class: 't', style: 'margin-top:12px' }, el('thead', {}, el('tr', {}, el('th', {}, 'share of time'), used.map((s) => el('th', {}, s)))),
      el('tbody', {}, shares.map(([n, t]) => { const sum = Object.values(t).reduce((x, y) => x + y, 0) || 1; return el('tr', {}, el('td', {}, who(M, n) + (M.workers.get(n) && M.workers.get(n).left ? ' (left)' : '')), used.map((s) => el('td', {}, t[s] ? Math.round((100 * t[s]) / sum) + '%' : '–'))); })))));

  body.push(...teamChart(M, from, to));

  // Usage.
  const turns = M.turns.filter((t) => inR(t.ts));
  const per = new Map(ws.map((w) => [w.name, { turns: 0, tokens: 0 }]));
  for (const t of turns) { const p = per.get(t.worker) || { turns: 0, tokens: 0 }; p.turns++; p.tokens += t.tokens; per.set(t.worker, p); }
  const totTurns = turns.length, totTokens = turns.reduce((x, t) => x + t.tokens, 0);
  const b = M.budget || (S.info && S.info.run && S.info.run.budget) || {};
  const usage = live() && S.info && S.info.run && S.info.run.usage;
  const rl = [...M.workers.values()].reduce((x, w) => x + w.rateLimited, 0), intr = [...M.workers.values()].reduce((x, w) => x + w.interrupted, 0);
  body.push(el('h2', { class: 'sec' }, 'Usage'));
  body.push(el('div', { class: 'cards' },
    kpi('Turns', fmtNum(totTurns), b.daily_turns ? 'daily budget ' + b.daily_turns : 'no daily budget'),
    kpi('Tokens', fmtNum(totTokens), b.daily_tokens ? 'daily budget ' + fmtNum(b.daily_tokens) : ''),
    usage ? kpi('Today (budget day)', usage.turns + (b.daily_turns ? ' / ' + b.daily_turns : ''), fmtNum(usage.tokens) + ' tokens' + (b.daily_tokens ? ' / ' + fmtNum(b.daily_tokens) : '')) : '',
    kpi('Rate-limit retries', String(rl), intr ? intr + ' interrupted turns' : ''),
    kpi('Over budget', M.overBudget ? 'yes' : 'no', b.max_concurrent ? 'agent parallelism ' + b.max_concurrent : '')));
  const urows = [...per.entries()].map(([n, p]) => ({ label: who(M, n), value: p.turns }));
  body.push(el('div', { class: 'grid2' },
    el('div', { class: 'card' }, el('h3', { style: 'margin:0 0 6px' }, 'Turns per worker'), hbars(urows, { label: 'turns per worker' })),
    el('div', { class: 'card' }, el('h3', { style: 'margin:0 0 6px' }, 'Tokens per worker'), hbars([...per.entries()].map(([n, p]) => ({ label: who(M, n), value: p.tokens })), { fmt: fmtNum, cls: 'barf2', label: 'tokens per worker' }))));
  const roles = new Map();
  for (const [n, p] of per) { const r = (M.workers.get(n) || {}).role || '?'; const x = roles.get(r) || 0; roles.set(r, x + p.turns); }
  if (roles.size > 1) body.push(el('div', { class: 'card', style: 'margin-top:16px' }, el('h3', { style: 'margin:0 0 6px' }, 'Turns per role'), hbars([...roles].map(([r, v]) => ({ label: r, value: v })), { label: 'turns per role' })));
  // Agents and models side by side: turns, tokens and tokens per turn.
  const byAgent = new Map();
  for (const [n, p] of per) {
    const w = M.workers.get(n) || {};
    const k = agentName(w.agent || 'claude-code') + (w.model ? ' · ' + modelName(w.model) : '');
    const a = byAgent.get(k) || { workers: 0, turns: 0, tokens: 0 };
    a.workers++; a.turns += p.turns; a.tokens += p.tokens;
    byAgent.set(k, a);
  }
  if (byAgent.size > 1) body.push(el('div', { class: 'card', style: 'margin-top:16px' }, el('h3', { style: 'margin:0 0 6px' }, 'By agent and model'),
    el('table', { class: 't' }, el('thead', {}, el('tr', {}, ['agent · model', 'workers', 'turns', 'tokens', 'tokens per turn'].map((h) => el('th', {}, h)))),
      el('tbody', {}, [...byAgent].map(([k, a]) => el('tr', {}, el('td', {}, k), el('td', {}, String(a.workers)), el('td', {}, String(a.turns)), el('td', {}, fmtNum(a.tokens)), el('td', {}, a.turns ? fmtNum(a.tokens / a.turns) : '—')))))));

  // Mail.
  const mails = [...M.mails.values()].filter((m) => inR(m.ts));
  const sent = new Map(), recv = new Map();
  for (const m of mails) { sent.set(m.from, (sent.get(m.from) || 0) + 1); for (const r of new Set([...m.to, ...m.cc])) recv.set(r, (recv.get(r) || 0) + 1); }
  const assignObl = [...M.obl.values()].filter((o) => o.kind === 'assignment');
  const opened = assignObl.filter((o) => inR(o.created)).length, closed = assignObl.filter((o) => o.settled && inR(o.settled.ts)).length;
  const replyTimes = M.settled.filter((s) => inR(s.ts)).map((s) => s.ts - s.o.created);
  const nowOverdue = openObl(M).filter((o) => overdue(o, to)).length;
  const followUps = mails.filter((m) => m.kind === 'follow-up').length;
  const escal = M.escalations.filter((e) => inR(e.ts)).length;
  body.push(el('h2', { class: 'sec' }, 'Mail'));
  body.push(el('div', { class: 'cards' }, kpi('Messages', String(mails.length)), kpi('Assignments opened', String(opened)), kpi('Assignments closed', String(closed)),
    kpi('Median time to answer', fmtDur(median(replyTimes)), replyTimes.length + ' answered'), kpi('Overdue now', String(nowOverdue)), kpi('Follow-ups', String(followUps)), kpi('Escalations', String(escal))));
  const people = [...new Set([...ws.map((w) => w.name), ...sent.keys(), ...recv.keys()])].filter((n) => n !== 'office');
  body.push(el('table', { class: 't' }, el('thead', {}, el('tr', {}, ['', 'sent', 'received', 'owes now', 'waits on now'].map((h) => el('th', {}, h)))),
    el('tbody', {}, people.map((n) => { const d = desk(M, n, to); return el('tr', {}, el('td', {}, who(M, n)), el('td', {}, String(sent.get(n) || 0)), el('td', {}, String(recv.get(n) || 0)), el('td', {}, String(d.owes.length)), el('td', {}, String(d.waits.length))); }))));

  // Output.
  const pubs = M.published.filter((p) => inR(p.ts));
  const kinds = [...new Set(pubs.map((p) => p.kind))].sort();
  const authors = [...new Set(pubs.map((p) => p.author))].sort();
  body.push(el('h2', { class: 'sec' }, 'Output'));
  body.push(el('div', { class: 'cards' }, kpi('Artifacts', String(new Set(pubs.map((p) => p.artifact)).size)), kpi('Versions', String(pubs.length))));
  if (pubs.length) body.push(el('table', { class: 't' }, el('thead', {}, el('tr', {}, el('th', {}, 'versions'), kinds.map((k) => el('th', {}, k)), el('th', {}, 'total'))),
    el('tbody', {}, authors.map((a) => el('tr', {}, el('td', {}, who(M, a)), kinds.map((k) => el('td', {}, String(pubs.filter((p) => p.author === a && p.kind === k).length || '–'))), el('td', {}, String(pubs.filter((p) => p.author === a).length)))))));

  // Security.
  const reqs = [...M.requests.values()].filter((r) => inR(Number(r.created) || 0));
  const cnt = (st) => reqs.filter((r) => r.state === st).length;
  const decisions = M.access.filter((a) => a.event === 'decided' && inR(a.ts) && a.request && a.request.decision);
  const byOfficer = new Map();
  for (const a of decisions) { const o = str(a.request.decision.by); byOfficer.set(o, (byOfficer.get(o) || 0) + 1); }
  const times = decisions.map((a) => Number(a.request.decision.at) - Number(a.request.created)).filter((x) => isFinite(x) && x >= 0);
  const added = decisions.filter((a) => a.request.decision.rule && a.request.decision.scope !== 'once').length;
  const revoked = M.access.filter((a) => a.event === 'revoked' && inR(a.ts)).length;
  body.push(el('h2', { class: 'sec' }, 'Security'));
  body.push(el('div', { class: 'cards' }, kpi('Requests', String(reqs.length)), kpi('Allowed', String(cnt('allowed'))), kpi('Denied', String(cnt('denied'))),
    kpi('Waiting', String(cnt('pending') + cnt('escalated'))), kpi('Median time to decide', fmtDur(median(times))), kpi('Standing added / revoked', added + ' / ' + revoked)));
  if (byOfficer.size) body.push(el('div', { class: 'card' }, el('h3', { style: 'margin:0 0 6px' }, 'Decisions per officer'), hbars([...byOfficer].map(([o, v]) => ({ label: who(M, o), value: v })), { label: 'decisions per officer' })));
  // Contracts: what the firm bought, and how well it negotiated.
  const ks = [...M.contracts.values()];
  if (ks.length) {
    body.push(el('h2', { class: 'sec' }, 'Contracts'));
    const agreed = ks.filter((k) => k.agreed);
    const saved = agreed.reduce((n, k) => n + Math.max(0, ((k.firstQuote && k.firstQuote.budget) || 0) - (k.agreed.budget || 0)), 0);
    body.push(el('div', { class: 'cards' },
      kpi('Offers', String(ks.length), ks.filter((k) => ['offered', 'quoted', 'countered'].includes(k.state)).length + ' negotiating'),
      kpi('Agreed', String(agreed.length), fmtTok(agreed.reduce((n, k) => n + (k.agreed.budget || 0), 0)) + ' tokens'),
      kpi('Delivered', String(ks.filter((k) => k.state === 'completed').length), ks.filter((k) => ['declined', 'withdrawn', 'lapsed'].includes(k.state)).length + ' fell through'),
      kpi('Negotiated down', fmtTok(saved), 'first quotes against agreed prices')));
    body.push(el('table', { class: 't', style: 'margin-top:12px' },
      el('thead', {}, el('tr', {}, ['contract', 'job', 'hirer', 'contractor', 'state', 'offered', 'first quote', 'agreed', 'counters', 'spent'].map((h) => el('th', {}, h)))),
      el('tbody', {}, ks.map((k) => {
        const w = M.workers.get(k.worker);
        return el('tr', {}, el('td', {}, k.id, ' ', threadLink(k.thread)), el('td', {}, k.title), el('td', {}, k.by), el('td', {}, k.worker), el('td', {}, k.state + (k.ended ? ': ' + clip(k.ended, 60) : '')),
          el('td', {}, termsText(k.firstOffer) || '—'), el('td', {}, termsText(k.firstQuote) || '—'), el('td', {}, termsText(k.agreed) || '—'), el('td', {}, String(k.rounds)), el('td', {}, w ? fmtTok(w.tokens) : '—'));
      }))));
  }
  $('st-body').replaceChildren(...body);
}
