'use strict';
// The office page: the Work and People tabs.

// ─────────────────────────────── work ───────────────────────────────
//
// Assignments as tickets, derived from the mail: an assignment is a ticket
// for each recipient, a report closes it, and the thread it was sent from
// makes it a sub-task. Nothing here is written by workers.

const WORK = { view: 'board', owner: '', sig: null };
const WORK_COLS = [['todo', 'To do'], ['doing', 'In progress'], ['waiting', 'Waiting on others'], ['done', 'Done']];

function tickets(M, now) {
  const list = [...M.obl.values()].filter((o) => o.expects === 'done').map((o) => {
    const t = M.threads.get(o.thread) || {};
    const m = M.mails.get(o.message) || {};
    return { o, id: o.key, thread: o.thread, message: o.message, owner: o.debtor, by: o.creditor, subject: m.subject || t.subject || '(no subject)',
      created: o.created, due: o.due, end: o.settled ? o.settled.ts : null, parent: null, kids: [] };
  });
  const byThread = new Map();
  for (const k of list) { if (!byThread.has(k.thread)) byThread.set(k.thread, []); byThread.get(k.thread).push(k); }
  // The parent: in the thread the assignment was sent from, the ticket its
  // sender owed there (or any ticket of that thread).
  for (const k of list) {
    const t = M.threads.get(k.thread);
    const cands = t && t.context && t.context !== k.thread ? byThread.get(t.context) || [] : [];
    const p = cands.find((x) => x.owner === k.by) || cands[0];
    if (p && p !== k) { k.parent = p; p.kids.push(k); }
  }
  for (const k of list) {
    const w = M.workers.get(k.owner);
    k.overdue = !k.end && k.due && now > k.due;
    k.working = !k.end && !!(w && w.inTurn && w.turnThreads.has(k.thread));
    k.openKids = k.kids.filter((x) => !x.end).length;
    k.status = k.end ? 'done' : k.working ? 'doing' : k.openKids ? 'waiting' : w && w.unread.has(k.message) ? 'todo' : 'doing';
  }
  return list;
}

// ─────────────────────────── People: hiring, firing, and what they said ───────────────────────────

const PPL = { sig: null, open: new Set() };
function mountPeople() {
  $('tab-people').replaceChildren(el('div', { id: 'ppl-body' }));
  PPL.sig = null;
}
const involves = (m, n) => m.from === n || m.to.includes(n) || m.cc.includes(n);

// Everything that changed who is in the firm, newest first: hires,
// background checks, contracts, departures and the scenario's actions.
function peopleLog(M) {
  const log = [];
  for (const w of M.workers.values()) {
    if (w.hiredAt != null) {
      log.push({ ts: w.hiredAt, kind: 'hired', who: w.name, text: cap(w.name) + ' hired by ' + who(M, w.hiredBy) + ' as ' + (w.title || w.role) + (w.contract ? ' (contractor, ' + w.contract + ')' : '') + ', budget ' + fmtTok(w.hireBudget), note: w.hireReason });
    }
    if (w.left) {
      log.push({ ts: w.left.ts, kind: 'left', who: w.name, text: cap(w.name) + ' let go by ' + (w.left.by === BOARD ? 'the board' : who(M, w.left.by)) + (w.left.returned ? '; ' + fmtTok(w.left.returned) + ' tokens back to ' + who(M, w.left.to) : ''), note: w.left.reason });
    }
  }
  for (const c of M.screenings.values()) {
    if (c.state === 'pending') continue;
    log.push({ ts: c.decided || c.ts, kind: c.state, who: c.worker, text: 'Background check of ' + cap(c.worker) + ' ' + c.state + ' by ' + (c.decidedBy === BOARD ? 'the board' : who(M, c.decidedBy)), note: c.note });
  }
  for (const c of M.contracts.values()) {
    for (const l of arr(c.log)) log.push({ ts: l.ts, kind: 'contract', who: c.worker, text: 'Contract ' + c.id + ' (' + cap(c.worker) + ', ' + c.title + '): ' + l.ev + (l.by ? ' by ' + who(M, l.by) : ''), note: l.note || '' });
  }
  for (const x of M.scenarioLog) {
    const text = x.event === 'debrief' ? cap(x.worker) + ' gets a last turn to debrief the board' : x.event === 'hire' ? 'Scenario slot filled by ' + cap(x.worker) : x.event === 'step' ? 'Scenario step ' + x.index + ' played' : x.event === 'waiting' ? 'Scenario step ' + x.index + ' waits' : x.event === 'failed' ? 'Scenario step ' + x.index + ' failed' : x.event === 'released' ? 'Scenario slot freed by ' + cap(x.worker) : 'Scenario ' + x.event;
    log.push({ ts: x.ts, kind: 'scenario', who: x.worker, text, note: x.why || (x.what ? JSON.stringify(x.what).slice(0, 300) : '') });
  }
  return log.sort((a, b) => b.ts - a.ts);
}

