'use strict';
// The office page: the Kickoff tab.

// ────────────────────────────── kickoff ──────────────────────────────
//
// The board's deal with the CEO, and only the CEO: the task, their
// conversation, the budget the CEO asks for and the board approves, and
// the numbers the board follows the run by. Every word of it is mail.

const KO = { sig: null, msgSig: null, form: null, formFor: null };

function mountKickoff() {
  $('tab-kickoff').replaceChildren(el('div', { id: 'ko-start' }), el('div', { class: 'ko', id: 'ko-main' },
    el('div', {}, el('div', { id: 'ko-conv' }), el('div', { id: 'ko-write' })),
    el('div', { id: 'ko-side' })));
  KO.sig = null; KO.msgSig = null; KO.form = null; KO.formFor = null; KO.starting = false; KO.startEl = null; KO.startKey = null;
}

// The company's name: the run's, else the firm's default.
const companyName = (M) => (live() ? S.info && S.info.company : M && M.company) || (M && M.company) || (S.info && S.info.company) || S.firm || 'office';

// Who holds security now, in words.
function securityHolder(M, root) {
  const officers = M ? [...M.workers.values()].filter((w) => w.inChart && w.officer).map((w) => w.name) : [];
  if (!officers.length) return 'the board';
  if (officers.length === 1 && officers[0] === root) return cap(root) + ' (the CEO, until they hire an officer)';
  return caps(officers);
}

// The run's charter, folded: the articles, the terms and the board's own.
function charterView(M) {
  const ch = arr(S.info && S.info.start && S.info.start.charter);
  if (!ch.length) return '';
  const b = (M && M.budget) || {};
  const terms = [b.seed ? 'seed ' + fmtTok(b.seed) + ' tokens' : '', b.seed ? 'security\'s share ' + Math.round(Number(b.security_share || 0) * 100) + '%' : '',
    b.max_concurrent ? 'agent parallelism ' + b.max_concurrent : '', b.daily_turns ? b.daily_turns + ' turns a day' : ''].filter(Boolean).join(' · ');
  return el('details', { class: 'ko-charter', open: !!KO.charterOpen, ontoggle: (ev) => { KO.charterOpen = ev.target.open; } }, el('summary', { class: 'muted small' }, 'Charter'),
    ...ch.map((a, i) => el('div', { class: 'small', style: 'margin:4px 0' }, el('b', {}, (i + 1) + '. ' + str(a.title) + '. '), str(a.text))),
    terms ? el('div', { class: 'small', style: 'margin:4px 0' }, el('b', {}, 'Terms: '), terms) : '',
    M && M.articles ? el('div', { class: 'small', style: 'margin:4px 0' }, el('b', {}, 'The board\'s own articles:'), renderMarkdown(M.articles)) : '');
}

// A name input with a die that fills in a random one.
function dice(input, gen) {
  const roll = el('button', { class: 'btn dice', type: 'button', title: 'A random name', 'aria-label': 'A random name', onclick: () => { input.value = gen(); input.focus(); } }, '🎲');
  return el('span', { class: 'dice-row' }, input, roll);
}
const pick = (list) => list[Math.floor(Math.random() * list.length)];
function randomCompany() {
  return pick(['Northwind', 'Cobalt', 'Lumen', 'Brightline', 'Quill', 'Harbor', 'Juniper', 'Orbit', 'Pioneer', 'Saffron', 'Tidal', 'Vertex', 'Kestrel', 'Meridian', 'Aster', 'Fathom'])
    + ' ' + pick(['Labs', 'Works', 'Systems', '& Co.', 'Studio', 'Industries', 'Collective', 'Analytics', 'Dynamics', 'Foundry', 'Inc.', 'Ltd']);
}
function randomPerson() {
  return cap(pick(['ada', 'grace', 'alan', 'linus', 'margaret', 'barbara', 'dennis', 'hedy', 'katherine', 'edsger', 'donald', 'frances', 'radia', 'sophie', 'vint', 'guido', 'anders', 'bjarne', 'annie', 'ellen', 'mary', 'john', 'niklaus', 'tim']));
}

