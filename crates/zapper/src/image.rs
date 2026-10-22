//! Root filesystem images from OCI images.
//!
//! An image is an ext4 file made from a container image's filesystem. With
//! podman the extraction runs inside its user namespace (`podman unshare`),
//! so file ownership comes out as in the image without root. Other engines
//! (docker) have no such thing: a helper container with e2fsprogs extracts
//! the filesystem as root and writes the ext4 file. The guest agent is not
//! part of the image: it boots from an initramfs the daemon builds.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use serde_json::json;

use zapper_core::daemon::vm::parse_size;

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

/// A local image with tar and mkfs.ext4, built once.
fn mkfs_image(oci: &str) -> Result<String> {
    let tag = "localhost/zapper-mkfs:1";
    if run(Command::new(oci).args(["image", "inspect", tag])).is_ok() {
        return Ok(tag.into());
    }
    let work = tempdir()?;
    std::fs::write(
        work.join("Containerfile"),
        "FROM docker.io/library/ubuntu:26.04\nRUN apt-get update && apt-get install -y --no-install-recommends e2fsprogs tar && rm -rf /var/lib/apt/lists/*\n",
    )?;
    eprintln!("building {tag}");
    let status = Command::new(oci)
        .args(["build", "-t", tag, "-f"])
        .arg(work.join("Containerfile"))
        .arg(&work)
        .status()?;
    let _ = std::fs::remove_dir_all(&work);
    if !status.success() {
        bail!("{oci} build of {tag} failed");
    }
    Ok(tag.into())
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

pub struct Build<'a> {
    pub name: &'a str,
    pub from: &'a str,
    /// Shell commands run in the image at build time, as `RUN` steps.
    pub run: &'a [String],
    pub size: Option<&'a str>,
    pub dir: &'a Path,
}

pub fn build(b: Build<'_>) -> Result<PathBuf> {
    if !b
        .name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
    {
        bail!(
            "image name {:?}: use letters, digits, '-', '_' and '.'",
            b.name
        );
    }
    std::fs::create_dir_all(b.dir)?;
    let oci = engine();
    let work = tempdir()?;
    let reference = if b.run.is_empty() {
        eprintln!("pulling {}", b.from);
        run(Command::new(&oci).args(["pull", "-q", b.from]))?;
        b.from.to_string()
    } else {
        let mut file = format!("FROM {}\n", b.from);
        for r in b.run {
            file.push_str(&format!("RUN {r}\n"));
        }
        std::fs::write(work.join("Containerfile"), file)?;
        let tag = format!("localhost/zapper-image-{}", b.name);
        eprintln!("building {tag} from {}", b.from);
        let status = Command::new(&oci)
            .args(["build", "-t", &tag, "-f"])
            .arg(work.join("Containerfile"))
            .arg(&work)
            .status()?;
        if !status.success() {
            bail!("{oci} build failed");
        }
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
    let root = work.join("root");
    eprintln!("writing {} ({} MiB)", out.display(), size >> 20);
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
        let helper = mkfs_image(&oci)?;
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
    std::fs::rename(&tmp, &out)?;
    let meta = json!({ "name": b.name, "from": b.from, "run": b.run, "size": size, "built": crate::daemon::now() as u64 });
    std::fs::write(
        b.dir.join(format!("{}.json", b.name)),
        serde_json::to_vec_pretty(&meta)?,
    )?;
    let _ = std::fs::remove_dir_all(&work);
    Ok(out)
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
            out.push(json!({ "name": name, "from": meta["from"], "size": e.metadata()?.len(), "built": meta["built"] }));
        }
    }
    out.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    Ok(out)
}

fn tempdir() -> Result<PathBuf> {
    let base = std::env::var("TMPDIR").unwrap_or_else(|_| "/var/tmp".into());
    let dir = PathBuf::from(base).join(format!("zapper-image-{}", crate::daemon::random_id(8)));
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}
