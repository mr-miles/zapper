'use strict';
// The office page: the Git tab, and the page shown while the firm is off.

// ──────────────────────────────── git ────────────────────────────────
//
// The run's repository: branches, the selected branch's files and commits,
// a file's text and a commit's diff. Everything is text from workers.

const GIT = { repo: null, repos: [], branch: 'main', path: '', file: null, commit: null, branches: [], tree: null, log: [], readme: null, view: null, run: null, at: 0, busy: false, err: null };

function mountGit() {
  $('tab-git').replaceChildren(el('div', { class: 'git-empty hidden', id: 'git-empty' }), el('div', { class: 'split', id: 'git-split' },
    el('div', {},
      el('div', { class: 'bar' }, el('select', { id: 'git-repo', class: 'hidden', onchange: (e) => gitRepo(e.target.value) }), el('select', { id: 'git-branch', onchange: (e) => gitOpen({ branch: e.target.value, path: '', file: null, commit: null }) })),
      el('h2', { class: 'sec' }, 'Commits'), el('div', { class: 'card commits', id: 'git-log' }, el('span', { class: 'muted' }, 'Loading…'))),
    el('div', {}, el('div', { class: 'git-crumbs', id: 'git-crumbs' }), el('div', { id: 'git-main' }))));
  GIT.run = null;
}
// The selected repository rides on every request; `null` asks for none.
async function gitFetch(path, repo) {
  const r = repo === undefined ? GIT.repo : repo;
  return api(runPath() + '/git' + path + (r ? (path.includes('?') ? '&' : '?') + 'repo=' + enc(r) : ''));
}
// Another of the run's repositories: its main, from the top.
function gitRepo(name) {
  Object.assign(GIT, { repo: name, branch: 'main', path: '', file: null, commit: null, view: null });
  loadGitTab(true);
}
// The repository picker, when the run has more than one.
function gitRepoSelect(sel) {
  sel.classList.toggle('hidden', GIT.repos.length < 2);
  sel.replaceChildren(...GIT.repos.map((r) => opt(r, r + '.git', r === GIT.repo)));
  return sel;
}
// Loads branches and the branch's log and tree; the view (file or commit)
// separately.
async function loadGitTab(force) {
  if (!S.run) { gitShowEmpty(gitOffView()); return; }
  if (GIT.busy) return;
  const fresh = GIT.run === S.run && !force && (S.mode === 'replay' || Date.now() - GIT.at < 15000);
  if (fresh) return;
  GIT.busy = true;
  const run = S.run;
  try {
    GIT.repos = arr(await gitFetch('/repos', null)).map(str);
    if (!GIT.repos.includes(GIT.repo)) GIT.repo = GIT.repos[0] || null;
    GIT.branches = arr(await gitFetch('/branches'));
    if (GIT.branches.length && !GIT.branches.some((b) => b.name === GIT.branch)) GIT.branch = GIT.branches[0].name;
    if (GIT.branches.length) {
      const q = '?ref=' + enc(GIT.branch);
      [GIT.log, GIT.tree] = await Promise.all([gitFetch('/log' + q + '&limit=200').then(arr), gitFetch('/tree' + q + '&path=' + enc(GIT.path)).then(arr)]);
      // The directory's README, shown under its files.
      const rm = gitReadme(GIT.tree);
      GIT.readme = rm ? await gitFetch('/blob' + q + '&path=' + enc((GIT.path ? GIT.path + '/' : '') + rm.name)).catch(() => null) : null;
    } else { GIT.log = []; GIT.tree = []; GIT.readme = null; }
    GIT.err = null;
  } catch (e) { GIT.err = e.message; }
  GIT.busy = false;
  if (run !== S.run) return;
  GIT.run = run; GIT.at = Date.now();
  renderGit();
}
async function gitOpen(o) {
  Object.assign(GIT, o);
  GIT.view = null;
  if ('branch' in o || 'path' in o) { GIT.run = null; await loadGitTab(true); }
  if (GIT.file) {
    try { GIT.view = { file: await gitFetch('/blob?ref=' + enc(GIT.branch) + '&path=' + enc(GIT.file)) }; } catch (e) { GIT.view = { error: e.message }; }
  } else if (GIT.commit) {
    try { GIT.view = { commit: await gitFetch('/commits/' + enc(GIT.commit)) }; } catch (e) { GIT.view = { error: e.message }; }
  }
  renderGit();
}
// In replay the commits follow the timeline; the rest loads once.
function updateGit() { loadGitTab(false); if (S.mode === 'replay' && GIT.run === S.run && !GIT.err && GIT.branches.length) renderGitLog(); }
function gitReadme(tree) {
  const files = arr(tree).filter((e) => e.type === 'blob');
  return files.find((e) => /^readme\.(md|markdown)$/i.test(str(e.name))) || files.find((e) => /^readme(\.txt)?$/i.test(str(e.name)));
}
function renderGit() {
  const sel = $('git-branch');
  if (!sel) return;
  if ($('git-repo')) gitRepoSelect($('git-repo'));
  if (GIT.err) { gitShowEmpty(null); $('git-main').replaceChildren(el('div', { class: 'empty card' }, 'No repository: ' + GIT.err)); return; }
  // Nothing pushed yet: one calm page instead of empty panes.
  if (!GIT.branches.length) { gitShowEmpty(gitEmptyView()); return; }
  gitShowEmpty(null);
  sel.replaceChildren(...(GIT.branches.length ? GIT.branches.map((b) => opt(b.name, b.name + ' · ' + str(b.commit).slice(0, 7), b.name === GIT.branch)) : [opt('', 'no branches yet')]));
  sel.disabled = !GIT.branches.length;
  GIT.logSig = null;
  renderGitLog();
  // Breadcrumbs: branch / dir / dir / file.
  const crumbs = [el('a', { class: 'link', onclick: () => gitOpen({ path: '', file: null, commit: null }) }, GIT.branch || 'repository')];
  const parts = GIT.path ? GIT.path.split('/') : [];
  parts.forEach((p, i) => crumbs.push(' / ', el('a', { class: 'link', onclick: () => gitOpen({ path: parts.slice(0, i + 1).join('/'), file: null, commit: null }) }, p)));
  if (GIT.file) crumbs.push(' / ', el('b', {}, GIT.file.split('/').pop()));
  if (GIT.commit) crumbs.push(' · commit ', el('span', { class: 'mono' }, GIT.commit.slice(0, 8)));
  $('git-crumbs').replaceChildren(...crumbs);
  const main = $('git-main');
  if (GIT.view && GIT.view.error) { main.replaceChildren(el('div', { class: 'empty card' }, GIT.view.error)); return; }
  if (GIT.view && GIT.view.file) { main.replaceChildren(gitFileView(GIT.view.file)); return; }
  if (GIT.view && GIT.view.commit) { main.replaceChildren(gitCommitView(GIT.view.commit)); return; }
  // The directory.
  const rows = arr(GIT.tree).map((e) => {
    const dir = e.type === 'tree';
    const full = (GIT.path ? GIT.path + '/' : '') + str(e.name);
    return el('div', { class: 'git-row', onclick: () => (dir ? gitOpen({ path: full, file: null, commit: null }) : gitOpen({ file: full, commit: null })) },
      el('span', { class: 'git-ic' }, dir ? '▸' : '·'), el('span', { class: dir ? 'git-dir' : '' }, str(e.name) + (dir ? '/' : '')),
      el('span', { class: 'muted small' }, dir || e.size == null ? '' : fmtBytes(e.size)));
  });
  if (GIT.path) {
    const up = GIT.path.includes('/') ? GIT.path.slice(0, GIT.path.lastIndexOf('/')) : '';
    rows.unshift(el('div', { class: 'git-row', title: 'Up to ' + (up || GIT.branch), onclick: () => gitOpen({ path: up, file: null, commit: null }) },
      el('span', { class: 'git-ic' }, '▴'), el('span', { class: 'git-dir' }, '..'), el('span', {})));
  }
  const rm = GIT.readme;
  main.replaceChildren(el('div', { class: 'card git-tree' }, rows.length ? rows : el('span', { class: 'muted' }, GIT.branches.length ? 'Empty directory.' : 'The repository is empty.')),
    rm && rm.text != null ? el('div', { class: 'card git-readme' }, el('div', { class: 'h mono' }, str(rm.path).split('/').pop()),
      /\.(md|markdown)$/i.test(str(rm.path)) ? renderMarkdown(rm.text) : el('pre', { class: 'mono git-pre' }, rm.text)) : '');
}
// The branch's commits; in replay, those made by the timeline's position.
function renderGitLog() {
  const commits = S.mode === 'replay' ? GIT.log.filter((c) => !c.ts || Number(c.ts) <= T()) : GIT.log;
  const later = GIT.log.length - commits.length;
  const sig = [GIT.branch, GIT.commit, commits.length, GIT.log.length, GIT.at].join('|');
  if (sig === GIT.logSig) return;
  GIT.logSig = sig;
  // Subject, author and age; the description's first line under them.
  $('git-log').replaceChildren(...(commits.length ? commits.map((c) => el('div', { class: 'gc' + (GIT.commit === c.commit ? ' sel' : ''), title: str(c.commit).slice(0, 8) + ' · ' + str(c.author) + (c.ts ? ' · ' + fmtDate(Number(c.ts)) : '') + '\n' + str(c.subject) + (c.body ? '\n' + str(c.body) : ''), onclick: () => gitOpen({ commit: c.commit, file: null }) },
    el('span', { class: 's' }, clip(c.subject, 200)), el('span', { class: 'r' }, str(c.author) + (c.ts ? ' · ' + ago(Number(c.ts)) : '')),
    c.body ? el('span', { class: 'd' }, clip(c.body, 300)) : '')) : [el('span', { class: 'muted' }, later ? 'No commits yet at this point of the replay; ' + later + ' later.' : 'No commits.')]));
}
// One calm page in place of the panes, or the panes back with null.
function gitShowEmpty(view) {
  if (!$('git-split')) return;
  $('git-split').classList.toggle('hidden', !!view);
  $('git-empty').classList.toggle('hidden', !view);
  if (view && $('git-empty').dataset.v !== view.dataset.v) $('git-empty').replaceChildren(view);
  $('git-empty').dataset.v = view ? view.dataset.v : '';
}
// While no company runs, the firm's tabs all show this page: start one in
// Kickoff, or open a past run.
const OFF_TABS = ['overview', 'people', 'work', 'messages', 'security', 'drive', 'git', 'stats'];
function offView() {
  const v = el('div', { class: 'card git-empty-card' },
    el('h2', { class: 'serif' }, 'The firm is off'),
    el('p', { class: 'muted' }, 'Each run is a company of its own, with its people, mail, drive and repository, all starting empty. Start one in Kickoff; past runs are in Runs.'),
    el('div', { style: 'display:flex;gap:10px;justify-content:center' },
      el('button', { class: 'btn primary', type: 'button', onclick: () => showTab('kickoff') }, 'Go to Kickoff')));
  return v;
}
function syncOff() {
  const off = !!S.firm && companyOff() && OFF_TABS.includes(S.tab);
  $('tab-off').classList.toggle('on', off);
  const cur = $('tab-' + S.tab);
  if (cur) cur.classList.toggle('on', !off);
  if (off && $('tab-off').dataset.f !== S.firm) { $('tab-off').dataset.f = S.firm; $('tab-off').replaceChildren(offView()); }
}
// The firm is off: each run has its own repository, so offer the last one.
function gitOffView() {
  const v = el('div', { class: 'card git-empty-card' },
    el('h2', { class: 'serif' }, 'The firm is off'),
    el('p', { class: 'muted' }, 'Each run has its own repository, which starts empty. Start the firm with Up, or open a past run to browse what it pushed.'),
    el('button', { class: 'btn', type: 'button', onclick: async () => {
      try {
        const runs = arr(await api(F() + '/runs')).map((r) => str(r.run)).sort();
        if (runs.length) enterReplay(runs[runs.length - 1]); else toast('No runs yet');
      } catch (e) { toast(e.message); }
    } }, 'Open the last run'));
  v.dataset.v = 'off:' + S.firm;
  return v;
}
function gitEmptyView() {
  const domain = str(S.info && S.info.run && S.info.run.domain) || (S.firm + '.internal');
  const name = GIT.repo || 'product';
  const url = 'https://git.' + domain + '/' + name + '.git';
  const local = '~/.local/state/zapper/firms/' + S.firm + '/runs/' + S.run + '/git/' + name + '.git';
  // A branch drawing: main, a side branch, and a merge, in the firm's colours.
  const art = sv('svg', { class: 'git-art', viewBox: '0 0 160 90', 'aria-hidden': 'true' },
    sv('path', { d: 'M14 64 H146', class: 'gl main' }),
    sv('path', { d: 'M46 64 C 60 64 62 30 80 30 H 104 C 120 30 122 64 136 64', class: 'gl side' }),
    sv('circle', { cx: 14, cy: 64, r: 6, class: 'gd main' }), sv('circle', { cx: 46, cy: 64, r: 6, class: 'gd main' }),
    sv('circle', { cx: 92, cy: 30, r: 6, class: 'gd side' }), sv('circle', { cx: 136, cy: 64, r: 6, class: 'gd main' }),
    sv('circle', { cx: 92, cy: 64, r: 6, class: 'gd ghost' }));
  const copy = (text) => el('button', { class: 'btn small', type: 'button', onclick: async () => { try { await navigator.clipboard.writeText(text); toast('Copied'); } catch (e) { window.prompt('Copy:', text); } } }, 'Copy');
  const v = el('div', { class: 'card git-empty-card' }, art,
    el('h2', { class: 'serif' }, 'Nothing pushed yet'),
    el('p', { class: 'muted' }, S.mode === 'replay' ? 'This run\'s repository has no commits.' : 'The firm\'s repository is empty. Engineers and designers push to it as they build; branches, files and commits appear here as they do.'),
    el('div', { class: 'git-empty-rows' },
      ...(GIT.repos.length > 1 ? [el('div', { class: 'muted small' }, 'Repository'), gitRepoSelect(el('select', { onchange: (e) => gitRepo(e.target.value) })), el('span')] : []),
      el('div', { class: 'muted small' }, 'Workers push to'), el('code', {}, url), copy(url),
      el('div', { class: 'muted small' }, 'Clone it here'), el('code', {}, 'git clone ' + local), copy('git clone ' + local)));
  v.dataset.v = 'empty:' + S.run + ':' + name + ':' + GIT.repos.join(',');
  return v;
}
function gitFileView(f) {
  if (f.text == null) return el('div', { class: 'empty card' }, str(f.path) + ': ' + str(f.note || 'not shown') + ' (' + fmtBytes(f.size) + ')');
  const md = /\.(md|markdown)$/i.test(str(f.path));
  return el('div', { class: 'card', style: 'padding:16px 20px' },
    el('div', { class: 'muted small', style: 'margin-bottom:8px' }, str(f.path) + ' · ' + fmtBytes(f.size)),
    md ? renderMarkdown(f.text) : el('pre', { class: 'mono git-pre' }, f.text));
}
function gitCommitView(c) {
  const diff = el('pre', { class: 'mono git-pre git-diff' });
  for (const line of str(c.diff).split('\n')) {
    const cls = line.startsWith('+++') || line.startsWith('---') || line.startsWith('diff ') || line.startsWith('index ') ? 'dh'
      : line.startsWith('@@') ? 'dhunk' : line.startsWith('+') ? 'dadd' : line.startsWith('-') ? 'ddel' : '';
    diff.append(el('span', { class: cls }, line + '\n'));
  }
  return el('div', { class: 'card', style: 'padding:16px 20px' },
    el('div', { class: 'serif', style: 'font-size:17px' }, firstLine(c.message)),
    el('div', { class: 'muted small' }, str(c.author) + ' · ' + fmtDate(Number(c.ts)) + ' · ' + str(c.commit)),
    str(c.message).includes('\n') ? el('pre', { class: 'git-msg' }, str(c.message).split('\n').slice(1).join('\n').trim()) : '',
    el('div', { class: 'git-files small' }, arr(c.files).map((x) => el('div', {}, el('span', { class: 'dadd' }, '+' + str(x.added)), ' ', el('span', { class: 'ddel' }, '−' + str(x.removed)), ' ', el('span', { class: 'mono' }, str(x.path))))),
    diff, c.truncated ? el('div', { class: 'muted small' }, 'Diff cut at 1 MB.') : '');
}
