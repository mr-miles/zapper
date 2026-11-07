//! The office's side of its edge (`zapper_openshell::edge`): its workers'
//! sandboxes, the office's tools at `/mcp`, and the firm's git server at
//! `/git/`.

use std::sync::Arc;

use anyhow::Result;
use futures_util::future::BoxFuture;
use http_body_util::BodyExt;
use hyper::body::Incoming;
use hyper::{Request, Response, StatusCode};
use serde_json::Value;
use zapper_openshell::edge::{plain, Backend, Body, Caller, Edge};
use zapper_openshell::OpenShell;
use zapper_policy::facility::{McpCall, McpMethod};

/// The edge for the office's sandboxes on `os`.
pub fn for_office(office: Arc<crate::Office>, os: Arc<OpenShell>) -> Edge {
    let settings = os.edge_settings().clone();
    Edge::new(settings, Arc::new(OfficeBackend { office, os }))
}

/// The office as the edge's backend: callers are the OpenShell client's
/// sandboxes, the tools are `ops`, git is the firm's git server.
struct OfficeBackend {
    office: Arc<crate::Office>,
    os: Arc<OpenShell>,
}

impl OfficeBackend {
    /// The firm's current run `caller` works in.
    fn run(&self, caller: &Caller) -> Option<Arc<crate::FirmRun>> {
        let firm = caller.labels.get("firm")?;
        let run = self.office.run(firm).ok()?;
        (caller.labels.get("run") == Some(&run.id)).then_some(run)
    }
}

impl Backend for OfficeBackend {
    fn name(&self) -> &str {
        crate::ops::SERVER
    }

    fn serves_mcp(&self) -> bool {
        true
    }

    fn caller(&self, uuid: &str) -> Option<Caller> {
        self.os
            .caller(uuid)
            .map(|(session, labels)| Caller { session, labels })
    }

    fn tools<'a>(
        &'a self,
        caller: &'a Caller,
        op: &'a str,
        args: Value,
    ) -> BoxFuture<'a, Result<Value>> {
        Box::pin(async move {
            let list = op == "tools";
            let call = McpCall {
                session: caller.session.clone(),
                labels: caller.labels.clone(),
                server: crate::ops::SERVER.into(),
                method: if list {
                    McpMethod::ToolsList
                } else {
                    McpMethod::ToolsCall
                },
                tool: (!list).then(|| op.to_string()),
                arguments: (!list).then(|| args.clone()),
            };
            crate::ops::answer_verified(&self.office, &call, op, args).await
        })
    }

    fn instructions(&self) -> Option<String> {
        crate::ops::instructions()
    }

    fn git<'a>(
        &'a self,
        caller: &'a Caller,
        req: Request<Incoming>,
    ) -> BoxFuture<'a, Response<Body>> {
        Box::pin(async move {
            let Some(run) = self.run(caller) else {
                return plain(StatusCode::NOT_FOUND, "your firm isn't running");
            };
            if !run.config.git_internal() {
                return plain(StatusCode::NOT_FOUND, "the firm has no git server");
            }
            let member = caller.labels.get("member").cloned().unwrap_or_default();
            crate::facility::git_request_as(&run, &member, true, req)
                .await
                .map(|b| b.map_err(|n| match n {}).boxed())
        })
    }
}
