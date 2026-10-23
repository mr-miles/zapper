'use strict';
// The office page: the Runs tab.

// ─────────────────────────────── runs ───────────────────────────────
//
// The firm's runs that the board kicked off, with what each was asked to
// build; a run opens in replay. Runs nobody gave a task are left out.

const RUNS = { list: null, at: 0, busy: false, err: null, sig: null };

function mountRuns() {
  $('tab-runs').replaceChildren(el('div', { class: 'ovhead' }, el('h2', { class: 'sec', style: 'margin:0' }, 'Runs'), el('span', { class: 'muted', id: 'runs-sub' })), el('div', { id: 'runs-body' }, el('div', { class: 'empty card' }, 'Loading…')));
  RUNS.sig = null;
}
async function loadRuns() {
  if (RUNS.busy || !S.firm) return;
  RUNS.busy = true;
  try { RUNS.list = arr(await api(F() + '/runs')); RUNS.err = null; } catch (e) { RUNS.err = e.message; }
  RUNS.busy = false; RUNS.at = Date.now(); RUNS.sig = null;
  scheduleRender();
}
function updateRuns() {
  const body = $('runs-body');
  if (!body) return;
  if (!RUNS.at || Date.now() - RUNS.at > 15000) loadRuns();
  const liveRun = S.info && S.info.run ? str(S.info.run.id || S.info.run.run) : '';
  const sig = [RUNS.at, S.mode, S.run, liveRun].join('|');
  if (sig === RUNS.sig) return;
  RUNS.sig = sig;
  if (RUNS.err) { body.replaceChildren(el('div', { class: 'empty card' }, 'Could not list the runs: ' + RUNS.err)); return; }
  if (!RUNS.list) return;
  const shown = RUNS.list.filter((r) => r.kickoff);
  const hidden = RUNS.list.length - shown.length;
  $('runs-sub').textContent = shown.length + ' run' + (shown.length === 1 ? '' : 's') + ' of ' + S.firm + (hidden ? ' · ' + hidden + ' never given a task, not listed' : '');
  if (!shown.length) { body.replaceChildren(el('div', { class: 'empty card' }, 'No runs yet. Start the firm and give the CEO a task; each run is listed here and can be replayed.')); return; }
  body.replaceChildren(el('div', { class: 'runs' }, ...shown.map((r) => runCard(r, liveRun))));
}
function runCard(r, liveRun) {
  const id = str(r.run), k = r.kickoff || {};
  const isLive = id === liveRun && !r.ended && (!r.state || !['ended', 'failed'].includes(str(r.state)));
  const open = S.mode === 'replay' && S.run === id;
  const started = Number(r.started) || Number(k.ts) || 0, last = Number(r.last) || started;
  const go = () => { if (isLive) { if (S.mode === 'replay') backToLive(); else showTab(S.prevTab || 'overview'); } else enterReplay(id); };
  const del = el('button', { class: 'btn small danger', type: 'button', title: isLive ? 'Stop the firm to delete this run' : 'Delete this run: its mail, transcripts, shared drive and git repository',
    disabled: isLive, onclick: async (ev) => {
      ev.stopPropagation();
      if (!confirm('Delete run ' + id + ' ("' + clip(str(k.subject), 60) + '")? Its mail, transcripts, shared drive and git repository are removed for good.')) return;
      try {
        await write('DELETE', F() + '/runs/' + enc(id));
        if (S.mode === 'replay' && S.run === id) backToLive();
        toast('Deleted run ' + id);
      } catch (e) { toast(e.message, true); }
      RUNS.at = 0; loadRuns();
    } }, 'Delete');
  const cfg = el('button', { class: 'btn small', type: 'button', title: 'The config and skills this run started with', onclick: (ev) => { ev.stopPropagation(); openRunConfig(id); } }, 'Config');
  // The board's task as it was sent, to give the next run the same one.
  const task = str(k.subject) + (k.body ? '\n\n' + str(k.body) : '');
  const copyTask = k.subject ? el('button', { class: 'btn small', type: 'button', title: 'Copy the board\'s task (subject and message) to give another run the same one', onclick: async (ev) => {
    ev.stopPropagation();
    try { await navigator.clipboard.writeText(task); toast('Copied the task of run ' + id); }
    catch (e) { window.prompt('Copy the task:', task); }
  } }, 'Copy task') : '';
  // The board's own note, to find the run again for analysis.
  const noteBtn = el('button', { class: 'btn small', type: 'button', title: 'Your note on this run, to find it again later', onclick: async (ev) => {
    ev.stopPropagation();
    const text = prompt('Note on run ' + id + ' (empty removes it):', str(r.note));
    if (text == null) return;
    try { await write('PUT', runPath(id) + '/note', { note: text }); toast(text.trim() ? 'Noted run ' + id : 'Removed the note on run ' + id); } catch (e) { toast(e.message, true); }
    RUNS.at = 0; loadRuns();
  } }, r.note ? 'Edit note' : 'Note');
  // The run's record against the keeper's chains and signatures.
  const ver = el('button', { class: 'btn small', type: 'button', title: 'Check this run\'s record against the keeper\'s chains and signatures', onclick: async (ev) => {
    ev.stopPropagation();
    ver.disabled = true; ver.textContent = 'Verifying…';
    try {
      const v = await api(runPath(id) + '/verify');
      const n = arr(v.files).length, bad = arr(v.files).filter((f) => !f.ok);
      ver.textContent = v.ok ? '✓ intact' : '✗ changed';
      ver.title = v.ok ? n + ' files: every line matches its chain, every checkpoint is signed by the keeper' : bad.map((f) => str(f.file) + ': ' + str(f.error)).join('\n');
      toast(v.ok ? 'Run ' + id + ': the record is intact (' + n + ' files)' : 'Run ' + id + ': ' + bad.length + ' file(s) changed', !v.ok);
    } catch (e) { ver.textContent = 'Verify'; toast(e.message, true); }
    ver.disabled = false;
  } }, 'Verify');
  return el('div', { class: 'run card' + (open ? ' sel' : ''), tabindex: 0, role: 'button', title: isLive ? 'The running firm' : 'Replay this run', onclick: go,
    onkeydown: (ev) => { if (ev.key === 'Enter') go(); } },
    el('div', { class: 'run-h' }, el('span', { class: 'serif run-t' }, clip(str(k.subject) || '(no subject)', 120)),
      isLive ? pill('running', 'live') : open ? pill('accent', 'replaying') : '', copyTask, noteBtn, cfg, ver, del),
    r.note ? el('div', { class: 'run-note', title: str(r.note) }, clip(str(r.note), 400)) : '',
    k.preview ? el('div', { class: 'run-p' }, clip(str(k.preview), 240)) : '',
    el('div', { class: 'muted small run-m' },
      el('span', { class: 'mono' }, id), el('span', {}, '· ' + fmtDate(started)), el('span', {}, '· ' + fmtDur(Math.max(0, last - started)) + (isLive ? ' so far' : '')),
      el('span', {}, '· ' + (Number(r.messages) || 0) + ' messages'), el('span', {}, '· to ' + arr(k.to).map(str).join(', '))));
}

