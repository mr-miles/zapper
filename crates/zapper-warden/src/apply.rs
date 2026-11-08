//! Making the warden's templates (`zapper-warden apply`): the office's and
//! each worker profile's, from the prepared profiles, their files staged
//! under the driver's files root for it to copy into each sandbox made from
//! them before its workload starts, as the workbench makes its own. A
//! template made again replaces the one there; sandboxes already made keep
//! what they were made from.

use anyhow::Result;
use zapper_openshell::{OpenShell, Template};

use crate::profiles::Profiles;

/// Where the warden stages its templates' files, under the files root.
const STAGED: &str = "zapperclaw";

/// Makes every template; their names.
pub async fn apply(os: &OpenShell, profiles: &Profiles, log: &dyn Fn(&str)) -> Result<Vec<String>> {
    let mut made = Vec::new();
    for p in profiles.all() {
        let staged = format!("{STAGED}/{}", p.template);
        let driver = os.driver_config(&p.profile, &p.setup, &staged, None)?;
        let t = Template {
            name: p.template.clone(),
            image: os.image_of(&p.profile),
            vcpus: p.profile.engine.vcpus,
            memory_mib: p.profile.engine.memory_mib,
            labels: p.labels.clone(),
            env: p.setup.env.clone(),
            driver_config: Some(driver),
        };
        os.put_template(&t).await?;
        log(&format!(
            "template {} (profile {}, image {})",
            t.name, p.profile.name, t.image
        ));
        made.push(t.name);
    }
    Ok(made)
}
