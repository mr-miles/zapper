'use strict';
// The office page: the Organization tab: avatars, the chart's layout, the overview and the worker panel.

// ───────────────────────────── avatars ─────────────────────────────
//
// Schematic figures from parts: a round head with two dot eyes, a rounded
// body in the role's colour, a role prop, and a spark above the head as
// the thinking mark. States are classes; CSS animates them.

const STATES = ['off', 'booting', 'idle', 'waiting', 'queued', 'working', 'reviewing', 'over-budget', 'error'];
const STATE_ICON = { off: '○', booting: '◌', idle: '◯', waiting: '⧗', queued: '…', working: '✳', reviewing: '⛨', 'over-budget': '◔', error: '●' };

function sparkG(cx, cy, r1, r2) {
  const g = sv('g', { class: 'spark' });
  for (let k = 0; k < 8; k++) {
    const a = (k * Math.PI) / 4, c = Math.cos(a), s = Math.sin(a);
    const long = k % 2 === 0 ? r2 : r2 * 0.72;
    g.append(sv('line', { x1: (cx + c * r1).toFixed(2), y1: (cy + s * r1).toFixed(2), x2: (cx + c * long).toFixed(2), y2: (cy + s * long).toFixed(2) }));
  }
  return g;
}
function propFor(figure, color) {
  const g = sv('g', { class: 'prop' });
  switch (figure) {
    case 'ceo':
      g.append(
        sv('path', { class: 'f', d: 'M32 44.5 l-2.2 2.6 l2.2 11.5 l2.2 -11.5 z', fill: 'var(--av-stroke)' }),
        sv('path', { class: 'f', d: 'M30.3 43.2 h3.4 l-0.7 1.8 h-2 z', fill: 'var(--av-stroke)' }),
        sv('line', { class: 'ln', x1: 53, y1: 45, x2: 53, y2: 68 }),
        sv('path', { class: 'f acc flag', d: 'M53.4 45.5 h8 l-2.3 3.2 l2.3 3.2 h-8 z' }));
      break;
    case 'product-manager':
      g.append(
        sv('rect', { class: 'paper', x: 44, y: 45, width: 18, height: 14, rx: 1.8 }),
        sv('line', { class: 'ln', x1: 47, y1: 59, x2: 46, y2: 67 }), sv('line', { class: 'ln', x1: 59, y1: 59, x2: 60, y2: 67 }),
        sv('rect', { class: 'f note', x: 46.5, y: 47.5, width: 6.5, height: 6.5, rx: 0.8, fill: '#E9B949' }),
        sv('rect', { class: 'f', x: 55, y: 49, width: 4.8, height: 4.8, rx: 0.7, fill: '#9DBE9A' }),
        sv('rect', { class: 'f', x: 48, y: 55.3, width: 9, height: 1.4, rx: 0.7, fill: 'var(--av-stroke)', opacity: 0.35 }));
      break;
    case 'manager': {
      const checks = sv('g', { class: 'checks' },
        sv('path', { class: 'ln', d: 'M48 53 l1.6 1.6 l3 -3.2' }), sv('path', { class: 'ln', d: 'M48 59 l1.6 1.6 l3 -3.2' }));
      g.append(
        sv('rect', { class: 'paper', x: 45, y: 46, width: 14, height: 19, rx: 2 }),
        sv('rect', { class: 'f acc', x: 49, y: 44.3, width: 6, height: 3.4, rx: 1 }),
        checks,
        sv('line', { class: 'ln', x1: 54.5, y1: 53.5, x2: 57, y2: 53.5 }), sv('line', { class: 'ln', x1: 54.5, y1: 59.5, x2: 57, y2: 59.5 }));
      break;
    }
    case 'engineer': {
      const code = sv('text', { class: 'code', x: 32, y: 59.6, 'text-anchor': 'middle' });
      code.textContent = '</>';
      g.append(
        sv('rect', { class: 'screen', x: 20.5, y: 48.5, width: 23, height: 15, rx: 1.8 }),
        sv('rect', { class: 'f screen-glow', x: 22.5, y: 50.5, width: 19, height: 11, rx: 1, fill: color, 'fill-opacity': 0.18 }),
        code,
        sv('path', { class: 'paper', d: 'M17 63.5 h30 l-1.6 2.6 h-26.8 z' }));
      break;
    }
    case 'researcher':
      g.append(
        sv('rect', { class: 'paper', x: 44, y: 46, width: 14, height: 18, rx: 1.5 }),
        sv('rect', { class: 'paper pageflip', x: 51, y: 46, width: 7, height: 18, rx: 0.8 }),
        sv('line', { class: 'ln', x1: 46.5, y1: 50, x2: 55, y2: 50 }), sv('line', { class: 'ln', x1: 46.5, y1: 53.5, x2: 55, y2: 53.5 }), sv('line', { class: 'ln', x1: 46.5, y1: 57, x2: 52, y2: 57 }),
        sv('g', { class: 'lens' }, sv('circle', { class: 'ln', cx: 48, cy: 58, r: 4.6, fill: 'var(--paper)', 'fill-opacity': 0.5 }), sv('line', { class: 'ln', x1: 44.8, y1: 61.4, x2: 41.5, y2: 65 })));
      break;
    case 'analyst':
      g.append(
        sv('path', { class: 'ln', d: 'M44 46 V65 H62' }),
        sv('rect', { class: 'f acc bar1', x: 46.5, y: 56, width: 3.6, height: 9, rx: 0.6 }),
        sv('rect', { class: 'f bar2', x: 51.5, y: 51, width: 3.6, height: 14, rx: 0.6, fill: color }),
        sv('rect', { class: 'f bar3', x: 56.5, y: 47.5, width: 3.6, height: 17.5, rx: 0.6, fill: 'var(--av-stroke)', 'fill-opacity': 0.55 }));
      break;
    case 'designer':
      g.append(
        sv('path', { class: 'paper', d: 'M52 46 c6.5 0 10.5 4 10.5 9 c0 3.2 -2.6 4 -4.6 3.6 c-2 -0.4 -3 1 -2.2 2.8 c0.8 1.8 -0.4 3.4 -3.6 3.4 c-5.6 0 -9.6 -4.2 -9.6 -9.4 c0 -5.4 4.1 -9.4 9.5 -9.4 z' }),
        sv('circle', { class: 'f', cx: 47.6, cy: 52.4, r: 1.6, fill: '#D97757' }),
        sv('circle', { class: 'f', cx: 52.4, cy: 50, r: 1.6, fill: '#6A9BCC' }),
        sv('circle', { class: 'f', cx: 57.4, cy: 51.6, r: 1.6, fill: '#E9B949' }),
        sv('circle', { class: 'f', cx: 47.6, cy: 58, r: 1.6, fill: color }),
        sv('g', { class: 'brush' }, sv('line', { class: 'ln', x1: 40, y1: 66, x2: 50.5, y2: 55.5 }), sv('path', { class: 'f acc', d: 'M50 56 l2.6 -2.6 l1.8 1.8 l-2.6 2.6 z' })));
      break;
    case 'shield':
      g.append(sv('g', { class: 'shield' },
        sv('path', { class: 'paper', d: 'M32 44.5 l8.5 3 v6.5 c0 5.3 -3.8 8.6 -8.5 10.6 c-4.7 -2 -8.5 -5.3 -8.5 -10.6 v-6.5 z' }),
        sv('path', { class: 'ln', d: 'M28.4 54.4 l2.6 2.6 l4.8 -5.2', stroke: color })));
      break;
    default: break;
  }
  return g;
}
function avatar(figure, color, state, opts) {
  opts = opts || {};
  color = safeColor(color);
  const size = opts.size || 72;
  const s = sv('svg', { class: 'av' + (opts.static ? '' : ' anim') + ' st-' + (STATES.includes(state) ? state : 'idle'), viewBox: '0 0 72 74', width: size, height: (size * 74 / 72).toFixed(1), 'aria-hidden': 'true' });
  const fig = sv('g', { class: 'fig' },
    sv('path', { class: 'f f-body', d: 'M18 70 V56 a14 13.5 0 0 1 28 0 V70 z', fill: color }),
    sv('circle', { class: 'f f-head', cx: 32, cy: 30, r: 9.5 }),
    sv('g', { class: 'eyes' }, sv('circle', { class: 'eye', cx: 28.6, cy: 30.6, r: 1.15 }), sv('circle', { class: 'eye', cx: 35.4, cy: 30.6, r: 1.15 })),
    propFor(figure, color));
  s.append(fig, sparkG(32, 11.5, 2, 6));
  // State marks, top right.
  s.append(sv('g', { class: 'hourglass' }, sv('path', { d: 'M54 8 h8 l-4 6 l4 6 h-8 l4 -6 z' }), sv('path', { class: 'sand', d: 'M56 18.5 h4 l-2 -2.6 z' })));
  s.append(sv('g', { class: 'gauge' }, sv('path', { d: 'M52 19 a6 6 0 0 1 12 0' }), sv('path', { class: 'needle', d: 'M58 19 l4 -4' })));
  s.append(sv('circle', { class: 'errdot', cx: 60, cy: 12, r: 3.6 }));
  return s;
}
function setAvatarState(svgEl, state) {
  const st = STATES.includes(state) ? state : 'idle';
  const cls = svgEl.getAttribute('class').split(' ').filter((c) => !c.startsWith('st-'));
  cls.push('st-' + st);
  svgEl.setAttribute('class', cls.join(' '));
}
function podium() {
  const s = sv('svg', { class: 'av', viewBox: '0 0 72 74', width: 72, height: 74, 'aria-hidden': 'true' });
  s.append(
    sv('rect', { x: 10, y: 34, width: 52, height: 7, rx: 3.5, fill: 'var(--accent)' }),
    sv('path', { d: 'M16 41 h40 l-4 26 h-32 z', fill: 'var(--paper)', stroke: 'var(--av-stroke)', 'stroke-width': 1 }),
    sv('line', { x1: 24, y1: 50, x2: 48, y2: 50, stroke: 'var(--av-stroke)', 'stroke-width': 1, 'stroke-opacity': 0.4 }),
    sv('circle', { cx: 24, cy: 24, r: 5, fill: 'var(--head)', stroke: 'var(--av-stroke)', 'stroke-width': 1 }),
    sv('circle', { cx: 36, cy: 21, r: 5.5, fill: 'var(--head)', stroke: 'var(--av-stroke)', 'stroke-width': 1 }),
    sv('circle', { cx: 48, cy: 24, r: 5, fill: 'var(--head)', stroke: 'var(--av-stroke)', 'stroke-width': 1 }));
  return s;
}
function miniAvatar(name) {
  if (name === BOARD) { const p = podium(); p.setAttribute('class', 'av mini'); return p; }
  const w = S.M && S.M.workers.get(name);
  if (!w) { const s = sv('svg', { class: 'av mini', viewBox: '0 0 72 74' }, sv('circle', { cx: 36, cy: 37, r: 14, fill: 'var(--line2)' })); return s; }
  const a = avatar((w.avatar && w.avatar.figure) || 'plain', w.avatar && w.avatar.color, 'idle', { static: true, size: 26 });
  a.classList.add('mini');
  return a;
}
function envelopeIcon(kind) {
  const fill = kind === 'assignment' ? 'var(--accent)' : kind === 'report' ? 'var(--ok)' : kind === 'question' ? 'var(--info)' : 'var(--paper)';
  return sv('svg', { viewBox: '0 0 22 16' },
    sv('rect', { x: 1, y: 1, width: 20, height: 14, rx: 2.5, fill, stroke: 'var(--av-stroke)', 'stroke-width': 1 }),
    sv('path', { d: 'M2 2.5 l9 6.5 l9 -6.5', fill: 'none', stroke: 'var(--av-stroke)', 'stroke-width': 1 }));
}
const ICONS = {
  lock: () => sv('svg', { viewBox: '0 0 12 12' }, sv('rect', { x: 2, y: 5, width: 8, height: 6, rx: 1.2, fill: 'currentColor' }), sv('path', { d: 'M4 5 V3.5 a2 2 0 0 1 4 0 V5', fill: 'none', stroke: 'currentColor', 'stroke-width': 1.3 })),
  clock: () => sv('svg', { viewBox: '0 0 12 12' }, sv('circle', { cx: 6, cy: 6, r: 4.8, fill: 'none', stroke: 'currentColor', 'stroke-width': 1.3 }), sv('path', { d: 'M6 3.4 V6 l1.8 1.3', fill: 'none', stroke: 'currentColor', 'stroke-width': 1.3, 'stroke-linecap': 'round' })),
  mail: () => sv('svg', { viewBox: '0 0 12 12' }, sv('rect', { x: 1, y: 2.5, width: 10, height: 7, rx: 1.2, fill: 'none', stroke: 'currentColor', 'stroke-width': 1.2 }), sv('path', { d: 'M1.5 3 l4.5 3.4 l4.5 -3.4', fill: 'none', stroke: 'currentColor', 'stroke-width': 1.2 })),
};

