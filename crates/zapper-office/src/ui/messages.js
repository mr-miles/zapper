'use strict';
// The office page: the Messages tab.

// ───────────────────────────── messages ─────────────────────────────

const MSG = { view: 'inbox', focus: null, focusShown: null, thread: null, compose: false, treeRoot: null, sig: null, mainSig: null, reply: null, composeForm: null };

function mountMessages() {
  const root = $('tab-messages');
  const views = el('div', { class: 'seg', id: 'msg-views' });
  for (const [v, l] of [['inbox', 'Board inbox'], ['all', 'All threads'], ['tree', 'Delegation tree']]) {
    const b = el('button', { type: 'button', onclick: () => { MSG.view = v; MSG.sig = null; MSG.mainSig = null; updateMessages(); } }, l);
    b.dataset.v = v;
    views.append(b);
  }
  // From (author) and To (recipient, including cc): on messages; a thread
  // matches when one of its messages does.
  const fFrom = el('select', { id: 'mf-from', title: 'Written by', onchange: () => { MSG.sig = null; updateMessages(); } });
  const fTo = el('select', { id: 'mf-to', title: 'Received by (to or cc)', onchange: () => { MSG.sig = null; updateMessages(); } });
  const fKind = selectOf([['', 'any kind'], 'assignment', 'question', 'fyi', 'report', 'reply', 'follow-up'], '', { id: 'mf-kind', onchange: () => { MSG.sig = null; updateMessages(); } });
  const fState = selectOf([['', 'any state'], 'waiting', 'overdue', 'paused'], '', { id: 'mf-state', onchange: () => { MSG.sig = null; updateMessages(); } });
  const fBoard = el('label', { class: 'small muted', style: 'display:flex;gap:4px;align-items:center' }, el('input', { type: 'checkbox', id: 'mf-board', onchange: () => { MSG.sig = null; updateMessages(); } }), 'involves the board');
  const fq = el('input', { type: 'search', id: 'mf-q', placeholder: 'Search subject and text', oninput: () => { MSG.sig = null; updateMessages(); } });
  root.replaceChildren(el('div', { class: 'split' },
    el('div', {},
      el('div', { class: 'bar' }, views, el('button', { class: 'btn small', id: 'msg-compose', type: 'button', onclick: () => { MSG.compose = true; MSG.thread = null; MSG.mainSig = null; updateMessages(); } }, 'Compose')),
      el('div', { class: 'bar', id: 'msg-filters' }, fFrom, fTo, fKind, fState, fq, fBoard),
      el('div', { id: 'msg-pin' }),
      el('div', { class: 'list', id: 'msg-list' })),
    el('div', { id: 'msg-main' })));
  MSG.sig = null; MSG.mainSig = null;
}
// The From, To and kind filters, on one message; null when none is set.
function mailFilter() {
  const f = $('mf-from').value, to = $('mf-to').value, k = $('mf-kind').value;
  if (!f && !to && !k) return null;
  return (m) => !!m && (!f || m.from === f) && (!to || m.to.includes(to) || m.cc.includes(to)) && (!k || m.kind === k);
}
// The thread's messages that pass the From, To and kind filters.
function threadHits(M, t) {
  const ok = mailFilter();
  return ok ? t.mails.filter((id) => ok(M.mails.get(id))) : null;
}
function threadMatches(M, t, now) {
  const st = $('mf-state').value;
  if (MSG.view === 'inbox' && !t.board) return false;
  if (MSG.view === 'all' && $('mf-board').checked && !t.board) return false;
  // From, To and kind must hold for one and the same message.
  const hits = threadHits(M, t);
  if (hits && !hits.length) return false;
  const q = $('mf-q').value.trim().toLowerCase();
  if (st && threadState(M, t, now) !== st) return false;
  if (q) {
    const hay = (t.subject + '\n' + t.mails.map((id) => { const m = M.mails.get(id); return m ? m.from + ' ' + m.subject + ' ' + m.body : ''; }).join('\n')).toLowerCase();
    if (!hay.includes(q)) return false;
  }
  return true;
}
function updateMessages() {
  const M = S.M;
  if (!M) return;
  const now = T();
  for (const b of $('msg-views').children) b.classList.toggle('on', b.dataset.v === MSG.view);
  $('msg-compose').classList.toggle('hidden', !canWrite());
  $('mf-board').parentElement.classList.toggle('hidden', MSG.view !== 'all');
  $('msg-filters').classList.toggle('hidden', MSG.view === 'tree');
  // From and To options: the board, every worker, the post office.
  const people = [BOARD, ...[...M.workers.keys()].filter((n) => n !== BOARD), 'office'];
  for (const [id, any] of [['mf-from', 'from anyone'], ['mf-to', 'to anyone']]) {
    const sel = $(id);
    if (sel.dataset.sig === people.join(',')) continue;
    const cur = sel.value;
    sel.dataset.sig = people.join(',');
    sel.replaceChildren(opt('', any), ...people.map((n) => opt(n, (id === 'mf-from' ? 'from ' : 'to ') + (n === BOARD ? boardName() + ' (board)' : n === 'office' ? 'the post office' : who(M, n)), n === cur)));
  }
  const sig = [MSG.view, $('mf-from').value, $('mf-to').value, $('mf-kind').value, M.count, S.boardUnread.size, Math.floor(now / 30), MSG.thread, MSG.treeRoot].join('|');
  if (sig !== MSG.sig) {
    MSG.sig = sig;
    renderPin(M);
    const list = $('msg-list');
    if (MSG.view === 'tree') {
      const roots = [...M.threads.values()].filter((t) => t.board).sort((a, b) => b.last - a.last);
      if (!MSG.treeRoot && roots.length) MSG.treeRoot = roots[0].id;
      list.replaceChildren(...(roots.length ? roots.map((t) => threadItem(M, t, now, MSG.treeRoot === t.id, () => { MSG.treeRoot = t.id; MSG.sig = null; MSG.mainSig = null; updateMessages(); }))
        : [el('div', { class: 'empty' }, 'No threads with the board yet.')]));
    } else {
      // With person or kind filters, a thread opens at its first message
      // that matches, and says how many do.
      const ts = [...M.threads.values()].filter((t) => threadMatches(M, t, now)).sort((a, b) => b.last - a.last).slice(0, 500);
      list.replaceChildren(...(ts.length ? ts.map((t) => { const hits = threadHits(M, t); return threadItem(M, t, now, MSG.thread === t.id, () => openThread(t.id, hits ? hits[0] : null), hits); })
        : [el('div', { class: 'empty' }, MSG.view === 'inbox' ? 'The board has no mail yet.' : 'No threads match.')]));
    }
  }
  renderMsgMain(M, now);
}
function renderPin(M) {
  const pin = $('msg-pin');
  const packs = [...M.artifacts.values()].filter((a) => a.kind === 'board-pack').sort((a, b) => b.last - a.last);
  if (MSG.view !== 'inbox' || !packs.length) { pin.replaceChildren(); return; }
  const a = packs[0], v = a.versions[a.versions.length - 1];
  pin.replaceChildren(el('div', { class: 'card pinned', onclick: () => openDrive(a.id, v.version) },
    el('span', { class: 'badge' }, '★'),
    el('div', {}, el('div', { class: 'serif', style: 'font-size:16px' }, 'Board pack: ' + clip(a.title || a.id, 60)),
      el('div', { class: 'muted small' }, a.id + ' v' + v.version + ' · ' + a.author + ' · ' + ago(v.ts)))));
}
function threadUnread(M, t) {
  if (!live()) return 0;
  return t.mails.filter((id) => S.boardUnread.has(id)).length;
}
function threadItem(M, t, now, sel, onclick, hits) {
  const first = M.mails.get(t.first);
  const unread = threadUnread(M, t);
  return el('div', { class: 'li' + (sel ? ' sel' : ''), onclick },
    el('div', { class: 'top' }, el('span', { class: 'subj' }, unread ? el('span', { class: 'dot', title: 'unread' }) : '', unread ? ' ' : '', clip(t.subject || '(no subject)', 80)), threadPill(M, t, now)),
    el('div', { class: 'meta' }, first ? pill(first.kind, first.kind) : '', clip([...t.people].join(', '), 70), el('span', {}, '· ' + (hits ? hits.length + ' of ' + t.mails.length + ' match' : t.mails.length) + ' · ' + ago(t.last))));
}
function openThread(id, focus) {
  const M = S.M;
  const t = M && M.threads.get(id);
  if (!t) return;
  MSG.thread = id; MSG.compose = false; MSG.focus = focus || null;
  if (MSG.view === 'tree' || (MSG.view === 'inbox' && !t.board)) MSG.view = t.board ? 'inbox' : 'all';
  MSG.sig = null; MSG.mainSig = null;
  if (S.tab !== 'messages') showTab('messages'); else updateMessages();
}
function markRead(M, t) {
  if (!live() || !t) return;
  const ids = t.mails.filter((id) => S.boardUnread.has(id));
  if (!ids.length) return;
  for (const id of ids) S.boardUnread.delete(id);
  write('POST', F() + '/mail/read', { ids }).catch((e) => toast(e.message, true));
  MSG.sig = null;
  scheduleRender();
}
function renderMsgMain(M, now) {
  const main = $('msg-main');
  if (MSG.view === 'tree') {
    const sig = 'tree|' + MSG.treeRoot + '|' + M.count + '|' + Math.floor(now / 30);
    if (sig === MSG.mainSig) return;
    MSG.mainSig = sig;
    main.replaceChildren(delegationTree(M, MSG.treeRoot, now));
    return;
  }
  if (MSG.compose) {
    if (MSG.mainSig === 'compose') { if (MSG.composeForm) MSG.composeForm.refresh(); return; }
    MSG.mainSig = 'compose';
    MSG.composeForm = composeForm({ kinds: ['assignment', 'question', 'fyi'], kind: 'assignment', onSent: (r) => { MSG.compose = false; MSG.mainSig = null; if (r && r.thread) MSG.pendingThread = r.thread; } });
    main.replaceChildren(el('div', { class: 'card' }, el('h2', { class: 'sec' }, 'New message from the board'), MSG.composeForm));
    return;
  }
  if (MSG.pendingThread && M.threads.has(MSG.pendingThread)) { MSG.thread = MSG.pendingThread; MSG.pendingThread = null; }
  const t = MSG.thread && M.threads.get(MSG.thread);
  if (!t) {
    if (MSG.mainSig !== 'none') { MSG.mainSig = 'none'; main.replaceChildren(el('div', { class: 'empty card' }, 'Choose a thread.')); }
    return;
  }
  const sig = 'thread|' + t.id + '|' + t.mails.length + '|' + M.count + '|' + Math.floor(now / 30);
  if (sig === MSG.mainSig) { if (MSG.reply) MSG.reply.refresh(); return; }
  const sameThread = MSG.mainSig && MSG.mainSig.startsWith('thread|' + t.id + '|');
  MSG.mainSig = sig;
  markRead(M, t);
  const msgs = () => el('div', {}, t.mails.map((id) => M.mails.get(id)).filter(Boolean).map((m) => messageCard(M, m, now)));
  const obls = t.obl.map((k) => M.obl.get(k)).filter((o) => o && !o.settled);
  const kids = [...M.threads.values()].filter((x) => x.context === t.id);
  const ctxT = t.context && M.threads.get(t.context);
  const head = el('div', { class: 'card', style: 'margin-bottom:12px' },
    el('div', { style: 'display:flex;gap:10px;align-items:center;flex-wrap:wrap' }, el('h2', { class: 'sec', style: 'margin:0' }, t.subject || '(no subject)'), threadPill(M, t, now)),
    el('div', { class: 'muted small', style: 'margin-top:4px' }, [...t.people].map((p) => (p === BOARD ? boardName() + ' (board)' : p)).join(', ') + ' · ' + t.mails.length + ' message' + (t.mails.length > 1 ? 's' : '') + ' · started ' + fmtDate(t.started)),
    ctxT ? el('div', { class: 'small', style: 'margin-top:4px' }, 'Started while working on ', el('button', { class: 'linkish', type: 'button', onclick: () => openThread(ctxT.id) }, clip(ctxT.subject || ctxT.id, 60))) : '',
    kids.length ? el('div', { class: 'small', style: 'margin-top:4px' }, kids.length + ' thread' + (kids.length > 1 ? 's' : '') + ' spawned from this one: ', el('button', { class: 'linkish', type: 'button', onclick: () => { MSG.view = 'tree'; MSG.treeRoot = rootOf(M, t).id; MSG.sig = null; MSG.mainSig = null; updateMessages(); } }, 'delegation tree')) : '',
    obls.length ? el('div', { class: 'desk', style: 'margin-top:8px' }, obls.map((o) => el('div', { class: 'item' },
      overdue(o, now) ? pill('overdue', 'overdue') : pill('waiting', 'waiting'),
      el('span', {}, nameOf(o.debtor) + ' owes ' + nameOf(o.creditor) + ' ' + (o.expects === 'done' ? 'the work' : 'a reply') + ' (' + o.kind + ')'),
      o.due ? el('span', { class: 'muted small' }, 'due ' + fmtDate(o.due)) : '',
      o.escalated ? el('span', { class: 'muted small' }, 'escalated to ' + o.escalated) : ''))) : '');
  if (!sameThread) {
    MSG.reply = null;
    MSG.headBox = el('div'); MSG.msgsBox = el('div'); MSG.replyBox = el('div'); MSG.msgsSig = null;
    main.replaceChildren(MSG.headBox, MSG.msgsBox, MSG.replyBox);
  }
  // The board replies in any live thread it is in; the form appears as soon
  // as it can (a new message, the run going live) and keeps what was typed.
  const canReply = t.board && live();
  if (canReply && !MSG.reply) {
    MSG.reply = composeForm({ replyIn: t.id, kinds: ['reply', 'question', 'fyi', 'assignment'], kind: 'reply' });
    MSG.replyBox.replaceChildren(el('div', { class: 'card' }, el('h3', { style: 'margin:0 0 8px' }, 'Reply as the board'), MSG.reply));
  } else if (!canReply && MSG.reply) { MSG.reply = null; MSG.replyBox.replaceChildren(); }
  else if (MSG.reply && sameThread) MSG.reply.refresh();
  MSG.headBox.replaceChildren(head);
  const msgsSig = [t.mails.length, Math.floor(now / 30), $('mf-from').value, $('mf-to').value, $('mf-kind').value].join('|');
  if (msgsSig !== MSG.msgsSig || MSG.focusShown !== MSG.focus) {
    MSG.msgsSig = msgsSig; MSG.focusShown = MSG.focus;
    MSG.msgsBox.replaceChildren(msgs());
    const f = MSG.focus && MSG.msgsBox.querySelector('[data-mid="' + CSS.escape(MSG.focus) + '"]');
    if (f) requestAnimationFrame(() => f.scrollIntoView({ block: 'center', behavior: REDUCED.matches ? 'auto' : 'smooth' }));
  }
}
function rootOf(M, t) {
  const seen = new Set();
  while (t.context && M.threads.has(t.context) && !seen.has(t.id)) { seen.add(t.id); t = M.threads.get(t.context); }
  return t;
}
function messageCard(M, m, now) {
  const auto = m.automatic || m.from === 'office' || m.kind === 'follow-up';
  const obls = [...M.obl.values()].filter((o) => o.message === m.id);
  const late = obls.some((o) => overdue(o, now));
  return el('div', { class: 'msg' + (auto ? ' auto' : '') + (m.from === BOARD ? ' board' : '') + (MSG.focus === m.id ? ' focus' : '') + (MSG.view !== 'tree' && mailFilter() && mailFilter()(m) ? ' hit' : ''), 'data-mid': m.id },
    el('div', { class: 'mh' },
      miniAvatar(m.from),
      el('span', { class: 'who' }, m.from === BOARD ? boardName() + ' (board)' : m.from === 'office' ? 'post office' : cap(m.from)),
      el('span', { class: 'to' }, '→ ' + caps(m.to) + (m.cc.length ? ' · cc ' + caps(m.cc) : '')),
      pill(m.kind, m.kind),
      // While the message waits for an answer; gone once it came.
      waitPill(obls, now),
      m.due && obls.some((o) => !o.settled) ? pill(late ? 'overdue' : '', 'due ' + fmtDate(m.due)) : '',
      auto ? pill('', 'automatic') : '',
      budgetActions(M, m),
      el('span', { class: 'when', title: m.id }, fmtDate(m.ts))),
    // The subject on every card, as in a mail client: a reply without its
    // own takes the thread's.
    el('div', { class: 'ms' }, clip(m.subject || ((M.threads.get(m.thread) || {}).subject ? 'Re: ' + M.threads.get(m.thread).subject : '(no subject)'), 200)),
    m.attachments.length ? el('div', { class: 'chips', style: 'margin-top:8px' }, m.attachments.map((ref) => {
      const d = parseDriveRef(ref);
      const a = d && M.artifacts.get(d.artifact);
      return d ? el('button', { class: 'chip', type: 'button', title: ref, onclick: () => openDrive(d.artifact, d.version, d.path) }, '📎 ' + (a ? clip(a.title, 40) + ' ' : '') + ref) : el('span', { class: 'chip' }, ref);
    })) : '',
    el('div', { style: 'margin-top:8px' }, renderMarkdown(m.body, { breaks: true })));
}
// The budget request a message carries, if any.
const budgetReqOf = (M, m) => [...M.budgetReqs.values()].find((r) => r.message === m.id);
// In the header of the CEO's budget request: its state, and while it is
// open, Approve and Decline.
function budgetActions(M, m) {
  const r = budgetReqOf(M, m);
  if (!r) return '';
  if (r.state !== 'pending') return pill(r.state === 'approved' ? 'closed' : 'error', r.id + ' ' + r.state);
  if (!live() || !canWrite()) return pill('waiting', r.id + ' waiting for the board');
  // The same buttons across re-renders of the message, so a click between
  // press and release isn't lost.
  const key = S.run + '|' + r.id;
  if (BUDACT.key === key && BUDACT.el) return BUDACT.el;
  const busy = (on) => { for (const b of [yes, no]) b.disabled = on; };
  const decide = async (approve) => {
    let note = '';
    if (approve) {
      if (!confirm('Approve ' + r.id + ': ' + fmtTok(r.total) + ' tokens for ' + r.by + '?')) return;
    } else {
      const n = prompt('Decline ' + r.id + ' (' + fmtTok(r.total) + ' tokens). A note to ' + r.by + ': what to change?', '');
      if (n == null) return;
      note = n;
    }
    busy(true);
    try {
      await write('POST', F() + '/budget/' + enc(r.id), { approve, note });
      toast((approve ? 'Approved ' : 'Declined ') + r.id);
      KO.sig = null; KO.msgSig = null; MSG.sig = null;
      loadFirm();
    } catch (e) { toast(e.message, true); busy(false); }
  };
  const yes = el('button', { class: 'btn primary small', type: 'button', onclick: () => decide(true) }, 'Approve ' + fmtTok(r.total));
  const no = el('button', { class: 'btn small', type: 'button', onclick: () => decide(false) }, 'Decline');
  BUDACT.key = key;
  BUDACT.el = el('span', { class: 'bud-act' }, yes, no);
  return BUDACT.el;
}
const BUDACT = { key: null, el: null };
// To the open budget request's message in Kickoff, where its header has
// Approve and Decline.
function goDecideBudget() {
  const pend = pendingBudget(S.M);
  showTab('kickoff');
  requestAnimationFrame(() => {
    const c = pend && document.querySelector('#ko-conv [data-mid="' + CSS.escape(pend.message) + '"]');
    if (!c) return;
    c.scrollIntoView({ behavior: 'smooth', block: 'start' });
    c.classList.add('flash');
    setTimeout(() => c.classList.remove('flash'), 1500);
  });
}
function delegationTree(M, rootId, now) {
  const root = rootId && M.threads.get(rootId);
  if (!root) return el('div', { class: 'empty card' }, 'Choose a thread of the board on the left.');
  const kids = new Map();
  for (const t of M.threads.values()) if (t.context) { if (!kids.has(t.context)) kids.set(t.context, []); kids.get(t.context).push(t); }
  const seen = new Set();
  const node = (t, depth) => {
    seen.add(t.id);
    const first = M.mails.get(t.first);
    const li = el('li', {}, el('div', { class: 'tn' }, threadPill(M, t, now),
      el('button', { class: 'linkish', type: 'button', onclick: () => openThread(t.id) }, clip(t.subject || '(no subject)', 70)),
      first ? el('span', { class: 'muted small' }, first.from + ' → ' + first.to.join(', ')) : '',
      first ? pill(first.kind, first.kind) : '',
      el('span', { class: 'muted small' }, t.mails.length + ' msg · ' + ago(t.last))));
    const ks = (kids.get(t.id) || []).filter((k) => !seen.has(k.id)).sort((a, b) => a.started - b.started);
    if (ks.length && depth < 12) li.append(el('ul', { class: 'tree' }, ks.map((k) => node(k, depth + 1))));
    return li;
  };
  return el('div', { class: 'card' },
    el('h2', { class: 'sec' }, 'How "' + clip(root.subject || root.id, 60) + '" was split up'),
    el('div', { class: 'muted small' }, 'Threads started while working on this one, recursively, with the state of each branch.'),
    el('ul', { class: 'tree', style: 'border-left:0;padding-left:0' }, node(root, 0)));
}
