// The secrets facility's panel: a list of secrets as cards, each opening its
// own config page, and an "Add new" modal whose fields follow the kind picked.
// Values go in password fields and are sent once; nothing shows one back.
"use strict";
(() => {
  const root = document.querySelector('[data-facility="secrets"]');
  if (!root) return;
  const H = { "X-Workbench-UI": "1", "Content-Type": "application/json" };
  const el = (tag, text, cls) => { const e = document.createElement(tag); if (text !== undefined && text !== null) e.textContent = text; if (cls) e.className = cls; return e; };
  async function call(method, path, body) {
    const r = await fetch("/w/v1" + path, { method, headers: H, body: body === undefined ? undefined : JSON.stringify(body) });
    const v = r.headers.get("content-type")?.includes("json") ? await r.json() : await r.text();
    if (!r.ok) throw new Error(v.error || r.statusText);
    return v;
  }
  const field = (label, input) => { const l = el("label"); l.style.display = "grid"; l.style.gap = "2px"; l.append(el("span", label, "muted"), input); return l; };
  const input = (type, placeholder) => { const i = document.createElement("input"); i.type = type; i.placeholder = placeholder || ""; i.autocomplete = "off"; i.spellcheck = false; return i; };
  const kv = (rows) => {
    const t = el("table", null, "kv");
    for (const [k, v] of rows) {
      if (v === undefined || v === null || v === "" || (Array.isArray(v) && !v.length)) continue;
      const tr = el("tr"); tr.append(el("td", k), el("td", Array.isArray(v) ? v.join(" ") : String(v), "mono")); t.append(tr);
    }
    return t;
  };
  const SASL = ["SCRAM-SHA-256", "SCRAM-SHA-512", "PLAIN", "OAUTHBEARER"];

  let presets = [];       // kinds the workbench knows (from /secrets/presets)
  let secrets = [];       // the running config's secrets (from /secrets)
  let open = null;        // the name whose config page is shown, or null for the list
  const msg = el("div", "", "muted");

  function badges(s) {
    const b = el("div", null, "badges");
    const add = (text, cls) => b.append(el("span", text, "badge" + (cls ? " " + cls : "")));
    add(s.mode, "blue");
    for (const h of s.hosts || []) add(h);
    if (!s.workbench) add("the config's own (" + s.source + ")");
    if (s.workbench && !s.has_value) add("no value", "warn");
    if (s.plaintext) add("plaintext", "warn");
    return b;
  }

  async function refresh() {
    try { secrets = await call("GET", "/secrets"); } catch (e) { root.replaceChildren(el("span", e.message, "err")); return; }
    render();
  }
  function render() {
    if (open && !secrets.some(s => s.name === open)) open = null;
    root.replaceChildren();
    if (open) detail(secrets.find(s => s.name === open)); else listView();
    root.append(msg);
  }

  function listView() {
    const tools = el("div", null, "row");
    const add = el("button", "Add new", "primary"); add.onclick = addDialog;
    const save = el("button", "Save zapper's running config…"); save.onclick = saveConfig;
    tools.append(add, save);
    root.append(tools);
    const grid = el("div", null, "grid");
    if (!secrets.length) grid.append(el("div", "None yet. Add one with Add new.", "muted"));
    for (const s of secrets) {
      const c = el("div", null, "scard");
      const top = el("div", null, "top");
      top.append(el("strong", s.name, "mono"));
      c.append(top, badges(s));
      if ((s.used_by || []).length) c.append(el("div", "used by " + s.used_by.join(", "), "muted"));
      c.onclick = () => { open = s.name; msg.textContent = ""; render(); };
      grid.append(c);
    }
    root.append(grid);
  }

  function detail(s) {
    const back = el("a", "← Secrets", "back"); back.onclick = () => { open = null; msg.textContent = ""; render(); };
    root.append(back);
    root.append(el("h3", s.name, "mono"), badges(s));
    root.append(kv([
      ["mode", s.mode],
      ["hosts", s.hosts || []],
      ["source", s.source],
      ["value", s.workbench ? (s.has_value ? "stored by the workbench" : "none yet") : "the config's own"],
      ["user", s.user],
      ["database", s.database],
      ["mechanism", s.mechanism],
      ["plaintext", s.plaintext ? "yes — sent over non-TLS connections too" : null],
      ["used by", (s.used_by || []).join(", ")],
    ]));
    if (!s.workbench) {
      root.append(el("div", "This secret is defined in zapper's config by hand; change it there.", "muted"));
      return;
    }
    const v = input("password", "new value");
    const replace = el("button", "Replace value");
    replace.onclick = async () => {
      if (!v.value) return;
      try { await call("PUT", "/secrets/" + encodeURIComponent(s.name), { value: v.value }); v.value = ""; msg.textContent = s.name + ": value replaced (no restart needed)."; await refresh(); }
      catch (e) { msg.textContent = e.message; }
    };
    const rm = el("button", "Remove", "deny");
    rm.onclick = async () => {
      if (!confirm("Remove secret " + s.name + "?")) return;
      try { await call("DELETE", "/secrets/" + encodeURIComponent(s.name)); msg.textContent = s.name + " removed from the running config."; open = null; await refresh(); }
      catch (e) { msg.textContent = e.message; }
    };
    root.append(el("h3", "Value"), field("Replace the stored value", v), replace, el("h3", "Remove"), rm);
  }

  // Add a new secret: a modal whose fields follow the kind picked.
  function addDialog() {
    const dlg = document.createElement("dialog");
    const form = el("form"); form.method = "dialog";
    const kind = document.createElement("select");
    const name = input("text", "name, e.g. claude");
    const value = input("password", "value");
    const hosts = input("text", "hosts, comma-separated");
    const mode = document.createElement("select");
    for (const m of ["proxy", "guest", "aws", "dbauth"]) mode.append(el("option", m));
    const user = input("text", "database user");
    const database = input("text", "database (optional)");
    const mechanism = document.createElement("select");
    mechanism.append(el("option", "SCRAM-SHA-256 (default)")); mechanism.firstChild.value = "";
    for (const m of SASL) mechanism.append(Object.assign(el("option", m), { value: m }));
    const hint = el("div", "", "muted");
    const warn = el("div", "A guest secret's value is readable by everything in the session: scope it and make it expire.", "err");
    const note = el("div", "", "muted");
    const cancel = el("button", "Cancel"); cancel.type = "button"; cancel.onclick = () => dlg.close();
    const go = el("button", "Add", "primary"); go.type = "submit";
    const actions = el("div", null, "actions"); actions.append(cancel, go);
    form.append(el("h2", "Add a new secret"), field("Kind", kind), field("Name", name), field("Value", value), field("Hosts", hosts), field("How it reaches a session", mode), field("User", user), field("Database", database), field("SASL mechanism", mechanism), hint, warn, note, actions);
    dlg.append(form);
    dlg.addEventListener("close", () => dlg.remove());
    dlg.addEventListener("click", e => { if (e.target === dlg) dlg.close(); });
    document.body.append(dlg);
    dlg.showModal();

    const shown = () => {
      const p = presets.find(x => x.id === kind.value);
      const dbauth = p ? p.mode === "dbauth" : mode.value === "dbauth";
      hosts.parentElement.hidden = !!(p && p.hosts.length);
      mode.parentElement.hidden = !!p;
      user.parentElement.hidden = database.parentElement.hidden = mechanism.parentElement.hidden = !dbauth;
      user.required = dbauth;
      warn.hidden = !!p || mode.value !== "guest";
      hint.textContent = p ? p.hint + (p.env ? " · a profile gives it as " + p.env + ": {{secret:" + (name.value || p.id) + "}}" : "") : "";
      if (p && !name.value) name.placeholder = p.id;
    };
    kind.onchange = mode.onchange = shown;

    for (const p of presets) { const o = el("option", p.title); o.value = p.id; kind.append(o); }
    kind.append(Object.assign(el("option", "Other…"), { value: "" }));
    kind.value = presets.length ? presets[0].id : "";
    shown();
    (presets.length ? name : kind).focus();

    form.onsubmit = async e => {
      e.preventDefault();
      const p = presets.find(x => x.id === kind.value);
      const n = name.value || (p && p.id) || "";
      const body = { value: value.value };
      if (p) body.preset = p.id; else body.mode = mode.value;
      if (!hosts.parentElement.hidden && hosts.value) body.hosts = hosts.value.split(",").map(h => h.trim()).filter(Boolean);
      if (!user.parentElement.hidden && user.value) body.user = user.value;
      if (!database.parentElement.hidden && database.value) body.database = database.value;
      if (!mechanism.parentElement.hidden && mechanism.value) body.mechanism = mechanism.value;
      go.disabled = cancel.disabled = true; note.className = "muted"; note.textContent = "Adding…";
      try {
        await call("PUT", "/secrets/" + encodeURIComponent(n), body);
        dlg.close();
        msg.textContent = n + " added to the running config. Save it to keep it after zapper restarts.";
        open = n;
        await refresh();
      } catch (x) { note.className = "err"; note.textContent = x.message; go.disabled = cancel.disabled = false; }
    };
  }

  // Saving: the whole running config, to zapper's startup file.
  async function saveConfig() {
    try {
      const d = await call("GET", "/config/diff");
      const text = typeof d.diff === "string" ? d.diff : JSON.stringify(d.diff);
      if (!text.trim()) { msg.textContent = "Nothing to save: the startup file is as running."; return; }
      if (!confirm("Save everything running to zapper's startup file?\n\n" + text.slice(0, 2000))) return;
      const r = await call("POST", "/config/save");
      msg.textContent = "Saved to " + r.saved + ".";
    } catch (x) { msg.textContent = x.message; }
  }

  root.replaceChildren(el("div", "Loading…", "muted"));
  call("GET", "/secrets/presets")
    .then(ps => { presets = ps; return refresh(); })
    .catch(e => { root.replaceChildren(el("span", e.message, "muted")); });
})();
