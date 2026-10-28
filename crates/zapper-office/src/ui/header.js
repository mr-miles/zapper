'use strict';
// The office page: the header, the scenario's next step and the footer.

// ───────────────────────────── header ─────────────────────────────


function buildHeader() {
  const nav = $('tabs');
  nav.replaceChildren();
  for (const [t, l] of TABS) {
    const b = el('button', { type: 'button', class: t === 'runs' ? 'right' : null, onclick: () => showTab(t) }, l, el('span', { class: 'badge warn hidden', id: 'tb-' + t }));
    b.dataset.t = t;
    nav.append(b);
  }
  syncTabs();
}
// The Git tab browses the firm's own repositories: none in the config, no tab.
function tabShown(t) {
  return !(t === 'git' && S.info && !S.info.local_git);
}
function syncTabs() {
  for (const b of $('tabs').querySelectorAll('button')) b.classList.toggle('hidden', !tabShown(b.dataset.t));
  if (!tabShown(S.tab)) showTab('overview');
}
// ───────────────────────── the scenario's next step ─────────────────────────
//
// What the board's script does next and how close it is, from the journal:
// the steps that played or failed, what a due step waits for, and its
// triggers against the clock and the budget spent.

function durSecs(d) {
  const m = /^(\d+(?:\.\d+)?)\s*(s|m|h|d)?$/.exec(str(d).trim());
  return m ? Number(m[1]) * ({ s: 1, m: 60, h: 3600, d: 86400 }[m[2] || 's']) : null;
}
// 1:05:09, 4:07, 0:42: a countdown that ticks every second.
function countdown(secs) {
  const t = Math.max(0, Math.ceil(secs)), h = Math.floor(t / 3600), m = Math.floor(t % 3600 / 60), x = t % 60;
  const p2 = (n) => String(n).padStart(2, '0');
  return h ? h + ':' + p2(m) + ':' + p2(x) : m + ':' + p2(x);
}
function stepText(st) {
  if (st.kill) return 'kill ' + (st.kill.worker ? cap(str(st.kill.worker)) : 'the key ' + str(st.kill.role));
  if (st.break_cluster) return 'break the cluster (' + str(st.break_cluster.mode) + ')';
  if (st.mail) return 'mail ' + str(st.mail.to) + ': "' + clip(str(st.mail.subject), 40) + '"';
  return 'a step';
}
function scenarioNext(M, now) {
  if (!M || !M.scenario) return null;
  const script = M.scenario.script;
  const done = new Set(), waits = new Map();
  for (const x of M.scenarioLog) {
    if (x.ts > now) continue;
    if (x.event === 'step' || x.event === 'failed') { done.add(x.index); waits.delete(x.index); }
    if (x.event === 'waiting') waits.set(x.index, x.why);
  }
  const seed = Number(M.budget && M.budget.seed) || 0;
  const spent = [...M.workers.values()].reduce((n, w) => n + w.tokens, 0);
  const frac = M.approved > 0 ? spent / (seed + M.approved) : 0;
  let best = null;
  script.forEach((st, i) => {
    if (done.has(i)) return;
    const after = st.after != null ? durSecs(st.after) : null;
    const left = after != null && M.started ? M.started + after - now : 0;
    const need = st.spent != null ? Number(st.spent) : null;
    const gap = need != null ? need - frac : 0;
    // Close: within two minutes and 5% of the budget of every trigger.
    const due = left <= 0 && gap <= 0;
    const soon = !due && left <= 120 && gap <= 0.05;
    const parts = [];
    if (after != null && left > 0) parts.push('in ' + countdown(left));
    if (need != null && gap > 0) parts.push('in ' + Math.max(1, Math.round(gap * 100)) + '% of the budget (at ' + Math.round(need * 100) + '%, now ' + Math.round(frac * 100) + '%)');
    const n = { i, text: stepText(st), when: parts.join(', '), due, soon, waits: waits.get(i) || null, rank: due ? -1 : Math.max(left / 120, gap / 0.05) };
    if (!best || n.rank < best.rank) best = n;
  });
  return { name: M.scenario.name, total: script.length, played: done.size, next: best };
}
function scenarioButton(M, now) {
  const sc = scenarioNext(M, now);
  // Only while a scenario plays: none, or all its steps played, shows nothing.
  if (!sc || !sc.next || ['ended', 'failed'].includes(str(M.runState))) return null;
  const n = sc.next;
  let text, warn = false, title;
  if (n.waits) { text = 'step ' + n.i + ' waits: ' + n.waits; warn = true; title = n.text + ' is due and waits for its target'; }
  else if (n.due) { text = 'step ' + n.i + ' firing: ' + n.text; warn = true; title = 'Its triggers are met'; }
  else { text = 'next: ' + n.text + ' · '; warn = n.soon; title = n.soon ? 'About to fire' : 'The next step of the board\'s script'; }
  // The countdown ticks in place, so the button isn't rebuilt every second.
  const when = n.due || n.waits ? '' : n.when;
  if (SCN.whenEl && SCN.whenSig === [sc.name, sc.played, n.i, !!n.waits, n.due, warn].join('|')) SCN.whenEl.textContent = when;
  const w = el('span', { class: 'mono' }, when);
  const b = el('button', { class: 'btn small ' + (warn ? '' : 'ghost'), type: 'button', title: title + ' · ' + sc.played + ' of ' + sc.total + ' steps played · open People for what played', onclick: () => showTab('people') },
    warn ? el('span', { class: 'badge warn' }, '!') : '', warn ? ' ' : '', 'Scenario ', el('b', {}, sc.name), ' · ' + text, w);
  const sig = [sc.name, sc.played, n.i, !!n.waits, n.due, warn].join('|');
  return { el: b, sig: sig + '|' + (n.waits || ''), next: n, name: sc.name, adopt: () => { SCN.whenEl = w; SCN.whenSig = sig; } };
}
// A toast when a step is about to fire, and when it plays: once each.
const SCN = { told: new Set(), whenEl: null, whenSig: null };
function scenarioToasts(M, now, next) {
  if (!live() || !M) return;
  if (next && (next.soon || next.due) && !SCN.told.has('soon:' + S.run + ':' + next.i)) {
    SCN.told.add('soon:' + S.run + ':' + next.i);
    toast('Scenario: step ' + next.i + ' is about to fire — ' + next.text);
  }
  for (const x of M.scenarioLog) {
    if (x.event !== 'step' && x.event !== 'failed') continue;
    const k = 'played:' + S.run + ':' + x.index;
    if (SCN.told.has(k)) continue;
    SCN.told.add(k);
    // Steps already in the journal when the page opened aren't news.
    if (now - x.ts < 30) toast('Scenario: step ' + x.index + (x.event === 'step' ? ' played' : ' failed: ' + clip(x.why, 120)), x.event === 'failed');
  }
}

