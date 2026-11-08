//! An in-memory gateway for the warden's tests: sandboxes in a list, and
//! every exec's and forward's far end kept for the test to drive.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;

use anyhow::{bail, Result};
use futures_util::future::BoxFuture;
use tokio::io::DuplexStream;
use zapper_driver::process::DriveStream;
use zapper_openshell::{GatewaySandbox, Labels, Setup};

use crate::gateway::Gateway;

/// An exec the warden asked for.
#[derive(Debug, Clone)]
pub struct Exec {
    pub sandbox: String,
    pub argv: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub workdir: String,
}

#[derive(Default)]
pub struct Fake {
    pub sandboxes: Mutex<Vec<GatewaySandbox>>,
    /// `create <template> <name> <providers>`, `setup <name>`, `proposals
    /// <name>`, `delete <name>`, in order.
    pub calls: Mutex<Vec<String>>,
    pub execs: Mutex<Vec<Exec>>,
    /// `(sandbox, port)` of each forward.
    pub forwards: Mutex<Vec<(String, u16)>>,
    pub exec_ends: Mutex<Vec<DuplexStream>>,
    pub forward_ends: Mutex<Vec<DuplexStream>>,
    /// Setup fails while this is set.
    pub fail_setup: AtomicBool,
    next: AtomicU64,
}

impl Fake {
    pub fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }

    fn call(&self, c: String) {
        self.calls.lock().unwrap().push(c);
    }

    /// Adds a sandbox someone else made.
    pub fn add(&self, name: &str, template: Option<&str>, labels: Labels) {
        let n = self.next.fetch_add(1, Ordering::SeqCst);
        self.sandboxes.lock().unwrap().push(GatewaySandbox {
            id: format!("other-{n}"),
            name: name.into(),
            labels,
            template: template.map(String::from),
            ready: true,
        });
    }

    fn stream(ends: &Mutex<Vec<DuplexStream>>) -> Box<dyn DriveStream> {
        let (ours, theirs) = tokio::io::duplex(64 << 10);
        ends.lock().unwrap().push(theirs);
        Box::new(ours)
    }
}

impl Gateway for Fake {
    fn create<'a>(
        &'a self,
        template: &'a str,
        name: &'a str,
        labels: &'a Labels,
        providers: &'a [String],
    ) -> BoxFuture<'a, Result<String>> {
        Box::pin(async move {
            self.call(format!("create {template} {name} {}", providers.join(",")));
            let mut all = self.sandboxes.lock().unwrap();
            if all.iter().any(|s| s.name == name) {
                bail!("sandbox {name} exists");
            }
            let id = format!("uuid-{}", self.next.fetch_add(1, Ordering::SeqCst));
            all.push(GatewaySandbox {
                id: id.clone(),
                name: name.into(),
                labels: labels.clone(),
                template: Some(template.into()),
                ready: true,
            });
            Ok(id)
        })
    }

    fn delete<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.call(format!("delete {name}"));
            self.sandboxes.lock().unwrap().retain(|s| s.name != name);
            Ok(())
        })
    }

    fn sandboxes(&self) -> BoxFuture<'_, Result<Vec<GatewaySandbox>>> {
        Box::pin(async move { Ok(self.sandboxes.lock().unwrap().clone()) })
    }

    fn run_setup<'a>(&'a self, name: &'a str, _setup: &'a Setup) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.call(format!("setup {name}"));
            if self.fail_setup.load(Ordering::SeqCst) {
                bail!("setup failed");
            }
            Ok(())
        })
    }

    fn proposals_on<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.call(format!("proposals {name}"));
            Ok(())
        })
    }

    fn exec<'a>(
        &'a self,
        name: &'a str,
        argv: Vec<String>,
        env: BTreeMap<String, String>,
        workdir: String,
    ) -> BoxFuture<'a, Result<Box<dyn DriveStream>>> {
        Box::pin(async move {
            self.execs.lock().unwrap().push(Exec {
                sandbox: name.into(),
                argv,
                env,
                workdir,
            });
            Ok(Fake::stream(&self.exec_ends))
        })
    }

    fn forward<'a>(
        &'a self,
        name: &'a str,
        port: u16,
    ) -> BoxFuture<'a, Result<Box<dyn DriveStream>>> {
        Box::pin(async move {
            self.forwards.lock().unwrap().push((name.into(), port));
            Ok(Fake::stream(&self.forward_ends))
        })
    }

    fn jwks(&self) -> BoxFuture<'_, Result<Vec<u8>>> {
        Box::pin(async { Ok(br#"{"keys":[{"kid":"k1","kty":"EC"}]}"#.to_vec()) })
    }
}