function personCard(M, w, now) {
  const mails = [...M.mails.values()].filter((m) => involves(m, w.name)).sort((a, b) => a.ts - b.ts);
  const status = w.left ? pill('closed', 'left') : w.hiredAt == null ? pill('', 'founder') : pill('open', 'active');
  const box = el('div', { class: 'ppl-mail' });
  const fill = () => { box.replaceChildren(...mails.map((m) => messageCard(M, m, now))); };
  const det = el('details', { open: PPL.open.has(w.name), ontoggle: () => { if (det.open) { PPL.open.add(w.name); fill(); } else { PPL.open.delete(w.name); box.replaceChildren(); } } },
    el('summary', { class: 'small' }, 'Communication (' + mails.length + ')'), box);
  if (det.open) fill();
  return el('div', { class: 'card ppl' },
    el('div', { class: 'row', style: 'display:flex;gap:8px;align-items:center;flex-wrap:wrap' }, miniAvatar(w.name), el('b', {}, cap(w.name)), el('span', { class: 'muted' }, w.title || w.role), status, w.contract ? pill('', 'contractor') : '',
      w.officer ? pill('', 'security') : ''),
    el('div', { class: 'small muted' }, w.hiredAt == null ? 'Founder' : 'Hired ' + fmtDate(w.hiredAt) + ' by ' + who(M, w.hiredBy) + ' · budget ' + fmtTok(w.hireBudget) + (w.hireReason ? ' · ' + clip(w.hireReason, 160) : '')),
    w.left ? el('div', { class: 'small' }, 'Left ' + fmtDate(w.left.ts) + ', let go by ' + (w.left.by === BOARD ? 'the board' : who(M, w.left.by)) + ': ' + clip(w.left.reason, 240)) : '',
    det);
}

function updatePeople() {
  const M = S.M;
  if (!M || !$('ppl-body')) return;
  const now = T();
  const sig = [M.count, S.run].join('|');
  if (sig === PPL.sig) return;
  PPL.sig = sig;
  const log = peopleLog(M);
  const everyone = [...M.workers.values()].filter((w) => w.inChart || w.left || w.hiredAt != null).sort((a, b) => (a.hiredAt ?? 0) - (b.hiredAt ?? 0));
  const hired = everyone.filter((w) => w.hiredAt != null).length, left = everyone.filter((w) => w.left).length;
  $('ppl-body').replaceChildren(
    el('div', { class: 'muted small', style: 'margin:6px 0' }, `${everyone.length} people in this run · ${hired} hired · ${left} let go · ${everyone.length - left} in the firm now`),
    el('h2', { class: 'sec' }, 'Hiring and firing'),
    log.length ? el('div', { class: 'card' }, log.map((l) => el('div', { class: 'row', style: 'padding:4px 0;border-bottom:1px solid var(--line)' },
      el('span', { class: 'muted small', style: 'display:inline-block;width:110px' }, fmtDate(l.ts)), pill(l.kind === 'hired' || l.kind === 'cleared' ? 'open' : l.kind === 'left' || l.kind === 'objected' ? 'error' : '', l.kind === 'cleared' ? 'approved' : l.kind), ' ', l.text,
      l.note ? el('div', { class: 'small muted', style: 'margin-left:120px' }, clip(l.note, 400)) : ''))) : el('div', { class: 'empty card' }, 'Nobody has been hired or let go yet.'),
    el('h2', { class: 'sec' }, 'People'),
    ...everyone.map((w) => personCard(M, w, now)));
}

