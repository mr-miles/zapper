'use strict';
// The office page: the page's state and the journal fold: everything shown is derived from it.

// ────────────────────────────── state ──────────────────────────────

const TABS = [['kickoff', 'Kickoff'], ['overview', 'Organization'], ['people', 'People'], ['work', 'Work'], ['messages', 'Messages'], ['security', 'Security'], ['drive', 'Shared drive'], ['git', 'Git'], ['vms', 'VMs'], ['stats', 'Statistics'], ['runs', 'Runs']];
const S = {
  firms: [], firm: null, info: null, tab: 'overview',
  mode: 'live',          // 'live' or 'replay'
  run: null,             // the run being shown
  M: null,               // the folded journal
  seen: new Set(),       // journal seqs applied
  es: null,              // live journal stream
  boardUnread: new Set(),
  security: null,        // GET /security (live)
  rp: null,              // replay: {events, idx, t0, t1, pos, playing, speed, timer, skip}
  gen: 0,                // bumps when the source changes; stale async results are dropped
};
// "Now": wall clock live, the timeline position in replay.
const T = () => (S.mode === 'replay' && S.rp ? S.rp.pos : nowSec());
const live = () => S.mode === 'live';
const runState = () => (S.M && S.M.runState) || (S.info && S.info.run && S.info.run.state) || 'off';
const canWrite = () => live() && S.info && S.info.run && ['running', 'paused'].includes(S.info.run.state);
const BOARD = 'board';
const boardName = () => (S.M && S.M.board) || (S.info && S.info.board) || 'Board';

// ─────────────────────────── journal fold ───────────────────────────
//
// One fold serves live and replay: apply() takes one journal event at a
// time; live applies the stream as it comes, replay applies events up to
// the timeline position (and starts over when moving back).

function newModel() {
  return {
    chart: null, board: null, domain: '', budget: null, description: '',
    runState: null, started: null, last: null, runStates: [],
    workers: new Map(), mails: new Map(), threads: new Map(), obl: new Map(),
    requests: new Map(), access: [], standing: new Map(), artifacts: new Map(),
    published: [], digests: [], opErrors: [], turns: [], settled: [], escalations: [],
    fetched: [], overBudget: false, count: 0,
    // Turns and the threads they worked on: the effort inside the Work tab's bars.
    spans: [],
    // The run's money: moves between the board and workers, the CEO's
    // budget requests, what the board approved.
    fundsIn: new Map(), fundsOut: new Map(), moves: [], budgetReqs: new Map(), approved: 0, hires: [],
    // Background checks on new hires.
    screenings: new Map(), scenarioLog: [], scenario: null, closeWaiter: null,
    // Who held security when, and the board's own articles.
    officerChanges: [], articles: '',
    // Contracts: offers, negotiations, agreed terms, how they ended.
    contracts: new Map(),
    // Credentials: grants by worker and resource, and requests.
    grants: new Map(), credReqs: new Map(), grantLog: [],
  };
}
function setChart(M, chart) {
  M.chart = chart;
  M.board = chart.board || M.board;
  M.domain = chart.domain || '';
  M.description = chart.description || '';
  for (const c of arr(chart.workers)) {
    if (!c || typeof c.name !== 'string') continue;
    const w = worker(M, c.name);
    Object.assign(w, { role: str(c.role), title: str(c.title), reports_to: c.reports_to || null, officer: !!c.officer, avatar: c.avatar || null, model: c.model || null, agent: str(c.agent) || 'claude-code', profile: str(c.profile), inChart: true });
  }
}
function worker(M, name) {
  let w = M.workers.get(name);
  if (!w) {
    w = { name, role: '', title: '', reports_to: null, officer: false, avatar: null, inChart: false,
      state: 'off', detail: '', since: null, segs: [], session: null, sessions: [],
      unread: new Set(), readAt: new Map(), turns: 0, tokens: 0, rateLimited: 0, interrupted: 0, errors: 0,
      inTurn: false, reviewing: false, turnThreads: new Set(), span: null,
      hiredBy: null, hiredAt: null, hireReason: '', hireBudget: 0, left: null, exhausted: null };
    M.workers.set(name, w);
  }
  return w;
}
function setState(M, w, state, detail, ts) {
  const last = w.segs[w.segs.length - 1];
  if (last && last.to == null) {
    if (last.state === state) { w.detail = str(detail); return; }
    last.to = ts;
  }
  w.segs.push({ state, from: ts, to: null });
  w.state = state; w.detail = str(detail); w.since = ts;
}
const isWorker = (M, n) => M.workers.has(n);

