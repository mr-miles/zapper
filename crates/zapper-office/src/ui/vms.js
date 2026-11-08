'use strict';
// The office page: the VMs tab.

// ──────────────────────────────── VMs ────────────────────────────────
//
// Every sandbox the office runs on the OpenShell gateway, now, whatever run
// is shown: the firms' workers and facilities. Their terminals, logs and
// policy are OpenShell's (its CLI and UI).

const VM = { list: [], at: 0, sig: null, error: null };

function mountVms() {
  $('tab-vms').replaceChildren(
    el('div', { class: 'ovhead' }, el('h2', { class: 'sec', style: 'margin:0' }, 'VMs'), el('span', { class: 'muted', id: 'vm-sub' }, 'every sandbox the office runs')),
    // With no company running, the way to start one.
    el('div', { id: 'vm-off', class: 'hint hidden', style: 'display:flex;gap:10px;align-items:center' }, el('span', {}, 'No company is running.'),
      el('button', { class: 'btn primary small', type: 'button', onclick: () => showTab('kickoff') }, 'Go to Kickoff')),
    el('div', { id: 'vm-body' }, el('div', { class: 'empty card' }, 'Loading…')));
  VM.sig = null;
}
async function pollVms() {
  if (S.tab !== 'vms') return;
  try {
    VM.list = arr(await api('/sessions')).filter((x) => x.state === 'running' || x.state === 'starting');
    VM.error = null;
  } catch (e) { VM.error = e.message; }
  VM.at = Date.now();
  VM.sig = null;
  scheduleRender();
}
setInterval(() => { if (S.tab === 'vms' && !document.hidden) pollVms(); }, 4000);
function vmCard(x) {
  // The run's own chart (from its journal), not the config's: the config may
  // have changed since the run started.
  const rw = x.firm && x.firm === S.firm && S.M ? S.M.workers.get(x.member) : null;
  const chart = rw && rw.inChart ? rw : x.firm && x.firm === S.firm && S.info ? arr(S.info.chart).find((w) => w.name === x.member) : null;
  const facility = x.firm && !chart;
  const mib = (v) => (v >= 1024 ? (v / 1024).toFixed(1) + ' GiB' : Math.round(v) + ' MiB');
  const head = el('div', { class: 'vm-head' },
    chart ? avatar((chart.avatar && chart.avatar.figure) || 'plain', chart.avatar && chart.avatar.color, (S.M && S.M.workers.get(x.member) || {}).state || 'idle')
      : el('div', { class: 'vm-icon' + (facility ? ' fac' : '') }, facility ? '⎈' : '▣'),
    el('div', { class: 'vm-name' },
      el('div', { class: 'serif' }, x.member || x.id),
      el('div', { class: 'muted small' }, chart ? (chart.title || chart.role) + (agentLabel(chart) ? ' · ' + agentLabel(chart) : '') + ' · firm ' + x.firm : facility ? 'facility · firm ' + x.firm : 'profile ' + x.profile)),
    pill(x.state, x.state));
  const spec = el('div', { class: 'vm-spec small' },
    el('span', {}, x.vcpus + ' vCPU'), el('span', {}, mib(Number(x.memory_mib) || 0)), el('span', {}, x.image),
    el('span', {}, 'up ' + fmtDur(nowSec() - (Number(x.created) || nowSec()))));
  const actions = el('div', { class: 'vm-actions' });
  if (chart) actions.append(el('button', { class: 'btn small', type: 'button', onclick: () => openPanel(x.member) }, (chart.agent && chart.agent !== 'claude-code' ? agentName(chart.agent) : 'Claude') + ' transcript'));
  if (facility && x.firm === S.firm) actions.append(el('span', { class: 'vm-dash' }, dashboardButtons()));
  actions.append(el('span', { class: 'mono muted small', title: 'the sandbox; openshell sandbox connect ' + x.id }, x.id));
  return el('div', { class: 'vm card' + (facility ? ' facility' : '') }, head, spec, actions);
}
function updateVms() {
  const body = $('vm-body');
  if (!body) return;
  if ($('vm-off')) $('vm-off').classList.toggle('hidden', !(S.firm && companyOff()));
  const sig = String(VM.at) + S.firm + (S.M ? S.M.seq || 0 : 0) + dashboardSig();
  if (sig === VM.sig) return;
  VM.sig = sig;
  if (!VM.at) return;
  if (VM.error) { body.replaceChildren(el('div', { class: 'empty card' }, 'Could not load the sessions: ' + VM.error)); return; }
  const total = VM.list.reduce((a, x) => [a[0] + (Number(x.vcpus) || 0), a[1] + (Number(x.memory_mib) || 0)], [0, 0]);
  $('vm-sub').textContent = VM.list.length + ' running · ' + total[0] + ' vCPUs · ' + (total[1] / 1024).toFixed(1) + ' GiB · updated ' + fmtTime(VM.at / 1000);
  const host = el('div', { class: 'vm-host card' },
    el('div', { class: 'vm-host-t' }, el('span', { class: 'serif' }, 'Host'), el('span', { class: 'muted small' }, 'sandboxes of the OpenShell gateway: their network goes through its policy, the office through its edge')));
  const groups = new Map();
  for (const x of VM.list) {
    const g = x.firm ? 'firm:' + x.firm : '';
    if (!groups.has(g)) groups.set(g, []);
    groups.get(g).push(x);
  }
  const keys = [...groups.keys()].sort((a, b) => (a === 'firm:' + S.firm ? -1 : b === 'firm:' + S.firm ? 1 : a === '' ? 1 : b === '' ? -1 : a.localeCompare(b)));
  const out = [host];
  if (!VM.list.length) out.push(el('div', { class: 'empty card' }, 'No VMs are running.'));
  for (const g of keys) {
    const list = groups.get(g);
    const order = (x) => (x.firm && !(S.M && (S.M.workers.get(x.member) || {}).inChart) && !(S.info && arr(S.info.chart).some((w) => w.name === x.member)) ? 0 : 1);
    list.sort((a, b) => order(a) - order(b) || str(a.member || a.id).localeCompare(str(b.member || b.id)));
    out.push(el('div', { class: 'vm-group' },
      el('div', { class: 'vm-gh' }, el('span', { class: 'serif' }, g ? 'Firm ' + g.slice(5) : 'Sessions'), el('span', { class: 'muted small' }, list.length + (list.length === 1 ? ' VM' : ' VMs'))),
      el('div', { class: 'vm-grid' }, list.map(vmCard))));
  }
  body.replaceChildren(...out);
}
