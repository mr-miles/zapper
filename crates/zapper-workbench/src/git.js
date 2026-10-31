// The repository's panel: where to clone it, its branches (sessions'
// proposals among them), and the rule that lets sessions reach it. Built as
// the page is, text as text.
"use strict";
(() => {
  const root = document.querySelector('[data-facility="git"]');
  if (!root) return;
  const H = { "X-Workbench-UI": "1" };
  const el = (tag, text, cls) => { const e = document.createElement(tag); if (text !== undefined) e.textContent = text; if (cls) e.className = cls; return e; };
  async function call(method, path) {
    const r = await fetch("/w/v1/git" + path, { method, headers: H });
    const v = r.headers.get("content-type")?.includes("json") ? await r.json() : await r.text();
    if (!r.ok) throw new Error(v.error || r.statusText);
    return v;
  }
  const msg = el("div", "", "muted");
  async function load() {
    let g;
    try { g = await call("GET", "/"); } catch (e) { root.replaceChildren(el("span", e.message, "muted")); return; }
    const body = el("div");
    body.append(el("div", "On this host: " + g.clone, "mono"), el("div", "From a session: git clone " + g.guests, "mono muted"));
    if (!g.branches.length) body.append(el("div", "No commits yet: push main from your clone.", "muted"));
    for (const b of g.branches) {
      const row = el("div", null, "row");
      row.append(el("strong", b.name, "mono"), el("span", b.commit, "mono muted"), el("span", b.subject), el("span", b.by + " · " + new Date(b.when * 1000).toLocaleString(), "muted"));
      body.append(row);
    }
    body.append(el("div", "Sessions push branches, never main; merge one in your clone and push main.", "muted"));
    const rules = el("button", "Let workbench sessions use it…");
    rules.onclick = async () => {
      try {
        const usual = await call("GET", "/rules/usual");
        if (!confirm("Add this rule to zapper's global layer (for git.zapper, which only the workbench's sessions resolve)?\n\n" + usual.map(r => r.methods.join(",") + " " + r.path).join("\n"))) return;
        const r = await call("POST", "/rules");
        msg.textContent = r.added.length ? "Added " + r.added.join(", ") + " (running config; save it to keep)." : "It's there already.";
      } catch (x) { msg.textContent = x.message; }
    };
    body.append(rules, msg);
    root.replaceChildren(body);
  }
  load();
})();
