// The repository's panel: where to clone it, its branches (sessions'
// proposals among them), what of it is applied to zapper, checking a branch
// and applying main, the images it builds, and the sandboxes an apply has
// left behind. Built as the page is, text as text.
"use strict";
(() => {
  const root = document.querySelector('[data-facility="git"]');
  if (!root) return;
  const H = { "X-Workbench-UI": "1", "Content-Type": "application/json" };
  const el = (tag, text, cls) => { const e = document.createElement(tag); if (text !== undefined) e.textContent = text; if (cls) e.className = cls; return e; };
  async function call(method, path, body) {
    const r = await fetch("/w/v1/git" + path, { method, headers: H, body: body === undefined ? undefined : JSON.stringify(body) });
    const v = r.headers.get("content-type")?.includes("json") ? await r.json() : await r.text();
    if (!r.ok) throw new Error(v.error || r.statusText);
    return v;
  }
  const msg = el("div", "", "muted");
  const out = el("pre", "", "mono");
  out.style.whiteSpace = "pre-wrap";
  out.style.maxHeight = "320px";
  out.style.overflow = "auto";
  const short = c => (c || "").slice(0, 12);

  async function load() {
    let g;
    try { g = await call("GET", ""); } catch (e) { root.replaceChildren(el("span", e.message, "muted")); return; }
    const body = el("div");
    body.append(el("div", "On this host: " + g.clone, "mono"), el("div", "From a session: git clone " + g.guests, "mono muted"));

    const c = g.config || {};
    const live = c.live ? "applied: " + short(c.live.commit) + " " + c.live.subject + " (" + new Date(c.live.applied_at * 1000).toLocaleString() + ")" : "nothing applied yet";
    body.append(el("div", live, c.live ? "" : "muted"));
    if (c.main && (!c.live || c.main.commit !== c.live.commit)) body.append(el("div", "main is at " + short(c.main.commit) + " " + c.main.subject + ", not applied", "err"));
    for (const [name, b] of Object.entries(c.builds || {})) body.append(el("div", "image " + name + ": " + b.state + (b.detail ? " (" + b.detail + ")" : ""), b.state === "failed" ? "err" : "muted"));
    if ((c.out_of_date || []).length) {
      body.append(el("div", "Made from an older commit (not restarted): " + c.out_of_date.map(s => s.id + " " + s.profile + " @" + s.commit).join(", "), "muted"));
    }

    const which = document.createElement("select");
    for (const b of g.branches) { const o = el("option", b.name + " · " + b.subject); o.value = b.name; which.append(o); }
    const check = el("button", "Check");
    check.onclick = async () => {
      out.textContent = "Checking " + which.value + "…";
      try {
        const r = await call("POST", "/check", { ref: which.value });
        out.textContent = (r.diff || "No change to zapper's config.") + (r.images.length ? "\nImages to build again: " + r.images.join(", ") : "");
      } catch (x) { out.textContent = x.message; }
    };
    const apply = el("button", "Apply main…");
    apply.onclick = async () => {
      try {
        const r = await call("POST", "/check", { ref: "main" });
        if (!confirm("Apply main (" + short(r.commit) + " " + r.subject + ") as zapper's whole config, and save it?\n\n" + (r.diff || "No change to zapper's config.").slice(0, 3000))) return;
        const a = await call("POST", "/apply");
        msg.textContent = "Applied " + short(a.commit) + "." + (a.building.length ? " Building " + a.building.join(", ") + "." : "");
        load();
      } catch (x) { msg.textContent = x.message; }
    };
    const row = el("div", null, "row");
    row.append(which, check, apply);
    body.append(row);

    for (const b of g.branches) {
      const r = el("div", null, "row");
      r.append(el("strong", b.name, "mono"), el("span", b.commit, "mono muted"), el("span", b.subject), el("span", b.by + " · " + new Date(b.when * 1000).toLocaleString(), "muted"));
      body.append(r);
    }
    if (!g.branches.length) body.append(el("div", "No commits yet: push main from your clone.", "muted"));
    const rules = el("button", "Let workbench sessions use it…");
    rules.onclick = async () => {
      try {
        const usual = await call("GET", "/rules/usual");
        if (!confirm("Add this rule to zapper's global layer (for git.zapper, which only the workbench's sessions resolve)?\n\n" + usual.map(r => r.methods.join(",") + " " + r.path).join("\n"))) return;
        const r = await call("POST", "/rules");
        msg.textContent = r.added.length ? "Added " + r.added.join(", ") + " to the running config. An apply of the repository replaces it: to keep them, put them in its policy.yaml." : "It's there already.";
      } catch (x) { msg.textContent = x.message; }
    };
    body.append(rules, msg, out);
    root.replaceChildren(body);
  }
  load();
})();