// ───────────────────────────── layout ─────────────────────────────
//
// A tidy top-down tree: leaves take consecutive slots, a parent sits over
// the middle of its children. The board is the root; workers without a
// (known) manager, and any caught in a reporting cycle, hang under it.

const NW = 176, NH = 156, GX = 22, GY = 46;
function layoutTree(workers) {
  const names = new Set(workers.map((w) => w.name));
  const parent = new Map();
  for (const w of workers) parent.set(w.name, w.reports_to && names.has(w.reports_to) && w.reports_to !== w.name ? w.reports_to : BOARD);
  const kidsOf = () => {
    const k = new Map([[BOARD, []]]);
    for (const w of workers) k.set(w.name, []);
    for (const w of workers) k.get(parent.get(w.name)).push(w.name);
    return k;
  };
  let kids = kidsOf();
  for (;;) {
    const reach = new Set(), stack = [BOARD];
    while (stack.length) { const n = stack.pop(); if (reach.has(n)) continue; reach.add(n); stack.push(...kids.get(n)); }
    const lost = workers.find((w) => !reach.has(w.name));
    if (!lost) break;
    parent.set(lost.name, BOARD);
    kids = kidsOf();
  }
  const pos = new Map();
  let slot = 0, maxDepth = 0;
  const place = (n, depth) => {
    maxDepth = Math.max(maxDepth, depth);
    const ks = kids.get(n);
    let x;
    if (!ks.length) x = slot++;
    else { for (const k of ks) place(k, depth + 1); x = (pos.get(ks[0]).slot + pos.get(ks[ks.length - 1]).slot) / 2; }
    pos.set(n, { slot: x, depth, x: x * (NW + GX), y: depth * (NH + GY) });
  };
  place(BOARD, 0);
  return { pos, parent, width: Math.max(1, slot) * (NW + GX) - GX, height: (maxDepth + 1) * (NH + GY) - GY };
}