// A run's config: the whole zapper config it started with (secret values
// hidden) and every role's skills, as they were.
const CFG = { run: null, data: null, sel: 'config' };
async function openRunConfig(run) {
  const box = $('cfg');
  CFG.run = run; CFG.data = null; CFG.sel = 'config';
  box.classList.remove('hidden');
  renderRunConfig();
  try { CFG.data = await api(runPath(run) + '/config'); } catch (e) { CFG.data = { error: e.message }; }
  if (CFG.run === run) renderRunConfig();
}
function closeRunConfig() { CFG.run = null; $('cfg').classList.add('hidden'); }
function renderRunConfig() {
  const box = $('cfg'), d = CFG.data;
  const head = el('div', { class: 'cfg-h' }, el('h2', { class: 'serif' }, 'Config of run ', el('span', { class: 'mono' }, CFG.run)),
    el('span', { class: 'muted small' }, 'as it started: the whole config, secret values hidden, and the skills each role had'),
    el('button', { class: 'btn ghost close', type: 'button', title: 'Close', 'aria-label': 'Close', onclick: closeRunConfig }, '✕'));
  if (!d) { box.replaceChildren(el('div', { class: 'cfg-card card' }, head, el('div', { class: 'muted' }, 'Loading…'))); return; }
  if (d.error) { box.replaceChildren(el('div', { class: 'cfg-card card' }, head, el('div', { class: 'empty' }, d.error))); return; }
  const skills = arr(d.skills);
  const item = (key, label, sub) => el('div', { class: 'cfg-i' + (CFG.sel === key ? ' sel' : ''), onclick: () => { CFG.sel = key; renderRunConfig(); } }, label, sub ? el('span', { class: 'muted small' }, sub) : '');
  const nav = [item('config', 'zapper.yaml', d.config == null ? 'not kept' : '')];
  let role = null;
  skills.forEach((x, i) => {
    if (x.role !== role) { role = str(x.role); nav.push(el('div', { class: 'cfg-r muted small' }, role)); }
    nav.push(item('s' + i, str(x.skill), ''));
  });
  let main;
  if (CFG.sel === 'config') {
    main = d.config == null ? el('div', { class: 'empty' }, 'This run started before runs kept their config. Its skills are listed on the left.')
      : el('pre', { class: 'mono git-pre cfg-pre' }, str(d.config));
  } else {
    const x = skills[Number(CFG.sel.slice(1))] || {};
    main = el('div', {}, el('div', { class: 'muted small', style: 'margin-bottom:8px' }, str(x.role) + ' / ' + str(x.skill) + '/SKILL.md' + (arr(x.files).length ? ' · also ' + arr(x.files).map(str).join(', ') : '')),
      x.text == null ? el('div', { class: 'empty' }, 'No SKILL.md.') : renderMarkdown(str(x.text)));
  }
  box.replaceChildren(el('div', { class: 'cfg-card card' }, head, el('div', { class: 'cfg-b' }, el('div', { class: 'cfg-nav' }, ...nav), el('div', { class: 'cfg-main' }, main))));
}
document.addEventListener('keydown', (ev) => { if (ev.key === 'Escape' && CFG.run) closeRunConfig(); });
