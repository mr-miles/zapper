'use strict';
// The office page: the Security tab.

// ───────────────────────────── security ─────────────────────────────

// Traffic shows the latest TRAFFIC_PAGE requests; "Load more" adds a page.
const TRAFFIC_PAGE = 25;
const SEC = { cards: new Map(), stSig: null, open: new Set(), timer: null, traffic: [], trafficAt: 0, trafficShown: TRAFFIC_PAGE };

function mountSecurity() {
  const root = $('tab-security');
  root.replaceChildren(
    el('h2', { class: 'sec' }, 'Escalations waiting for the board'), el('div', { id: 'sec-esc', class: 'list' }),
    el('h2', { class: 'sec' }, 'Access'), el('div', { id: 'sec-access' }),
    el('h2', { class: 'sec' }, 'Background checks on new hires'), el('div', { id: 'sec-checks' }),
    el('h2', { class: 'sec' }, 'Standing approvals'), el('div', { id: 'sec-standing' }),
    el('h2', { class: 'sec' }, 'Traffic'), el('div', { id: 'sec-traffic' }));
  SEC.cards.clear(); SEC.stSig = null; SEC.open.clear(); SEC.trafficShown = TRAFFIC_PAGE;
}
const reqWhat = (r) => (r ? (r.method ? r.method + ' ' : r.methods ? arr(r.methods).join(',') + ' ' : '') + str(r.host) + (r.port && r.port !== 443 ? ':' + r.port : '') + str(r.path || '') : '');
const ruleWhat = (rule) => {
  if (!rule) return '';
  const ports = arr(rule.ports).length ? ':' + arr(rule.ports).join(',') : '';
  const methods = arr(rule.methods).length ? arr(rule.methods).join(',') + ' ' : '';
  return methods + str(rule.host) + ports + str(rule.path || '');
};
function updateSecurity() {
  const M = S.M;
  if (!M) return;
  const now = T();
  renderChecks(M);
  renderAccess(M);
  // Escalations.
  const esc = escalationsWaiting(M).concat(withOfficers(M));
  const box = $('sec-esc');
  const ids = new Set(esc.map((r) => r.id));
  for (const [id, c] of SEC.cards) if (!ids.has(id)) { c.el.remove(); SEC.cards.delete(id); }
  for (const r of esc) {
    let c = SEC.cards.get(r.id);
    if (!c) {
      const scope = selectOf([['once', 'once'], ['worker', 'this worker'], ['role', 'this role']], 'once');
      const reason = el('input', { placeholder: 'Reason (shown to the worker)', style: 'flex:1;min-width:200px', maxlength: 500 });
      const allow = el('button', { class: 'btn ok', type: 'button', onclick: () => decide(r.id, true, scope.value, reason.value) }, 'Allow');
      const deny = el('button', { class: 'btn danger', type: 'button', onclick: () => decide(r.id, false, scope.value, reason.value) }, 'Deny');
      const info = el('div');
      c = { el: el('div', { class: 'card' }, info, el('div', { class: 'bar', style: 'margin:10px 0 0' }, scope, reason, allow, deny)), info, ctl: [scope, reason, allow, deny] };
      SEC.cards.set(r.id, c);
      box.append(c.el);
    }
    const rec = r.decision && r.decision.recommendation;
    c.info.replaceChildren(
      el('div', { style: 'display:flex;gap:8px;align-items:center;flex-wrap:wrap' }, r.state === 'escalated' ? pill('escalated', 'escalated') : r.board_asked ? pill('escalated', 'unanswered') : pill('waiting', 'with ' + str(r.officer || 'an officer') + ' · you may decide it'),
        el('b', {}, r.worker), el('span', { class: 'mono' }, reqWhat(r)), el('span', { class: 'muted small', style: 'margin-left:auto' }, r.id + ' · ' + ago(r.created))),
      r.title ? el('div', { style: 'margin-top:6px' }, r.title) : '',
      el('div', { class: 'small', style: 'margin-top:4px' }, el('span', { class: 'muted' }, 'Reason: '), r.reason),
      r.assignment ? el('div', { class: 'small' }, el('span', { class: 'muted' }, 'Working on: '), threadLink(r.assignment)) : '',
      rec ? el('div', { class: 'small', style: 'margin-top:6px' }, el('span', { class: 'muted' }, 'Recommendation from ' + str(r.decision.escalated_by || r.officer || 'the officer') + ':'), renderMarkdown(rec, { breaks: true })) : r.officer ? el('div', { class: 'small muted' }, 'Officer: ' + r.officer) : '');
    for (const x of c.ctl) x.disabled = !canWrite();
  }
  if (!esc.length && !box.querySelector('.empty')) box.replaceChildren(el('div', { class: 'empty card' }, 'Nothing waits for the board.'));
  else if (esc.length) { const e = box.querySelector('.empty'); if (e) e.remove(); }
  // Standing approvals.
  const fromApi = live() && S.security && Array.isArray(S.security.standing);
  const standing = fromApi ? S.security.standing : [...M.standing.values()].filter((s) => !s.ended);
  const ended = [...M.standing.values()].filter((s) => s.ended);
  const stSig = JSON.stringify([standing, ended.length, canWrite(), Math.floor(now / 30)]);
  if (stSig !== SEC.stSig) {
    SEC.stSig = stSig;
    const rows = standing.map((s) => {
      const rule = s.rule || {};
      const action = str(rule.action || 'allow');
      return el('tr', {},
        el('td', { class: 'mono' }, str(s.id)),
        el('td', {}, pill(action === 'allow' ? 'allow' : 'deny', action), ' ', el('span', { class: 'mono' }, ruleWhat(rule))),
        el('td', {}, str(s.scope)),
        el('td', {}, s.expires ? (s.expires < now ? 'expired' : 'in ' + fmtDur(s.expires - now)) : 'never'),
        el('td', {}, str(s.by)),
        el('td', {}, clip(s.reason, 160)),
        el('td', { class: 'small' }, fmtDate(s.created)),
        el('td', {}, fromApi ? String(s.uses ?? 0) : String(s.uses || 0)),
        el('td', {}, live() ? el('span', { style: 'display:flex;gap:6px' },
          el('button', { class: 'btn small danger', type: 'button', disabled: !canWrite(), onclick: () => revoke(s.id) }, 'Revoke'),
          el('button', { class: 'btn small', type: 'button', disabled: !canWrite(), title: 'Copy into the running config at the role\'s profile', onclick: () => promote(s.id) }, 'Promote to config')) : ''));
    });
    $('sec-standing').replaceChildren(
      rows.length ? el('table', { class: 't' }, el('thead', {}, el('tr', {}, ['rule', 'what', 'scope', 'expires', 'officer', 'reason', 'added', 'uses', ''].map((h) => el('th', {}, h)))), el('tbody', {}, rows))
        : el('div', { class: 'empty card' }, 'No standing approvals.'),
      ended.length ? el('details', { style: 'margin-top:8px' }, el('summary', { class: 'muted small' }, ended.length + ' revoked or expired'),
        el('div', { class: 'desk' }, ended.map((s) => el('div', { class: 'item' }, el('span', { class: 'mono' }, s.id), ruleWhat(s.rule), el('span', { class: 'muted small' }, s.ended.how + ' ' + fmtDate(s.ended.ts) + (s.ended.by ? ' by ' + s.ended.by : '')))))) : '');
  }
  renderTraffic(M);
}
function threadLink(id) {
  const t = S.M.threads.get(str(id));
  return t ? el('button', { class: 'linkish', type: 'button', onclick: () => openThread(t.id) }, clip(t.subject || t.id, 60)) : el('span', { class: 'mono' }, str(id));
}
function renderTraffic(M) {
  const box = $('sec-traffic');
  const dig = M.digests.slice(-20).reverse();
  const digests = dig.length ? el('details', { style: 'margin-bottom:10px' }, el('summary', { class: 'muted' }, 'Officers\' digests (' + M.digests.length + ')'),
    el('div', { class: 'desk' }, dig.map((d) => el('div', { class: 'item' }, el('span', { class: 'muted small' }, fmtDate(d.ts)), d.requests + ' requests, ' + d.allowed + ' allowed', d.new_hosts.length ? el('span', { class: 'mono small' }, 'new: ' + clip(d.new_hosts.join(', '), 200)) : '')))) : '';
  if (!live()) { box.replaceChildren(digests, el('div', { class: 'empty card' }, 'Live traffic is shown for the running firm only.')); return; }
  const sig = SEC.trafficAt + '|' + M.digests.length + '|' + SEC.trafficShown;
  if (box.dataset.sig === sig) return;
  box.dataset.sig = sig;
  const shown = Math.min(SEC.trafficShown, SEC.traffic.length);
  const rows = SEC.traffic.slice(0, shown).map((r) => el('tr', {},
    el('td', { class: 'small' }, typeof r.ts === 'number' ? fmtTime(r.ts) : str(r.ts)),
    el('td', {}, r.worker),
    el('td', {}, str(r.method)),
    el('td', { class: 'mono wrap' }, clip(r.url, 200)),
    el('td', {}, r.status != null ? String(r.status) : r.error ? clip(r.error, 60) : r.done ? '' : '…'),
    el('td', {}, pill(r.action === 'allow' ? 'allow' : r.action === 'deny' ? 'deny' : 'ask', str(r.action))),
    el('td', { class: 'mono small' }, str(r.rule)),
    el('td', { class: 'small' }, fmtBytes(r.req_size) + ' / ' + fmtBytes(r.resp_size)),
    el('td', { class: 'small' }, r.total_ms != null ? Math.round(r.total_ms) + ' ms' : ''),
    el('td', { class: 'small' }, r.inspected ? 'inspected' : '')));
  box.replaceChildren(digests, rows.length
    ? el('table', { class: 't' }, el('thead', {}, el('tr', {}, ['time', 'worker', 'method', 'url', 'status', 'action', 'rule', 'size', 'time', ''].map((h) => el('th', {}, h)))), el('tbody', {}, rows))
    : el('div', { class: 'empty card' }, SEC.trafficAt ? 'No requests yet.' : 'Loading…'),
    shown < SEC.traffic.length ? el('div', { class: 'more-row' }, el('span', { class: 'muted small' }, 'Latest ' + shown + ' of ' + SEC.traffic.length),
      el('button', { class: 'btn small', type: 'button', onclick: () => { SEC.trafficShown += TRAFFIC_PAGE; renderTraffic(S.M); } }, 'Load more')) : '');
}
async function pollSecurity() {
  if (!live() || S.tab !== 'security' || !S.info || !S.info.run) return;
  const gen = S.gen;
  try { S.security = await api(F() + '/security'); } catch (e) { S.security = null; }
  const sessions = [];
  for (const w of arr(S.info.run.workers)) if (w.session) sessions.push([w.name, w.session]);
  const lists = await Promise.all(sessions.map(([name, id]) => api('/sessions/' + enc(id) + '/requests').then((l) => arr(l).map((r) => Object.assign({}, r, { worker: name })), () => [])));
  if (gen !== S.gen) return;
  SEC.traffic = lists.flat().sort((a, b) => (Number(b.ts) || 0) - (Number(a.ts) || 0)).slice(0, 1000);
  SEC.trafficAt = Date.now();
  SEC.stSig = null;
  scheduleRender();
}
// Background checks: pending first, with the board's answer; then decided.
// Who may use what: credential requests waiting for the board, and a
// matrix of every person's level on each resource.
function renderAccess(M) {
  const box = $('sec-access');
  if (!box) return;
  const people = [...M.workers.values()].filter((w) => w.inChart);
  const repos = arr(S.info && S.info.run && S.info.run.repos).map((r) => 'git:' + str(r));
  const resources = [...new Set([...(repos.length ? repos : ['git']), 'cluster', ...[...M.grants.values()].flatMap((m) => [...m.keys()]), ...[...M.credReqs.values()].map((q) => q.resource)])]
    .sort((a, b) => (a.startsWith('drive:') ? 1 : 0) - (b.startsWith('drive:') ? 1 : 0) || a.localeCompare(b));
  const waiting = credWaiting(M);
  const open = [...M.credReqs.values()].filter((q) => q.state === 'pending' || q.state === 'escalated');
  const sig = [people.map((w) => w.name).join(','), resources.join(','), [...M.grants.entries()].map(([w, m]) => w + ':' + [...m.entries()].map(([r, g]) => r + g.level + (g.expires || '')).join('/')).join(';'), open.map((q) => q.id + q.state + q.decider).join(','), Math.floor(T() / 60), canWrite()].join('|');
  if (box.dataset.sig === sig) return;
  box.dataset.sig = sig;
  const decideCred = (q, grant) => async () => {
    const note = grant ? '' : prompt('Deny ' + q.id + ' (' + q.resource + ' ' + q.level + ' for ' + cap(q.worker) + '). The reason, for ' + cap(q.worker) + ':', '');
    if (!grant && note == null) return;
    try { await write('POST', F() + '/credentials/requests/' + enc(q.id), { grant, note: grant ? 'granted by the board' : note }); toast((grant ? 'Granted ' : 'Denied ') + q.id); box.dataset.sig = ''; loadFirm(); }
    catch (e) { toast(e.message, true); }
  };
  const revokeCred = (w, r) => async () => {
    const reason = prompt('Revoke ' + cap(w) + '\'s ' + r + '? The reason:', '');
    if (reason == null) return;
    try { await write('DELETE', F() + '/credentials/grants/' + enc(w) + '/' + enc(r) + '?reason=' + enc(reason || 'revoked by the board')); toast('Revoked ' + r + ' for ' + cap(w)); box.dataset.sig = ''; loadFirm(); }
    catch (e) { toast(e.message, true); }
  };
  const can = live() && canWrite();
  const reqRow = (q) => el('div', { class: 'card', style: 'margin-bottom:8px' + (!q.decider ? ';border-left:3px solid var(--warn)' : '') },
    el('div', { style: 'display:flex;gap:8px;align-items:baseline;flex-wrap:wrap' },
      miniAvatar(q.worker), el('b', {}, cap(q.worker)), el('span', {}, 'asks ' + q.level + ' on ' + q.resource), el('span', { class: 'muted small' }, q.id + ', ' + ago(q.ts)),
      pill('waiting', q.decider ? 'waiting for ' + cap(q.decider) + ' · you may decide it' : 'waiting for you'),
      can ? el('span', { class: 'bud-act' }, el('button', { class: 'btn ok small', type: 'button', onclick: decideCred(q, true) }, 'Grant'), el('button', { class: 'btn small', type: 'button', onclick: decideCred(q, false) }, 'Deny')) : ''),
    q.detail ? el('div', { class: 'small', style: 'margin-top:4px' }, el('b', {}, 'Allows: '), q.detail) : '',
    el('div', { class: 'small', style: 'margin-top:4px' }, el('b', {}, 'Reason: '), q.reason));
  const cell = (w, r) => {
    const g = grantOf(M, w.name, r);
    if (!g) return el('td', { class: 'muted' }, '—');
    const tip = g.level + ' by ' + (g.by === BOARD ? boardName() : cap(g.by)) + (g.expires ? ', until ' + fmtDate(g.expires) : ', while they hold the role') + (g.note ? ': ' + g.note : '');
    return el('td', { title: tip }, el('span', { class: 'pill ' + (g.level === 'admin' ? 'error' : g.level === 'write' ? 'waiting' : 'closed') }, g.level),
      g.expires ? el('span', { class: 'muted small' }, ' ' + fmtDur(Math.max(0, g.expires - T()))) : '',
      can && g.expires ? el('button', { class: 'linkish small', type: 'button', title: 'Revoke', onclick: revokeCred(w.name, r) }, ' ×') : '');
  };
  box.replaceChildren(
    ...(open.length ? [el('div', { class: 'muted small', style: 'margin-bottom:6px' }, open.length + ' credential request' + (open.length > 1 ? 's' : '') + ' open' + (waiting.length ? ', ' + waiting.length + ' for you' : ''))].concat(open.map(reqRow)) : []),
    people.length ? el('div', { class: 'card', style: 'overflow-x:auto' }, el('table', { class: 't' },
      el('thead', {}, el('tr', {}, el('th', {}, 'person'), resources.map((r) => el('th', {}, r)))),
      el('tbody', {}, people.map((w) => el('tr', {}, el('td', {}, who(M, w.name)), resources.map((r) => cell(w, r))))))) : el('div', { class: 'empty card' }, 'Nobody yet.'),
    el('div', { class: 'muted small', style: 'margin-top:6px' }, 'Grants are decided by the security officer and the CEO (admin goes to the CEO); the CEO and the officers hold admin while they have the role. Everything else expires.'));
}
// A check the board has answered and whose decision hasn't come back yet:
// its id, the button pressed and when; the reason typed, kept across
// redraws.
const CHECK_BUSY = new Map();
const CHECK_NOTE = new Map();
function renderChecks(M) {
  const box = $('sec-checks');
  if (!box) return;
  CHECK_BUSY.lastM = M;
  const list = [...M.screenings.values()].sort((a, b) => (a.state === 'pending' ? 0 : 1) - (b.state === 'pending' ? 0 : 1) || b.ts - a.ts);
  // Decided, or no word for half a minute: the buttons come back.
  for (const [id, b] of CHECK_BUSY) {
    const c = M.screenings.get(id);
    if (!c || c.state !== 'pending' || Date.now() - b.at > 30000) { CHECK_BUSY.delete(id); CHECK_NOTE.delete(id); }
  }
  const sig = list.map((c) => c.id + c.state + (c.boardAsked ? '!' : '') + (CHECK_BUSY.has(c.id) ? CHECK_BUSY.get(c.id).clear : '')).join(',') + '|' + canWrite();
  if (box.dataset.sig === sig) return;
  box.dataset.sig = sig;
  if (!list.length) { box.replaceChildren(el('div', { class: 'empty card' }, 'No hires yet. Every new hire waits for a security officer\'s background check before they start.')); return; }
  box.replaceChildren(...list.map((c) => {
    const busy = CHECK_BUSY.get(c.id);
    const note = el('input', { placeholder: 'Reason (required to object; the hirer reads it)', style: 'flex:1;min-width:220px', maxlength: 500, value: CHECK_NOTE.get(c.id) || '', disabled: !!busy, oninput: (e) => CHECK_NOTE.set(c.id, e.target.value) });
    const redraw = () => { box.dataset.sig = ''; renderChecks(CHECK_BUSY.lastM); };
    const act = (clear) => async () => {
      if (CHECK_BUSY.has(c.id)) return;
      CHECK_BUSY.set(c.id, { clear, at: Date.now() });
      CHECK_NOTE.set(c.id, note.value);
      redraw();
      try { await write('POST', F() + '/security/screenings/' + enc(c.id), { clear, note: note.value }); toast((clear ? 'Approved ' : 'Objected to ') + c.worker); loadFirm(); }
      catch (e) { CHECK_BUSY.delete(c.id); toast(e.message, true); redraw(); }
    };
    // The button pressed spins until the decision comes back.
    const button = (clear, cls, label) => el('button', { class: 'btn ' + cls, type: 'button', disabled: !!busy, 'aria-busy': busy && busy.clear === clear ? 'true' : null, onclick: act(clear) },
      busy && busy.clear === clear ? el('span', { class: 'spinner', 'aria-hidden': 'true' }) : '', label);
    const pending = c.state === 'pending';
    return el('div', { class: 'card', style: 'margin-bottom:10px' + (pending ? ';border-left:3px solid var(--warn)' : '') },
      el('div', { style: 'display:flex;gap:8px;align-items:baseline;flex-wrap:wrap' },
        miniAvatar(c.worker), el('b', {}, c.worker), el('span', {}, c.title + ' (' + c.role + ')'), el('span', { class: 'muted small' }, 'hired by ' + c.by + ' with ' + fmtTok(c.budget) + ', ' + ago(c.ts)),
        pill(pending ? 'waiting' : c.state === 'cleared' ? 'closed' : 'error', pending ? (c.boardAsked || !c.officer ? 'waiting for you' : 'waiting for ' + c.officer) : (c.state === 'cleared' ? 'approved' : c.state) + ' by ' + c.decidedBy)),
      c.note ? el('div', { class: 'small', style: 'margin-top:4px' }, c.note) : '',
      el('details', { style: 'margin-top:6px' }, el('summary', { class: 'muted small' }, 'Access, reason and brief'),
        el('pre', { class: 'mono small', style: 'white-space:pre-wrap;margin:6px 0' }, c.access),
        el('div', { class: 'small' }, el('b', {}, 'Reason: '), c.reason),
        el('div', { class: 'small', style: 'margin-top:4px' }, el('b', {}, 'Brief: '), clip(c.brief, 1500))),
      pending && live() && canWrite() ? el('div', { class: 'bar', style: 'margin:10px 0 0' }, note,
        button(true, 'ok', 'Approve'),
        button(false, 'danger', 'Object')) : '');
  }));
}