// ───────────────────────────── overview ─────────────────────────────

const OV = { sig: null, nodes: new Map(), lay: null, scale: 1, built: false };

// No company running: Organization has nothing to show yet.
const companyOff = () => live() && !(S.info && S.info.run && !['ended', 'failed'].includes(str(S.info.run.state)));
function syncOverviewOff() {
  const empty = $('ov-empty'), body = $('ov-body');
  if (!empty || !body) return true;
  const off = companyOff();
  empty.classList.toggle('hidden', !off);
  body.classList.toggle('hidden', off);
  return off;
}

function mountOverview() {
  const root = $('tab-overview');
  // The panel may be docked in what is about to be replaced.
  if (root.contains($('panel'))) document.body.insertBefore($('panel'), $('toast'));
  root.replaceChildren(
    el('div', { class: 'ko-center hidden', id: 'ov-empty' }, el('div', { class: 'card', style: 'text-align:center' },
      el('h2', { class: 'serif' }, 'No company yet'),
      el('p', { class: 'muted' }, 'The organization appears here once a company is running: start one in Kickoff, with its name and its CEO.'),
      el('button', { class: 'btn primary', type: 'button', onclick: () => showTab('kickoff') }, 'Go to Kickoff'))),
    el('div', { id: 'ov-body' }));
  $('ov-body').append(
    el('div', { class: 'ovhead' }, el('h2', { class: 'sec', style: 'margin:0' }, 'Organization'), el('span', { class: 'muted', id: 'ov-sub' })),
    el('div', { class: 'hint hidden', id: 'ov-hint' }),
    el('div', { class: 'ovrow' },
      el('div', { class: 'stagebox', id: 'stagebox' }, el('div', { class: 'stage', id: 'stage' })),
      el('div', { class: 'legend side', id: 'ov-legend' })),
    el('div', { id: 'ov-dock' }));
  const lg = $('ov-legend');
  const row = (...kids) => el('div', { class: 'lg' }, ...kids);
  lg.append(el('h4', {}, 'State'));
  for (const s of ['idle', 'working', 'reviewing', 'waiting', 'queued', 'booting', 'over-budget', 'error', 'off']) {
    lg.append(row(sv('svg', { width: 10, height: 10 }, sv('rect', { class: 's-' + s, width: 10, height: 10, rx: 3 })), s));
  }
  // The bar under each worker: what they spent of the tokens they were given.
  const gauge = (pct, color) => el('span', { class: 'gauge' }, el('i', { style: 'width:' + pct + '%;background:var(--' + color + ')' }));
  lg.append(el('div', { id: 'ov-legend-budget' }, el('h4', {}, 'Budget'),
    row(gauge(60, 'ok'), 'spent of what they were given'),
    row(gauge(90, 'warn'), 'under 15% left'),
    row(gauge(100, 'bad'), 'out of tokens'),
    el('div', { class: 'muted small' }, 'Hover a bar for the numbers.')));
  lg.append(el('h4', {}, 'Marks'),
    row(el('span', { class: 'ic mail' }, ICONS.mail(), '2'), 'unread mail'),
    row(el('span', { class: 'ic lock' }, ICONS.lock()), 'waiting for an access decision: a site, git, the cluster or a drive space'),
    row(el('span', { class: 'ic clock' }, ICONS.clock(), '1'), 'overdue work'),
    row(el('span', { class: 'dashed' }), 'contractor'),
    row(el('span', { class: 'agentsw' }), 'on another agent (pi); its name and model under the title'),
    el('div', { class: 'muted small' }, 'Envelopes fly as mail is sent; those from the post office box at the top right are its automatic notices. Click a worker to write to them.'));
  OV.sig = null;
  syncOverviewOff();
  placePanel();
}
function buildOverview() {
  const ws = chartWorkers();
  const sig = JSON.stringify([boardName(), ws.map((w) => [w.name, w.reports_to, w.title, w.avatar, w.contract, w.agent, w.model])]);
  if (sig === OV.sig) return;
  OV.sig = sig;
  const stage = $('stage');
  stage.replaceChildren();
  OV.nodes.clear();
  if (!ws.length) { OV.lay = null; stage.append(el('div', { class: 'empty', style: 'width:600px' }, 'No workers in this firm.')); fitOverview(); return; }
  const lay = layoutTree(ws);
  OV.lay = lay;
  stage.style.width = lay.width + 'px';
  stage.style.height = lay.height + 'px';
  const edges = sv('svg', { class: 'edges', width: lay.width, height: lay.height });
  for (const w of ws) {
    const a = lay.pos.get(lay.parent.get(w.name)), b = lay.pos.get(w.name);
    const x1 = a.x + NW / 2, y1 = a.y + NH - 6, x2 = b.x + NW / 2, y2 = b.y + 4, my = (y1 + y2) / 2;
    edges.append(sv('path', { d: `M${x1} ${y1} C${x1} ${my} ${x2} ${my} ${x2} ${y2}` }));
  }
  stage.append(edges);
  // The board.
  const bp = lay.pos.get(BOARD);
  const bBadge = el('div', { class: 'bdg' });
  const bnode = el('div', { class: 'node', style: `left:${bp.x}px;top:${bp.y}px`, title: 'The board: open its inbox', onclick: () => { MSG.view = 'inbox'; showTab('messages'); } },
    podium(), el('div', { class: 'nm' }, boardName()), el('div', { class: 'tt' }, 'the board'), el('div', { class: 'st', id: 'ov-board-st' }, ''), bBadge);
  stage.append(bnode);
  OV.nodes.set(BOARD, { el: bnode, bdg: bBadge });
  // The post office: where its automatic notices (access decisions,
  // follow-ups, welcomes) fly from.
  stage.append(el('div', { class: 'postoffice', style: `left:${lay.width - 92}px;top:2px`, title: 'The post office: automatic notices (access decisions, follow-ups, budget notices) come from here' },
    el('span', { class: 'ic' }, ICONS.mail()), 'post office'));
  for (const w of ws) {
    const p = lay.pos.get(w.name);
    const av = avatar((w.avatar && w.avatar.figure) || 'plain', w.avatar && w.avatar.color, w.state);
    const st = el('div', { class: 'st' });
    const rm = el('div', { class: 'rm-state' });
    const bdg = el('div', { class: 'bdg' });
    // Budget: spent of what they were given.
    const fill = el('i');
    const bar = el('div', { class: 'nbud' + (fundsOn(S.M) ? '' : ' hidden') }, fill);
    const fresh = w.hiredAt != null && T() - w.hiredAt < 8;
    const node = el('div', { class: 'node' + (fresh ? ' joined' : '') + (w.contract ? ' contractor' : '') + (w.agent && w.agent !== 'claude-code' ? ' agent-other' : ''), style: `left:${p.x}px;top:${p.y}px`, tabindex: 0, role: 'button', onclick: () => openPanel(w.name),
      onkeydown: (ev) => { if (ev.key === 'Enter' || ev.key === ' ') { ev.preventDefault(); openPanel(w.name); } } },
    av, el('div', { class: 'nm' }, cap(w.name)), el('div', { class: 'tt', title: agentName(w.agent) + (w.model ? ', model ' + w.model : '') + (w.profile ? ', profile ' + w.profile : '') }, (w.contract ? 'contractor · ' : '') + (w.title || w.role) + (agentLabel(w) ? ' · ' + agentLabel(w) : '')), st, rm, bar, bdg);
    stage.append(node);
    OV.nodes.set(w.name, { el: node, av, st, rm, bdg, bar, fill });
  }
  fitOverview();
}
function fitOverview() {
  const box = $('stagebox'), stage = $('stage');
  if (!box || !OV.lay) { if (box) box.style.height = '120px'; return; }
  const avail = Math.max(200, box.clientWidth - 24);
  const scale = Math.min(1, avail / OV.lay.width);
  OV.scale = scale;
  const left = (box.clientWidth - OV.lay.width * scale) / 2;
  stage.style.transform = `translate(${left}px, 18px) scale(${scale})`;
  box.style.height = Math.ceil(OV.lay.height * scale + 36) + 'px';
}
function updateOverview() {
  if (syncOverviewOff()) return;
  buildOverview();
  const M = S.M, now = T();
  const sub = [];
  if (M.domain) sub.push(M.domain);
  if (M.started) sub.push((live() ? 'run started ' : 'run of ') + fmtDate(M.started));
  $('ov-sub').textContent = sub.join(' · ');
  $('ov-legend-budget').classList.toggle('hidden', !fundsOn(M));
  for (const [name, n] of OV.nodes) {
    if (name === BOARD) {
      const unread = live() ? S.boardUnread.size : 0;
      const open = M ? [...M.obl.values()].filter((o) => !o.settled && o.creditor === BOARD).length : 0;
      $('ov-board-st').textContent = open ? 'waiting on ' + open + ' answer' + (open > 1 ? 's' : '') : '';
      n.bdg.replaceChildren(unread ? el('span', { class: 'ic mail', title: unread + ' unread' }, ICONS.mail(), unread) : '');
      continue;
    }
    const w = M.workers.get(name);
    if (!w) continue;
    // A candidate still negotiating is a pale ghost of a contractor.
    const k = w.contract && M.contracts.get(w.contract);
    n.el.classList.toggle('quoting', !!k && ['offered', 'quoted', 'countered'].includes(k.state));
    setAvatarState(n.av, w.state);
    const sl = statusLine(M, w, now);
    n.st.textContent = sl.text; n.st.title = sl.text;
    if (n.bar && fundsOn(M)) {
      const got = M.fundsIn.get(name) || 0, bal = balanceOf(M, name);
      const frac = got > 0 ? Math.min(1, Math.max(0, 1 - bal / got)) : bal <= 0 ? 1 : 0;
      n.fill.style.width = (frac * 100).toFixed(1) + '%';
      n.bar.classList.toggle('low', got > 0 && bal < got * 0.15);
      n.bar.classList.toggle('out', bal <= 0);
      n.bar.title = 'budget: ' + balanceText(bal, got) + ' · ' + fmtTok(w.tokens) + ' spent';
    }
    n.st.className = 'st' + (sl.bad ? ' bad' : '');
    n.rm.textContent = (STATE_ICON[w.state] || '') + ' ' + w.state;
    const d = desk(M, name, now), unread = workerUnread(name);
    n.bdg.replaceChildren(
      unread ? el('span', { class: 'ic mail', title: unread + ' unread: click to see them', onclick: (ev) => { ev.stopPropagation(); openPanel(name); } }, ICONS.mail(), unread) : '',
      d.reqs.length + d.creds.length ? el('span', { class: 'ic lock', title: [d.reqs.length ? d.reqs.length + ' site(s)' : '', d.creds.length ? d.creds.map((q) => q.resource).join(', ') : ''].filter(Boolean).join(' and ') + ': waiting for an access decision' }, ICONS.lock()) : '',
      d.overdue.length ? el('span', { class: 'ic clock', title: d.overdue.length + ' overdue' }, ICONS.clock(), d.overdue.length) : '');
    n.el.classList.toggle('sel', PANEL.name === name);
  }
  // Kickoff: a running firm with no threads yet gets its first task.
  const hint = $('ov-hint');
  const kick = live() && M.runState === 'running' && M.threads.size === 0;
  hint.classList.toggle('hidden', !kick);
  if (kick) {
    hint.textContent = 'The firm is running and idle. Give ' + (rootWorker() || 'the CEO') + ' the firm\'s first task: click here, or on ' + (rootWorker() || 'the CEO') + '.';
    hint.style.cursor = 'pointer';
    hint.onclick = () => { if (rootWorker()) openPanel(rootWorker(), { kickoff: true }); };

  }
}
// An envelope from one node to another.
function flyMail(m) {
  if (REDUCED.matches || S.tab !== 'overview' || !OV.lay || document.hidden) return;
  const stage = $('stage');
  if (!stage || stage.querySelectorAll('.env').length > 30) return;
  const centre = (n) => { const p = OV.lay.pos.get(n); return p ? { x: p.x + NW / 2, y: p.y + 38 } : null; };
  // Mail from no one in the chart (the post office's notices) comes from
  // its box at the top right.
  const from = centre(m.from) || { x: OV.lay.width - 60, y: 8 };
  const targets = [...new Set([...m.to, ...m.cc])].filter((r) => r !== m.from);
  targets.forEach((r, i) => {
    const to = centre(r);
    if (!to) return;
    const dx = to.x - from.x, dy = to.y - from.y, len = Math.hypot(dx, dy) || 1;
    const bend = Math.min(90, len * 0.2) * (i % 2 ? -1 : 1);
    const cx = (from.x + to.x) / 2 - (dy / len) * bend, cy = (from.y + to.y) / 2 + (dx / len) * bend;
    const frames = [];
    for (let k = 0; k <= 24; k++) {
      const t = k / 24, u = 1 - t;
      const x = u * u * from.x + 2 * u * t * cx + t * t * to.x, y = u * u * from.y + 2 * u * t * cy + t * t * to.y;
      frames.push({ transform: `translate(${x.toFixed(1)}px, ${y.toFixed(1)}px) scale(${(1 + 0.25 * Math.sin(Math.PI * t)).toFixed(3)})`, opacity: k === 24 ? 0 : 1 });
    }
    const env = el('div', { class: 'env' }, envelopeIcon(m.kind));
    env.style.transform = frames[0].transform;
    stage.append(env);
    const anim = env.animate(frames, { duration: 1100 + i * 120, easing: 'ease-in-out', delay: i * 60 });
    anim.onfinish = () => env.remove();
    anim.oncancel = () => env.remove();
  });
}
window.addEventListener('resize', () => fitOverview());

