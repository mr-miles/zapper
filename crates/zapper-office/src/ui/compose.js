'use strict';
// The office page: the compose form, for the board's mail.

// ─────────────────────────── compose form ───────────────────────────
//
// opts: fixedTo (names) or a recipient picker; kinds; kind; replyIn (a
// thread: a reply in it). Without replyIn, a message starts a new thread;
// the subject is optional (the first line stands in for it).

function composeForm(opts) {
  const f = el('form', { class: 'form', onsubmit: (ev) => { ev.preventDefault(); send(); } });
  const note = el('div', { class: 'muted small' });
  let toPick = null, ccPick = null;
  if (!opts.fixedTo && !opts.replyIn) {
    toPick = el('div', { class: 'picker' });
    ccPick = el('div', { class: 'picker' });
    for (const w of chartWorkers()) {
      toPick.append(el('label', {}, el('input', { type: 'checkbox', value: w.name }), w.name + (w.title ? ' — ' + w.title : '')));
      ccPick.append(el('label', {}, el('input', { type: 'checkbox', value: w.name }), w.name));
    }
    f.append(el('label', { class: 'f' }, 'To', toPick), el('details', {}, el('summary', { class: 'muted small' }, 'Cc'), ccPick));
  } else if (opts.fixedTo) {
    f.append(el('div', { class: 'muted small' }, 'To ' + opts.fixedTo.join(', ') + ', from ' + boardName() + ' (the board)'));
  }
  const kindSel = selectOf(opts.kinds, opts.kind || opts.kinds[0], { name: 'kind' });
  const subject = el('input', { name: 'subject', placeholder: 'Subject (optional: the first line otherwise)', maxlength: 300 });
  const subjLabel = el('label', { class: 'f' + (opts.replyIn ? ' hidden' : '') }, 'Subject', subject);
  const body = el('textarea', { name: 'body', placeholder: 'Write in Markdown', maxlength: 65536 });
  const due = el('input', { name: 'due', placeholder: 'e.g. 2h, 1d', maxlength: 10 });
  const attach = el('div', { class: 'picker' });
  const attachBox = el('details', {}, el('summary', { class: 'muted small' }, 'Attach from the shared drive'), attach);
  // Small files from this computer: uploaded to the shared drive as the
  // board's artifact when the message is sent, and attached to it.
  let local = [], uploaded = null;
  const fileInput = el('input', { type: 'file', multiple: true, class: 'hidden', onchange: () => { addFiles([...fileInput.files]); fileInput.value = ''; } });
  const fileList = el('span', { class: 'chips' });
  const fileBtn = el('button', { class: 'btn small', type: 'button', onclick: () => fileInput.click() }, '📎 Attach files');
  const filesRow = el('div', { class: 'attach-row' }, fileBtn, fileInput, fileList);
  const UPLOAD_MAX = 5 << 20;
  function addFiles(list) {
    uploaded = null;
    for (const x of list) if (!local.some((y) => y.name === x.name)) local.push(x);
    const total = local.reduce((n, x) => n + x.size, 0);
    if (local.length > 10 || total > UPLOAD_MAX) { local = local.slice(0, local.length - list.length); toast('Up to 10 files and 5 MiB together.', true); }
    showFiles();
  }
  function showFiles() {
    fileList.replaceChildren(...local.map((x) => el('span', { class: 'chip' }, clip(x.name, 40) + ' · ' + fmtBytes(x.size) + ' ',
      el('button', { class: 'linkish', type: 'button', title: 'Remove', onclick: () => { local = local.filter((y) => y !== x); uploaded = null; showFiles(); } }, '×'))));
  }
  const b64 = (file) => new Promise((ok, fail) => { const r = new FileReader(); r.onload = () => ok(String(r.result).split(',')[1] || ''); r.onerror = () => fail(new Error('could not read ' + file.name)); r.readAsDataURL(file); });
  const btn = el('button', { class: 'btn primary', type: 'submit' }, 'Send');
  // Kind and due date are options: a new message is an assignment, an
  // answer in a thread a reply, unless the board picks otherwise.
  const optSummary = el('summary', { class: 'muted small' });
  const syncSummary = () => { optSummary.textContent = 'Options: ' + kindSel.value + ' · ' + (due.value.trim() ? 'due in ' + due.value.trim() : 'no due date'); };
  kindSel.addEventListener('change', syncSummary);
  due.addEventListener('input', syncSummary);
  const optBox = el('details', {}, optSummary,
    el('div', { class: 'row2' }, el('label', { class: 'f' }, 'Kind', kindSel), el('label', { class: 'f' }, 'Due (optional)', due)),
    el('div', { class: 'muted small' }, 'assignment: they owe you a report, with follow-ups when overdue · question: they owe you a reply · fyi: nothing is owed'));
  f.append(subjLabel, el('label', { class: 'f' }, 'Message', body), filesRow, optBox, attachBox, el('div', { style: 'display:flex;gap:10px;align-items:center' }, btn, note));
  syncSummary();
  f.refresh = () => {
    const ok = canWrite() && S.info.run.state === 'running';
    for (const x of f.querySelectorAll('input,select,textarea,button')) x.disabled = !ok;
    note.textContent = ok ? '' : S.mode === 'replay' ? 'Writing is off in replay.' : 'The firm isn\'t running.';
    // Attachments.
    const checked = new Set([...attach.querySelectorAll('input:checked')].map((x) => x.value));
    const arts = S.M ? [...S.M.artifacts.values()].reverse() : [];
    const sig = arts.map((a) => a.id + '@' + a.versions.length).join(',');
    if (attach.dataset.sig !== sig) {
      attach.dataset.sig = sig;
      attach.replaceChildren(...(arts.length ? arts.map((a) => {
        const ref = 'drive:' + a.id + '@v' + a.versions[a.versions.length - 1].version;
        return el('label', {}, el('input', { type: 'checkbox', value: ref, checked: checked.has(ref), disabled: !ok }), clip(a.title || a.id, 50) + ' (' + ref + ')');
      }) : [el('span', { class: 'muted small' }, 'Nothing published yet.')]));
    }
  };
  async function send() {
    if (!canWrite()) return;
    const M = S.M;
    let to = opts.fixedTo ? opts.fixedTo.slice() : toPick ? [...toPick.querySelectorAll('input:checked')].map((x) => x.value) : [];
    const cc = ccPick ? [...ccPick.querySelectorAll('input:checked')].map((x) => x.value) : [];
    const threadId = opts.replyIn || null;
    let in_reply_to = null, subj = subject.value.trim();
    if (threadId) {
      const t = M.threads.get(threadId);
      const ms = t ? t.mails.map((id) => M.mails.get(id)).filter(Boolean) : [];
      const other = [...ms].reverse().find((m) => m.from !== BOARD && isWorker(M, m.from)) || ms[ms.length - 1];
      if (!other) { toast('That thread has no messages.', true); return; }
      in_reply_to = other.id;
      if (opts.replyIn) to = [other.from !== BOARD ? other.from : other.to.find((x) => x !== BOARD)].filter((x) => x && isWorker(M, x));
      subj = '';
    }
    const d = due.value.trim();
    if (d && !/^\d+\s*[smhd]$/.test(d)) { toast('Due is like 30m, 2h or 1d.', true); return; }
    if (!to.length) { toast('Choose at least one recipient.', true); return; }
    if (!body.value.trim()) { toast('Write a message.', true); return; }
    const attachments = [...attach.querySelectorAll('input:checked')].map((x) => x.value);
    btn.disabled = true;
    try {
      // Uploaded once: if sending fails, a retry attaches the same artifact.
      if (local.length && !uploaded) {
        note.textContent = 'Uploading ' + local.length + ' file' + (local.length > 1 ? 's' : '') + '…';
        const files = await Promise.all(local.map(async (x) => ({ name: x.name, data: await b64(x) })));
        uploaded = str((await write('POST', F() + '/drive/upload', { files, thread: threadId, title: threadId ? '' : subj })).ref);
        note.textContent = '';
      }
      if (local.length && uploaded) attachments.push(uploaded);
      const r = await write('POST', F() + '/mail', { to, cc, subject: subj, body: body.value, kind: kindSel.value, due: d ? d.replace(/\s+/g, '') : null, in_reply_to, attachments });
      toast('Sent ' + str(r && r.id) + (r && r.recipients ? ' to ' + arr(r.recipients).join(', ') : ''));
      body.value = ''; subject.value = ''; due.value = ''; syncSummary();
      local = []; uploaded = null; showFiles();
      for (const x of attach.querySelectorAll('input:checked')) x.checked = false;
      if (toPick) for (const x of f.querySelectorAll('.picker input:checked')) x.checked = false;
      if (opts.onSent) opts.onSent(r);
    } catch (e) { toast(e.message, true); }
    btn.disabled = false;
    f.refresh();
  }
  f.refresh();
  return f;
}
