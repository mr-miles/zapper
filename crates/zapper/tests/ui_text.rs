//! The pages render what guests and workers wrote as text: their scripts
//! build the DOM with createElement and textContent, never by parsing a
//! string as HTML or running it as code.

use std::path::{Path, PathBuf};

/// Calls that turn a string into markup or code.
const FORBIDDEN: &[&str] = &[
    "innerHTML",
    "outerHTML",
    "insertAdjacentHTML",
    "document.write",
    "createContextualFragment",
    "setHTMLUnsafe",
    "srcdoc",
    "eval(",
    "new Function",
    "DOMParser",
];

/// Each exception, where it is and why it is safe.
const ALLOWED: &[(&str, &str, &str)] = &[(
    "zapper-office/src/ui/drive.js",
    "DOMParser",
    // An SVG sprite is parsed to cut out its symbols, each shown only
    // through <img>, where SVG runs no scripts and loads nothing.
    "new DOMParser().parseFromString(text, 'image/svg+xml')",
)];

/// The pages' own scripts: every .html and .js under a crate's src, but
/// vendored assets and what runs inside the guest (browse.js).
fn sources() -> Vec<PathBuf> {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let mut out = Vec::new();
    let mut dirs = vec![crates.clone()];
    while let Some(dir) = dirs.pop() {
        for e in std::fs::read_dir(&dir).unwrap().flatten() {
            let p = e.path();
            let rel = p
                .strip_prefix(&crates)
                .unwrap()
                .to_string_lossy()
                .to_string();
            if p.is_dir() {
                if !rel.contains("target") && !rel.ends_with("assets") && !rel.contains("/tests") {
                    dirs.push(p);
                }
            } else if rel.contains("/src/")
                && (rel.ends_with(".html") || rel.ends_with(".js"))
                && !rel.ends_with("browse.js")
            {
                out.push(p);
            }
        }
    }
    out.sort();
    out
}

#[test]
fn pages_render_text_as_text() {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let files = sources();
    let names: Vec<String> = files
        .iter()
        .map(|p| {
            p.strip_prefix(&crates)
                .unwrap()
                .to_string_lossy()
                .to_string()
        })
        .collect();
    for page in [
        "zapper-api/src/ui.html",
        "zapper-office/src/office.html",
        "zapper-office/src/ui/markdown.js",
        "zapper-keeper/src/ui.html",
    ] {
        assert!(
            names.iter().any(|n| n == page),
            "{page} not found in {names:?}"
        );
    }
    let mut found = Vec::new();
    for (path, name) in files.iter().zip(&names) {
        let text = std::fs::read_to_string(path).unwrap();
        for (n, line) in text.lines().enumerate() {
            // Comments may name what the code avoids.
            let code = line.trim_start();
            if code.starts_with("//") || code.starts_with('*') || code.starts_with("/*") {
                continue;
            }
            for f in FORBIDDEN {
                if !line.contains(f) {
                    continue;
                }
                let allowed = ALLOWED
                    .iter()
                    .any(|(file, what, at)| name == file && what == f && line.contains(at));
                if !allowed {
                    found.push(format!("{name}:{}: {f}: {}", n + 1, line.trim()));
                }
            }
        }
    }
    assert!(
        found.is_empty(),
        "markup or code from strings:\n{}",
        found.join("\n")
    );
}