function renderHeader() {
  const M = S.M;
  const pick = $('firmpick');
  if (S.firms.length > 1) {
    const sig = S.firms.map((f) => f.name).join(',');
    if (pick.dataset.sig !== sig || !pick.firstChild) {
      pick.dataset.sig = sig;
      pick.replaceChildren(selectOf(S.firms.map((f) => [f.name, f.name]), S.firm, { onchange: (ev) => switchFirm(ev.target.value), 'aria-label': 'Firm' }));
    } else if (pick.firstChild.value !== S.firm) pick.firstChild.value = S.firm;
    $('firmname').classList.add('hidden');
  } else { pick.replaceChildren(); $('firmname').classList.remove('hidden'); }
  $('firmname').textContent = S.firm ? companyName(M) : 'office';
  $('firmdesc').textContent = (S.info && S.info.description) || (M && M.description) || '';
  document.title = (S.firm ? companyName(M) + ' · ' : '') + 'zapper office';
  const st = live() ? (S.info && S.info.run ? str(S.info.run.state) : 'off') : (M && M.runState) || 'starting';
  // The firm controls sit where its state is: Start instead of "off",
  // the state with Pause and Stop while it runs.
  const stats = [];
  if (live() && S.firm) {
    const rs = S.info && S.info.run ? str(S.info.run.state) : null;
    const active = rs && !['ended', 'failed'].includes(rs);
    if (!active) stats.push(el('button', { class: 'btn primary small', type: 'button', title: 'Start a company: its name, its CEO and the seed, in the Kickoff tab', onclick: () => showTab('kickoff') }, 'Start'));
    else stats.push(pill(st, st));
    if (rs === 'running') stats.push(el('button', { class: 'btn small', type: 'button', onclick: () => firmOp('pause', { paused: true }) }, 'Pause'));
    if (rs === 'paused') stats.push(el('button', { class: 'btn small', type: 'button', onclick: () => firmOp('pause', { paused: false }) }, 'Resume'));
    if (active) stats.push(el('button', { class: 'btn danger small', type: 'button', onclick: () => { if (confirm('Stop ' + S.firm + '? Workers stop and the run ends; it can be replayed from Runs, and the next Start begins a fresh run.')) firmOp('down'); } }, 'Stop'));
  } else stats.push(pill(st, st));
  if (M && M.overBudget && (S.mode === 'replay' || (S.info && S.info.run))) stats.push(pill('error', 'budget reached'));
  // The run's budget, and the CEO's request waiting for the board.
  let bsig = '';
  if (fundsOn(M)) {
    const total = (Number(M.budget.seed) || 0) + M.approved;
    const spent = [...M.workers.values()].reduce((n, w) => n + w.tokens, 0);
    bsig = fmtTok(total) + '/' + fmtTok(total - spent);
    stats.push(el('button', { class: 'btn small ghost', type: 'button', title: 'The run\'s budget: open the Kickoff tab', onclick: () => showTab('kickoff') },
      'Budget ', el('b', {}, fmtTok(total)), ' · ' + fmtTok(total - spent) + ' left'));
    const pend = pendingBudget(M);
    if (pend) { bsig += '|' + pend.id; stats.push(el('button', { class: 'btn small', type: 'button', onclick: goDecideBudget }, el('span', { class: 'badge warn' }, '!'), ' budget request ' + pend.id + ': ' + fmtTok(pend.total) + ' — decide')); }
  }
  // The scenario's next step, with a countdown, while one is playing: its
  // own line under the logo.
  const scb = scenarioButton(M, T());
  const hsc = $('hscen');
  const ssig = scb ? scb.sig : '';
  if (hsc.dataset.sig !== ssig) {
    hsc.dataset.sig = ssig;
    hsc.replaceChildren(...(scb ? [scb.el] : []));
    hsc.classList.toggle('hidden', !scb);
    if (scb) scb.adopt(); else SCN.whenEl = null;
  }
  if (scb) scenarioToasts(M, T(), scb.next);
  const esc = M ? escalationsWaiting(M).length + checksWaiting(M).length + credWaiting(M).length : 0;
  if (esc) stats.push(el('button', { class: 'btn small', type: 'button', onclick: () => showTab('security') }, el('span', { class: 'badge warn' }, String(esc)), ' waiting for the board'));
  if (live() && S.boardUnread.size) stats.push(el('button', { class: 'btn small', type: 'button', onclick: () => { MSG.view = 'inbox'; MSG.sig = null; showTab('messages'); } }, el('span', { class: 'badge' }, String(S.boardUnread.size)), ' unread'));
  // Rebuilt only when something in it changes, so a click isn't lost to a
  // render between press and release.
  const hsig = [S.mode, S.firm, st, live() && S.info && S.info.run ? str(S.info.run.state) : '', M && M.overBudget, esc, live() ? S.boardUnread.size : 0, bsig].join('|');
  if ($('hstats').dataset.sig !== hsig) { $('hstats').dataset.sig = hsig; $('hstats').replaceChildren(...stats); }
  // The product (the cluster's ingress on the host) and the cluster's
  // dashboard, top right on every tab.
  const pp = str(S.info && S.info.product_port);
  const lsig = (S.firm || '') + '|' + pp + '|' + dashboardSig() + '|' + companyOff() + '|' + S.mode;
  const hl = $('hlinks');
  if (hl.dataset.sig !== lsig) {
    hl.dataset.sig = lsig;
    // Only while a company runs: before, nothing serves the product.
    const on = S.firm && !companyOff() && S.mode === 'live';
    const a = on ? productLink(pp) : null;
    hl.replaceChildren(...(a ? [a] : []), ...(on ? dashboardButtons() : []));
  }
  const tbSec = $('tb-security');
  if (tbSec) { tbSec.textContent = String(esc); tbSec.classList.toggle('hidden', !esc); }
  const tbMsg = $('tb-messages');
  if (tbMsg) { const n = live() ? S.boardUnread.size : 0; tbMsg.textContent = String(n); tbMsg.className = 'badge' + (n ? '' : ' hidden'); }
}
// ────────────────────────────── footer ──────────────────────────────
//
// The numbers worth watching while on any tab; each opens its tab, and a
// value that changed flashes.