async function decide(id, allow, scope, reason) {
  try {
    await write('POST', F() + '/security/requests/' + enc(id), { allow, scope, reason: reason.trim() || (allow ? 'allowed by the board' : 'denied by the board') });
    toast((allow ? 'Allowed ' : 'Denied ') + id);
    pollSecurity();
  } catch (e) { toast(e.message, true); }
}
async function revoke(id) {
  if (!confirm('Revoke standing approval ' + id + '?')) return;
  try { await write('DELETE', F() + '/security/rules/' + enc(id)); toast('Revoked ' + id); pollSecurity(); } catch (e) { toast(e.message, true); }
}
async function promote(id) {
  if (!confirm('Copy ' + id + ' into the running config? `zapper config save` keeps it.')) return;
  try { const r = await write('POST', F() + '/security/rules/' + enc(id) + '/promote'); toast('Promoted ' + id + ' to ' + str(r && r.scope)); pollSecurity(); } catch (e) { toast(e.message, true); }
}

// A link to an address from the config: loopback as it is, an address
// for every interface (0.0.0.0, [::]) at the host this page came from, so
// it works on a remote host too; anything else is no link.
function addrLink(addr, label) {
  const m = /^(127\.0\.0\.1|localhost|\[::1\]|0\.0\.0\.0|\[::\]):(\d{1,5})$/.exec(str(addr));
  if (!m) return null;
  const any = m[1] === '0.0.0.0' || m[1] === '[::]';
  const a = el('a', { target: '_blank', rel: 'noopener noreferrer', class: 'btn small' }, label);
  a.href = 'http://' + (any ? location.hostname : m[1]) + ':' + m[2] + '/';
  return a;
}
// The product's address from the config, as a link: an IP literal, and
// for every interface (0.0.0.0, [::]) the host this page came from.
function productLink(addr) {
  const m = /^(\d{1,3}(?:\.\d{1,3}){3}|\[[0-9a-fA-F:]+\]):(\d{1,5})$/.exec(str(addr));
  if (!m) return null;
  const any = m[1] === '0.0.0.0' || m[1] === '[::]';
  const hp = (any ? location.hostname : m[1]) + ':' + m[2];
  const a = el('a', { target: '_blank', rel: 'noopener noreferrer', class: 'btn small', title: any ? 'Served on every interface: on your LAN at this machine\'s address, port ' + m[2] : 'The product on ' + addr }, 'Product ↗ ' + hp + (any ? ' · LAN' : ''));
  a.href = 'http://' + hp + '/';
  return a;
}
// The cluster's dashboard (Headlamp) and the token to sign in with.
function dashboardInfo() {
  const live = S.mode === 'live' && S.info && S.info.run && S.info.run.state !== 'ended';
  const f = live ? S.info.run.facilities || {} : {};
  return { addr: S.info && S.info.dashboard_port, on: !!f.dashboard, token: f.dashboard_token || null };
}
function dashboardSig() { const d = dashboardInfo(); return [d.addr, d.on, !!d.token].join(','); }
function dashboardButtons() {
  const d = dashboardInfo();
  if (!d.on || !d.addr) return [];
  const a = addrLink(d.addr, 'Kubernetes dashboard ↗');
  if (!a) return [];
  if (!d.token) return [a, el('span', { class: 'muted small' }, 'starting…')];
  const copy = el('button', { class: 'btn small', type: 'button', title: 'Headlamp asks for a token: paste this one (cluster-admin, this run only)',
    onclick: async () => {
      try { await navigator.clipboard.writeText(dashboardInfo().token); toast('Token copied: paste it into the dashboard\'s sign-in'); }
      catch (e) { window.prompt('Copy the sign-in token:', dashboardInfo().token); }
    } }, 'Copy token');
  return [a, copy];
}
