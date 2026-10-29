//! Workers' agent definitions and skills, read from a firm's `agents_dir`
//! and `skills_dir` (`agents` and `skills` next to the config file unless
//! set), where `zapper init` writes the defaults from `config/`.
//!
//! - `skills/<name>/SKILL.md`: a skill any role may use.
//! - `agents/<agent>/AGENT.md`: a role's brief, added to its workers'
//!   preamble, with front matter naming the skills from `skills/` it uses.
//! - `agents/<agent>/skills/<name>/SKILL.md`: skills of that role only.
//!
//! Each role's skills (its own, the ones its brief names, and those every
//! worker or its place in the firm needs) are copied into the run with its
//! brief, and attached to its workers read-only at `~/.claude/skills`.

use std::path::{Path, PathBuf};

use crate::firm::{Firm, Role};
use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use zapper_policy::{expand_home, valid_worker_name, Config};

use super::FirmRun;

macro_rules! defaults {
    ($($path:literal),* $(,)?) => {
        &[$(($path, include_str!(concat!("../../../config/", $path)))),*]
    };
}

/// The files `zapper init` writes, by their path under the config's
/// directory.
pub(crate) const DEFAULTS: &[(&str, &str)] = defaults![
    "skills/office-mail/SKILL.md",
    "skills/office-publish/SKILL.md",
    "skills/office-resources/SKILL.md",
    "skills/office-git/SKILL.md",
    "skills/knowledge-base/SKILL.md",
    "skills/manager/SKILL.md",
    "skills/security-review/SKILL.md",
    "skills/reviewing-work/SKILL.md",
    "skills/checking-the-product/SKILL.md",
    "skills/releases/SKILL.md",
    "skills/research-sources/SKILL.md",
    "agents/ceo/AGENT.md",
    "agents/ceo/skills/budget/SKILL.md",
    "agents/ceo/skills/building-the-team/SKILL.md",
    "agents/ceo/skills/the-board/SKILL.md",
    "agents/cto/AGENT.md",
    "agents/manager/AGENT.md",
    "agents/product-manager/AGENT.md",
    "agents/product-manager/skills/product-brief/SKILL.md",
    "agents/software-engineer/AGENT.md",
    "agents/software-engineer/skills/build-and-deploy/SKILL.md",
    "agents/designer/AGENT.md",
    "agents/designer/skills/design-deliverables/SKILL.md",
    "agents/researcher/AGENT.md",
    "agents/researcher/skills/findings-pack/SKILL.md",
    "agents/research-manager/AGENT.md",
    "agents/research-manager/skills/research-plan/SKILL.md",
    "agents/research-manager/skills/collating-packs/SKILL.md",
    "agents/prototype-engineer/AGENT.md",
    "agents/prototype-engineer/skills/prototyping/SKILL.md",
    "agents/analyst/AGENT.md",
    "agents/analyst/skills/analysis/SKILL.md",
    "agents/process-improvement/AGENT.md",
    "agents/process-improvement/skills/retrospective/SKILL.md",
    "agents/security-officer/AGENT.md",
    "agents/librarian/AGENT.md",
    "agents/librarian/skills/curating/SKILL.md",
];

/// A firm's `skills_dir` when it sets none, relative to the config file.
pub const DEFAULT_DIR: &str = "skills";
/// A firm's `agents_dir` when it sets none, relative to the config file.
pub const DEFAULT_AGENTS_DIR: &str = "agents";

/// Every worker's: mail, the drive, the firm's resources.
const EVERYONE: &[&str] = &["office-mail", "office-publish", "office-resources"];
/// The skill officers work by.
const SECURITY: &str = "security-review";
/// The skill of every role that hires.
const MANAGER: &str = "manager";
/// Every worker's, in a firm with a repository: branches, review, who
/// merges to main.
const GIT: &str = "office-git";
/// Every worker's, in a firm with a knowledge base: reading it, and
/// contributing to it through the librarian.
const KNOWLEDGE_BASE: &str = "knowledge-base";

/// Where a firm's skills are.
pub fn dir(firm: &Firm) -> PathBuf {
    PathBuf::from(expand_home(
        firm.skills_dir.as_deref().unwrap_or(DEFAULT_DIR),
    ))
}

/// Where a firm's agent definitions are.
pub fn agents_dir(firm: &Firm) -> PathBuf {
    PathBuf::from(expand_home(
        firm.agents_dir.as_deref().unwrap_or(DEFAULT_AGENTS_DIR),
    ))
}

