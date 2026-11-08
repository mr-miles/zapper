//! What openshell-firecracker puts in a sandbox before its workload starts,
//! from its `driver_config`: the size of its writable disk
//! (`overlay_disk_mib`) and files copied in from the host (`files`), each
//! with its owner and mode. The files' sources are paths under the driver's
//! `OPENSHELL_VM_FILES_ROOT` (`files_root` in `openshell.yaml`), so a
//! profile's files and directories are staged there first, one directory
//! per sandbox (the office's) or per template (the workbench's).
//!
//! The gateway passes a sandbox's `driver_config` on only with
//! `allow_driver_config` on, at the gateway and in the driver's
//! `OPENSHELL_DRIVER_ADMISSION_CONFIG_JSON`.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

use crate::Setup;

/// Where a sandbox's files are staged, under the driver's files root.
#[derive(Debug, Clone)]
pub struct Staging {
    root: PathBuf,
}

impl Staging {
    /// Staging under `root`, the driver's `OPENSHELL_VM_FILES_ROOT`.
    pub fn new(root: PathBuf) -> Staging {
        Staging { root }
    }

    /// Stages `setup`'s files and directories in `dir` under the root
    /// (replacing what was there), with `{workdir}` in a mount's host path
    /// as `workdir` (with none, those mounts are left out). The driver's
    /// `files` entries for them.
    pub fn stage(&self, dir: &str, setup: &Setup, workdir: Option<&str>) -> Result<Vec<Value>> {
        let rel = checked(dir)?;
        let fresh = self.root.join(format!("{rel}.new"));
        let _ = std::fs::remove_dir_all(&fresh);
        std::fs::create_dir_all(&fresh)
            .with_context(|| format!("creating {} (the driver's files root)", fresh.display()))?;
        let mut entries = Vec::new();
        for (i, f) in setup.files.iter().enumerate() {
            let name = format!("f{i}");
            std::fs::write(fresh.join(&name), &f.content)
                .with_context(|| format!("staging {}", f.path))?;
            entries.push(json!({
                "source": format!("{rel}/{name}"),
                "target": f.path,
                "owner": if f.user_owned { "sandbox" } else { "root" },
                "mode": format!("{:04o}", f.mode & 0o777),
            }));
        }
        for (i, m) in setup.mounts.iter().enumerate() {
            let host = match workdir {
                Some(w) => m.host.replace("{workdir}", w),
                None if m.host.contains("{workdir}") => continue,
                None => m.host.clone(),
            };
            let host = PathBuf::from(zapper_policy::expand_home(&host));
            if !host.is_dir() {
                bail!(
                    "{}: not a directory (to copy to {})",
                    host.display(),
                    m.guest
                );
            }
            let name = format!("d{i}");
            copy_tree(&host, &fresh.join(&name))
                .with_context(|| format!("staging {} for {}", host.display(), m.guest))?;
            entries.push(json!({ "source": format!("{rel}/{name}"), "target": m.guest }));
        }
        // In place of what was staged before, in one step.
        let live = self.root.join(rel);
        let old = self.root.join(format!("{rel}.old"));
        let _ = std::fs::remove_dir_all(&old);
        if live.exists() {
            std::fs::rename(&live, &old)?;
        }
        std::fs::rename(&fresh, &live)?;
        let _ = std::fs::remove_dir_all(&old);
        Ok(entries)
    }

    /// Removes what was staged in `dir`.
    pub fn remove(&self, dir: &str) {
        if let Ok(rel) = checked(dir) {
            let _ = std::fs::remove_dir_all(self.root.join(rel));
        }
    }
}

/// The sandbox's `driver_config`: the block for driver `driver` (as the
/// gateway names it), with the overlay disk's size and the files.
pub fn driver_config(driver: &str, disk_mib: Option<u64>, files: Vec<Value>) -> Value {
    let mut block = serde_json::Map::new();
    if let Some(mib) = disk_mib {
        block.insert("overlay_disk_mib".into(), json!(mib));
    }
    if !files.is_empty() {
        block.insert("files".into(), Value::Array(files));
    }
    json!({ driver: block })
}

/// Profile `disk` (e.g. `8GiB`) in MiB; none for an empty one.
pub fn disk_mib(disk: &str) -> Result<Option<u64>> {
    if disk.trim().is_empty() {
        return Ok(None);
    }
    let bytes = zapper_policy::parse_size(disk).with_context(|| format!("engine.disk {disk:?}"))?;
    Ok(Some(bytes.div_ceil(1 << 20)))
}