function apply(M, e) {
  if (!e || typeof e !== 'object') return;
  const ts = Number(e.ts) || M.last || 0;
  if (M.started == null) M.started = ts;
  M.last = ts;
  M.count++;
  switch (e.type) {
    case 'run': {
      if (e.chart && typeof e.chart === 'object') setChart(M, e.chart);
      if (e.budget) M.budget = e.budget;
      if (typeof e.company === 'string') M.company = e.company;
      if (typeof e.articles === 'string') M.articles = e.articles;
      if (e.scenario && typeof e.scenario === 'object' && e.scenario.scenario) M.scenario = { name: str(e.scenario.name), script: arr(e.scenario.scenario.script) };
      if (e.state) { M.runState = e.state; M.runStates.push({ state: e.state, ts, reason: e.reason || e.error || '' }); }
      if (e.state === 'ended' || e.state === 'failed') for (const w of M.workers.values()) setState(M, w, 'off', '', ts);
      break;
    }
    case 'worker': {
      if (typeof e.worker !== 'string') break;
      const w = worker(M, e.worker);
      setState(M, w, str(e.state) || 'idle', e.detail, ts);
      if (e.state === 'error') w.errors++;
      break;
    }
    case 'hired': {
      const x = e.worker;
      if (!x || typeof x !== 'object' || typeof x.name !== 'string') break;
      const w = worker(M, x.name);
      Object.assign(w, { role: str(x.role), title: str(x.title), reports_to: x.reports_to ? str(x.reports_to) : null, officer: false, avatar: x.avatar || null, model: x.model || null, agent: str(x.agent) || 'claude-code', profile: str(x.profile), inChart: true,
        hiredBy: str(e.by), hiredAt: ts, hireReason: str(e.reason), hireBudget: Number(e.budget) || 0, left: null, contract: e.contract ? str(e.contract) : null });
      M.hires.push({ name: w.name, by: w.hiredBy, ts, budget: w.hireBudget, reason: w.hireReason });
      break;
    }
    case 'left': {
      const w = M.workers.get(str(e.worker));
      if (!w) break;
      const to = str(e.to);
      w.inChart = false;
      w.left = { ts, by: str(e.by), reason: str(e.reason), to, returned: Number(e.returned) || 0 };
      setState(M, w, 'off', '', ts);
      for (const o of M.workers.values()) if (o.reports_to === w.name) o.reports_to = to;
      // What they hadn't read, and the requests they hadn't answered, go to their manager to read.
      const mgr = M.workers.get(to);
      if (mgr) {
        for (const id of [...w.unread, ...arr(e.moved).map(str)]) { const m = M.mails.get(id); if (m && m.from !== to) mgr.unread.add(id); }
      }
      w.unread = new Set();
      // Their open work moves to their manager: re-key what they owed.
      for (const [k, o] of [...M.obl]) {
        if (o.settled) continue;
        if (o.creditor === w.name) o.creditor = to;
        // What the manager asked of them is nobody's debt once they are gone.
        if ((o.debtor === w.name && o.creditor === to) || (o.debtor === to && o.creditor === to)) {
          M.obl.delete(k);
          const t = M.threads.get(o.thread);
          if (t) t.obl = t.obl.filter((x) => x !== k);
          continue;
        }
        if (o.debtor === w.name) {
          M.obl.delete(k);
          o.debtor = to; o.key = o.message + '|' + to;
          M.obl.set(o.key, o);
          const t = M.threads.get(o.thread);
          if (t) t.obl = t.obl.map((x) => (x === k ? o.key : x));
        }
      }
      break;
    }
    case 'scenario': {
      M.scenarioLog.push({ ts, event: str(e.event), worker: str(e.worker), why: str(e.why || e.error), what: e.what && typeof e.what === 'object' ? e.what : null, index: e.index });
      break;
    }
    case 'credentials': {
      const ev = str(e.event);
      if (ev === 'requested' && e.request && typeof e.request.id === 'string') {
        const q = e.request;
        M.credReqs.set(q.id, { id: q.id, worker: str(q.worker), resource: str(q.resource), level: str(q.level), reason: str(q.reason), state: str(q.state), decider: q.decider ? str(q.decider) : null, ts, decision: null });
      } else if (ev === 'decided') {
        const q = M.credReqs.get(str(e.id));
        if (q) { q.state = e.decision && e.decision.grant ? 'granted' : 'denied'; q.decision = e.decision || null; }
      } else if (ev === 'escalated') {
        const q = M.credReqs.get(str(e.id));
        if (q) { q.state = 'escalated'; q.decider = e.to && e.to !== BOARD ? str(e.to) : null; }
      } else if (ev === 'granted' && e.grant) {
        const g = e.grant;
        if (!M.grants.has(str(g.worker))) M.grants.set(str(g.worker), new Map());
        M.grants.get(str(g.worker)).set(str(g.resource), { level: str(g.level), by: str(g.by), note: str(g.note), expires: g.expires != null ? Number(g.expires) : null, ts });
        M.grantLog.push({ ts, worker: str(g.worker), resource: str(g.resource), level: str(g.level), by: str(g.by) });
      } else if (ev === 'revoked') {
        const m = M.grants.get(str(e.worker));
        if (m) m.delete(str(e.resource));
        M.grantLog.push({ ts, worker: str(e.worker), resource: str(e.resource), level: 'none', by: str(e.by), reason: str(e.reason) });
      }
      break;
    }
    case 'contract': {
      const ev = str(e.event);
      const terms = (t) => (t && typeof t === 'object' ? { budget: t.budget != null ? Number(t.budget) : null, due: t.due != null ? Number(t.due) : null, scope: str(t.scope) } : null);
      if (ev === 'offered' && e.contract && typeof e.contract.id === 'string') {
        const x = e.contract;
        M.contracts.set(x.id, { id: x.id, worker: str(x.worker), by: str(x.by), role: str(x.role), title: str(x.title), brief: str(x.brief), state: 'offered',
          offer: terms(x.offer), firstOffer: terms(x.offer), firstQuote: null, quote: null, agreed: null, rounds: 0, thread: str(x.thread), added: [], created: ts, updated: ts, ended: null, endedAt: null, log: [{ ts, ev, by: str(x.by) }] });
        break;
      }
      const c = M.contracts.get(str(e.id));
      if (!c) break;
      c.updated = ts;
      c.log.push({ ts, ev, by: str(e.by || e.worker), terms: terms(e.terms), note: str(e.note || e.reason) });
      if (ev === 'quoted') { c.state = 'quoted'; c.quote = terms(e.terms); if (!c.firstQuote) c.firstQuote = c.quote; }
      else if (ev === 'countered') { c.state = 'countered'; c.offer = terms(e.terms); c.rounds = Number(e.round) || c.rounds + 1; }
      else if (ev === 'accepted') { c.state = 'active'; c.agreed = terms(e.terms); }
      else if (ev === 'added') c.added.push(str(e.worker));
      else { c.state = ev; c.ended = str(e.reason); c.endedAt = ts; }
      break;
    }
    case 'security': {
      const ev = str(e.event);
      if (ev === 'officers') {
        const now = new Set(arr(e.officers).map(str));
        for (const w of M.workers.values()) w.officer = now.has(w.name);
        M.officerChanges.push({ ts, officers: [...now], by: str(e.by), reason: str(e.reason) });
      } else if (ev === 'reassigned') {
        const c = M.screenings.get(str(e.id));
        if (c) { c.officer = e.officer ? str(e.officer) : null; if (!c.officer) c.boardAsked = true; }
      }
      break;
    }
    case 'screening': {
      const ev = str(e.event);
      if (ev === 'requested' && e.screening && typeof e.screening.id === 'string') {
        const x = e.screening;
        M.screenings.set(x.id, { id: x.id, worker: str(x.worker), role: str(x.role), title: str(x.title), by: str(x.by), budget: Number(x.budget) || 0,
          reason: str(x.reason), brief: str(x.brief), access: str(x.access), officer: x.officer ? str(x.officer) : null, state: 'pending', boardAsked: !!x.board_asked, decidedBy: null, note: '', ts });
      } else if (ev === 'decided') {
        const c = M.screenings.get(str(e.id));
        if (c) { c.state = e.cleared ? 'cleared' : 'objected'; c.decidedBy = str(e.by); c.note = str(e.note); c.decided = ts; }
      } else if (ev === 'board_asked') {
        const c = M.screenings.get(str(e.id));
        if (c) c.boardAsked = true;
      }
      break;
    }
    case 'funds': {
      const ev = str(e.event);
      if (ev === 'move') {
        const t = Number(e.tokens) || 0;
        M.fundsIn.set(str(e.to), (M.fundsIn.get(str(e.to)) || 0) + t);
        if (e.from !== BOARD) M.fundsOut.set(str(e.from), (M.fundsOut.get(str(e.from)) || 0) + t);
        M.moves.push({ from: str(e.from), to: str(e.to), tokens: t, reason: str(e.reason), ts });
      } else if (ev === 'request' && typeof e.id === 'string') {
        M.budgetReqs.set(e.id, { id: e.id, by: str(e.by), total: Number(e.total) || 0, plan: str(e.plan), reason: str(e.reason), message: str(e.message), thread: str(e.thread), ts, state: 'pending', note: '', decided: null });
      } else if (ev === 'decided') {
        const r = M.budgetReqs.get(str(e.id));
        if (r) { r.state = e.approved ? 'approved' : 'declined'; r.note = str(e.note); r.decided = ts; if (e.approved) M.approved += e.granted != null ? Number(e.granted) || 0 : r.total; }
      } else if (ev === 'exhausted') {
        const w = M.workers.get(str(e.worker));
        if (w) w.exhausted = ts;
      }
      break;
    }
    case 'worker_session': {
      if (typeof e.worker !== 'string') break;
      const w = worker(M, e.worker);
      w.session = str(e.session); w.sessions.push(w.session);
      break;
    }
    case 'mail': {
      if (typeof e.id !== 'string') break;
      const m = { id: e.id, thread: str(e.thread) || e.id, from: str(e.from), to: arr(e.to).map(str), cc: arr(e.cc).map(str),
        subject: str(e.subject), kind: str(e.kind) || 'fyi', expects: str(e.expects) || 'nothing', due: Number(e.due) || null,
        in_reply_to: e.in_reply_to || null, attachments: arr(e.attachments).map(str), automatic: !!e.automatic,
        close: !!e.close, context: e.context || null, body: str(e.body), ts, seq: e.seq };
      M.mails.set(m.id, m);
      let t = M.threads.get(m.thread);
      if (!t) {
        t = { id: m.thread, subject: m.subject, kind: m.kind, first: m.id, mails: [], people: new Set(), context: m.context, started: ts, last: ts, closed: false, paused: false, obl: [], board: false };
        M.threads.set(t.id, t);
      }
      if (!t.subject && m.subject) t.subject = m.subject;
      t.mails.push(m.id); t.last = ts;
      if (m.close) t.closed = true;
      for (const p of [m.from, ...m.to, ...m.cc]) if (p) t.people.add(p);
      if (m.from === BOARD || m.to.includes(BOARD) || m.cc.includes(BOARD)) t.board = true;
      for (const r of [...m.to, ...m.cc]) if (r !== BOARD && r !== m.from && isWorker(M, r)) worker(M, r).unread.add(m.id);
      if (m.expects === 'reply' || m.expects === 'done') {
        for (const d of m.to) {
          if (d === BOARD || d === m.from) continue;
          const key = m.id + '|' + d;
          const o = { key, message: m.id, thread: t.id, debtor: d, creditor: m.from, due: m.due, created: ts, kind: m.kind, expects: m.expects, settled: null, escalated: null };
          M.obl.set(key, o); t.obl.push(key);
        }
      }
      break;
    }
    case 'settled': {
      const o = M.obl.get(str(e.message) + '|' + str(e.debtor));
      if (o && !o.settled) { o.settled = { by: str(e.by), ts }; M.settled.push({ o, ts }); }
      break;
    }
    case 'escalated': {
      const o = M.obl.get(str(e.message) + '|' + str(e.debtor));
      if (o) o.escalated = str(e.manager);
      M.escalations.push({ message: str(e.message), thread: str(e.thread), debtor: str(e.debtor), creditor: str(e.creditor), manager: str(e.manager), ts });
      break;
    }
    case 'thread_paused': {
      const t = M.threads.get(str(e.thread));
      if (t) t.paused = true;
      break;
    }
    case 'turn': {
      if (typeof e.worker !== 'string') break;
      const w = worker(M, e.worker);
      const endSpan = () => { if (w.span) { w.span.to = ts; w.span = null; } w.turnThreads = new Set(); };
      if (e.event === 'start') {
        endSpan();
        w.inTurn = true; w.reviewing = !!e.reviewing;
        for (const id of arr(e.messages)) { w.unread.delete(str(id)); w.readAt.set(str(id), ts); }
        for (const id of arr(e.messages)) { const m = M.mails.get(str(id)); if (m) w.turnThreads.add(m.thread); }
        if (w.turnThreads.size) { w.span = { worker: w.name, threads: new Set(w.turnThreads), from: ts, to: null }; M.spans.push(w.span); }
      } else if (e.event === 'end') {
        endSpan();
        w.inTurn = false; w.turns++;
        const tok = Number(e.tokens) || 0;
        w.tokens += tok;
        M.turns.push({ worker: w.name, ts, tokens: tok, ms: Number(e.ms) || 0, error: !!e.is_error });
      } else if (e.event === 'extended') {
        // Mail handed over while the turn ran: read, like at the start.
        for (const id of arr(e.messages)) {
          w.unread.delete(str(id));
          w.readAt.set(str(id), ts);
          const m = M.mails.get(str(id));
          if (m) { w.turnThreads.add(m.thread); if (w.span) w.span.threads.add(m.thread); }
        }
      } else if (e.event === 'interrupted') { endSpan(); w.inTurn = false; w.interrupted++; }
      else if (e.event === 'rate_limited') { endSpan(); w.inTurn = false; w.rateLimited++; }
      break;
    }
    case 'access': {
      const rec = { seq: e.seq, ts, event: str(e.event), by: e.by ? str(e.by) : null, reason: e.reason ? str(e.reason) : null, rule: e.rule ? str(e.rule) : null, raw: e };
      let r = null;
      if (e.request && typeof e.request === 'object' && typeof e.request.id === 'string') {
        r = Object.assign({}, M.requests.get(e.request.id) || {}, e.request);
        M.requests.set(r.id, r);
      } else if (typeof e.request === 'string') {
        r = M.requests.get(e.request) || null;
        if (r && e.event === 'board_asked') { r = Object.assign({}, r, { board_asked: true }); M.requests.set(r.id, r); }
      }
      rec.request = r ? Object.assign({}, r) : null;
      M.access.push(rec);
      if (e.event === 'decided' && r && r.decision && r.decision.rule && (r.decision.scope === 'worker' || r.decision.scope === 'role')) {
        const d = r.decision;
        const role = M.workers.has(r.worker) ? M.workers.get(r.worker).role : '';
        M.standing.set(str(d.rule), {
          id: str(d.rule), rule: { action: d.allow ? 'allow' : 'deny', host: r.host, ports: r.port ? [r.port] : [], methods: d.methods || r.methods || null, path: d.path || null },
          scope: d.scope === 'role' ? 'role:' + role : 'worker:' + r.worker, by: str(d.by), reason: str(d.reason), request: r.id,
          created: Number(d.at) || ts, expires: Number(d.expires) || null, uses: 0, ended: null,
        });
      }
      if ((e.event === 'revoked' || e.event === 'expired') && e.rule) {
        const st = M.standing.get(str(e.rule));
        if (st) st.ended = { how: e.event, ts, by: e.by ? str(e.by) : null };
      }
      if (e.event === 'used' && e.rule) { const st = M.standing.get(str(e.rule)); if (st) st.uses++; }
      break;
    }
    case 'published': {
      const id = str(e.artifact);
      if (!id) break;
      let a = M.artifacts.get(id);
      if (!a) { a = { id, title: '', kind: '', author: '', space: '', thread: null, versions: [], created: ts }; M.artifacts.set(id, a); }
      Object.assign(a, { title: str(e.title), kind: str(e.kind), author: str(e.author), space: str(e.space || (e.audience === 'board' ? 'board' : 'general')), thread: e.thread || a.thread, last: ts });
      const files = arr(e.files).filter((f) => f && typeof f.path === 'string').map((f) => ({ path: f.path, size: Number(f.size) || 0, sha256: str(f.sha256) }));
      a.versions.push({ version: Number(e.version) || a.versions.length + 1, summary: str(e.summary), changes: str(e.changes), files, ts, title: str(e.title) });
      M.published.push({ artifact: id, version: Number(e.version) || a.versions.length, author: a.author, kind: a.kind, ts });
      break;
    }
    case 'fetched': M.fetched.push({ artifact: str(e.artifact), version: e.version, worker: str(e.worker), dest: str(e.dest), ts }); break;
    case 'digest': M.digests.push({ requests: Number(e.requests) || 0, allowed: Number(e.allowed) || 0, new_hosts: arr(e.new_hosts).map(str), ts }); break;
    case 'budget': if (e.over) M.overBudget = true; break;
    // The CEO tried to close the board's objective while the team was busy:
    // they wait for it to go idle.
    case 'objective': M.closeWaiter = e.event === 'waiting' ? str(e.worker) : null; break;
    case 'op_error': M.opErrors.push({ worker: str(e.worker), op: str(e.op), error: str(e.error), ts }); break;
  }
}