#[derive(Deserialize)]
struct Front {
    name: String,
    #[serde(default)]
    #[allow(dead_code)]
    description: String,
    /// Skills from the firm's skills directory the role uses.
    #[serde(default)]
    skills: Vec<String>,
}

/// An agent definition: its brief and the skills it brings.
#[derive(Debug, Clone, PartialEq)]
pub struct Agent {
    pub name: String,
    /// The brief, without its front matter: the preamble's part for the role.
    pub brief: String,
    /// Skills from the firm's skills directory the brief names.
    pub uses: Vec<String>,
    /// The role's own skills, in the definition's `skills/`.
    pub own: Vec<String>,
}

/// Splits `---` front matter from a Markdown file's body.
fn front_matter(text: &str) -> Option<(&str, &str)> {
    let rest = text.strip_prefix("---\n")?;
    let end = rest.find("\n---\n")?;
    Some((&rest[..end], rest[end + 5..].trim_start_matches('\n')))
}

/// Reads `<agents_dir>/<name>/`.
pub fn agent(agents: &Path, name: &str) -> Result<Agent> {
    let dir = agents.join(name);
    let path = dir.join("AGENT.md");
    let text = std::fs::read_to_string(&path).with_context(|| {
        format!(
            "agent {name}: reading {} (`zapper init` writes the default agents)",
            path.display()
        )
    })?;
    let (front, body) = front_matter(&text)
        .ok_or_else(|| anyhow!("agent {name}: {} has no --- front matter", path.display()))?;
    let front: Front = serde_yaml::from_str(front)
        .with_context(|| format!("agent {name}: the front matter of {}", path.display()))?;
    if front.name != name {
        bail!(
            "agent {name}: {} names itself {}",
            path.display(),
            front.name
        );
    }
    let mut own: Vec<String> = std::fs::read_dir(dir.join("skills"))
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().join("SKILL.md").is_file())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    own.sort();
    for s in front.skills.iter().chain(&own) {
        if !valid_worker_name(s) {
            bail!("agent {name}: skill name {s:?}: lowercase letters, digits and dashes");
        }
    }
    if let Some(s) = own.iter().find(|s| front.skills.contains(s)) {
        bail!("agent {name}: {s} is both its own skill and one it uses from the skills directory");
    }
    Ok(Agent {
        name: name.to_string(),
        brief: body.trim().to_string(),
        uses: front.skills,
        own,
    })
}

/// The skills from the skills directory a role has whatever its brief
/// says: everyone's, the git skill in a firm with a repository (and the
/// knowledge base's in a firm with one), the manager
/// skill for roles that hire, and the security skill for officers and the
/// CEO (who holds security until an officer is hired).
pub(crate) fn automatic(firm: &Firm, role_name: &str, role: &Role) -> Vec<&'static str> {
    let root_role = firm
        .root()
        .and_then(|r| firm.worker(r))
        .map(|w| w.role.clone());
    let mut names = EVERYONE.to_vec();
    // With a remote, the repositories are found when a run starts.
    if !firm.git_repos().is_empty() || firm.git_remote().is_some() {
        names.push(GIT);
    }
    if firm.knowledge_base().is_some() {
        names.push(KNOWLEDGE_BASE);
    }
    if !role.hires.is_empty() {
        names.push(MANAGER);
    }
    if role.officer || root_role.as_deref() == Some(role_name) {
        names.push(SECURITY);
    }
    names
}

/// One skill of a role, and where it comes from.
pub(crate) struct Needed {
    pub role: String,
    pub skill: String,
    /// `<dir>/<skill>/SKILL.md`: the skills directory, or the agent's own.
    pub from: PathBuf,
}

/// Who leads engineers commands their releases and checks them: a role
/// that hires one whose agent works by the releases skill gets these.
const LEADS_ENGINEERS: &[&str] = &[RELEASES, "checking-the-product"];
const RELEASES: &str = "releases";

