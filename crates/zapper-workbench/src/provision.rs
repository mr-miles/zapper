//! Setting sandboxes up as their profiles once they are made. A template
//! carries a profile's image, resources, environment and labels; the rest
//! (its harness's files, the skills and directories it mounted, its setup
//! commands) is put in each sandbox made from it, by whoever made it (the
//! OpenShell CLI or UI, or `zapper-workbench new`), when it is first ready.
//! `serve` looks for such sandboxes every few seconds; `new` sets up the one
//! it makes itself (labelled as its template is, which `serve` leaves be).
//! What was done is kept in `provisioned.json`, by the gateway's sandbox
//! id, so a sandbox is set up once: a profile's setup commands needn't be
//! safe to run twice.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use zapper_openshell::{GatewaySandbox, OpenShell};

use crate::apply::{self, Applier, TEMPLATE_PREFIX};

/// How often `serve` looks for new sandboxes.
const EVERY: Duration = Duration::from_secs(5);
/// How many times a sandbox's setup is tried before it is left as it is.
const ATTEMPTS: u32 = 3;

/// A sandbox set up, or tried.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Record {
    pub sandbox: String,
    pub profile: String,
    /// The commit its profile was taken from.
    pub commit: String,
    pub at: u64,
    pub attempts: u32,
    /// Why the last attempt failed; none once it is done.
    pub error: Option<String>,
}

impl Record {
    fn settled(&self) -> bool {
        self.error.is_none() || self.attempts >= ATTEMPTS
    }
}

/// The records kept in `path` (`provisioned.json`), by sandbox id.
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

    fn records(&self) -> Result<BTreeMap<String, Record>> {
        records(&self.path)
    }

    fn save(&self, r: &BTreeMap<String, Record>) -> Result<()> {
        let tmp = self.path.with_extension("tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(r)?)?;
        std::fs::rename(tmp, &self.path)?;
        Ok(())
    }

    /// Looks for new sandboxes until the process ends.
    pub async fn run(self: Arc<Self>) {
        loop {
            if let Err(e) = self.pass().await {
                tracing::warn!("looking for sandboxes to set up: {e:#}");
            }
            tokio::time::sleep(EVERY).await;
        }
    }

    /// Sets up every ready sandbox made from a workbench template that isn't
    /// yet; forgets those the gateway no longer has.
    async fn pass(&self) -> Result<()> {
        let all = self.os.gateway_sandboxes().await?;
        let mut records = self.records()?;
        let before = records.len();
        records.retain(|id, _| all.iter().any(|s| &s.id == id));
        let mut changed = records.len() != before;
        for s in &all {
            // Made by `new` (its own labels its template's), which sets it up.
            let by_new = s.labels.contains_key("zapper-workbench");
            if !s.ready || by_new || records.get(&s.id).is_some_and(Record::settled) {
                continue;
            }
            let Some(profile) = self.profile_of(s).await? else {
                continue;
            };
            let attempts = records.get(&s.id).map_or(0, |r| r.attempts);
            let r = self.set_up(s, &profile, attempts).await;
            records.insert(s.id.clone(), r);
            changed = true;
        }
        if changed {
            self.save(&records)?;
        }
        Ok(())
    }

    /// The profile `s`'s template is the workbench's for, if it is one.
    async fn profile_of(&self, s: &GatewaySandbox) -> Result<Option<String>> {
        let Some(t) = s.template.as_deref() else {
            return Ok(None);
        };
        if !t.starts_with(TEMPLATE_PREFIX) {
            return Ok(None);
        }
        let Some(labels) = self.os.template_labels(t).await? else {
            return Ok(None);
        };
        if labels.get("zapper-workbench").map(String::as_str) != Some("1") {
            return Ok(None);
        }
        Ok(labels.get("zapper-profile").cloned())
    }

    /// Sets `s` up as the live commit's `profile`, after `attempts` tries.
    async fn set_up(&self, s: &GatewaySandbox, profile: &str, attempts: u32) -> Record {
        let mut r = Record {
            sandbox: s.name.clone(),
            profile: profile.to_string(),
            commit: String::new(),
            at: zapper_util::now() as u64,
            attempts: attempts + 1,
            error: None,
        };
        let done = async {
            let (commit, p) = self.applier.live_profile(profile)?;
            r.commit = commit;
            let setup = apply::setup(&p)?;
            self.os.provision(&s.name, &setup, None).await
        }
        .await;
        match done {
            Ok(()) => tracing::info!(sandbox = %s.name, profile, "set up"),
            Err(e) => {
                tracing::warn!(sandbox = %s.name, profile, attempt = r.attempts, "setting up: {e:#}");
                r.error = Some(format!("{e:#}"));
            }
        }
        r
    }

    /// Makes sandbox `name` from profile `profile`'s template, with its
    /// labels, and sets it up.
    pub async fn new_sandbox(&self, profile: &str, name: &str) -> Result<Record> {
        let (commit, _) = self.applier.live_profile(profile)?;
        let template = apply::template_name(profile);
        self.os
            .create_from_template(&template, name, &apply::labels(profile, &commit))
            .await?;
        let s = self
            .os
            .gateway_sandboxes()
            .await?
            .into_iter()
            .find(|s| s.name == name)
            .with_context(|| format!("sandbox {name}, just made, isn't listed"))?;
        let r = self.set_up(&s, profile, 0).await;
        let mut records = self.records()?;
        records.insert(s.id.clone(), r.clone());
        self.save(&records)?;
        if let Some(e) = &r.error {
            anyhow::bail!("sandbox {name} was made but not set up: {e}");
        }
        Ok(r)
    }
}