function mountWork() {
  const seg = el('div', { class: 'seg', id: 'wk-view' });
  for (const [v, l] of [['board', 'Kanban'], ['timeline', 'Timeline']]) {
    const b = el('button', { type: 'button', onclick: () => { WORK.view = v; WORK.sig = null; updateWork(); } }, l);
    b.dataset.v = v; seg.append(b);
  }
  const owner = el('select', { id: 'wk-owner', onchange: (e) => { WORK.owner = e.target.value; WORK.sig = null; updateWork(); } });
  $('tab-work').replaceChildren(el('div', { class: 'bar' }, seg, owner, el('span', { class: 'muted small', id: 'wk-sum' })), el('div', { id: 'wk-body' }));
  WORK.sig = null;
}

function updateWork() {
  const M = S.M;
  if (!M || !$('wk-body')) return;
  const now = T();
  for (const b of $('wk-view').children) b.classList.toggle('on', b.dataset.v === WORK.view);
  const people = chartWorkers().map((w) => w.name);
  const sel = $('wk-owner');
  if (sel.dataset.sig !== people.join(',')) {
    sel.dataset.sig = people.join(',');
    sel.replaceChildren(opt('', 'everyone'), ...people.map((n) => opt(n, who(M, n), n === WORK.owner)));
  }
  const busy = [...M.workers.values()].filter((w) => w.inTurn).map((w) => w.name + [...w.turnThreads].join('')).join(',');
  const sig = [WORK.view, WORK.owner, M.count, busy, Math.floor(now / (WORK.view === 'timeline' ? 5 : 30))].join('|');
  if (sig === WORK.sig) return;
  WORK.sig = sig;
  const all = tickets(M, now);
  const mine = WORK.owner ? all.filter((k) => k.owner === WORK.owner) : all;
  const n = (st) => mine.filter((k) => k.status === st).length;
  const late = mine.filter((k) => k.overdue).length;
  $('wk-sum').textContent = mine.length ? `${mine.length} assignments · ${n('todo') + n('doing') + n('waiting')} open · ${n('done')} done` + (late ? ` · ${late} overdue` : '') : '';
  if (!all.length) {
    $('wk-body').replaceChildren(el('div', { class: 'empty card' }, 'No assignments yet. Every assignment sent by mail becomes a ticket here, for each recipient; a report closes it.'));
    return;
  }
  $('wk-body').replaceChildren(WORK.view === 'timeline' ? workTimeline(M, mine, all, now) : workBoard(M, mine, now));
}

function ticketCard(M, k, now) {
  const age = k.end ? 'took ' + fmtDur(k.end - k.created) : (k.status === 'todo' ? 'waiting ' : 'open ') + fmtDur(now - k.created);
  return el('div', { class: 'tk' + (k.overdue ? ' late' : '') + (k.working ? ' live' : ''), title: 'Open the thread', onclick: () => openThread(k.thread, k.message) },
    el('div', { class: 'tk-s' }, k.working ? el('span', { class: 'dot pulse', title: k.owner + ' is working on it now' }) : '', clip(k.subject, 90), planBadge(M, k.thread)),
    el('div', { class: 'tk-m' }, miniAvatar(k.owner), el('b', {}, k.owner), el('span', { class: 'muted' }, '← ' + (k.by === BOARD ? boardName() : k.by))),
    el('div', { class: 'tk-m muted small' },
      el('span', {}, age),
      k.kids.length ? el('span', {}, '· ' + (k.kids.length - k.openKids) + '/' + k.kids.length + ' delegated done') : '',
      k.due && !k.end ? el('span', { class: k.overdue ? 'bad' : '' }, '· due ' + fmtDate(k.due)) : '',
      k.parent ? el('span', { title: 'part of: ' + k.parent.subject }, '· part of ' + clip(k.parent.subject, 30)) : ''));
}

function workBoard(M, list, now) {
  const cols = WORK_COLS.map(([st, label]) => {
    let ks = list.filter((k) => k.status === st);
    ks.sort(st === 'done' ? (a, b) => b.end - a.end : (a, b) => (b.overdue - a.overdue) || (b.working - a.working) || a.created - b.created);
    return el('div', { class: 'tk-col' }, el('div', { class: 'tk-h' }, el('span', {}, label), el('span', { class: 'muted' }, String(ks.length))),
      ...(ks.length ? ks.slice(0, 200).map((k) => ticketCard(M, k, now)) : [el('div', { class: 'muted small tk-none' }, '—')]));
  });
  return el('div', { class: 'tk-board' }, ...cols);
}