/// Each role's skills, and their sources; an agent that can't be read is an
/// error.
pub(crate) fn needed(firm: &Firm) -> Result<Vec<Needed>> {
    let (skills, agents) = (dir(firm), agents_dir(firm));
    let mut defs = std::collections::BTreeMap::new();
    for (role_name, role) in &firm.roles {
        let a = agent(&agents, role.agent_name(role_name))
            .with_context(|| format!("role {role_name}"))?;
        defs.insert(role_name.clone(), a);
    }
    let mut out = Vec::new();
    for (role_name, role) in &firm.roles {
        let a = &defs[role_name];
        let mut general: Vec<String> = automatic(firm, role_name, role)
            .into_iter()
            .map(str::to_string)
            .collect();
        let leads = role
            .hires
            .iter()
            .filter_map(|h| defs.get(h))
            .any(|d| d.uses.iter().any(|u| u == RELEASES));
        if leads {
            general.extend(LEADS_ENGINEERS.iter().map(|s| s.to_string()));
        }
        for s in &a.uses {
            if !general.contains(s) {
                general.push(s.clone());
            }
        }
        general.dedup();
        let mut seen = std::collections::BTreeSet::new();
        general.retain(|s| seen.insert(s.clone()));
        let own_dir = agents.join(&a.name).join("skills");
        for s in &a.own {
            if general.contains(s) {
                bail!("role {role_name}: agent {}'s own skill {s} has the name of one it gets from the skills directory", a.name);
            }
        }
        out.extend(general.into_iter().map(|s| Needed {
            role: role_name.clone(),
            skill: s,
            from: skills.clone(),
        }));
        out.extend(a.own.iter().map(|s| Needed {
            role: role_name.clone(),
            skill: s.clone(),
            from: own_dir.clone(),
        }));
    }
    Ok(out)
}

/// What a firm needs that its directories lack: agents (`agents/<name>`)
/// and skills (`<name>`), with why.
pub fn missing(firm: &Firm) -> Vec<String> {
    let agents = agents_dir(firm);
    let mut out = Vec::new();
    for (role_name, role) in &firm.roles {
        let name = role.agent_name(role_name);
        if let Err(e) = agent(&agents, name) {
            out.push(format!("agents/{name} ({:#})", e.root_cause()));
        }
    }
    if out.is_empty() {
        match needed(firm) {
            Ok(n) => out.extend(
                n.into_iter()
                    .filter(|n| !n.from.join(&n.skill).join("SKILL.md").is_file())
                    .map(|n| n.skill),
            ),
            Err(e) => out.push(format!("{e:#}")),
        }
    }
    out.sort();
    out.dedup();
    out
}

/// A skill's `SKILL.md` from `dir`.
pub(crate) fn text(dir: &Path, name: &str) -> Result<String> {
    let path = dir.join(name).join("SKILL.md");
    std::fs::read_to_string(&path).with_context(|| {
        format!(
            "skill {name}: reading {} (`zapper init` writes the default skills)",
            path.display()
        )
    })
}

/// Writes the default skills and agents under `base` (`skills/…`,
/// `agents/…`), keeping any file already there. Returns the paths written.
pub fn install(base: &Path) -> Result<Vec<&'static str>> {
    let mut written = Vec::new();
    for (rel, text) in DEFAULTS {
        let path = base.join(rel);
        if path.exists() {
            continue;
        }
        std::fs::create_dir_all(path.parent().unwrap())
            .with_context(|| format!("creating {}", path.parent().unwrap().display()))?;
        std::fs::write(&path, text).with_context(|| format!("writing {}", path.display()))?;
        written.push(*rel);
    }
    Ok(written)
}

/// Writes `skills/<role>/<skill>/SKILL.md` and `agents/<role>/AGENT.md` in
/// the run directory: what each role's workers have, as it was.
pub(crate) fn prepare(run: &FirmRun, _config: &Config) -> Result<()> {
    for n in needed(&run.config)? {
        let out = run.dir.join("skills").join(&n.role).join(&n.skill);
        std::fs::create_dir_all(&out)?;
        std::fs::write(
            out.join("SKILL.md"),
            text(&n.from, &n.skill).with_context(|| format!("role {}", n.role))?,
        )?;
    }
    let agents = agents_dir(&run.config);
    for (role_name, role) in &run.config.roles {
        let from = agents.join(role.agent_name(role_name)).join("AGENT.md");
        let out = run.dir.join("agents").join(role_name);
        std::fs::create_dir_all(&out)?;
        std::fs::copy(&from, out.join("AGENT.md"))
            .with_context(|| format!("role {role_name}: {}", from.display()))?;
    }
    Ok(())
}