// Starting a company: its name, its founders, and its charter (the rules
// the office enforces, with the terms the board sets), for this run only.
function startForm(running) {
  const info = S.info || {};
  const st = info.start || {};
  const company = el('input', { value: str(info.company), maxlength: 80, placeholder: 'Acme Inc.' });
  const ceo = el('input', { value: cap(str(st.ceo)), maxlength: 32, placeholder: 'Barbara' });
  // An extra prompt: the board's own instructions to the CEO for this run,
  // over their skill.
  const ceoPrompt = el('textarea', { rows: 4, maxlength: 20000, placeholder: 'Optional: instructions for the CEO for this run, e.g. "Keep the team small; ship a first version within an hour."' });
  const founder = (title, nameInput, promptInput, hint) => el('fieldset', { class: 'ko-founder' }, el('legend', {}, title),
    el('label', { class: 'f' }, 'Name', dice(nameInput, randomPerson)),
    el('details', {}, el('summary', { class: 'muted small' }, 'Extra prompt (optional): ' + hint), promptInput));
  // The terms.
  const seed = el('input', { value: st.seed ? fmtTok(st.seed) : '', maxlength: 12, placeholder: '1.2M' });
  const share = el('input', { type: 'number', min: 0, max: 100, step: 1, value: st.security_share != null ? String(Math.round(Number(st.security_share) * 100)) : '20' });
  const conc = el('input', { type: 'number', min: 1, max: 64, step: 1, value: String(st.max_concurrent || 3) });
  const turns = el('input', { type: 'number', min: 0, step: 10, value: String(st.daily_turns != null ? st.daily_turns : 400) });
  const articles = el('textarea', { rows: 4, maxlength: 20000, placeholder: 'Optional: your own articles, binding on everyone, e.g. "No paid services without the board\'s approval." "Everything we ship is open source."' });
  // The task: sent by the board as the CEO's first assignment once the
  // company is up. First line the subject, as Copy task in Runs gives it.
  const task = el('textarea', { rows: 6, maxlength: 60000, placeholder: 'Optional: the task for the CEO, sent by the board as soon as the company is up. First line: the subject; then the message. Paste one from Runs (Copy task) to repeat it.' });
  // A scenario: hires with extra instructions and board actions during the
  // run, from the config's presets, all editable here.
  const scenarios = st.scenarios || {};
  const hireRoles = arr(st.roles).filter((r) => !r.officer && r.name !== 'ceo').map((r) => str(r.name));
  const scSel = el('select', { onchange: () => loadScenario() }, el('option', { value: '' }, 'None: a plain run'), ...Object.keys(scenarios).map((n) => el('option', { value: n }, scenarios[n].note ? n + ' (' + str(scenarios[n].note) + ')' : n)));
  const scDesc = el('div', { class: 'muted small' });
  const slotsBox = el('div');
  const stepsBox = el('div');
  const scBody = el('div', { class: 'hidden' },
    el('h4', { style: 'margin:8px 0 2px' }, 'Hires with extra instructions'),
    el('div', { class: 'muted small' }, 'The first hires of the role, in this order, get the instructions; later hires are plain.'),
    slotsBox,
    el('button', { class: 'btn', type: 'button', onclick: () => addSlot({ role: hireRoles[0] || '', count: 1, contract: null, prompt: '', board_line: false }) }, 'Add hire'),
    el('h4', { style: 'margin:12px 0 2px' }, 'What the board does during the run'),
    stepsBox);
  const slotRows = [];
  const stepRows = [];
  const addSlot = (h) => {
    const role = el('select', {}, ...hireRoles.map((r) => el('option', { value: r, selected: r === h.role }, r)));
    if (h.role && !hireRoles.includes(h.role)) role.append(el('option', { value: h.role, selected: true }, h.role));
    const count = el('input', { type: 'number', min: 1, max: 20, value: String(h.count || 1) });
    const kind = el('select', {}, el('option', { value: '', selected: h.contract == null }, 'staff or contractor'), el('option', { value: 'staff', selected: h.contract === false }, 'staff only'), el('option', { value: 'contract', selected: h.contract === true }, 'contractor only'));
    const line = el('input', { type: 'checkbox', checked: !!h.board_line });
    // Another agent or model for these hires: a profile of their own.
    const profile = el('select', { title: 'The profile these hires run on: the role\'s, or another agent or model' }, el('option', { value: '', selected: !h.profile }, 'role\'s profile'),
      ...arr(st.profiles).map((p) => el('option', { value: str(p.name), selected: str(p.name) === str(h.profile) }, str(p.name) + ' · ' + agentName(p.agent) + (p.model ? ' · ' + modelName(p.model) : ''))));
    if (h.profile && !arr(st.profiles).some((p) => str(p.name) === str(h.profile))) profile.append(el('option', { value: str(h.profile), selected: true }, str(h.profile)));
    const prompt = el('textarea', { rows: 6, maxlength: 20000, placeholder: 'The board\'s instructions for these hires' });
    prompt.value = str(h.prompt);
    const box = el('div', { class: 'ko-slot' });
    const row = { role, count, kind, line, profile, prompt, box };
    slotRows.push(row);
    box.append(el('div', { class: 'row' }, role, el('label', {}, '× ', count), kind, profile, el('label', { class: 'small' }, line, ' may mail the board'),
      el('button', { class: 'btn', type: 'button', onclick: () => { slotRows.splice(slotRows.indexOf(row), 1); box.remove(); } }, 'Remove')), prompt);
    slotsBox.append(box);
  };
  const stepText = (x) => x.kill ? 'Kill ' + (x.kill.worker ? x.kill.worker : 'the busiest ' + str(x.kill.role)) + ' and take them out of the firm' : x.break_cluster ? 'Break the cluster (' + str(x.break_cluster.mode) + ')' : x.mail ? 'Mail ' + str(x.mail.to) : '';
  const addStep = (x) => {
    const on = el('input', { type: 'checkbox', checked: true });
    const after = el('input', { class: 'after', maxlength: 8, placeholder: '90m', value: str(x.after) });
    const spent = el('input', { type: 'number', min: 0, max: 100, step: 1, value: x.spent != null ? String(Math.round(Number(x.spent) * 100)) : '' });
    const row = { x, on, after, spent };
    const box = el('div', { class: 'ko-step' }, el('div', { class: 'row' }, el('label', {}, on, ' ' + stepText(x)),
      el('label', { class: 'small' }, 'after ', after), el('label', { class: 'small' }, 'or once ', spent, ' % of the budget is spent (both if both)')));
    if (x.mail) {
      row.subject = el('input', { maxlength: 200, value: str(x.mail.subject) });
      row.body = el('textarea', { rows: 5, maxlength: 20000 });
      row.body.value = str(x.mail.body);
      box.append(row.subject, row.body);
    }
    stepRows.push(row);
    stepsBox.append(box);
  };
  const loadScenario = () => {
    slotRows.length = 0; stepRows.length = 0;
    slotsBox.replaceChildren(); stepsBox.replaceChildren();
    const sc = scenarios[scSel.value];
    scBody.classList.toggle('hidden', !sc);
    scDesc.textContent = sc ? str(sc.description) : '';
    if (!sc) return;
    arr(sc.hires).forEach(addSlot);
    arr(sc.script).forEach(addStep);
    if (!arr(sc.script).length) stepsBox.append(el('div', { class: 'muted small' }, 'Nothing: the run is left alone.'));
  };
  // The scenario as the board shaped it, for the start request.
  const scenarioBody = () => {
    if (!scSel.value) return {};
    const hires = slotRows.map((r) => ({ role: r.role.value, count: Number(r.count.value) || 1, contract: r.kind.value === '' ? null : r.kind.value === 'contract', prompt: r.prompt.value.trim(), board_line: r.line.checked, ...(r.profile.value ? { profile: r.profile.value } : {}) }));
    const script = stepRows.filter((r) => r.on.checked).map((r) => {
      const x = JSON.parse(JSON.stringify(r.x));
      x.after = r.after.value.trim() || null;
      x.spent = r.spent.value === '' ? null : Number(r.spent.value) / 100;
      if (x.mail) { x.mail.subject = r.subject.value.trim(); x.mail.body = r.body.value.trim(); }
      return x;
    });
    return { scenario: scSel.value, hires, script };
  };
  // The rules, on the right: the charter's articles, which the terms on
  // the left fill in.
  const charter = arr(st.charter).map((a, i) => el('div', { class: 'ko-art' },
    el('div', { class: 'ko-art-h' }, 'Article ' + (i + 1) + '. ' + str(a.title)),
    el('div', { class: 'small' }, str(a.text))));
  const go = el('button', { class: 'btn primary', type: 'submit' }, running ? 'Stop this one and start' : 'Start');
  const note = el('div', { class: 'muted small' });
  const form = el('form', { class: 'form ko-new', onsubmit: async (ev) => {
    ev.preventDefault();
    if (running && !confirm('Start a new company? ' + companyName(S.M) + '\'s run ends: workers stop, and it stays replayable under Runs.')) return;
    const pct = Number(share.value);
    if (!(pct >= 0 && pct <= 100)) { toast('Security\'s share: a percentage from 0 to 100', true); return; }
    go.disabled = true;
    KO.busy = true;
    try {
      if (running) { note.textContent = 'Stopping ' + companyName(S.M) + '…'; await write('POST', F() + '/down', {}); }
      note.textContent = 'Starting ' + company.value.trim() + '… (the CEO\'s VM boots in about half a minute)';
      await write('POST', F() + '/up', {
        company: company.value.trim() || null, ceo: ceo.value.trim() || null,
        ceo_prompt: ceoPrompt.value.trim() || null,
        seed: seed.value.trim() || null, security_share: pct / 100,
        max_concurrent: Number(conc.value) || null, daily_turns: turns.value === '' ? null : Number(turns.value),
        articles: articles.value.trim() || null,
        task: task.value.trim() || null,
        ...scenarioBody(),
      });
      KO.starting = false; KO.sig = null; KO.startEl = null;
      toast('Started ' + (company.value.trim() || S.firm));
      // A new company: a fresh page for it, nothing kept from the last one.
      if (running) { resetPage(); return; }
    } catch (e) { toast(e.message, true); note.textContent = ''; }
    KO.busy = false;
    go.disabled = false;
    loadFirm();
  } },
    el('h2', { class: 'serif' }, running ? 'Start a new company' : 'Start a company'),
    el('p', { class: 'muted' }, 'It starts with its CEO, who also holds security until they hire a security officer. You give the CEO the task; they scope it on the seed, ask you for the budget it needs, and hire the team, under the charter on the right.'),
    el('label', { class: 'f' }, 'Company name', dice(company, randomCompany)),
    founder('CEO', ceo, ceoPrompt, 'takes precedence over the CEO\'s skill'),
    el('label', { class: 'f' }, 'The task (the board\'s first assignment to the CEO)', task),
    el('h3', { class: 'ko-ch' }, 'Terms'),
    el('div', { class: 'ko-terms' }, el('label', { class: 'f' }, 'Seed (tokens, to scope the task)', seed), el('label', { class: 'f' }, 'Security\'s share of every grant (%)', share)),
    el('div', { class: 'ko-terms' }, el('label', { class: 'f' }, 'Agent parallelism', conc), el('label', { class: 'f' }, 'Turns a day, whole firm (0: no limit)', turns)),
    el('label', { class: 'f' }, 'Your own articles, added to the charter', articles),
    Object.keys(scenarios).length ? el('fieldset', { class: 'ko-founder' }, el('legend', {}, 'Scenario'), el('label', { class: 'f' }, 'Simulate', scSel), scDesc, scBody) : '',
    el('div', { style: 'display:flex;gap:10px;align-items:center' }, go,
      running ? el('button', { class: 'btn', type: 'button', onclick: () => { KO.starting = false; KO.sig = null; KO.startEl = null; updateKickoff(); } }, 'Cancel') : '', note));
  const rules = el('div', { class: 'ko-rules' },
    el('h3', { class: 'ko-ch' }, 'Charter'),
    el('p', { class: 'muted small' }, 'The company\'s law: the office enforces it, and every worker reads it, with the terms and your own articles.'),
    ...charter);
  return el('div', { class: 'ko-center' }, el('div', { class: 'card ko-start' }, form, rules));
}