// A Gantt chart: a bar per assignment from sent to reported, nested under
// the assignment it was delegated from; inside, the owner's turns on it.
function workTimeline(M, list, all, now) {
  const inList = new Set(list);
  const rows = [];
  const visit = (k, depth) => {
    if (inList.has(k)) rows.push([k, depth]);
    for (const c of k.kids.slice().sort((a, b) => a.created - b.created)) visit(c, inList.has(k) ? depth + 1 : depth);
  };
  for (const k of all.filter((x) => !x.parent).sort((a, b) => a.created - b.created)) visit(k, 0);
  const from = Math.min(...rows.map(([k]) => k.created), M.started || now);
  const to = Math.max(now, from + 60);
  const span = to - from, W = 1100, L = 330, BW = W - L - 12, RH = 24;
  const x = (t) => L + ((Math.min(Math.max(t, from), to) - from) / span) * BW;
  const svg = sv('svg', { class: 'chart gantt', viewBox: `0 0 ${W} ${rows.length * RH + 30}`, role: 'img', 'aria-label': 'assignments over time' });
  for (let k = 0; k <= 6; k++) {
    const gx = L + (k / 6) * BW;
    svg.append(sv('line', { class: 'grid', x1: gx, x2: gx, y1: 0, y2: rows.length * RH + 4 }));
    const t = sv('text', { x: gx, y: rows.length * RH + 20, 'text-anchor': k === 0 ? 'start' : k === 6 ? 'end' : 'middle' });
    t.textContent = span > 86400 ? fmtDate(from + (k / 6) * span) : fmtTime(from + (k / 6) * span).slice(0, 5);
    svg.append(t);
  }
  if (now < to) svg.append(sv('line', { class: 'now', x1: x(now), x2: x(now), y1: 0, y2: rows.length * RH + 4 }));
  rows.forEach(([k, depth], i) => {
    const y = i * RH + 4;
    const g = sv('g', { class: 'grow', onclick: () => openThread(k.thread, k.message) });
    const lbl = sv('text', { class: 'lbl', x: 8 + depth * 14, y: y + 14 });
    lbl.textContent = (depth ? '↳ ' : '') + clip(k.subject, Math.max(16, 40 - depth * 2));
    const own = sv('text', { class: 'own', x: L - 8, y: y + 14, 'text-anchor': 'end' });
    own.textContent = k.owner;
    const x0 = x(k.created), x1 = x(k.end || now);
    const bar = sv('rect', { class: 'gb ' + k.status + (k.overdue ? ' late' : ''), x: x0, y: y + 4, width: Math.max(2, x1 - x0), height: 14, rx: 3 });
    const tt = sv('title');
    tt.textContent = k.subject + '\n' + k.owner + ' ← ' + k.by + ' · ' + (k.end ? 'done in ' + fmtDur(k.end - k.created) : k.status + ' for ' + fmtDur(now - k.created)) + (k.due ? ' · due ' + fmtDate(k.due) : '') + (k.kids.length ? ' · ' + k.kids.length + ' delegated' : '');
    g.append(sv('rect', { class: 'hit', x: 0, y, width: W, height: RH }), lbl, own, bar, tt);
    // The owner's turns on this thread: when it was actually worked on.
    for (const sp of M.spans) {
      if (sp.worker !== k.owner || !sp.threads.has(k.thread)) continue;
      const a = Math.max(sp.from, k.created), b = Math.min(sp.to == null ? now : sp.to, k.end || now);
      if (b <= a) continue;
      g.append(sv('rect', { class: 'ge', x: x(a), y: y + 7, width: Math.max(1.5, x(b) - x(a)), height: 8, rx: 2 }));
    }
    if (k.due && k.due <= to) g.append(sv('line', { class: 'due' + (k.overdue ? ' late' : ''), x1: x(k.due), x2: x(k.due), y1: y + 2, y2: y + 20 }));
    svg.append(g);
  });
  return el('div', { class: 'card' }, svg,
    el('div', { class: 'legend' },
      ...[['todo', 'to do'], ['doing', 'in progress'], ['waiting', 'waiting on others'], ['done', 'done']].map(([c, l]) => el('span', {}, sv('svg', { width: 12, height: 10 }, sv('rect', { class: 'gb ' + c, width: 12, height: 10, rx: 2 })), ' ', l)),
      el('span', {}, sv('svg', { width: 12, height: 10 }, sv('rect', { class: 'ge', y: 2, width: 12, height: 6, rx: 2 })), ' turns on it'),
      el('span', { class: 'muted' }, '| due date · click a row for its thread')));
}