// ─────────────────────────── worker panel ───────────────────────────

const PANEL = { name: null, form: null, tr: null };

function openPanel(name, opts) {
  opts = opts || {};
  const M = S.M, w = M && M.workers.get(name);
  if (!w) return;
  closeTranscript();
  PANEL.name = name;
  const p = $('panel');
  const fig = (w.avatar && w.avatar.figure) || 'plain';
  PANEL.av = avatar(fig, w.avatar && w.avatar.color, w.state, { size: 60 });
  PANEL.state = el('div', { class: 'small pstate' });
  PANEL.form = composeForm({ fixedTo: [name], kinds: ['assignment', 'question', 'fyi'], kind: 'assignment' });
  PANEL.desk = el('div', { class: 'desk' });
  PANEL.feed = el('div', { class: 'feed' });
  PANEL.unread = el('div', { class: 'list' });
  // Published work and mail, with counts on their headers: the mail open,
  // published work collapsed; each can be opened or closed.
  const group = (title, open, count, ...body) => el('details', { class: 'grp', open }, el('summary', {}, el('span', { class: 'gh' }, title), count), ...body);
  PANEL.unreadN = el('span', { class: 'cnt' });
  PANEL.readN = el('span', { class: 'cnt' });
  PANEL.pubN = el('span', { class: 'cnt' });
  PANEL.unreadBox = group('Unread mail', true, PANEL.unreadN, el('div', { class: 'muted small', id: 'panel-unread-why' }), PANEL.unread);
  PANEL.read = el('div', { class: 'list' });
  PANEL.readAll = false;
  PANEL.readMore = el('button', { class: 'btn small ghost hidden', type: 'button', onclick: () => { PANEL.readAll = !PANEL.readAll; updatePanel(); } }, '');
  PANEL.pub = el('div', { class: 'list' });
  PANEL.earlier = el('button', { class: 'btn small ghost', onclick: () => { if (PANEL.tr) { PANEL.tr.all = !PANEL.tr.all; renderFeed(); } } }, 'Show earlier turns');
  // Three groups: one column on the side; side by side under the org
  // chart, the turn's log in the middle.
  p.replaceChildren(
    el('div', { class: 'pg-head' },
      el('div', { class: 'ph' }, PANEL.av,
        el('div', {}, el('h2', {}, cap(name)), el('div', { class: 'muted' }, [w.title, w.role !== w.title ? w.role : '', w.officer ? 'security officer' : '', agentName(w.agent), w.model ? modelName(w.model) : ''].filter(Boolean).join(' · '))),
        el('button', { class: 'btn ghost close', title: 'Close', onclick: closePanel }, '✕')),
      PANEL.state,
      opts.kickoff ? el('div', { class: 'hint', style: 'margin-top:12px' }, 'Give ' + name + ' the firm\'s first task.') : ''),
    el('div', { class: 'pg-a' },
      el('h3', {}, 'Write as the board'), PANEL.form,
      el('h3', {}, 'Desk'), PANEL.desk),
    el('div', { class: 'pg-b' },
      el('div', { style: 'display:flex;align-items:baseline;gap:8px' }, el('h3', {}, 'Now'), PANEL.earlier), PANEL.feed),
    el('div', { class: 'pg-c' },
      group('Published', false, PANEL.pubN, PANEL.pub),
      PANEL.unreadBox,
      group('Read mail', true, PANEL.readN, PANEL.read, PANEL.readMore)));
  p.classList.add('open');
  p.scrollTop = 0;
  placePanel();
  if (p.classList.contains('docked')) requestAnimationFrame(() => p.scrollIntoView({ block: 'nearest', behavior: REDUCED.matches ? 'auto' : 'smooth' }));
  if (opts.kickoff) setTimeout(() => { const s = PANEL.form.querySelector('input[name=subject]'); if (s) s.focus(); }, 250);
  openTranscript(name);
  updatePanel();
  if (S.tab === 'overview') updateOverview();
}
function closePanel() {
  closeTranscript();
  PANEL.name = null;
  $('panel').classList.remove('open');
  placePanel();
  if (S.tab === 'overview' && S.M) updateOverview();
}
// On the Organization tab the worker's panel sits under the chart; on
// the others it slides in from the side.
function placePanel() {
  const p = $('panel'), dock = $('ov-dock');
  const docked = !!(dock && PANEL.name && S.tab === 'overview');
  if (docked && p.parentNode !== dock) dock.append(p);
  if (!docked && p.parentNode !== document.body) document.body.insertBefore(p, $('toast'));
  p.classList.toggle('docked', docked);
}
function updatePanel() {
  if (!PANEL.name || !S.M) return;
  const M = S.M, name = PANEL.name, w = M.workers.get(name), now = T();
  if (!w) return;
  setAvatarState(PANEL.av, w.state);
  const sess = workerSession(name);
  const sl = statusLine(M, w, now);
  PANEL.state.replaceChildren(el('div', { style: 'display:flex;gap:8px;align-items:center;flex-wrap:wrap;margin-top:10px' },
    pill(w.state === 'error' ? 'error' : w.state === 'working' || w.state === 'reviewing' ? 'accent' : '', w.state), el('span', {}, sl.text),
    sess ? el('span', { class: 'muted small', style: 'margin-left:auto' }, sessionsLink('Session'), ' ', el('span', { class: 'mono' }, sess)) : ''),
  el('div', { class: 'muted small', style: 'margin-top:4px' }, `${w.turns} turns · ${fmtNum(w.tokens)} tokens` + (w.rateLimited ? ` · ${w.rateLimited} rate-limited` : '')),
  fundsOn(M) ? el('div', { class: 'small', style: 'margin-top:4px' }, 'Budget: ', ...((p) => [el('b', {}, p[0]), p[1]])(balanceParts(balanceOf(M, name), M.fundsIn.get(name) || 0)), ((M.fundsOut.get(name) || 0) ? ', ' + fmtTok(M.fundsOut.get(name)) + ' passed on to their team' : '') + (w.exhausted && balanceOf(M, name) <= 0 ? ' · out of budget' : '')) : '',
  contractLine(M, w),
  grantsLine(M, w),
  w.hiredBy && !w.contract ? el('div', { class: 'small muted', style: 'margin-top:4px' }, 'Hired by ' + w.hiredBy + ', ' + fmtDate(w.hiredAt) + ', with ' + fmtTok(w.hireBudget) + ': ' + clip(w.hireReason, 200)) : '',
  w.left ? el('div', { class: 'small', style: 'margin-top:4px' }, 'Left ' + fmtDate(w.left.ts) + ', let go by ' + w.left.by + ': ' + clip(w.left.reason, 200)) : '');
  PANEL.form.refresh();
  // Desk.
  const d = desk(M, name, now);
  const items = [];
  const oblItem = (o, owes) => {
    const m = M.mails.get(o.message);
    const other = owes ? o.creditor : o.debtor;
    return el('div', { class: 'item' },
      overdue(o, now) ? pill('overdue', 'overdue') : pill(o.kind, o.kind),
      el('span', {}, (owes ? 'owes ' : 'waits on ') + other + ': '),
      el('button', { class: 'linkish', onclick: () => openThread(o.thread) }, clip((m && (m.subject || (M.threads.get(o.thread) || {}).subject)) || o.thread, 60)),
      o.due ? el('span', { class: 'muted small' }, 'due ' + fmtDate(o.due)) : '');
  };
  for (const o of d.owes) items.push(oblItem(o, true));
  for (const o of d.waits) items.push(oblItem(o, false));
  for (const r of d.reqs) items.push(el('div', { class: 'item' }, pill(r.state, r.state), el('span', {}, 'access: ' + (r.method ? r.method + ' ' : '') + r.host + (r.port && r.port !== 443 ? ':' + r.port : '') + (r.path || '')),
    el('button', { class: 'linkish', onclick: () => showTab('security') }, 'security')));
  for (const q of d.creds) items.push(el('div', { class: 'item' }, pill(q.state, q.state), el('span', {}, 'grant: ' + q.resource + ' ' + q.level),
    el('button', { class: 'linkish', onclick: () => showTab('security') }, 'security')));
  PANEL.desk.replaceChildren(...(items.length ? items : [el('div', { class: 'muted' }, 'Nothing on the desk.')]));
  // Unread: delivered, not yet handed to a turn. A worker reads its mail
  // when it wakes, so say when that will be.
  const unread = [...w.unread].map((id) => M.mails.get(id)).filter(Boolean).sort((a, b) => b.ts - a.ts);
  PANEL.unreadBox.classList.toggle('unread-box', !!unread.length);
  PANEL.unreadN.textContent = unread.length;
  PANEL.unreadN.classList.toggle('hot', !!unread.length);
  PANEL.unreadN.title = unread.length ? unread.length + ' not read yet' : 'nothing unread';
  if (!unread.length) {
    $('panel-unread-why').textContent = '';
    PANEL.unread.replaceChildren(el('div', { class: 'muted' }, 'Nothing unread.'));
  } else {
    const why = { working: name + ' is in a turn and reads these when it ends.', reviewing: name + ' is in a turn and reads these when it ends.',
      queued: name + ' is queued for a turn (the firm runs a few at once) and reads these then.', booting: name + '\'s VM is starting; it reads these once up.',
      'over-budget': name + ' is over budget: these wait until the budget allows a turn.', error: name + ' had an error; these wait for the next turn.',
      off: 'The firm is off: these were never read.' }[w.state] || name + ' reads these at its next turn.';
    $('panel-unread-why').textContent = M.runState === 'paused' ? 'The firm is paused: ' + name + ' reads these when it resumes.' : why;
    PANEL.unread.replaceChildren(...unread.map((m) => mailItem(m, true)));
  }
  // Read: handed to a turn (at its start, or while it ran), newest first.
  const read = [...w.readAt].map(([id, at]) => [M.mails.get(id), at]).filter(([m]) => m).sort((a, b) => b[1] - a[1]);
  const READ_SHOWN = 10;
  PANEL.readN.textContent = read.length;
  const shown = PANEL.readAll ? read : read.slice(0, READ_SHOWN);
  PANEL.read.replaceChildren(...(shown.length ? shown.map(([m, at]) => mailItem(m, false, at)) : [el('div', { class: 'muted' }, 'Nothing read yet.')]));
  PANEL.readMore.classList.toggle('hidden', read.length <= READ_SHOWN);
  PANEL.readMore.textContent = PANEL.readAll ? 'Show the latest ' + READ_SHOWN : 'Show all ' + read.length;
  const arts = [...M.artifacts.values()].filter((a) => a.author === name).reverse();
  PANEL.pub.replaceChildren(...(arts.length ? arts.map((a) => artifactItem(a)) : [el('div', { class: 'muted' }, 'Nothing published.')]));
  PANEL.pubN.textContent = arts.length;
  if (S.mode === 'replay') renderFeed();
}
// The sessions UI, with the firm to come back to.
function sessionsHref() { return '/sessions' + (S.firm ? '?firm=' + enc(S.firm) : ''); }
function sessionsLink(text) { const a = el('a', {}, text); a.href = sessionsHref(); return a; }
// readAt: when the worker read it, for the panel's Read mail.
function mailItem(m, unread, readAt) {
  const t = S.M.threads.get(m.thread);
  return el('div', { class: 'li', onclick: () => openThread(m.thread, m.id) },
    el('div', { class: 'top' }, el('span', { class: 'subj' }, unread ? el('span', { class: 'dot', title: 'not read yet' }) : '', unread ? ' ' : '', clip((t && t.subject) || m.subject || '(no subject)', 70)), pill(m.kind, m.kind)),
    el('div', { class: 'meta' }, (m.from === BOARD ? boardName() : cap(m.from)) + ' → ' + caps(m.to), el('span', {}, '· sent ' + ago(m.ts)),
      readAt ? el('span', { title: fmtDate(readAt) }, ' · read ' + ago(readAt) + (readAt - m.ts >= 60 ? ' (after ' + fmtDur(readAt - m.ts) + ')' : '')) : ''));
}
function artifactItem(a) {
  const v = a.versions[a.versions.length - 1];
  return el('div', { class: 'li', onclick: () => openDrive(a.id, v.version) },
    el('div', { class: 'top' }, el('span', { class: 'subj' }, clip(a.title || a.id, 70)), pill(a.kind, a.kind)),
    el('div', { class: 'meta' }, a.id + ' v' + v.version + ' · ' + a.author + ' · ' + ago(v.ts)));
}
