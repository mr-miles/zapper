#!/bin/sh
# Checks the syntax of the pages' scripts, which no Rust build looks at:
# each inline <script> of the crates' HTML pages (the UI, the office, the
# keeper's), and the scripts served as files. Needs node.
#
#   scripts/check-js.sh
set -eu

HERE=$(cd "$(dirname "$0")/.." && pwd)
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

find "$HERE/crates" -path '*/target' -prune -o -path '*/assets' -prune -o \
    \( -name '*.html' -o -name '*.js' \) -print | sort > "$TMP/files"

node - "$HERE" "$TMP" <<'EOF'
const fs = require("fs");
const path = require("path");
const { spawnSync } = require("child_process");
const [here, tmp] = process.argv.slice(2);
const files = fs.readFileSync(path.join(tmp, "files"), "utf8").split("\n").filter(Boolean);
let bad = 0, checked = 0;
function check(file, label) {
  const r = spawnSync(process.execPath, ["--check", file], { encoding: "utf8" });
  checked++;
  if (r.status !== 0) {
    bad++;
    console.error(`${label}:\n${r.stderr.trim()}\n`);
  }
}
for (const f of files) {
  const rel = path.relative(here, f);
  if (f.endsWith(".js")) {
    check(f, rel);
    continue;
  }
  const html = fs.readFileSync(f, "utf8");
  const re = /<script(\s[^>]*)?>([\s\S]*?)<\/script>/gi;
  let m, n = 0;
  while ((m = re.exec(html))) {
    const attrs = m[1] || "";
    if (/\ssrc\s*=/i.test(attrs)) continue;
    const type = (attrs.match(/\stype\s*=\s*["']?([^"'\s>]+)/i) || [])[1];
    if (type && !/^(module|text\/javascript)$/i.test(type)) continue;
    n++;
    // Blank lines before the script keep node's line numbers the page's.
    const line = html.slice(0, m.index + m[0].indexOf(">") + 1).split("\n").length - 1;
    const out = path.join(tmp, `${rel.replace(/[\/.]/g, "_")}-${n}.${type === "module" ? "mjs" : "js"}`);
    fs.writeFileSync(out, "\n".repeat(line) + m[2]);
    check(out, `${rel} (script ${n})`);
  }
}
console.log(`${checked} scripts checked, ${bad} with errors`);
process.exit(bad ? 1 : 0);
EOF
