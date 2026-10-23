'use strict';
// The office page: tabs, the URL hash, live and replay, and the start.

// ────────────────────────────── tabs ──────────────────────────────

function showTab(t) {
  if (!TABS.some(([x]) => x === t)) t = 'overview';
  if (t === 'runs' && S.tab !== 'runs') { S.prevTab = S.tab; RUNS.at = 0; }
  S.tab = t;
  for (const s of document.querySelectorAll('section.tab')) s.classList.toggle('on', s.id === 'tab-' + t);
  for (const b of $('tabs').querySelectorAll('button')) b.classList.toggle('on', b.dataset.t === t);
  setHash();
  if (t === 'overview') { OV.sig = null; }
  placePanel();
  if (t === 'security') pollSecurity();
  if (t === 'vms') { VM.sig = null; pollVms(); }
  render();
  if (t === 'overview') requestAnimationFrame(fitOverview);
}
function mountAll() {
  mountKickoff(); mountOverview(); mountPeople(); mountWork(); mountRuns(); mountMessages(); mountSecurity(); mountDrive(); mountGit(); mountStats(); mountVms();
  MSG.thread = null; MSG.treeRoot = null; MSG.compose = false; MSG.reply = null;
  DRV.sel = null; SEC.traffic = []; SEC.trafficAt = 0; S.security = null;
}
let renderQueued = false;
function scheduleRender() {
  if (renderQueued) return;
  renderQueued = true;
  requestAnimationFrame(() => { renderQueued = false; render(); });
}
function render() {
  syncOff();
  renderHeader();
  renderFooter();
  if (S.tab === 'overview') syncOverviewOff();
  // The VMs are the daemon's, not the firm's: shown with or without a run.
  if (S.tab === 'vms') { try { updateVms(); } catch (e) { console.error(e); } }
  if (!S.M) return;
  try {
    if (S.tab === 'overview') updateOverview();
    else if (S.tab === 'people') updatePeople();
    else if (S.tab === 'work') updateWork();
    else if (S.tab === 'kickoff') updateKickoff();
    else if (S.tab === 'runs') updateRuns();
    else if (S.tab === 'messages') updateMessages();
    else if (S.tab === 'security') updateSecurity();
    else if (S.tab === 'drive') updateDrive();
    else if (S.tab === 'git') updateGit();
    else if (S.tab === 'stats') updateStats();
    updatePanel();
  } catch (e) { console.error(e); toast('page error: ' + e.message, true); }
  if (S.mode === 'replay') updateReplayBar();
}

// ─────────────────────────── URL hash ───────────────────────────

function readHash() {
  const p = new URLSearchParams(location.hash.slice(1));
  return { firm: p.get('firm'), tab: p.get('tab'), replay: p.get('replay') };
}
function setHash() {
  const p = new URLSearchParams();
  if (S.firm) p.set('firm', S.firm);
  if (S.tab !== 'overview') p.set('tab', S.tab);
  if (S.mode === 'replay' && S.run) p.set('replay', S.run);
  const h = '#' + p.toString();
  if (location.hash !== h) history.replaceState(null, '', h);
}
async function applyHash() {
  const h = readHash();
  if (h.firm && h.firm !== S.firm && S.firms.some((f) => f.name === h.firm)) await switchFirm(h.firm, true);
  if (h.replay && (S.mode !== 'replay' || S.run !== h.replay)) enterReplay(h.replay);
  else if (!h.replay && S.mode === 'replay') backToLive();
  if (h.tab && h.tab !== S.tab) showTab(h.tab);
}
window.addEventListener('hashchange', applyHash);

// ───────────────────────────── live ─────────────────────────────

