//! What the warden asks of the gateway, behind a trait: [`OpenShell`] in
//! production, an in-memory fake in the tests. Each method is one thing the
//! warden does; nothing here decides anything.

use std::collections::BTreeMap;

use anyhow::Result;
use futures_util::future::BoxFuture;
use zapper_driver::process::DriveStream;
use zapper_openshell::{GatewaySandbox, Labels, OpenShell, Setup};

pub trait Gateway: Send + Sync {
    /// Makes sandbox `name` from template `template` with `labels` and
    /// `providers`, waits for it to be ready, and answers its id.
    fn create<'a>(
        &'a self,
        template: &'a str,
        name: &'a str,
        labels: &'a Labels,
        providers: &'a [String],
    ) -> BoxFuture<'a, Result<String>>;
    /// Deletes sandbox `name`; none there is no error.
    fn delete<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Result<()>>;
    /// The workspace's sandboxes, of any maker.
    fn sandboxes(&self) -> BoxFuture<'_, Result<Vec<GatewaySandbox>>>;
    /// Runs `setup`'s commands in sandbox `name`.
    fn run_setup<'a>(&'a self, name: &'a str, setup: &'a Setup) -> BoxFuture<'a, Result<()>>;
    /// Lets sandbox `name` propose rules (`policy.local`), for a person to
    /// decide at the gateway.
    fn proposals_on<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Result<()>>;
    /// Runs `argv` in sandbox `name`: its frame stream.
    fn exec<'a>(
        &'a self,
        name: &'a str,
        argv: Vec<String>,
        env: BTreeMap<String, String>,
        workdir: String,
    ) -> BoxFuture<'a, Result<Box<dyn DriveStream>>>;
    /// A byte stream to port `port` on sandbox `name`'s loopback.
    fn forward<'a>(&'a self, name: &'a str, port: u16)
        -> BoxFuture<'a, Result<Box<dyn DriveStream>>>;
    /// The driver's JWKS, as it serves it.
    fn jwks(&self) -> BoxFuture<'_, Result<Vec<u8>>>;
}

impl Gateway for OpenShell {
    fn create<'a>(
        &'a self,
        template: &'a str,
        name: &'a str,
        labels: &'a Labels,
        providers: &'a [String],
    ) -> BoxFuture<'a, Result<String>> {
        Box::pin(self.create_from_template_with(template, name, labels, providers))
    }

    fn delete<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(self.delete_named(name))
    }

    fn sandboxes(&self) -> BoxFuture<'_, Result<Vec<GatewaySandbox>>> {
        Box::pin(self.gateway_sandboxes())
    }

    fn run_setup<'a>(&'a self, name: &'a str, setup: &'a Setup) -> BoxFuture<'a, Result<()>> {
        Box::pin(OpenShell::run_setup(self, name, setup))
    }

    fn proposals_on<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(self.advisor_on(name))
    }

    fn exec<'a>(
        &'a self,
        name: &'a str,
        argv: Vec<String>,
        env: BTreeMap<String, String>,
        workdir: String,
    ) -> BoxFuture<'a, Result<Box<dyn DriveStream>>> {
        Box::pin(self.exec_stream(name, argv, env, workdir))
    }

    fn forward<'a>(
        &'a self,
        name: &'a str,
        port: u16,
    ) -> BoxFuture<'a, Result<Box<dyn DriveStream>>> {
        Box::pin(OpenShell::forward(self, name, port.into()))
    }

    fn jwks(&self) -> BoxFuture<'_, Result<Vec<u8>>> {
        Box::pin(zapper_openshell::edge::http_get(&self.edge_settings().jwks))
    }
}
