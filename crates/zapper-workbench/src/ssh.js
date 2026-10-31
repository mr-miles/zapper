// The SSH connector's panel: the keys, what each reaches, and the lines the
// user's machine needs. Built as the page is, text as text.
"use strict";
(() => {
  const root = document.querySelector('[data-facility="ssh"]');
  if (!root) return;
  const H = { "X-Workbench-UI": "1", "Content-Type": "application/json" };
  const el = (tag, text, cls) => { const e = document.createElement(tag); if (text !== undefined) e.textContent = text; if (cls) e.className = cls; return e; };
  async function call(method, path, body) {
    const r = await fetch("/w/v1/ssh" + path, { method, headers: H, body: body === undefined ? undefined : JSON.stringify(body) });
    const v = r.headers.get("content-type")?.includes("json") ? await r.json() : await r.text();
    if (!r.ok) throw new Error(v.error || r.statusText);
    return v;
  }
  const field = (label, input) => { const l = el("label"); l.style.display = "grid"; l.style.gap = "2px"; l.append(el("span", label, "muted"), input); return l; };
  const reachText = r => r === "all" ? "every sandbox" : r.labels ? "sandboxes with " + Object.entries(r.labels).map(([k, v]) => k + "=" + v).join(" ") : "sandboxes " + (r.sessions || []).join(", ");

  const list = el("div");
  const msg = el("div", "", "muted");
  root.append(list);

  async function load() {
    let ks;
    try { ks = await call("GET", "/keys"); } catch (e) { root.replaceChildren(el("span", e.message, "muted")); return; }
    list.replaceChildren();
    if (!ks.length) list.append(el("div", "No keys yet.", "muted"));
    for (const k of ks) {
      const row = el("div", null, "row");
      row.append(el("strong", k.name, "mono"), el("span", k.key, "mono muted"), el("span", reachText(k.reach), "muted"));
      const rm = el("button", "Remove", "deny");
      rm.onclick = async () => {
        if (!confirm("Remove key " + k.name + "? It reaches nothing from then on.")) return;
        try { await call("DELETE", "/keys/" + encodeURIComponent(k.name)); load(); } catch (e) { msg.textContent = e.message; }
      };
      row.append(rm);
      list.append(row);
    }
  }

  const form = el("form");
  form.style.display = "grid";
  form.style.gap = "6px";
  form.style.marginTop = "10px";
  const name = document.createElement("input"); name.placeholder = "name, e.g. laptop";
  const key = document.createElement("textarea"); key.rows = 2; key.placeholder = "ssh-ed25519 AAAA… (the .pub file)"; key.spellcheck = false;
  const kind = document.createElement("select");
  for (const [v, t] of [["all", "every sandbox"], ["labels", "sandboxes with labels"], ["sessions", "these sandboxes"]]) { const o = el("option", t); o.value = v; kind.append(o); }
  const which = document.createElement("input"); which.placeholder = "project=x  or  abc234,def567";
  const add = el("button", "Add key"); add.type = "submit";
  form.append(el("strong", "New key"), field("Name", name), field("Public key", key), field("Reaches", kind), field("Which", which), add);
  const shown = () => { which.parentElement.hidden = kind.value === "all"; };
  kind.onchange = shown;
  shown();
  form.onsubmit = async e => {
    e.preventDefault();
    const parts = which.value.split(/[\s,]+/).filter(Boolean);
    let reach = "all";
    if (kind.value === "labels") reach = { labels: Object.fromEntries(parts.map(p => p.split("=", 2))) };
    if (kind.value === "sessions") reach = { sessions: parts };
    try {
      await call("PUT", "/keys/" + encodeURIComponent(name.value), { key: key.value, reach });
      name.value = ""; key.value = ""; which.value = "";
      msg.textContent = "Added: it reaches " + reachText(reach) + " that has SSH on.";
      load();
    } catch (x) { msg.textContent = x.message; }
  };

  // The lines for the user's machine.
  const host = document.createElement("input"); host.placeholder = "you@this-host, as your machine reaches it";
  const show = el("button", "Show my machine's lines");
  const out = el("pre", "", "mono");
  out.style.whiteSpace = "pre-wrap";
  show.onclick = async () => {
    try {
      const c = await call("GET", "/config" + (host.value ? "?host=" + encodeURIComponent(host.value) : ""));
      out.textContent = "# ~/.ssh/config\n" + c.ssh_config + "\n# ~/.ssh/known_hosts\n" + c.known_hosts + "\n\n# then: ssh ID.zapper";
    } catch (x) { out.textContent = x.message; }
  };
  const lines = el("div", null, "row");
  lines.append(host, show);
  root.append(form, msg, lines, out);
  load();
})();
