// The drive's panel: its files, a folder at a time, uploaded and taken
// away, built as the page is, text as text. Downloads come as attachments.
"use strict";
(() => {
  const root = document.querySelector('[data-facility="drive"]');
  if (!root) return;
  const H = { "X-Workbench-UI": "1" };
  const el = (tag, text, cls) => { const e = document.createElement(tag); if (text !== undefined) e.textContent = text; if (cls) e.className = cls; return e; };
  const enc = path => path.split("/").filter(Boolean).map(encodeURIComponent).join("/");
  async function call(method, path, body, headers) {
    const r = await fetch("/w/v1/drive" + path, { method, headers: { ...H, ...(headers || {}) }, body });
    const v = r.headers.get("content-type")?.includes("json") ? await r.json() : await r.text();
    if (!r.ok) throw new Error(v.error || r.statusText);
    return v;
  }
  let here = "";
  const crumbs = el("div", null, "row mono");
  const list = el("div");
  const msg = el("div", "", "muted");
  const size = n => n < 1024 ? n + " B" : n < 1 << 20 ? (n / 1024).toFixed(1) + " KiB" : (n / (1 << 20)).toFixed(1) + " MiB";

  async function load(path) {
    here = path;
    crumbs.replaceChildren();
    const parts = path.split("/").filter(Boolean);
    const go = (label, to) => { const b = el("button", label); b.onclick = () => load(to); crumbs.append(b); };
    go("drive", "");
    parts.forEach((p, i) => go(p, parts.slice(0, i + 1).join("/")));
    let d;
    try { d = await call("GET", "/files/" + enc(path)); } catch (e) { list.replaceChildren(el("span", e.message, "muted")); return; }
    list.replaceChildren();
    if (!d.entries.length) list.append(el("div", "Empty.", "muted"));
    for (const e of d.entries) {
      const row = el("div", null, "row");
      const to = (path ? path + "/" : "") + e.name;
      if (e.kind === "dir") {
        const b = el("button", e.name + "/"); b.onclick = () => load(to); row.append(b);
      } else {
        const a = el("a", e.name, "mono"); a.href = "/w/v1/drive/files/" + enc(to); a.download = e.name;
        row.append(a, el("span", size(e.size), "muted"));
      }
      const rm = el("button", "Remove", "deny");
      rm.onclick = async () => {
        if (!confirm("Remove " + to + "?")) return;
        try { await call("DELETE", "/files/" + enc(to)); load(here); } catch (x) { msg.textContent = x.message; }
      };
      row.append(rm);
      list.append(row);
    }
  }

  const pick = document.createElement("input"); pick.type = "file"; pick.multiple = true;
  const up = el("button", "Upload here");
  up.onclick = async () => {
    for (const f of pick.files) {
      try { await call("PUT", "/files/" + enc((here ? here + "/" : "") + f.name), f, { "Content-Type": "application/octet-stream" }); msg.textContent = f.name + " uploaded."; }
      catch (x) { msg.textContent = f.name + ": " + x.message; }
    }
    pick.value = "";
    load(here);
  };
  const rules = el("button", "Let workbench sessions use it…");
  rules.onclick = async () => {
    try {
      const usual = await call("GET", "/rules/usual");
      const text = usual.map(r => r.methods.join(",") + " " + r.path).join("\n");
      if (!confirm("Add these rules to zapper's global layer (for drive.zapper, which only the workbench's sessions resolve)?\n\n" + text)) return;
      const r = await call("POST", "/rules");
      msg.textContent = r.added.length ? "Added " + r.added.join(", ") + " (running config; save it to keep)." : "They're there already.";
    } catch (x) { msg.textContent = x.message; }
  };
  const bar = el("div", null, "row");
  bar.append(pick, up, rules);
  root.append(crumbs, list, bar, msg);
  load("");
})();
