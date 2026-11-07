//! The images the office's sandboxes run: `zapper-office image NAME` builds
//! the recipe `NAME` of the config's `images:` with the container engine,
//! as `localhost/zapper-openshell-NAME` (the OpenShell client's default for
//! a profile's image).
//!
//! On top of the recipe it adds what OpenShell's microVM driver needs and
//! zapper's guest agent used to set up at boot: the profile's user as uid
//! 1000 (every command runs as it, never root) with its home, `/work` theirs
//! and the working directory, the `browse` command the office would write
//! outside the user's reach, and the static `zapper-agent` as `office` (the
//! workers' CLI, Stop hook and MCP bridge, which reach the office edge).

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{anyhow, bail, Context, Result};

/// The `browse` command, installed in every image.
const BROWSE: &str = include_str!("browse.js");

/// What to build.
pub struct Build {
    /// The recipe's name in the config.
    pub name: String,
    /// `NAME=PATH` for the recipe's arguments.
    pub args: Vec<String>,
    /// The account commands run as (the profiles' `agent.user`).
    pub user: String,
    /// The static guest binary installed as `office`.
    pub agent: PathBuf,
}

/// The container engine: `$ZAPPER_OCI`, else podman.
pub fn engine() -> String {
    std::env::var("ZAPPER_OCI").unwrap_or_else(|_| "podman".into())
}

/// The tag image `name` is built as.
pub fn tag(name: &str) -> String {
    format!("localhost/zapper-openshell-{name}")
}

/// Builds `b` from `config`'s recipe, each line of the engine's output to
/// `log`; the image's tag.
pub fn build(config: &zapper_policy::Config, b: &Build, log: &dyn Fn(&str)) -> Result<String> {
    check_name(&b.name)?;
    check_name(&b.user)?;
    let r = config.images.get(&b.name).ok_or_else(|| {
        let names: Vec<&str> = config.images.keys().map(String::as_str).collect();
        anyhow!(
            "no recipe for image {:?} in the config (images: {})",
            b.name,
            if names.is_empty() {
                "none".into()
            } else {
                names.join(", ")
            }
        )
    })?;
    if !b.agent.is_file() {
        bail!(
            "no {}: build the static guest binary first (make build)",
            b.agent.display()
        );
    }
    let mut given = std::collections::BTreeMap::new();
    for a in &b.args {
        let (k, v) = a
            .split_once('=')
            .ok_or_else(|| anyhow!("--arg {a:?}: NAME=PATH"))?;
        if !r.args.contains_key(k) {
            bail!(
                "image {} takes no argument {k:?} (it takes: {})",
                b.name,
                if r.args.is_empty() {
                    "none".to_string()
                } else {
                    r.args.keys().cloned().collect::<Vec<_>>().join(", ")
                }
            );
        }
        given.insert(k.to_string(), v.to_string());
    }

    let work = tempdir()?;
    let mut file = format!("FROM {}\n", r.from);
    for step in &r.run {
        file.push_str(&format!("RUN {step}\n"));
    }
    // Each argument into the build's context under a name of its own, then
    // into the image where the recipe says, modes kept.
    let mut index = std::collections::BTreeMap::new();
    for (i, (k, arg)) in r.args.iter().enumerate() {
        let path = match (given.get(k), &arg.default) {
            (Some(v), _) => PathBuf::from(v),
            (None, Some(d)) => PathBuf::from(zapper_policy::expand_home(d)),
            (None, None) => bail!("image {} needs --arg {k}=PATH: {}", b.name, arg.description),
        };
        if !path.is_file() && !path.is_dir() {
            bail!(
                "image {}: {k} is {}, which isn't a file or a directory",
                b.name,
                path.display()
            );
        }
        copy_into(&path, &work.0.join(format!("input-{i}")))?;
        index.insert(k.clone(), i);
    }
    for c in &r.copy {
        let i = index.get(&c.from).ok_or_else(|| {
            anyhow!(
                "image {}: copy from {:?}, which isn't one of its args",
                b.name,
                c.from
            )
        })?;
        file.push_str(&format!("COPY input-{i} {}\n", c.to));
    }
    for step in &r.after {
        file.push_str(&format!("RUN {step}\n"));
    }
    std::fs::write(work.0.join("browse"), BROWSE)?;
    std::fs::copy(&b.agent, work.0.join("zapper-agent"))
        .with_context(|| format!("reading {}", b.agent.display()))?;
    file.push_str(&openshell_layer(&b.user));

    let tag = tag(&b.name);
    std::fs::write(work.0.join("Containerfile"), &file)?;
    log(&format!("building {tag} from {}", r.from));
    let oci = engine();
    run_logged(
        Command::new(&oci)
            .args(["build", "-t", &tag, "-f"])
            .arg(work.0.join("Containerfile"))
            .arg(&work.0),
        log,
    )
    .with_context(|| format!("{oci} build of {tag}"))?;
    Ok(tag)
}