function stopLive() {
  if (S.es) { S.es.close(); S.es = null; }
}
function modelFromInfo(info) {
  const M = newModel();
  setChart(M, { board: info.board, description: info.description, workers: arr(info.chart) });
  return M;
}
async function startLive(run) {
  stopLive();
  const gen = ++S.gen;
  closePanel();
  S.run = run; S.seen = new Set(); S.boardUnread = new Set();
  S.M = S.info ? modelFromInfo(S.info) : newModel();
  mountAll();
  let bytes = 0;
  try {
    const text = await api(runPath(run) + '/journal');
    if (gen !== S.gen) return;
    const c = completeBytes(typeof text === 'string' ? text : '');
    bytes = c.bytes;
    for (const e of parseLines(c.text)) ingest(e, false);
  } catch (e) { /* not written yet: follow from the start */ }
  if (gen !== S.gen) return;
  S.es = new EventSource('/api' + runPath(run) + '/journal?follow=true&from=' + bytes);
  S.es.onmessage = (ev) => {
    if (gen !== S.gen) return;
    let e;
    try { e = JSON.parse(ev.data); } catch (x) { return; }
    ingest(e, true);
  };
  scheduleRender();
  refreshUnread();
}
function ingest(e, fresh) {
  if (!e || typeof e !== 'object') return;
  if (e.seq != null) { if (S.seen.has(e.seq)) return; S.seen.add(e.seq); }
  apply(S.M, e);
  if (!fresh) return;
  if (e.type === 'mail') {
    const m = S.M.mails.get(e.id);
    if (m) { flyMail(m); if (m.to.includes(BOARD) || m.cc.includes(BOARD)) S.boardUnread.add(m.id); }
  }
  if (e.type === 'access' && S.tab === 'security') pollSecurity();
  scheduleRender();
}
async function refreshUnread() {
  if (!live() || !S.info || !S.info.run || ['ended', 'failed'].includes(str(S.info.run.state))) { S.boardUnread = new Set(); return; }
  const gen = S.gen;
  try {
    const list = arr(await api(F() + '/mail?for=board&unread=true'));
    if (gen === S.gen) S.boardUnread = new Set(list.map((m) => str(m.id)));
  } catch (e) { /* no run */ }
}
async function loadFirm() {
  if (!S.firm) return;
  const firm = S.firm, gen = S.gen;
  let info;
  try { info = await api(F()); } catch (e) { toast(e.message, true); return; }
  if (firm !== S.firm) return;
  S.info = info;
  if (S.mode === 'live' && gen === S.gen) {
    // The live view follows the active run only: a stopped one is discarded
    // here (its mail, unread marks, budget) and lives on in Runs and replay.
    const active = info.run && !['ended', 'failed'].includes(str(info.run.state)) ? str(info.run.run) : null;
    const run = active;
    // When the company stops (or another run takes its place), nothing of
    // it may linger in the page: start over with a fresh one, on Kickoff.
    // The Kickoff form's stop-and-start reloads itself once the new run is up.
    if (S.activeRun && active !== S.activeRun && !KO.busy) { resetPage(); return; }
    S.activeRun = active;
    if (run && run !== S.run) { startLive(run); return; }
    if (!run && (S.run || !S.M)) { stopLive(); S.gen++; S.run = null; S.boardUnread = new Set(); closePanel(); S.M = modelFromInfo(info); mountAll(); }
    if (run) await refreshUnread();
  }
  scheduleRender();
}
function resetPage() {
  stopLive(); stopReplay();
  S.tab = 'kickoff'; S.mode = 'live';
  setHash();
  location.reload();
}
async function loadFirms() {
  try { S.firms = arr(await api('/firms')); } catch (e) { toast(e.message, true); S.firms = []; }
}
async function switchFirm(name, fromHash) {
  stopLive(); stopReplay();
  S.gen++;
  S.firm = name; S.info = null; S.run = null; S.M = null; S.mode = 'live'; S.boardUnread = new Set(); S.activeRun = null;
  closePanel();
  $('replaybar').classList.add('hidden');
  mountAll();
  if (!fromHash) setHash();
  await loadFirm();
}

