//! Workers' skills: read from a firm's `skills_dir` (`skills` next to the
//! config file unless set), where `zapper init` writes the defaults from
//! `config/skills/`. Each role's skills are copied into the run and attached
//! to its workers read-only at `~/.claude/skills`.

use std::path::{Path, PathBuf};

use crate::firm::Firm;
use anyhow::{Context, Result};
use zapper_policy::{expand_home, Config};

use super::FirmRun;

macro_rules! defaults {
    ($($name:literal),* $(,)?) => {
        &[$(($name, include_str!(concat!("../../../config/skills/", $name, "/SKILL.md")))),*]
    };
}

/// The skills `zapper init` writes.
pub const DEFAULTS: &[(&str, &str)] = defaults![
    "office-mail",
    "office-publish",
    "office-resources",
    "ceo",
    "product-manager",
    "manager",
    "cto",
    "software-engineer",
    "designer",
    "researcher",
    "analyst",
    "security-review",
];

/// A firm's `skills_dir` when it sets none, relative to the config file.
pub const DEFAULT_DIR: &str = "skills";

/// The skill officers work by.
const SECURITY: &str = "security-review";
/// The skill of every role that hires.
const MANAGER: &str = "manager";
/// Every worker's training: the firm's resources and how to get access.
const RESOURCES: &str = "office-resources";

/// Where a firm's skills are.
pub fn dir(firm: &Firm) -> PathBuf {
    PathBuf::from(expand_home(
        firm.skills_dir.as_deref().unwrap_or(DEFAULT_DIR),
    ))
}

/// Each role's skills: everyone's shared skills, the role's own work skill,
/// the manager skill for roles that hire, and the security skill for
/// officers and the CEO (who holds security until an officer is hired).
pub fn needed(firm: &Firm) -> Vec<(String, String)> {
    let root_role = firm
        .root()
        .and_then(|r| firm.worker(r))
        .map(|w| w.role.clone());
    let mut out = Vec::new();
    for (role_name, role) in &firm.roles {
        let mut names = role.skills.names();
        names.push(RESOURCES);
        let mut add = |n: &'static str| {
            if !names.contains(&n) {
                names.push(n);
            }
        };
        if !role.hires.is_empty() {
            add(MANAGER);
        }
        if role.officer || root_role.as_deref() == Some(role_name.as_str()) {
            add(SECURITY);
        }
        out.extend(
            names
                .into_iter()
                .map(|n| (role_name.clone(), n.to_string())),
        );
    }
    out
}

/// The skills a firm needs that its directory lacks, by name.
pub fn missing(firm: &Firm) -> Vec<String> {
    let dir = dir(firm);
    let mut names: Vec<String> = needed(firm)
        .into_iter()
        .map(|(_, n)| n)
        .filter(|n| !dir.join(n).join("SKILL.md").is_file())
        .collect();
    names.sort();
    names.dedup();
    names
}

/// A skill's `SKILL.md` from `dir`.
pub fn text(dir: &Path, name: &str) -> Result<String> {
    let path = dir.join(name).join("SKILL.md");
    std::fs::read_to_string(&path).with_context(|| {
        format!(
            "skill {name}: reading {} (`zapper init` writes the default skills)",
            path.display()
        )
    })
}

/// Writes the default skills into `dir`, keeping any already there. Returns
/// the names written.
pub fn install(dir: &Path) -> Result<Vec<&'static str>> {
    let mut written = Vec::new();
    for (name, text) in DEFAULTS {
        let path = dir.join(name).join("SKILL.md");
        if path.exists() {
            continue;
        }
        std::fs::create_dir_all(path.parent().unwrap())
            .with_context(|| format!("creating {}", dir.display()))?;
        std::fs::write(&path, text).with_context(|| format!("writing {}", path.display()))?;
        written.push(*name);
    }
    Ok(written)
}

/// Writes `skills/<role>/<skill>/SKILL.md` in the run directory.
pub fn prepare(run: &FirmRun, _config: &Config) -> Result<()> {
    let dir = dir(&run.config);
    for (role_name, skill) in needed(&run.config) {
        let out = run.dir.join("skills").join(&role_name).join(&skill);
        std::fs::create_dir_all(&out)?;
        std::fs::write(
            out.join("SKILL.md"),
            text(&dir, &skill).with_context(|| format!("role {role_name}"))?,
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_have_frontmatter() {
        for (name, text) in DEFAULTS {
            assert!(text.starts_with("---\n"), "{name}");
            assert!(text.contains(&format!("name: {name}\n")), "{name}");
        }
    }

    #[test]
    fn install_keeps_edits_and_skills_come_only_from_the_directory() {
        let tmp = std::env::temp_dir().join(format!("zapper-skills-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        assert!(text(&tmp, "ceo").is_err(), "no fallback to built-in skills");

        assert_eq!(install(&tmp).unwrap().len(), DEFAULTS.len());
        std::fs::write(tmp.join("ceo/SKILL.md"), "edited").unwrap();
        std::fs::remove_file(tmp.join("cto/SKILL.md")).unwrap();
        assert_eq!(install(&tmp).unwrap(), vec!["cto"]);
        assert_eq!(text(&tmp, "ceo").unwrap(), "edited");

        let mut firm = Firm {
            skills_dir: Some(tmp.to_string_lossy().into()),
            ..Default::default()
        };
        firm.roles.insert(
            "boss".into(),
            serde_yaml::from_str("{ skills: { work: ceo } }").unwrap(),
        );
        assert!(missing(&firm).is_empty());
        std::fs::remove_file(tmp.join("office-mail/SKILL.md")).unwrap();
        assert_eq!(missing(&firm), vec!["office-mail"]);
        std::fs::remove_dir_all(&tmp).unwrap();
    }
}