// The board's conversation with the CEO: mail between them, oldest first.
function ceoMail(M, root) {
  return [...M.mails.values()].filter((m) => (m.from === BOARD && (m.to.includes(root) || m.cc.includes(root))) || (m.from === root && (m.to.includes(BOARD) || m.cc.includes(BOARD)))).sort((a, b) => a.ts - b.ts);
}

function updateKickoff() {
  const M = S.M;
  if (!$('ko-side')) return;
  const now = T();
  const root = rootWorker();
  const off = live() && !(S.info && S.info.run && !['ended', 'failed'].includes(str(S.info.run.state)));
  // What the board sees here is read, as in Messages.
  if (M && root && !off && live() && S.tab === 'kickoff' && !document.hidden) {
    const ids = ceoMail(M, root).map((m) => m.id).filter((id) => S.boardUnread.has(id));
    if (ids.length) {
      for (const id of ids) S.boardUnread.delete(id);
      write('POST', F() + '/mail/read', { ids }).catch((e) => toast(e.message, true));
      scheduleRender();
    }
  }
  const sig = [S.mode, S.run, off, KO.starting, M ? M.count : 0, Math.floor(now / 15), canWrite(), root, S.info && S.info.company].join('|');
  if (sig === KO.sig) { if (KO.form) KO.form.refresh(); return; }
  KO.sig = sig;
  // Off, or starting anew: only the form, in the middle; the run's numbers
  // and conversation come once it runs.
  if (off || KO.starting) {
    $('ko-main').classList.add('hidden');
    // Built once, so what the board types survives refreshes.
    const key = live() ? (off ? 'new' : 'again') + '|' + S.firm + '|' + !!(S.info && S.info.start) : '';
    if (KO.startKey !== key || !KO.startEl) {
      KO.startKey = key;
      KO.startEl = key ? startForm(!off) : null;
      $('ko-start').replaceChildren(KO.startEl || '');
    }
    return;
  }
  if (KO.startEl) { KO.startEl = null; KO.startKey = null; }
  $('ko-main').classList.remove('hidden');
  // The company and its CEO.
  const started = M && M.started;
  $('ko-start').replaceChildren(el('div', { class: 'ko-head' },
    el('div', {}, el('div', { class: 'serif ko-co' }, companyName(M)),
      el('div', { class: 'muted' }, root ? 'CEO ' + cap(root) + (started ? ' · started ' + fmtDate(started) : '') + ' · security: ' + securityHolder(M, root) : ''),
      charterView(M)),
    live() && canWrite() ? el('button', { class: 'btn', type: 'button', onclick: () => { KO.starting = true; KO.sig = null; updateKickoff(); } }, 'Start a new company') : ''));
  if (!M || !root) { $('ko-conv').replaceChildren(); $('ko-write').replaceChildren(); $('ko-side').replaceChildren(); return; }
  // The conversation.
  const conv = ceoMail(M, root);
  const msgSig = conv.map((m) => m.id).join(',') + '|' + Math.floor(now / 60);
  if (msgSig !== KO.msgSig) {
    KO.msgSig = msgSig;
    $('ko-conv').replaceChildren(
      el('h2', { class: 'sec' }, 'With ' + root + ' (' + ((M.workers.get(root) || {}).title || 'CEO') + ')'),
      ...(conv.length ? conv.map((m) => messageCard(M, m, now)) : [el('div', { class: 'empty card' }, off ? 'Start the firm, then give the CEO the task.' : 'Nothing yet: give ' + root + ' the task below.')]));
  }
  // Writing to the CEO, kept across renders so typing survives.
  if (live() && !off) {
    if (KO.formFor !== root) {
      KO.formFor = root;
      KO.form = composeForm({ fixedTo: [root], kinds: ['assignment', 'question', 'fyi'], kind: 'assignment' });
      $('ko-write').replaceChildren(el('div', { class: 'card', style: 'margin-top:12px' }, el('h3', { style: 'margin:0 0 8px', id: 'ko-write-h' }), KO.form));
    }
    $('ko-write-h').textContent = conv.length ? 'Write to ' + root : 'Give ' + root + ' the task';
    KO.form.refresh();
  } else if (KO.form) { KO.form = null; KO.formFor = null; $('ko-write').replaceChildren(); }
  $('ko-side').replaceChildren(...kickoffSide(M, root, now));
}

