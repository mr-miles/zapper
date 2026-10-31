//! A commit of the workbench's repository rendered as zapper's config: the
//! files in zapper's own format, each under its section.
//!
//! ```text
//! policy.yaml                  policy
//! secrets.yaml                 secrets (their hosts and modes; values are the secrets facility's)
//! images/NAME.yaml             images.NAME
//! profiles/NAME.yaml           profiles.NAME
//! agents/NAME/AGENT.md         a profile's CLAUDE.md, for one that says `agent.definition: NAME`
//! agents/NAME/skills/SKILL/    that agent's own skills
//! skills/SKILL/                skills every profile gets
//! ```
//!
//! What the render does besides placing files: a profile's
//! `agent.definition` (the one key that isn't zapper's) becomes its
//! `instructions`, the definition's `AGENT.md` without its front matter;
//! every profile gets a skills directory of its own, `skills/` with its
//! agent's own laid over it; a path relative to the repository (in mounts,
//! config mounts and image arguments) becomes the checkout's; a secret with
//! no source gets the secrets facility's file. The `daemon` section isn't
//! the repository's: it is given.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Map, Value};

/// A commit rendered: zapper's config, and each image's hash (its recipe
/// and the files its arguments name), to tell which changed.
#[derive(Debug)]
pub struct Rendered {
    pub config: Value,
    pub images: BTreeMap<String, String>,
}

fn load(path: &Path) -> Result<Value> {
    let text = std::fs::read_to_string(path).with_context(|| format!("{}", path.display()))?;
    let v: Value = serde_yaml::from_str(&text).with_context(|| format!("{}", path.display()))?;
    Ok(if v.is_null() { json!({}) } else { v })
}

/// The `NAME.yaml` files of `dir`, by name.
fn yaml_files(dir: &Path) -> Result<BTreeMap<String, Value>> {
    let mut out = BTreeMap::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Ok(out);
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.extension().and_then(|x| x.to_str()) == Some("yaml") {
            let name = p.file_stem().unwrap().to_string_lossy().into_owned();
            out.insert(name, load(&p)?);
        }
    }
    Ok(out)
}

/// Whether a path is the repository's: not absolute, not a home's
/// (`~/…`), not a placeholder (`{workdir}`).
fn relative(p: &str) -> bool {
    !(p.starts_with('/') || p.starts_with('~') || p.starts_with('{'))
}

/// `p` in the checkout, if it is the repository's; and never out of it.
fn in_checkout(repo: &Path, p: &str, what: &str) -> Result<String> {
    if !relative(p) {
        return Ok(p.to_string());
    }
    if p.split('/').any(|c| c == "..") {
        bail!("{what}: {p:?} leads out of the repository");
    }
    Ok(repo.join(p).display().to_string())
}

/// `AGENT.md` without its front matter.
fn definition_body(text: &str) -> &str {
    text.strip_prefix("---\n")
        .and_then(|t| t.split_once("\n---\n"))
        .map(|(_, b)| b)
        .unwrap_or(text)
        .trim_start()
}

/// Copies `from`'s tree into `to`, a file replacing one of the same name;
/// links and special files left out.
fn copy_tree(from: &Path, to: &Path) -> Result<()> {
    let Ok(entries) = std::fs::read_dir(from) else {
        return Ok(());
    };
    std::fs::create_dir_all(to)?;
    for e in entries.flatten() {
        let t = e.file_type()?;
        let (src, dst) = (e.path(), to.join(e.file_name()));
        if t.is_dir() {
            copy_tree(&src, &dst)?;
        } else if t.is_file() {
            std::fs::copy(&src, &dst)?;
        }
    }
    Ok(())
}

