//! Root filesystem images from OCI images.
//!
//! An image is an ext4 file made from a container image's filesystem. With
//! podman the extraction runs inside its user namespace (`podman unshare`),
//! so file ownership comes out as in the image without root. Other engines
//! (docker) have no such thing: a helper container with e2fsprogs extracts
//! the filesystem as root and writes the ext4 file. The guest agent is not
//! part of the image: it boots from an initramfs the daemon builds.
//!
//! A build reports what it does, the container engine's output among it, a
//! line at a time to the log it is given: the terminal for the CLI, the
//! build's record in the daemon (`daemon::builds`).

use std::io::{BufRead, BufReader};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{anyhow, bail, Context, Result};
use serde_json::json;

use zapper_policy::parse_size;

/// The container engine: `$ZAPPER_OCI`, else podman, else docker.
pub fn engine() -> String {
    if let Ok(e) = std::env::var("ZAPPER_OCI") {
        return e;
    }
    let has = |e: &str| {
        Command::new(e)
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
    };
    if !has("podman") && has("docker") {
        "docker".into()
    } else {
        "podman".into()
    }
}

/// Whether the engine can extract in a user namespace (`unshare`).
fn has_unshare(engine: &str) -> bool {
    Path::new(engine).file_name().is_some_and(|n| n == "podman")
}

/// Where a build's lines go.
pub type Log<'a> = &'a dyn Fn(&str);

/// A local image with tar and mkfs.ext4, built once.
fn mkfs_image(oci: &str, log: Log) -> Result<String> {
    let tag = "localhost/zapper-mkfs:1";
    if run(Command::new(oci).args(["image", "inspect", tag])).is_ok() {
        return Ok(tag.into());
    }
    let work = tempdir()?;
    std::fs::write(
        work.join("Containerfile"),
        "FROM docker.io/library/ubuntu:26.04\nRUN apt-get update && apt-get install -y --no-install-recommends e2fsprogs tar && rm -rf /var/lib/apt/lists/*\n",
    )?;
    log(&format!("building {tag}"));
    let built = run_logged(
        Command::new(oci)
            .args(["build", "-t", tag, "-f"])
            .arg(work.join("Containerfile"))
            .arg(&*work),
        log,
    );
    built.with_context(|| format!("{oci} build of {tag}"))?;
    Ok(tag.into())
}

