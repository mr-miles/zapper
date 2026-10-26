'use strict';
// The office page: the Plans tab.
//
// Staged plans and what became of each child. Plans are rebuilt from journal
// snapshots (see model.js's `plan` case), so this view works live and in
// replay. Read-only: the plan tools belong to the workers, not the board.

const PLANS = { sel: null, sig: null };

function mountPlans() {
  $('tab-plans').replaceChildren(
    el('div', { class: 'ovhead' }, el('h2', { class: 'sec', style: 'margin:0' }, 'Plans'), el('span', { class: 'muted', id: 'plans-sub' })),
    el('div', { id: 'plans-body' }, el('div', { class: 'empty card' }, 'No plans yet.')),
  );
  PLANS.sig = null;
}

// running | paused | closed → a pill class the stylesheet already colours.
const PLAN_PILL = { active: 'running', paused: 'paused', closed: 'closed' };
const planChildState = (c) => (c.done ? 'done' : c.thread ? 'sent' : 'pending');

function updatePlans() {
  const M = S.M, body = $('plans-body');
  if (!body) return;
  const plans = [...M.plans.values()].sort((a, b) => (a.ts || 0) - (b.ts || 0) || str(a.id).localeCompare(str(b.id)));
  // Re-render only when something a card shows has changed.
  const shape = (p) => p.id + '/' + p.status + '/' + p.stage + '/' + (p.problems ? p.problems.length : 0) + '/' +
    arr(p.stages).map((s) => arr(s.children).map(planChildState).join('')).join('|');
  const sig = [plans.map(shape).join(';'), PLANS.sel].join('#');
  if (sig === PLANS.sig) return;
  PLANS.sig = sig;
  $('plans-sub').textContent = plans.length ? plans.length + ' plan' + (plans.length === 1 ? '' : 's') : '';
  if (!plans.length) {
    body.replaceChildren(el('div', { class: 'empty card' }, 'No plans yet. A manager lays out a staged plan with the plan tool; its stages and child assignments show here as they run, and each assignment is tagged with its plan in the Work and Messages tabs.'));
    return;
  }
  body.replaceChildren(el('div', { class: 'plans' }, ...plans.map((p) => planCard(M, p))));
}

function planCard(M, p) {
  const ownerName = p.owner === BOARD ? boardName() : cap(str(p.owner));
  const head = el('div', { class: 'plan-head' },
    el('span', { class: 'subj' }, 'plan ' + str(p.id).replace(/^plan-/, '#')),
    pill(PLAN_PILL[p.status] || '', str(p.status) || ''),
    el('span', { class: 'muted' }, 'owner ' + ownerName),
    p.source ? el('button', { class: 'btn small', type: 'button', title: 'Open the task this plan is under', onclick: (ev) => { ev.stopPropagation(); openThread(str(p.source)); } }, 'task ' + clip(str(p.source), 24)) : '');
  const kids = [head];
  if (p.problems && p.problems.length) {
    kids.push(el('div', { class: 'plan-problems' },
      el('div', { class: 'muted' }, 'Could not staff:'),
      ...p.problems.map((x) => el('div', {}, el('span', { class: 'slot' }, str(x.slot)), ' — ' + str(x.error)))));
  }
  arr(p.stages).forEach((st, i) => {
    const when = i === p.stage ? ' · current' : i < p.stage ? ' · done' : ' · later';
    kids.push(el('div', { class: 'plan-stage' + (i === p.stage ? ' active' : '') },
      el('div', { class: 'plan-stage-h' }, 'Stage ' + (i + 1) + when),
      ...arr(st.children).map((c) => planChild(p, c))));
  });
  return el('div', { class: 'card plan-card' + (PLANS.sel === p.id ? ' sel' : ''), onclick: () => { PLANS.sel = p.id; PLANS.sig = null; updatePlans(); } }, ...kids);
}

function planChild(p, c) {
  const slot = c.spec && c.spec.slot ? str(c.spec.slot) : '';
  const bound = p.bindings && p.bindings[slot];
  const team = (p.team && p.team[slot]) || {};
  const target = bound ? cap(str(bound))
    : team.name ? cap(str(team.name)) + ' (named)'
    : team.role ? 'role ' + str(team.role) + (team.budget ? ' · ' + fmtNum(team.budget) : '')
    : slot;
  const state = planChildState(c);
  const stPill = pill(state === 'done' ? 'closed' : state === 'sent' ? 'running' : 'pending', state === 'sent' ? 'in progress' : state);
  const link = c.thread ? el('button', { class: 'btn small', type: 'button', title: 'Open this assignment', onclick: (ev) => { ev.stopPropagation(); openThread(str(c.thread)); } }, 'open') : '';
  return el('div', { class: 'plan-child' }, stPill, el('span', { class: 'slot' }, slot + ' → ' + target), el('span', { class: 'subj' }, clip(str(c.spec && c.spec.subject) || '(no subject)', 60)), link);
}