const FOOT = { prev: new Map(), log: null, logRun: null, logAt: 0, busy: false };

function renderFooter() {
  const foot = $('foot');
  const M = S.M;
  const items = [];
  const item = (key, tab, parts, warn) => items.push({ key, tab, parts, warn });
  if (M && S.run) {
    const now = T();
    const ws = [...M.workers.values()].filter((w) => w.inChart);
    const count = (...st) => ws.filter((w) => st.includes(w.state)).length;
    const trouble = count('error', 'over-budget');
    const wparts = [[count('working', 'reviewing'), 'working'], [count('waiting', 'queued'), 'waiting'], [count('idle'), 'idle'], [count('booting'), 'booting'], [count('off'), 'off'], [trouble, 'in trouble']].filter(([n]) => n);
    item('workers', 'overview', wparts.length ? wparts : [[ws.length, 'workers']], trouble > 0);
    const open = openObl(M), late = open.filter((o) => overdue(o, now)).length;
    item('mail', 'messages', [[M.mails.size, 'messages'], [open.length, 'open']].concat(late ? [[late, 'overdue']] : []), late > 0);
    const pending = [...M.requests.values()].filter((r) => r.state === 'pending' || r.state === 'escalated').length;
    item('access', 'security', [[pending, 'asks pending'], [M.access.filter((a) => a.event === 'decided').length, 'decided']], pending > 0);
    if (fundsOn(M)) {
      const total = (Number(M.budget.seed) || 0) + M.approved, spent = ws.reduce((n, w) => n + w.tokens, 0) + [...M.workers.values()].filter((w) => !w.inChart).reduce((n, w) => n + w.tokens, 0);
      item('budget', 'kickoff', [[fmtTok(total - spent), 'of ' + fmtTok(total) + ' left']].concat(pendingBudget(M) ? [['1', 'request waiting']] : []), total - spent <= 0 || !!pendingBudget(M));
    }
    const ks = [...M.contracts.values()];
    if (ks.length) item('contracts', 'stats', [[ks.filter((k) => k.state === 'active').length, 'contractors'], [ks.filter((k) => ['offered', 'quoted', 'countered'].includes(k.state)).length, 'negotiating']].filter(([n]) => n).concat([[ks.length, 'contracts']]).slice(0, 2));
    item('drive', 'drive', [[M.artifacts.size, 'artifacts'], [M.published.length, 'versions']]);
    if (FOOT.logRun === S.run && FOOT.log) item('git', 'git', [[S.mode === 'replay' ? FOOT.log.filter((c) => !c.ts || Number(c.ts) <= now).length : FOOT.log.length, 'commits']]);
    const tok = ws.reduce((n, w) => n + w.tokens, 0);
    item('turns', 'stats', [[M.turns.length, 'turns'], [fmtNum(tok), 'tokens']]);
    if (M.started) item('time', 'stats', [[fmtDur(Math.max(0, now - M.started)), S.mode === 'replay' ? 'into the run' : 'running']]);
    footCommits();
  }
  const els = [];
  for (const it of items) {
    if (els.length) els.push(el('span', { class: 'sep' }, '·'));
    const kids = [];
    it.parts.forEach(([v, l], i) => { if (i) kids.push(el('span', {}, ' ')); kids.push(el('b', {}, String(v)), el('span', {}, l)); });
    const e = el('span', { class: 'fi' + (it.warn ? ' warn' : ''), title: 'Open ' + (TABS.find(([t]) => t === it.tab) || [, it.tab])[1], onclick: () => showTab(it.tab) }, ...kids);
    const sig = it.parts.map(([v]) => v).join('|');
    const was = FOOT.prev.get(it.key);
    if (was != null && was !== sig && it.key !== 'time') { e.classList.add('bump'); setTimeout(() => e.classList.remove('bump'), 1200); }
    FOOT.prev.set(it.key, sig);
    els.push(e);
  }
  // zapper's own pages, under the office.
  const a = el('a', { class: 'right', title: 'zapper, the sandbox the office runs on: every session, its network, terminals and approvals' }, 'zapper sessions ↗');
  a.href = sessionsHref();
  const sig = (S.firm || '') + '#' + items.map((it) => it.key + ':' + it.parts.map(([v]) => v).join('|') + (it.warn ? '!' : '')).join(',');
  if (foot.dataset.sig === sig) return;
  foot.dataset.sig = sig;
  foot.replaceChildren(...els, a);
}
// The commits on main, across the run's repositories: polled while live,
// read once for a replay.
async function footCommits() {
  const run = S.run;
  if (!tabShown('git')) return;
  if (FOOT.busy || (FOOT.logRun === run && (S.mode === 'replay' || Date.now() - FOOT.logAt < 15000))) return;
  FOOT.busy = true; FOOT.logAt = Date.now();
  let log = [];
  try {
    for (const repo of arr(await gitFetch('/repos', null)).map(str)) {
      if (arr(await gitFetch('/branches', repo)).some((x) => x.name === 'main')) log = log.concat(arr(await gitFetch('/log?ref=main&limit=1000', repo)));
    }
  } catch (e) { /* no repository */ }
  FOOT.busy = false;
  if (run !== S.run) return;
  FOOT.log = log.map((c) => ({ ts: c.ts })); FOOT.logRun = run;
  scheduleRender();
}

async function firmOp(op, body) {
  try {
    await write('POST', F() + '/' + op, body === undefined ? {} : body);
    toast(op === 'up' ? 'Starting ' + S.firm + '…' : op === 'down' ? 'Stopping ' + S.firm + '…' : body && body.paused ? 'Paused' : 'Resumed');
  } catch (e) { toast(e.message, true); }
  loadFirm();
}