/// Runs `cmd`, each line of its output (stdout and stderr) to `log` as it
/// comes; an error if it fails.
fn run_logged(cmd: &mut Command, log: Log) -> Result<()> {
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

fn run(cmd: &mut Command) -> Result<String> {
    let out = cmd.output().with_context(|| format!("running {cmd:?}"))?;
    if !out.status.success() {
        bail!(
            "{cmd:?} failed:\n{}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// What an image is built from: its base, its steps, the host files (or
/// directories) its arguments name, where each is copied, and the steps run
/// once they are in.
pub struct Recipe {
    pub from: String,
    pub run: Vec<String>,
    /// The arguments' files, each once.
    pub inputs: Vec<PathBuf>,
    /// In order: which input, and where in the image.
    pub copies: Vec<(usize, String)>,
    pub after: Vec<String>,
}

/// What to build for `name`: its recipe in the config, with the files its
/// arguments name (`--arg NAME=PATH`, else the arg's default); or, with
/// `from`, the base and steps given on the command line, which take no
/// arguments.
pub fn recipe(
    config: &zapper_policy::Config,
    name: &str,
    from: Option<String>,
    run: Vec<String>,
    args: &[String],
) -> Result<Recipe> {
    check_name(name)?;
    if let Some(from) = from {
        if !args.is_empty() {
            bail!("--arg is for an image's recipe; with --from there is none");
        }
        return Ok(Recipe {
            from,
            run,
            inputs: vec![],
            copies: vec![],
            after: vec![],
        });
    }
    if !run.is_empty() {
        bail!("--run goes with --from; a recipe has its own steps");
    }
    let r = config.images.get(name).ok_or_else(|| {
        let names: Vec<&str> = config.images.keys().map(String::as_str).collect();
        anyhow!(
            "no recipe for image {name:?} in the config (images: {}); or give --from and --run",
            if names.is_empty() {
                "none".into()
            } else {
                names.join(", ")
            }
        )
    })?;
    let mut given: std::collections::BTreeMap<&str, &str> = Default::default();
    for a in args {
        let (k, v) = a
            .split_once('=')
            .ok_or_else(|| anyhow!("--arg {a:?}: NAME=PATH"))?;
        if !r.args.contains_key(k) {
            bail!(
                "image {name} takes no argument {k:?} (it takes: {})",
                if r.args.is_empty() {
                    "none".to_string()
                } else {
                    r.args.keys().cloned().collect::<Vec<_>>().join(", ")
                }
            );
        }
        given.insert(k, v);
    }
    let mut inputs: Vec<PathBuf> = Vec::new();
    let mut index: std::collections::BTreeMap<&str, usize> = Default::default();
    for (k, arg) in &r.args {
        let path = match (given.get(k.as_str()), &arg.default) {
            (Some(v), _) => PathBuf::from(v),
            (None, Some(d)) => PathBuf::from(zapper_policy::expand_home(d)),
            (None, None) => bail!("image {name} needs --arg {k}=PATH: {}", arg.description),
        };
        if !path.is_file() && !path.is_dir() {
            bail!(
                "image {name}: {k} is {}, which isn't a file or a directory{}",
                path.display(),
                if arg.description.is_empty() {
                    String::new()
                } else {
                    format!(" ({})", arg.description)
                }
            );
        }
        index.insert(k, inputs.len());
        inputs.push(path);
    }
    let copies = r
        .copy
        .iter()
        .map(|c| {
            index
                .get(c.from.as_str())
                .map(|&i| (i, c.to.clone()))
                .ok_or_else(|| {
                    anyhow!(
                        "image {name}: copy from {:?}, which isn't one of its args",
                        c.from
                    )
                })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(Recipe {
        from: r.from.clone(),
        run: r.run.clone(),
        inputs,
        copies,
        after: r.after.clone(),
    })
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

pub struct Build<'a> {
    pub name: &'a str,
    pub from: &'a str,
    /// Shell commands run in the image at build time, as `RUN` steps.
    pub run: &'a [String],
    /// Host files and directories the copies take from.
    pub inputs: &'a [PathBuf],
    /// After `run`, in order, as `COPY` steps (so a changed input rebuilds
    /// only from there): which input, and where in the image.
    pub copies: &'a [(usize, String)],
    /// Shell commands run once the copies are in.
    pub after: &'a [String],
    pub size: Option<&'a str>,
    pub dir: &'a Path,
}

/// An image's name: a file name in the images directory, never hidden (a
/// build's own files are) or a path out of it.
pub fn check_name(name: &str) -> Result<()> {
    let starts_well = name.starts_with(|c: char| c.is_ascii_alphanumeric());
    if !starts_well
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
    {
        bail!(
            "image name {name:?}: use letters, digits, '-', '_' and '.', starting with a letter or digit"
        );
    }
    Ok(())
}

pub fn build(b: Build<'_>, log: Log) -> Result<PathBuf> {
    check_name(b.name)?;
    std::fs::create_dir_all(b.dir)?;
    let _lock = lock(b.dir, b.name)?;
    let oci = engine();
    let work = tempdir()?;
    let reference = if b.run.is_empty() && b.copies.is_empty() && b.after.is_empty() {
        log(&format!("pulling {}", b.from));
        run(Command::new(&oci).args(["pull", "-q", b.from]))?;
        b.from.to_string()
    } else {
        let mut file = format!("FROM {}\n", b.from);
        for r in b.run {
            file.push_str(&format!("RUN {r}\n"));
        }
        // Each input into the build's context under a name of its own, then
        // into the image where the copies say, modes kept (a directory's
        // contents go into the directory named).
        for (i, src) in b.inputs.iter().enumerate() {
            copy_into(src, &work.join(format!("input-{i}")))?;
        }
        for (i, to) in b.copies {
            file.push_str(&format!("COPY input-{i} {to}\n"));
        }
        for r in b.after {
            file.push_str(&format!("RUN {r}\n"));
        }
        std::fs::write(work.join("Containerfile"), file)?;
        let tag = format!("localhost/zapper-image-{}", b.name);
        log(&format!("building {tag} from {}", b.from));
        run_logged(
            Command::new(&oci)
                .args(["build", "-t", &tag, "-f"])
                .arg(work.join("Containerfile"))
                .arg(&*work),
            log,
        )
        .with_context(|| format!("{oci} build"))?;
        tag
    };
    let cid = run(Command::new(&oci).args(["create", &reference, "/bin/true"]))?;
    let tar = work.join("rootfs.tar");
    let exported = run(Command::new(&oci).args(["export", &cid, "-o"]).arg(&tar));
    let _ = run(Command::new(&oci).args(["rm", "-f", &cid]));
    exported?;

    let tar_size = std::fs::metadata(&tar)?.len();
    let size = match b.size {
        Some(s) => parse_size(s)?,
        None => tar_size + tar_size / 4 + (256 << 20),
    };
    let out = b.dir.join(format!("{}.ext4", b.name));
    let tmp = b.dir.join(format!(".{}.ext4.tmp", b.name));
    // Gone however the build ends: renamed into place, or left half-written.
    let _partial = Removed(tmp.clone());
    let root = work.join("root");
    log(&format!("writing {} ({} MiB)", out.display(), size >> 20));
    let kib = format!("{}k", size / 1024);
    if has_unshare(&oci) {
        let script = r#"set -e; mkdir "$1"; tar -C "$1" -xf "$2"; mkfs.ext4 -q -F -L zapper -d "$1" "$3" "$4"; rm -rf "$1""#;
        let mut unshare = Command::new(&oci);
        unshare
            .args(["unshare", "sh", "-c", script, "sh"])
            .arg(&root)
            .arg(&tar)
            .arg(&tmp)
            .arg(&kib);
        run(&mut unshare)?;
    } else {
        // As root in a container, which also owns the output until chown.
        let helper = mkfs_image(&oci, log)?;
        let out_dir = std::fs::canonicalize(b.dir)?;
        let tmp_name = tmp.file_name().unwrap().to_string_lossy().into_owned();
        let script = r#"set -e; mkdir /r; tar -C /r -xpf /w/rootfs.tar; mkfs.ext4 -q -F -L zapper -d /r "/out/$1" "$2"; chown "$3" "/out/$1""#;
        let owner = format!("{}:{}", unsafe { libc::getuid() }, unsafe {
            libc::getgid()
        });
        let mut docker = Command::new(&oci);
        docker
            .args(["run", "--rm", "-v"])
            .arg(format!("{}:/w:ro", work.display()))
            .arg("-v")
            .arg(format!("{}:/out", out_dir.display()))
            .arg(&helper)
            .args(["sh", "-c", script, "sh", &tmp_name, &kib, &owner]);
        run(&mut docker)?;
    }
    // What the image is, as built: checked when a persistent session is
    // made from it, so an image changed since shows. Written before the
    // image is put in place, so an image is never there with an older
    // image's hash: a build that ends between the two leaves the old image
    // with the new hash, which is refused, not trusted.
    let sha256 = crate::util::sha256_file(&tmp)?;
    log(&format!("sha256 {sha256}"));
    let meta = json!({ "name": b.name, "sha256": sha256, "from": b.from, "run": b.run, "copy": b.copies.iter().map(|(i, to)| json!({ "from": b.inputs[*i], "to": to })).collect::<Vec<_>>(), "after": b.after, "size": size, "built": crate::util::now() as u64 });
    let json = b.dir.join(format!("{}.json", b.name));
    let json_tmp = b.dir.join(format!(".{}.json.tmp", b.name));
    std::fs::write(&json_tmp, serde_json::to_vec_pretty(&meta)?)?;
    std::fs::rename(&json_tmp, &json)?;
    std::fs::rename(&tmp, &out)?;
    Ok(out)
}

/// Image `name` in `dir`, held for one build at a time, by this daemon or
/// a `zapper image build --local` beside it: they would write the same
/// files. Released when dropped.
fn lock(dir: &Path, name: &str) -> Result<std::fs::File> {
    let f = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join(format!(".{name}.lock")))?;
    if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        bail!("image {name} is being built by another zapper");
    }
    Ok(f)
}

/// A path removed when dropped (a directory with what it holds), if it is
/// still there.
struct Removed(PathBuf);

impl std::ops::Deref for Removed {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.0
    }
}

impl Drop for Removed {
    fn drop(&mut self) {
        if self.0.is_dir() {
            let _ = std::fs::remove_dir_all(&self.0);
        } else {
            let _ = std::fs::remove_file(&self.0);
        }
    }
}

pub fn list(dir: &Path) -> Result<Vec<serde_json::Value>> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Ok(out);
    };
    for e in entries.flatten() {
        let path = e.path();
        if path.extension().is_some_and(|x| x == "ext4") {
            let name = path.file_stem().unwrap().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue;
            }
            let meta: serde_json::Value = std::fs::read(path.with_extension("json"))
                .ok()
                .and_then(|b| serde_json::from_slice(&b).ok())
                .unwrap_or(json!({}));
            out.push(json!({ "name": name, "from": meta["from"], "size": e.metadata()?.len(), "built": meta["built"], "sha256": meta["sha256"] }));
        }
    }
    out.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    Ok(out)
}