/// A JSON object as a protobuf `Struct`, as the gateway takes a
/// `driver_config`.
pub fn to_struct(v: &Value) -> prost_types::Struct {
    use prost_types::value::Kind;
    fn value(v: &Value) -> prost_types::Value {
        let kind = match v {
            Value::Null => Kind::NullValue(0),
            Value::Bool(b) => Kind::BoolValue(*b),
            Value::Number(n) => Kind::NumberValue(n.as_f64().unwrap_or(0.0)),
            Value::String(s) => Kind::StringValue(s.clone()),
            Value::Array(a) => Kind::ListValue(prost_types::ListValue {
                values: a.iter().map(value).collect(),
            }),
            Value::Object(_) => Kind::StructValue(to_struct(v)),
        };
        prost_types::Value { kind: Some(kind) }
    }
    prost_types::Struct {
        fields: v
            .as_object()
            .map(|o| o.iter().map(|(k, v)| (k.clone(), value(v))).collect())
            .unwrap_or_default(),
    }
}

/// `dir` as a relative path under the root: no `..`, nothing absolute.
fn checked(dir: &str) -> Result<&str> {
    if dir.is_empty()
        || dir.starts_with('/')
        || dir
            .split('/')
            .any(|c| c.is_empty() || c == "." || c == "..")
    {
        bail!("{dir:?} isn't a directory under the files root");
    }
    Ok(dir)
}

/// Copies `from` to `to`, leaving symbolic links out (the driver refuses
/// them).
fn copy_tree(from: &Path, to: &Path) -> Result<()> {
    std::fs::create_dir_all(to)?;
    for e in std::fs::read_dir(from)? {
        let e = e?;
        let t = e.file_type()?;
        if t.is_symlink() {
            continue;
        }
        let dest = to.join(e.file_name());
        if t.is_dir() {
            copy_tree(&e.path(), &dest)?;
        } else if t.is_file() {
            std::fs::copy(e.path(), &dest)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use zapper_policy::proto::GuestFile;
    use zapper_policy::Mount;

    #[test]
    fn a_setup_is_staged_as_the_drivers_files() {
        let root = std::env::temp_dir().join(format!("zapper-files-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let skills = root.join("host-skills");
        std::fs::create_dir_all(skills.join("a")).unwrap();
        std::fs::write(skills.join("a/SKILL.md"), "x").unwrap();
        std::os::unix::fs::symlink("/etc/passwd", skills.join("link")).unwrap();
        let setup = Setup {
            files: vec![
                GuestFile {
                    path: "/home/agent/.claude.json".into(),
                    content: "{}".into(),
                    mode: 0o644,
                    user_owned: true,
                },
                GuestFile {
                    path: "/usr/local/bin/browse".into(),
                    content: "#!".into(),
                    mode: 0o755,
                    user_owned: false,
                },
            ],
            mounts: vec![
                Mount {
                    host: skills.display().to_string(),
                    guest: "/home/agent/.claude/skills".into(),
                    mode: Default::default(),
                    extra: None,
                },
                Mount {
                    host: "{workdir}".into(),
                    guest: "/work".into(),
                    mode: Default::default(),
                    extra: None,
                },
            ],
            ..Default::default()
        };
        let s = Staging::new(root.join("files"));
        let e = s.stage("office/zo-1", &setup, None).unwrap();
        assert_eq!(e.len(), 3, "the workdir mount left out: {e:?}");
        assert_eq!(e[0]["owner"], "sandbox");
        assert_eq!(e[1]["owner"], "root");
        assert_eq!(e[1]["mode"], "0755");
        assert_eq!(e[2]["source"], "office/zo-1/d0");
        let staged = root.join("files/office/zo-1");
        assert_eq!(std::fs::read_to_string(staged.join("f0")).unwrap(), "{}");
        assert!(staged.join("d0/a/SKILL.md").is_file());
        assert!(!staged.join("d0/link").exists(), "no symbolic links");
        // Staged again, it is replaced; removed, it is gone.
        s.stage("office/zo-1", &Setup::default(), None).unwrap();
        assert!(!staged.join("f0").exists());
        s.remove("office/zo-1");
        assert!(!staged.exists());
        assert!(s.stage("../x", &Setup::default(), None).is_err());
        let c = driver_config("firecracker", disk_mib("8GiB").unwrap(), e);
        assert_eq!(c["firecracker"]["overlay_disk_mib"], 8192);
        let st = to_struct(&c);
        assert!(st.fields.contains_key("firecracker"));
        let _ = std::fs::remove_dir_all(root);
    }
}