/// A role's brief in a run, for its workers' preamble: empty if the run has
/// none (it started before agent definitions).
pub(crate) fn brief(run: &FirmRun, role: &str) -> String {
    std::fs::read_to_string(run.dir.join("agents").join(role).join("AGENT.md"))
        .ok()
        .and_then(|t| front_matter(&t).map(|(_, body)| body.trim().to_string()))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("zapper-skills-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    /// The embedded defaults are what config/ holds, no more, no less, and
    /// each has front matter naming it.
    #[test]
    fn the_defaults_are_the_config_directory() {
        let config = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config");
        let mut on_disk = Vec::new();
        for top in ["skills", "agents"] {
            let mut dirs = vec![config.join(top)];
            while let Some(d) = dirs.pop() {
                for e in std::fs::read_dir(&d).unwrap().flatten() {
                    let p = e.path();
                    if p.is_dir() {
                        dirs.push(p);
                    } else if p.extension().is_some_and(|x| x == "md") {
                        on_disk.push(
                            p.strip_prefix(&config)
                                .unwrap()
                                .to_string_lossy()
                                .into_owned(),
                        );
                    }
                }
            }
        }
        on_disk.sort();
        let mut embedded: Vec<String> = DEFAULTS.iter().map(|(p, _)| p.to_string()).collect();
        embedded.sort();
        assert_eq!(embedded, on_disk);
        for (path, text) in DEFAULTS {
            let name = Path::new(path)
                .parent()
                .unwrap()
                .file_name()
                .unwrap()
                .to_string_lossy();
            let (front, body) = front_matter(text).unwrap_or_else(|| panic!("{path}"));
            // Strict YAML, as agents read it: a bare `description: a: b` isn't.
            let v: serde_yaml::Value =
                serde_yaml::from_str(front).unwrap_or_else(|e| panic!("{path}: {e}"));
            assert!(
                v["description"].as_str().is_some_and(|d| !d.is_empty()),
                "{path}: a description"
            );
            assert!(
                front.contains(&format!("name: {name}\n"))
                    || front.ends_with(&format!("name: {name}")),
                "{path}"
            );
            assert!(!body.trim().is_empty(), "{path}");
        }
    }

    /// Every agent reads, names only skills the skills directory has, and
    /// keeps its brief short: the preamble carries it on every turn.
    #[test]
    fn the_default_agents_read_and_their_briefs_are_short() {
        let config = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config");
        for (path, _) in DEFAULTS.iter().filter(|(p, _)| p.ends_with("AGENT.md")) {
            let name = path.split('/').nth(1).unwrap();
            let a = agent(&config.join("agents"), name).unwrap();
            for s in &a.uses {
                assert!(
                    config.join("skills").join(s).join("SKILL.md").is_file(),
                    "{name} uses {s}"
                );
            }
            let words = a.brief.split_whitespace().count();
            assert!(words <= 400, "{name}'s brief is {words} words");
        }
    }

    fn firm(roles: &str, extra: &str) -> Firm {
        serde_yaml::from_str(&format!(
            "roles:\n{roles}workers: [{{ name: ada, role: ceo }}]\n{extra}"
        ))
        .unwrap()
    }

    /// Every role learns the branch policy in a firm with a repository, and
    /// none in a firm without one; hirers manage, the CEO holds security.
    #[test]
    fn what_every_role_gets_from_its_place_in_the_firm() {
        let roles = "  ceo: { agent: ceo, hires: [eng] }\n  eng: { agent: software-engineer }\n";
        let has = |f: &Firm, role: &str, s: &str| automatic(f, role, &f.roles[role]).contains(&s);
        let f = firm(roles, "git: {}\n");
        assert!(has(&f, "ceo", "office-git") && has(&f, "eng", "office-git"));
        assert!(has(&f, "ceo", "manager") && !has(&f, "eng", "manager"));
        assert!(has(&f, "ceo", "security-review") && !has(&f, "eng", "security-review"));
        let f = firm(roles, "");
        assert!(!has(&f, "ceo", "office-git") && !has(&f, "eng", "office-git"));
    }

    /// A role's skills are its agent's own, the ones its brief names, and
    /// what its place gives; the run keeps them with the brief.
    #[test]
    fn a_role_gets_its_agents_skills_and_brief() {
        let base = tmp("assemble");
        std::fs::create_dir_all(base.join("skills/a-general")).unwrap();
        std::fs::create_dir_all(base.join("agents/boss/skills/own-one")).unwrap();
        std::fs::write(
            base.join("skills/a-general/SKILL.md"),
            "---\nname: a-general\n---\ngeneral",
        )
        .unwrap();
        std::fs::write(
            base.join("agents/boss/skills/own-one/SKILL.md"),
            "---\nname: own-one\n---\nown",
        )
        .unwrap();
        std::fs::write(
            base.join("agents/boss/AGENT.md"),
            "---\nname: boss\ndescription: x\nskills: [a-general]\n---\n\nYou run things.\n",
        )
        .unwrap();
        let a = agent(&base.join("agents"), "boss").unwrap();
        assert_eq!(a.brief, "You run things.");
        assert_eq!(
            (a.uses.clone(), a.own.clone()),
            (vec!["a-general".to_string()], vec!["own-one".to_string()])
        );
        let mut f = firm("  ceo: { agent: boss }\n", "");
        f.skills_dir = Some(base.join("skills").to_string_lossy().into());
        f.agents_dir = Some(base.join("agents").to_string_lossy().into());
        let got: Vec<String> = needed(&f).unwrap().into_iter().map(|n| n.skill).collect();
        for s in [
            "office-mail",
            "office-publish",
            "office-resources",
            "security-review",
            "a-general",
            "own-one",
        ] {
            assert!(got.contains(&s.to_string()), "{s}: {got:?}");
        }
        assert_eq!(
            missing(&f),
            [
                "office-mail",
                "office-publish",
                "office-resources",
                "security-review"
            ]
        );
        // An own skill can't shadow a general one.
        std::fs::create_dir_all(base.join("agents/boss/skills/a-general")).unwrap();
        std::fs::write(base.join("agents/boss/skills/a-general/SKILL.md"), "x").unwrap();
        assert!(agent(&base.join("agents"), "boss").is_err());
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// A role that hires engineers (whose agent works by the releases skill)
    /// gets releases and checking-the-product; one that doesn't, doesn't.
    #[test]
    fn who_leads_engineers_gets_releases_and_checking() {
        let config = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config");
        let mut f = firm(
            "  ceo: { agent: ceo, hires: [lead, rm, pm] }\n  lead: { agent: manager, hires: [eng] }\n  pm: { agent: product-manager, hires: [eng] }\n  eng: { agent: software-engineer }\n  rm: { agent: research-manager, hires: [res] }\n  res: { agent: researcher }\n",
            "",
        );
        f.skills_dir = Some(config.join("skills").to_string_lossy().into());
        f.agents_dir = Some(config.join("agents").to_string_lossy().into());
        let n = needed(&f).unwrap();
        let has = |role: &str, s: &str| n.iter().any(|x| x.role == role && x.skill == s);
        assert!(has("lead", "releases") && has("lead", "checking-the-product"));
        assert!(!has("rm", "releases") && !has("rm", "checking-the-product"));
        assert!(!has("ceo", "releases"), "the CEO hires no engineer here");
        let count = n
            .iter()
            .filter(|x| x.role == "pm" && x.skill == "checking-the-product")
            .count();
        assert_eq!(count, 1, "once, though the PM's brief names it too");
    }

    #[test]
    fn install_keeps_edits_and_nothing_comes_from_elsewhere() {
        let base = tmp("install");
        assert!(
            agent(&base.join("agents"), "ceo").is_err(),
            "no built-in fallback"
        );
        assert_eq!(install(&base).unwrap().len(), DEFAULTS.len());
        std::fs::write(
            base.join("agents/ceo/AGENT.md"),
            "---\nname: ceo\n---\nedited",
        )
        .unwrap();
        std::fs::remove_file(base.join("skills/manager/SKILL.md")).unwrap();
        assert_eq!(install(&base).unwrap(), vec!["skills/manager/SKILL.md"]);
        assert_eq!(agent(&base.join("agents"), "ceo").unwrap().brief, "edited");

        let mut f = firm("  ceo: { agent: ceo }\n", "");
        f.skills_dir = Some(base.join("skills").to_string_lossy().into());
        f.agents_dir = Some(base.join("agents").to_string_lossy().into());
        assert!(missing(&f).is_empty(), "{:?}", missing(&f));
        std::fs::remove_file(base.join("skills/office-mail/SKILL.md")).unwrap();
        assert_eq!(missing(&f), vec!["office-mail"]);
        let mut f2 = f.clone();
        f2.roles.insert(
            "eng".into(),
            serde_yaml::from_str("{ agent: nosuch }").unwrap(),
        );
        assert!(
            missing(&f2).iter().any(|m| m.starts_with("agents/nosuch")),
            "{:?}",
            missing(&f2)
        );
        std::fs::remove_dir_all(&base).unwrap();
    }
}
