//! Making a sandbox from a profile's template and setting it up as the
//! profile (`zapper-workbench new`). The template carries the profile's
//! image, resources, environment, labels, disk and files (which the driver
//! copies in before the workload starts); its setup commands run once the
//! gateway's stream of its status says it is ready. What
//! was done is kept in `provisioned.json`, by sandbox name, for `status`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use zapper_openshell::OpenShell;

use crate::apply::{self, Applier};

/// A sandbox made and set up, or that failed to be.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Record {
    pub sandbox: String,
    pub profile: String,
    /// The commit its profile was taken from.
    pub commit: String,
    pub at: u64,
    /// Why setting it up failed; none if it didn't.
    pub error: Option<String>,
}

/// The records kept in `path` (`provisioned.json`), by sandbox name.
pub fn records(path: &Path) -> Result<BTreeMap<String, Record>> {
    match std::fs::read(path) {
        Ok(b) => serde_json::from_slice(&b).context("provisioned.json"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
        Err(e) => Err(e.into()),
    }
}

pub struct Provisioner {
    os: Arc<OpenShell>,
    applier: Arc<Applier>,
    path: PathBuf,
}

impl Provisioner {
    pub fn new(os: Arc<OpenShell>, applier: Arc<Applier>, path: PathBuf) -> Provisioner {
        Provisioner { os, applier, path }
    }

    fn save(&self, r: &BTreeMap<String, Record>) -> Result<()> {
        let tmp = self.path.with_extension("tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(r)?)?;
        std::fs::rename(tmp, &self.path)?;
        Ok(())
    }

    /// Makes sandbox `name` from the live commit's profile `profile`'s
    /// template, labelled as the template is, waits for it to be ready and
    /// sets it up.
    pub async fn new_sandbox(&self, profile: &str, name: &str) -> Result<Record> {
        let (commit, p) = self.applier.live_profile(profile)?;
        self.os
            .create_from_template(
                &apply::template_name(profile),
                name,
                &apply::labels(profile, &commit),
            )
            .await?;
        let done = async {
            let setup = apply::setup(&p)?;
            self.os.run_setup(name, &setup).await
        }
        .await;
        let r = Record {
            sandbox: name.to_string(),
            profile: profile.to_string(),
            commit,
            at: zapper_util::now() as u64,
            error: done.as_ref().err().map(|e| format!("{e:#}")),
        };
        let mut all = records(&self.path)?;
        all.insert(name.to_string(), r.clone());
        self.save(&all)?;
        if let Some(e) = &r.error {
            anyhow::bail!("sandbox {name} was made but not set up: {e}");
        }
        Ok(r)
    }
}