function kickoffSide(M, root, now) {
  const out = [];
  if (!fundsOn(M)) {
    out.push(el('div', { class: 'card' }, el('h3', { style: 'margin:0 0 6px' }, 'Budget'), el('div', { class: 'muted' }, 'This firm has no run budget: its founders are its whole team, within the daily limits in the config.')));
    return out;
  }
  // The numbers the board follows the run by.
  const ws = [...M.workers.values()];
  const spent = ws.reduce((n, w) => n + w.tokens, 0);
  const seed = Number(M.budget.seed) || 0, total = seed + M.approved, left = total - spent;
  const hour = M.turns.filter((t) => t.ts > now - 3600).reduce((n, t) => n + t.tokens, 0);
  const sinceStart = Math.max(60, now - (M.started || now));
  // Tokens per hour: over the last hour, or the run so far if it is younger.
  const burn = sinceStart >= 3600 ? hour : spent / (sinceStart / 3600);
  const staff = ws.filter((w) => w.inChart).length, hired = M.hires.length, gone = ws.filter((w) => w.left).length;
  const jobs = [...M.obl.values()].filter((o) => o.expects === 'done');
  const done = jobs.filter((o) => o.settled).length;
  out.push(el('h2', { class: 'sec' }, 'The run in numbers'),
    el('div', { class: 'cards' },
      kpi('Budget', fmtTok(total), M.approved ? 'approved, seed ' + fmtTok(seed) + ' included' : 'seed only: nothing approved yet'),
      kpi('Spent', fmtTok(spent), total ? Math.round((100 * spent) / total) + '% of the budget' : ''),
      kpi('Left', fmtTok(left), left <= 0 ? 'spent: work stops' : ''),
      kpi('Burn rate', fmtTok(burn) + '/h', burn > 0 && left > 0 ? 'runway ' + fmtDur((left / burn) * 3600) : ''),
      kpi('Team', String(staff), hired ? hired + ' hired' + (gone ? ', ' + gone + ' left' : '') : 'founders only'),
      kpi('Assignments', done + ' / ' + jobs.length, 'closed with a report')));
  // Spending by team: the CEO, each direct report's whole team, security.
  const chart = new Map(ws.map((w) => [w.name, w]));
  const top = (n) => { let cur = chart.get(n), hops = 0; while (cur && cur.reports_to && cur.reports_to !== root && hops++ < 50) cur = chart.get(cur.reports_to); return cur ? cur.name : n; };
  const teams = new Map();
  for (const w of ws) {
    if (!w.tokens) continue;
    const key = w.name === root ? root + ' (CEO)' : w.officer ? 'security' : top(w.name);
    teams.set(key, (teams.get(key) || 0) + w.tokens);
  }
  if (teams.size) {
    const rows = [...teams].sort((a, b) => b[1] - a[1]);
    const max = Math.max(1, ...rows.map(([, v]) => v));
    out.push(el('div', { class: 'card ko-teams', style: 'margin-top:4px' }, el('h3', { style: 'margin:0 0 8px' }, 'Spending by team'),
      ...rows.map(([k, v]) => el('div', { class: 'tr' }, el('span', { class: 'tl' }, k === root + ' (CEO)' || k === 'security' ? k : k + "'s team"),
        el('span', { class: 'tb' }, el('i', { style: 'width:' + ((100 * v) / max).toFixed(1) + '%' })), el('span', { class: 'tv' }, fmtTok(v))))));
  }
  // The requests so far.
  const reqs = [...M.budgetReqs.values()];
  if (reqs.length) out.push(el('div', { class: 'card ko-hist', style: 'margin-top:14px' }, el('h3', { style: 'margin:0 0 6px' }, 'Budget requests'),
    ...reqs.slice().reverse().map((r) => el('div', { class: 'row' }, el('b', {}, r.id), el('span', {}, fmtTok(r.total)), pill(r.state === 'approved' ? 'closed' : r.state === 'declined' ? 'error' : 'waiting', r.state),
      el('span', { class: 'muted' }, clip(r.note || r.reason, 90)), el('button', { class: 'linkish', type: 'button', style: 'margin-left:auto', onclick: () => openThread(r.thread, r.message) }, 'thread')))));
  return out;
}