// ───────────────────────────── replay ─────────────────────────────
//
// The whole journal is loaded and folded up to the timeline position;
// moving back starts the fold over. Writes are off.

function stopReplay() {
  if (S.rp && S.rp.timer) clearInterval(S.rp.timer);
  if (S.rp) S.rp.timer = null;
}
async function enterReplay(run) {
  // Where ✕ returns to: the tab before Runs (or before this replay).
  if (S.mode !== 'replay') S.returnTab = S.tab === 'runs' ? S.prevTab || 'overview' : S.tab;
  stopLive(); stopReplay();
  const gen = ++S.gen;
  closePanel();
  S.mode = 'replay'; S.run = run; S.M = newModel(); S.seen = new Set(); S.rp = null;
  mountAll();
  if (S.tab === 'runs') showTab('overview');
  setHash();
  let text;
  try { text = await api(runPath(run) + '/journal'); } catch (e) { toast('Can\'t open run ' + run + ': ' + e.message, true); if (gen === S.gen) backToLive(); return; }
  if (gen !== S.gen) return;
  const events = parseLines(typeof text === 'string' ? text : '').filter((e) => isFinite(Number(e.ts)))
    .map((e, i) => [e, i]).sort((a, b) => Number(a[0].ts) - Number(b[0].ts) || a[1] - b[1]).map(([e]) => Object.assign(e, { ts: Number(e.ts) }));
  if (!events.length) { toast('Run ' + run + ' has no events.', true); backToLive(); return; }
  S.rp = { events, idx: 0, t0: events[0].ts, t1: events[events.length - 1].ts, pos: events[0].ts, playing: false, speed: 10, timer: null, skip: false, last: 0 };
  buildReplayBar();
  seek(S.rp.t0, false);
}
function backToLive() {
  stopReplay();
  S.gen++;
  S.mode = 'live'; S.rp = null; S.run = null; S.M = null;
  closePanel();
  $('replaybar').classList.add('hidden');
  mountAll();
  showTab(S.returnTab || 'overview');
  S.returnTab = null;
  loadFirm();
}
function seek(t, animate) {
  const rp = S.rp;
  if (!rp) return;
  t = Math.max(rp.t0, Math.min(rp.t1, t));
  if (t < rp.pos || !S.M || rp.idx === 0) { S.M = newModel(); rp.idx = 0; }
  const mails = [];
  while (rp.idx < rp.events.length && rp.events[rp.idx].ts <= t) {
    const e = rp.events[rp.idx++];
    apply(S.M, e);
    if (animate && e.type === 'mail') mails.push(e.id);
  }
  rp.pos = t;
  for (const id of mails.slice(-12)) { const m = S.M.mails.get(id); if (m) flyMail(m); }
  scheduleRender();
}
function play(on) {
  const rp = S.rp;
  if (!rp) return;
  if (on && rp.pos >= rp.t1) seek(rp.t0, false);
  rp.playing = on;
  if (rp.timer) { clearInterval(rp.timer); rp.timer = null; }
  if (on) {
    rp.last = performance.now();
    rp.timer = setInterval(() => {
      const now = performance.now(), dt = (now - rp.last) / 1000;
      rp.last = now;
      let target = rp.pos + dt * rp.speed;
      const next = rp.events[rp.idx];
      // Long quiet stretches are skipped: jump to a second before the next event.
      if (rp.skip && next && (next.ts - rp.pos) / rp.speed > 3) target = Math.max(target, next.ts - rp.speed);
      seek(target, true);
      if (rp.pos >= rp.t1) play(false);
    }, 100);
  }
  updateReplayBar();
}
function buildReplayBar() {
  const rp = S.rp, bar = $('replaybar');
  const playBtn = el('button', { class: 'btn primary', type: 'button', id: 'rp-play', onclick: () => play(!rp.playing) }, 'Play');
  const speeds = el('div', { class: 'seg' });
  for (const s of [1, 10, 60, 120]) {
    const b = el('button', { type: 'button', onclick: () => { rp.speed = s; updateReplayBar(); } }, s + '×');
    b.dataset.s = String(s);
    speeds.append(b);
  }
  const slider = el('input', { type: 'range', id: 'rp-range', min: String(rp.t0), max: String(Math.max(rp.t1, rp.t0 + 1)), step: 'any', 'aria-label': 'Timeline',
    oninput: (ev) => seek(Number(ev.target.value), false) });
  bar.replaceChildren(
    el('span', { class: 'title' }, 'Replay of run ', el('span', { class: 'mono' }, S.run)),
    playBtn, speeds,
    el('button', { class: 'btn', type: 'button', title: 'Jump to the next event', onclick: () => { const n = rp.events[rp.idx]; if (n) seek(n.ts, true); } }, 'Next event ⏭'),
    slider,
    el('span', { class: 'mono small', id: 'rp-time' }),
    el('label', { class: 'small muted', style: 'display:flex;gap:4px;align-items:center' }, el('input', { type: 'checkbox', checked: rp.skip, onchange: (ev) => { rp.skip = ev.target.checked; } }), 'skip quiet stretches'),
    el('button', { class: 'btn small', type: 'button', title: 'The config and skills this run started with', onclick: () => openRunConfig(S.run) }, 'Config'),
    el('button', { class: 'btn ghost close', type: 'button', title: 'Close the replay', 'aria-label': 'Close the replay', onclick: backToLive }, '✕'));
  bar.classList.remove('hidden');
  updateReplayBar();
}
function updateReplayBar() {
  const rp = S.rp;
  if (!rp) return;
  const b = $('rp-play');
  if (!b) return;
  b.textContent = rp.playing ? 'Pause' : 'Play';
  for (const x of b.parentElement.querySelectorAll('.seg button')) x.classList.toggle('on', x.dataset.s === String(rp.speed));
  const r = $('rp-range');
  if (document.activeElement !== r) r.value = String(rp.pos);
  $('rp-time').textContent = fmtDate(rp.pos) + ':' + pad2(new Date(rp.pos * 1000).getSeconds()) + ' · ' + fmtDur(rp.pos - rp.t0) + ' of ' + fmtDur(rp.t1 - rp.t0) + ' · ' + rp.idx + '/' + rp.events.length + ' events';
}
document.addEventListener('keydown', (ev) => {
  if (S.mode !== 'replay' || !S.rp || ev.target.closest('input,textarea,select,.slides')) return;
  if (ev.key === ' ') { ev.preventDefault(); play(!S.rp.playing); }
});

// ───────────────────────────── start ─────────────────────────────

async function start() {
  buildHeader();
  mountAll();
  await loadFirms();
  const h = readHash();
  S.tab = TABS.some(([t]) => t === h.tab) ? h.tab : 'kickoff';
  S.firm = (h.firm && S.firms.some((f) => f.name === h.firm) && h.firm) || (S.firms[0] && S.firms[0].name) || null;
  showTab(S.tab);
  if (!S.firm) {
    $('tab-overview').replaceChildren(el('div', { class: 'empty card' }, 'No firms are configured. Add one under `firms:` in the config; see docs/office.md.'));
    renderHeader();
    return;
  }
  if (h.replay) enterReplay(h.replay);
  else await loadFirm();
  setInterval(() => {
    if (document.hidden) return;
    if (live()) loadFirm();
    if (S.tab === 'security') pollSecurity();
    scheduleRender();
  }, 5000);
  setInterval(() => { if (!document.hidden) loadFirms().then(renderHeader); }, 30000);
  // The scenario's countdown ticks every second (in place; see scenarioButton).
  setInterval(() => { if (!document.hidden && SCN.whenEl && live()) renderHeader(); }, 1000);
}
start();
