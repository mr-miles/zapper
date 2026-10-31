// The secrets facility's panel: built as the page is, text as text. Values
// go in password fields and are sent once; nothing shows one back.
"use strict";
(() => {
  const root = document.querySelector('[data-facility="secrets"]');
  if (!root) return;
  const H = { "X-Workbench-UI": "1", "Content-Type": "application/json" };
  const el = (tag, text, cls) => { const e = document.createElement(tag); if (text !== undefined) e.textContent = text; if (cls) e.className = cls; return e; };
  async function call(method, path, body) {
    const r = await fetch("/w/v1" + path, { method, headers: H, body: body === undefined ? undefined : JSON.stringify(body) });
    const v = r.headers.get("content-type")?.includes("json") ? await r.json() : await r.text();
    if (!r.ok) throw new Error(v.error || r.statusText);
    return v;
  }
  const field = (label, input) => { const l = el("label"); l.style.display = "grid"; l.style.gap = "2px"; l.append(el("span", label, "muted"), input); return l; };
  const input = (type, placeholder) => { const i = document.createElement("input"); i.type = type; i.placeholder = placeholder || ""; i.autocomplete = "off"; i.spellcheck = false; return i; };

  const list = el("div");
  const msg = el("div", "", "muted");
  root.append(list);

  async function load() {
    let ss;
    try { ss = await call("GET", "/secrets"); } catch (e) { list.replaceChildren(el("span", e.message, "err")); return; }
    list.replaceChildren();
    if (!ss.length) list.append(el("div", "None yet.", "muted"));
    for (const s of ss) {
      const row = el("div");
      row.style.borderBottom = "1px solid var(--line)";
      row.style.padding = "6px 0";
      const head = el("div", null, "row");
      head.append(el("strong", s.name, "mono"), el("span", s.mode + " → " + (s.hosts || []).join(", "), "muted"));
      if (!s.workbench) head.append(el("span", "the config's own (" + s.source + ")", "muted"));
      if (s.workbench && !s.has_value) head.append(el("span", "no value", "err"));
      row.append(head);
      if (s.used_by.length) row.append(el("div", "used by " + s.used_by.join(", "), "muted mono"));
      if (s.workbench) {
        const acts = el("div", null, "row");
        const v = input("password", "new value");
        const replace = el("button", "Replace value");
        replace.onclick = async () => {
          if (!v.value) return;
          try { await call("PUT", "/secrets/" + encodeURIComponent(s.name), { value: v.value }); v.value = ""; msg.textContent = s.name + ": value replaced (no restart needed)."; }
          catch (e) { msg.textContent = e.message; }
        };
        const rm = el("button", "Remove", "deny");
        rm.onclick = async () => {
          if (!confirm("Remove secret " + s.name + "?")) return;
          try { await call("DELETE", "/secrets/" + encodeURIComponent(s.name)); msg.textContent = s.name + " removed from the running config."; load(); }
          catch (e) { msg.textContent = e.message; }
        };
        acts.append(v, replace, rm);
        row.append(acts);
      }
      list.append(row);
    }
  }

  // A new secret: a preset fills in its hosts and mode.
  const form = el("form");
  form.style.display = "grid";
  form.style.gap = "6px";
  form.style.marginTop = "10px";
  const preset = document.createElement("select");
  const name = input("text", "name, e.g. claude");
  const value = input("password", "value");
  const hosts = input("text", "hosts, comma-separated");
  const mode = document.createElement("select");
  for (const m of ["proxy", "guest", "aws", "dbauth"]) mode.append(el("option", m));
  const user = input("text", "database user");
  const hint = el("div", "", "muted");
  const warn = el("div", "A guest secret's value is readable by everything in the session: scope it and make it expire.", "err");
  const add = el("button", "Add");
  add.type = "submit";
  form.append(el("strong", "New secret"), field("Kind", preset), field("Name", name), field("Value", value), field("Hosts", hosts), field("How it reaches a session", mode), field("User", user), hint, warn, add);
  let presets = [];
  const shown = () => {
    const p = presets.find(x => x.id === preset.value);
    hosts.parentElement.hidden = !!(p && p.hosts.length);
    mode.parentElement.hidden = !!p;
    user.parentElement.hidden = !(p ? p.mode === "dbauth" : mode.value === "dbauth");
    warn.hidden = !!p || mode.value !== "guest";
    hint.textContent = p ? p.hint + (p.env ? " · a profile gives it as " + p.env + ": {{secret:" + (name.value || p.id) + "}}" : "") : "";
    if (p && !name.value) name.placeholder = p.id;
  };
  preset.onchange = shown;
  mode.onchange = shown;
  form.onsubmit = async e => {
    e.preventDefault();
    const p = presets.find(x => x.id === preset.value);
    const n = name.value || (p && p.id) || "";
    const body = { value: value.value };
    if (p) body.preset = p.id; else body.mode = mode.value;
    if (!hosts.parentElement.hidden && hosts.value) body.hosts = hosts.value.split(",").map(h => h.trim()).filter(Boolean);
    if (!user.parentElement.hidden && user.value) body.user = user.value;
    try {
      await call("PUT", "/secrets/" + encodeURIComponent(n), body);
      value.value = ""; name.value = ""; hosts.value = ""; user.value = "";
      msg.textContent = n + " added to the running config. Save it to keep it after zapper restarts.";
      load();
    } catch (x) { msg.textContent = x.message; }
  };

  // Saving: the whole running config, to zapper's startup file.
  const save = el("button", "Save zapper's running config…");
  save.onclick = async () => {
    try {
      const d = await call("GET", "/config/diff");
      const text = typeof d.diff === "string" ? d.diff : JSON.stringify(d.diff);
      if (!text.trim()) { msg.textContent = "Nothing to save: the startup file is as running."; return; }
      if (!confirm("Save everything running to zapper's startup file?\n\n" + text.slice(0, 2000))) return;
      const r = await call("POST", "/config/save");
      msg.textContent = "Saved to " + r.saved + ".";
    } catch (x) { msg.textContent = x.message; }
  };

  root.append(form, el("div", null, "row"), msg);
  root.lastElementChild.previousElementSibling.append(save);
  call("GET", "/secrets/presets").then(ps => {
    presets = ps;
    preset.append(el("option", "Other…"));
    preset.firstChild.value = "";
    for (const p of ps) { const o = el("option", p.title); o.value = p.id; preset.append(o); }
    preset.value = ps.length ? ps[0].id : "";
    shown();
  }).catch(e => { form.hidden = true; save.hidden = true; list.replaceChildren(el("span", e.message, "muted")); });
  load();
})();