/// What every image gets on top of its recipe, for `user`.
fn openshell_layer(user: &str) -> String {
    format!(
        "# Ubuntu's images have an `ubuntu` account at uid 1000: the user takes its place.\n\
         RUN old=$(getent passwd 1000 | cut -d: -f1); if [ -n \"$old\" ] && [ \"$old\" != \"{user}\" ]; then userdel -r \"$old\"; fi; \\\n    \
         id -u {user} >/dev/null 2>&1 || useradd -m -u 1000 -U -s /bin/bash {user}; \\\n    \
         mkdir -p /work && chown {user}:{user} /work\n\
         COPY --chmod=755 browse /usr/local/bin/browse\n\
         COPY --chmod=755 zapper-agent /usr/local/bin/zapper-agent\n\
         RUN ln -sf zapper-agent /usr/local/bin/office\n\
         ENV HOME=/home/{user}\n\
         WORKDIR /work\n"
    )
}

/// A name that is safe in a tag and a shell: letters, digits, `-`, `_` and
/// `.`, starting with a letter or digit.
fn check_name(name: &str) -> Result<()> {
    let starts_well = name.starts_with(|c: char| c.is_ascii_alphanumeric());
    if !starts_well
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
    {
        bail!("{name:?}: use letters, digits, '-', '_' and '.', starting with a letter or digit");
    }
    Ok(())
}

/// Copies a file, or a directory and all it holds, modes kept.
fn copy_into(src: &Path, dest: &Path) -> Result<()> {
    if src.is_dir() {
        std::fs::create_dir_all(dest)?;
        for e in std::fs::read_dir(src)? {
            let e = e?;
            copy_into(&e.path(), &dest.join(e.file_name()))?;
        }
        Ok(())
    } else {
        std::fs::copy(src, dest)
            .map(|_| ())
            .with_context(|| format!("reading {}", src.display()))
    }
}

/// Runs `cmd`, each line of its output (stdout and stderr) to `log` as it
/// comes; an error if it fails.
fn run_logged(cmd: &mut Command, log: &dyn Fn(&str)) -> Result<()> {
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("running {cmd:?}"))?;
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    let readers: Vec<_> = [
        child
            .stdout
            .take()
            .map(|o| Box::new(o) as Box<dyn std::io::Read + Send>),
        child
            .stderr
            .take()
            .map(|e| Box::new(e) as Box<dyn std::io::Read + Send>),
    ]
    .into_iter()
    .flatten()
    .map(|r| {
        let tx = tx.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(r).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        })
    })
    .collect();
    drop(tx);
    for line in rx {
        log(&line);
    }
    for r in readers {
        let _ = r.join();
    }
    let status = child.wait()?;
    if !status.success() {
        bail!("{:?} failed ({status})", cmd.get_program());
    }
    Ok(())
}

/// A directory removed when dropped.
struct TempDir(PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn tempdir() -> Result<TempDir> {
    let dir = std::env::temp_dir().join(format!(
        "zapper-office-image-{}-{}",
        std::process::id(),
        zapper_util::random_id(8)
    ));
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    Ok(TempDir(dir))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_image_gets_the_user_the_office_command_and_its_workdir() {
        let l = openshell_layer("agent");
        assert!(
            l.contains("useradd -m -u 1000 -U -s /bin/bash agent"),
            "{l}"
        );
        assert!(
            l.contains("ln -sf zapper-agent /usr/local/bin/office"),
            "{l}"
        );
        assert!(l.trim_end().ends_with("WORKDIR /work"), "{l}");
    }

    #[test]
    fn names_are_safe_in_a_tag_and_a_shell() {
        assert!(check_name("office-fake").is_ok());
        for bad in ["", "-x", "a b", "a;b", "../x", "a$b"] {
            assert!(check_name(bad).is_err(), "{bad}");
        }
        assert_eq!(tag("office"), "localhost/zapper-openshell-office");
    }

    #[test]
    fn a_name_with_no_recipe_says_which_there_are() {
        let c = zapper_policy::Config::from_yaml(
            "images:\n  office:\n    from: docker.io/library/ubuntu:26.04\n",
        )
        .unwrap();
        let b = Build {
            name: "nope".into(),
            args: vec![],
            user: "agent".into(),
            agent: "/nonexistent".into(),
        };
        let e = build(&c, &b, &|_| {}).unwrap_err().to_string();
        assert!(e.contains("images: office"), "{e}");
    }
}