/// The SHA-256 recorded when the image `name` in `dir` was built; `None`
/// for one built before images were hashed.
pub fn built_hash(dir: &Path, name: &str) -> Option<String> {
    let meta: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir.join(format!("{name}.json"))).ok()?).ok()?;
    meta["sha256"].as_str().map(str::to_string)
}

/// A build's work directory, removed with what it holds however the build
/// ends.
fn tempdir() -> Result<Removed> {
    let base = std::env::var("TMPDIR").unwrap_or_else(|_| "/var/tmp".into());
    let dir = PathBuf::from(base).join(format!("zapper-image-{}", crate::util::random_id(8)));
    std::fs::create_dir_all(&dir)?;
    Ok(Removed(dir))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("zapper-images-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    /// What a build is asked for is checked before anything runs: a name
    /// out of the directory, `--run` without `--from`, `--arg` with it, an
    /// argument the recipe doesn't take, one it needs left out.
    #[test]
    fn a_recipe_is_checked_before_a_build() {
        let config: zapper_policy::Config = serde_yaml::from_str(
            "images:\n  web:\n    from: docker.io/library/ubuntu:26.04\n    args:\n      src: { description: the sources }\n    copy:\n      - { from: src, to: /src }\n",
        )
        .unwrap();
        let err = |name: &str, from: Option<&str>, run: &[&str], args: &[&str]| {
            recipe(
                &config,
                name,
                from.map(str::to_string),
                run.iter().map(|s| s.to_string()).collect(),
                &args.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
            )
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default()
        };
        assert!(err("../web", None, &[], &[]).contains("image name"));
        assert!(err("web", None, &["true"], &[]).contains("--run goes with --from"));
        assert!(
            err("x", Some("docker.io/library/ubuntu:26.04"), &[], &["a=b"])
                .contains("--arg is for")
        );
        assert!(err("web", None, &[], &["other=/tmp"]).contains("takes no argument \"other\""));
        assert!(err("web", None, &[], &["src"]).contains("NAME=PATH"));
        assert!(err("web", None, &[], &[]).contains("needs --arg src=PATH"));
        assert!(err("nope", None, &[], &[]).contains("no recipe"));
        let src = std::env::temp_dir().to_string_lossy().into_owned();
        assert_eq!(err("web", None, &[], &[&format!("src={src}")]), "");
    }

    #[test]
    fn a_work_directory_goes_with_what_it_holds() {
        let work = tempdir().unwrap();
        let path = work.to_path_buf();
        std::fs::create_dir_all(work.join("root/etc")).unwrap();
        std::fs::write(work.join("rootfs.tar"), b"x").unwrap();
        drop(work);
        assert!(!path.exists());
    }

    #[test]
    fn one_build_of_an_image_at_a_time() {
        let d = dir("lock");
        std::fs::create_dir_all(&d).unwrap();
        let held = lock(&d, "web").unwrap();
        let e = lock(&d, "web").unwrap_err();
        assert!(e.to_string().contains("being built by another"), "{e}");
        lock(&d, "other").expect("another image's is free");
        drop(held);
        lock(&d, "web").expect("free once the first ends");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn an_image_name_cant_leave_its_directory() {
        let d = dir("names");
        for name in ["../x", "a/b", "", "..", ".hidden", "x y"] {
            let e = build(
                Build {
                    name,
                    from: "docker.io/library/ubuntu:26.04",
                    run: &[],
                    inputs: &[],
                    copies: &[],
                    after: &[],
                    size: None,
                    dir: &d,
                },
                &|_| {},
            )
            .unwrap_err();
            assert!(e.to_string().contains("image name"), "{name:?}: {e}");
        }
        assert!(!d.exists(), "refused before anything is written");
    }
}