/// The SHA-256 of `bytes` and of every file under `paths`, in order.
fn hash(bytes: &[u8], paths: &[PathBuf]) -> Result<String> {
    let mut ctx = ring::digest::Context::new(&ring::digest::SHA256);
    ctx.update(bytes);
    fn walk(ctx: &mut ring::digest::Context, p: &Path) -> Result<()> {
        let meta = std::fs::symlink_metadata(p)?;
        if meta.is_dir() {
            let mut names: Vec<_> = std::fs::read_dir(p)?.flatten().map(|e| e.path()).collect();
            names.sort();
            for n in names {
                ctx.update(n.file_name().unwrap().as_encoded_bytes());
                walk(ctx, &n)?;
            }
        } else if meta.is_file() {
            ctx.update(&std::fs::read(p)?);
        }
        Ok(())
    }
    for p in paths {
        walk(&mut ctx, p)?;
    }
    Ok(ctx
        .finish()
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

/// Renders the checkout `repo` as zapper's config, with `daemon` as its
/// daemon section, the profiles' skills directories made under `out`, and
/// secrets without a source taking their files from `secrets`.
pub fn render(repo: &Path, out: &Path, secrets: &Path, daemon: Value) -> Result<Rendered> {
    let mut config = Map::new();
    config.insert("daemon".into(), daemon);

    let policy = repo.join("policy.yaml");
    if policy.exists() {
        config.insert("policy".into(), load(&policy)?);
    }

    let mut secret_map = Map::new();
    let file = repo.join("secrets.yaml");
    if file.exists() {
        let s = load(&file)?;
        for (name, mut entry) in s.as_object().cloned().unwrap_or_default() {
            let e = entry
                .as_object_mut()
                .ok_or_else(|| anyhow!("secrets.yaml: {name} isn't a map"))?;
            if e.contains_key("value") {
                bail!("secrets.yaml: {name} has its value inline: values never go in the repository, set it in the secrets facility");
            }
            let sourced = ["file", "env", "command"]
                .iter()
                .any(|k| e.contains_key(*k));
            if !sourced {
                e.insert(
                    "file".into(),
                    json!(secrets.join(&name).display().to_string()),
                );
            }
            secret_map.insert(name, entry);
        }
    }
    config.insert("secrets".into(), Value::Object(secret_map));

    let mut images = Map::new();
    let mut hashes = BTreeMap::new();
    for (name, mut recipe) in yaml_files(&repo.join("images"))? {
        let mut inputs = vec![];
        if let Some(args) = recipe.get_mut("args").and_then(|a| a.as_object_mut()) {
            for (arg, a) in args.iter_mut() {
                if let Some(d) = a
                    .get("default")
                    .and_then(|d| d.as_str())
                    .map(str::to_string)
                {
                    let p = in_checkout(repo, &d, &format!("images/{name}.yaml: args.{arg}"))?;
                    inputs.push(PathBuf::from(&p));
                    a["default"] = json!(p);
                }
            }
        }
        // The hash is of the recipe as written, not of where it was checked
        // out, and of what its arguments hold.
        let written = load(&repo.join("images").join(format!("{name}.yaml")))?;
        hashes.insert(name.clone(), hash(written.to_string().as_bytes(), &inputs)?);
        images.insert(name, recipe);
    }
    config.insert("images".into(), Value::Object(images));

    let mut profiles = Map::new();
    for (name, mut p) in yaml_files(&repo.join("profiles"))? {
        let what = format!("profiles/{name}.yaml");
        let obj = p
            .as_object_mut()
            .ok_or_else(|| anyhow!("{what} isn't a map"))?;
        for m in obj
            .get_mut("mounts")
            .and_then(|m| m.as_array_mut())
            .into_iter()
            .flatten()
        {
            if let Some(h) = m.get("host").and_then(|h| h.as_str()).map(str::to_string) {
                m["host"] = json!(in_checkout(repo, &h, &format!("{what}: mounts"))?);
            }
        }
        let agent = obj
            .entry("agent")
            .or_insert_with(|| json!({}))
            .as_object_mut()
            .ok_or_else(|| anyhow!("{what}: agent isn't a map"))?;
        if agent.contains_key("skills") {
            bail!("{what}: agent.skills comes from the repository's skills/ and its agent's own: leave it out");
        }
        for m in agent
            .get_mut("config_mounts")
            .and_then(|m| m.as_array_mut())
            .into_iter()
            .flatten()
        {
            if let Some(h) = m.get("host").and_then(|h| h.as_str()).map(str::to_string) {
                m["host"] = json!(in_checkout(repo, &h, &format!("{what}: config_mounts"))?);
            }
        }
        // Every profile: the repository's skills, its agent's own over them.
        let skills = out.join("skills").join(&name);
        copy_tree(&repo.join("skills"), &skills)?;
        if let Some(def) = agent.remove("definition") {
            let def = def
                .as_str()
                .ok_or_else(|| anyhow!("{what}: agent.definition is a name"))?
                .to_string();
            if def.is_empty() || def.contains('/') || def.starts_with('.') {
                bail!("{what}: agent.definition {def:?} isn't an agent's name");
            }
            if agent.contains_key("instructions") {
                bail!("{what}: agent.definition gives its instructions; leave instructions out");
            }
            let dir = repo.join("agents").join(&def);
            let text = std::fs::read_to_string(dir.join("AGENT.md"))
                .map_err(|_| anyhow!("{what}: no agents/{def}/AGENT.md"))?;
            agent.insert("instructions".into(), json!(definition_body(&text)));
            copy_tree(&dir.join("skills"), &skills)?;
        }
        agent.insert("skills".into(), json!(skills.display().to_string()));
        profiles.insert(name, p);
    }
    config.insert("profiles".into(), Value::Object(profiles));
    Ok(Rendered {
        config: Value::Object(config),
        images: hashes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo(name: &str) -> (PathBuf, PathBuf) {
        let base =
            std::env::temp_dir().join(format!("zapper-wb-render-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let r = base.join("repo");
        let w = |p: &str, s: &str| {
            let f = r.join(p);
            std::fs::create_dir_all(f.parent().unwrap()).unwrap();
            std::fs::write(f, s).unwrap();
        };
        w("policy.yaml", "defaults: { net: deny, exec: allow }\n");
        w("secrets.yaml", "claude: { hosts: [api.anthropic.com] }\nminted: { command: [mint], hosts: [h.example] }\n");
        w("images/office.yaml", "from: docker.io/library/ubuntu:26.04\nargs:\n  tools: { default: files/tools }\ncopy: [{ from: tools, to: /opt/tools }]\n");
        w("files/tools/x.sh", "echo 1\n");
        w("profiles/claude.yaml", "engine: { image: office }\nagent: { kind: claude-code }\nmounts:\n  - { host: \"{workdir}\", guest: /work, mode: rw }\n  - { host: files, guest: /files }\n");
        w(
            "profiles/engineer.yaml",
            "extends: claude\nagent: { definition: software-engineer }\n",
        );
        w(
            "agents/software-engineer/AGENT.md",
            "---\nname: software-engineer\n---\n\nYou build.\n",
        );
        w(
            "agents/software-engineer/skills/building/SKILL.md",
            "build\n",
        );
        w(
            "agents/software-engineer/skills/reviewing/SKILL.md",
            "the engineer's own\n",
        );
        w("skills/reviewing/SKILL.md", "shared\n");
        w("skills/checking/SKILL.md", "check\n");
        (r, base.join("live"))
    }

    fn rendered(name: &str) -> (Rendered, PathBuf, PathBuf) {
        let (r, out) = repo(name);
        let x = render(&r, &out, Path::new("/s"), json!({ "state": "~/s" })).unwrap();
        (x, r, out)
    }

    #[test]
    fn each_file_goes_under_its_section_and_the_result_is_zappers_config() {
        let (x, r, _) = rendered("sections");
        let c = &x.config;
        assert_eq!(c["daemon"]["state"], "~/s");
        assert_eq!(c["policy"]["defaults"]["net"], "deny");
        assert_eq!(
            c["secrets"]["claude"]["file"], "/s/claude",
            "the facility's file"
        );
        assert_eq!(
            c["secrets"]["minted"]["command"],
            json!(["mint"]),
            "a source of its own kept"
        );
        assert!(c["secrets"]["minted"].get("file").is_none());
        assert_eq!(
            c["images"]["office"]["args"]["tools"]["default"],
            r.join("files/tools").display().to_string()
        );
        let mounts = &c["profiles"]["claude"]["mounts"];
        assert_eq!(mounts[0]["host"], "{workdir}", "a placeholder stays");
        assert_eq!(mounts[1]["host"], r.join("files").display().to_string());
        // zapper takes it.
        let yaml = serde_yaml::to_string(c).unwrap();
        assert!(yaml.contains("profiles:"));
    }

    #[test]
    fn a_definition_is_the_profiles_claude_md_and_its_skills_lie_over_the_shared() {
        let (x, _, out) = rendered("definition");
        let e = &x.config["profiles"]["engineer"]["agent"];
        assert_eq!(e["instructions"], "You build.\n");
        assert!(e.get("definition").is_none(), "consumed");
        let skills = PathBuf::from(e["skills"].as_str().unwrap());
        assert_eq!(skills, out.join("skills/engineer"));
        let read = |s: &str| std::fs::read_to_string(skills.join(s).join("SKILL.md")).unwrap();
        assert_eq!(read("checking"), "check\n", "shared");
        assert_eq!(read("building"), "build\n", "its own");
        assert_eq!(
            read("reviewing"),
            "the engineer's own\n",
            "its own over the shared"
        );
        // A profile with none gets the shared ones.
        let c = PathBuf::from(
            x.config["profiles"]["claude"]["agent"]["skills"]
                .as_str()
                .unwrap(),
        );
        assert_eq!(
            std::fs::read_to_string(c.join("reviewing/SKILL.md")).unwrap(),
            "shared\n"
        );
        assert!(!c.join("building").exists());
    }

    #[test]
    fn an_images_hash_follows_its_recipe_and_its_files() {
        let (x, r, out) = rendered("hash");
        let before = x.images["office"].clone();
        let again = render(&r, &out, Path::new("/s"), json!({})).unwrap();
        assert_eq!(
            again.images["office"], before,
            "the same commit, the same hash"
        );
        std::fs::write(r.join("files/tools/x.sh"), "echo 2\n").unwrap();
        let changed = render(&r, &out, Path::new("/s"), json!({})).unwrap();
        assert_ne!(changed.images["office"], before, "a file it copies changed");
    }

    #[test]
    fn what_doesnt_belong_is_refused_with_why() {
        let cases = [
            (
                "secrets.yaml",
                "x: { value: s3cret, hosts: [h] }\n",
                "values never go in the repository",
            ),
            (
                "profiles/bad.yaml",
                "agent: { skills: elsewhere }\n",
                "agent.skills comes from",
            ),
            (
                "profiles/bad.yaml",
                "agent: { definition: nobody }\n",
                "no agents/nobody/AGENT.md",
            ),
            (
                "profiles/bad.yaml",
                "agent: { definition: software-engineer, instructions: x }\n",
                "leave instructions out",
            ),
            (
                "profiles/bad.yaml",
                "mounts: [{ host: ../../etc, guest: /x }]\n",
                "leads out of the repository",
            ),
        ];
        for (i, (file, text, why)) in cases.iter().enumerate() {
            let (r, out) = repo(&format!("bad{i}"));
            std::fs::write(r.join(file), text).unwrap();
            let e = render(&r, &out, Path::new("/s"), json!({})).err().unwrap();
            assert!(format!("{e:#}").contains(why), "{file}: {e:#}");
        }
    }
}