// ─────────────────────── derived from the fold ───────────────────────

const openObl = (M) => [...M.obl.values()].filter((o) => !o.settled);
const overdue = (o, now) => !o.settled && o.due && now > o.due;
// A thread is waiting while someone owes an answer in it, overdue when one
// is late, paused by the loop guard; otherwise it has no state to show.
function threadState(M, t, now) {
  if (t.paused) return 'paused';
  const open = t.obl.map((k) => M.obl.get(k)).filter((o) => o && !o.settled);
  if (open.some((o) => overdue(o, now))) return 'overdue';
  if (open.length) return 'waiting';
  return '';
}
const nameOf = (n) => (n === BOARD ? boardName() : n === 'office' ? 'post office' : n);
// Who an open obligation waits for, as a pill: "waiting for ada".
function waitPill(obs, now) {
  const open = obs.filter((o) => o && !o.settled);
  if (!open.length) return '';
  const late = open.some((o) => overdue(o, now));
  const who = [...new Set(open.map((o) => nameOf(o.debtor)))].join(', ');
  return pill(late ? 'overdue' : 'waiting', (late ? 'overdue: ' : 'waiting for ') + clip(who, 40));
}
function threadPill(M, t, now) {
  if (t.paused) return pill('paused', 'paused');
  return waitPill(t.obl.map((k) => M.obl.get(k)), now);
}
function desk(M, name, now) {
  const owes = [], waits = [];
  for (const o of M.obl.values()) {
    if (o.settled) continue;
    if (o.debtor === name) owes.push(o);
    if (o.creditor === name) waits.push(o);
  }
  const reqs = [...M.requests.values()].filter((r) => r.worker === name && (r.state === 'pending' || r.state === 'escalated'));
  return { owes, waits, overdue: owes.filter((o) => overdue(o, now)), reqs };
}
function statusLine(M, w, now) {
  const d = desk(M, w.name, now);
  const parts = [];
  if (M.closeWaiter === w.name && !['working', 'reviewing', 'queued'].includes(w.state)) parts.push('waiting for the team');
  else if (w.state === 'waiting' && d.waits.length) parts.push('waiting for ' + [...new Set(d.waits.map((o) => o.debtor))].join(', '));
  else if (w.state === 'off') parts.push('off');
  else if (w.detail) parts.push(w.detail);
  else parts.push(w.state);
  if (d.overdue.length) parts.push('overdue: ' + d.overdue.length);
  return { text: parts.join(' · '), bad: d.overdue.length > 0 || w.state === 'error' };
}
// Credential requests waiting for the board, and a grant's level now.
const credWaiting = (M) => [...M.credReqs.values()].filter((q) => !q.decider && (q.state === 'pending' || q.state === 'escalated'));
const grantOf = (M, w, r) => { const g = M.grants.get(w) && M.grants.get(w).get(r); return g && (g.expires == null || g.expires > T()) ? g : null; };
// Pending requests an officer has but hasn't answered: the board sees them too and may decide them.
const withOfficers = (M) => [...M.requests.values()].filter((r) => r.state === 'pending' && !r.board_asked);
const escalationsWaiting = (M) => [...M.requests.values()].filter((r) => (r.state === 'escalated' || (r.board_asked && r.state === 'pending')));
// Background checks the board has to answer: no officer, or none in time.
const checksWaiting = (M) => [...M.screenings.values()].filter((c) => c.state === 'pending' && (c.boardAsked || !c.officer));
// The chart: from the run's first event, else from the firm's config.
// `claude-opus-5-5` → `Opus 5.5`; other names as they are.
function modelName(m) {
  const x = /^claude-([a-z]+)-(\d+)-(\d+)/.exec(str(m));
  // Other providers' ids often name their organization first.
  return x ? x[1][0].toUpperCase() + x[1].slice(1) + ' ' + x[2] + '.' + x[3] : str(m).split('/').pop();
}
// The agent a worker runs, by name.
const AGENT_NAMES = { 'claude-code': 'Claude Code', pi: 'pi' };
const agentName = (a) => AGENT_NAMES[a] || str(a);
// The agent and model, as a worker's label shows them: the agent only when
// it isn't Claude Code, which most workers run.
function agentLabel(w) {
  const m = w.model ? modelName(w.model) : '';
  return w.agent && w.agent !== 'claude-code' ? agentName(w.agent) + (m ? ' · ' + m : '') : m;
}
function chartWorkers() {
  const M = S.M;
  if (M && M.chart) return [...M.workers.values()].filter((w) => w.inChart);
  return [];
}
// The run's budget, from the journal: on when the firm has a seed.
const fundsOn = (M) => !!(M && M.budget && Number(M.budget.seed) > 0);
const balanceOf = (M, n) => (M.fundsIn.get(n) || 0) - (M.fundsOut.get(n) || 0) - ((M.workers.get(n) || {}).tokens || 0);
function fmtTok(n) { n = Number(n) || 0; const a = Math.abs(n); const s = a >= 1e6 ? (a / 1e6).toFixed(a >= 1e7 ? 0 : 1) + 'M' : a >= 1e3 ? Math.round(a / 1e3) + 'k' : String(Math.round(a)); return (n < 0 ? '−' : '') + s; }
const pendingBudget = (M) => (M ? [...M.budgetReqs.values()].find((r) => r.state === 'pending') : null);
function rootWorker() { const ws = chartWorkers(); return (ws.find((w) => !w.reports_to) || ws[0] || {}).name || null; }
function workerUnread(name) {
  if (live() && S.info && S.info.run) { const w = arr(S.info.run.workers).find((x) => x.name === name); if (w && typeof w.unread === 'number') return w.unread; }
  const w = S.M && S.M.workers.get(name);
  return w ? w.unread.size : 0;
}
function workerSession(name) {
  const w = S.M && S.M.workers.get(name);
  if (live() && S.info && S.info.run) { const x = arr(S.info.run.workers).find((x) => x.name === name); if (x && x.session) return x.session; }
  return w ? w.session : null;
}
