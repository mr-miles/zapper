//! The office's side of its edge (`zapper_openshell::edge`), inside the
//! office's sandbox: the warden forwards the workers' supervisors to it
//! unchanged. A caller is the roster worker whose sandbox the warden made
//! under that id; the tools are `tools`, the drive is the workbench's.

use std::sync::Arc;

use anyhow::{anyhow, Result};
use futures_util::future::BoxFuture;
use hyper::body::Incoming;
use hyper::{Request, Response};
use serde_json::Value;
use zapper_openshell::edge::{Backend, Body, Caller, Edge};
use zapper_openshell::EdgeSettings;

use crate::Office;

/// The label a caller's worker is under.
pub const WORKER: &str = "worker";

/// The edge, listening on `listen` in the office's sandbox, checking SVIDs
/// against the driver's keys as the warden passes them on (`jwks`).
pub fn for_office(office: Arc<Office>, listen: std::net::SocketAddr, jwks: String) -> Edge {
    let e = &office.config.edge;
    let settings = EdgeSettings {
        listen,
        url: e.url.clone(),
        token_url: e.token_url.clone(),
        jwks,
        issuer: e.issuer.clone(),
        trust_domain: e.trust_domain.clone(),
    };
    // Signed with a key kept with the office's state: the workers' tokens
    // outlive a restart of the office.
    let key = match zapper_openshell::edge::key_file(&office.dir.join("edge-key")) {
        Ok(k) => k,
        Err(e) => {
            tracing::warn!("the edge's token key: {e:#}; tokens won't outlive this run");
            rand::random()
        }
    };
    Edge::with_key(settings, Arc::new(OfficeBackend(office)), &key)
}

pub struct OfficeBackend(pub Arc<Office>);

impl Backend for OfficeBackend {
    fn name(&self) -> &str {
        crate::tools::SERVER
    }

    fn caller(&self, uuid: &str) -> Option<Caller> {
        let w = self.0.worker_by_id(uuid)?;
        Some(Caller {
            session: w.name.clone(),
            labels: [(WORKER.to_string(), w.name.clone())].into(),
        })
    }

    fn serves_mcp(&self) -> bool {
        true
    }

    fn tools<'a>(
        &'a self,
        caller: &'a Caller,
        op: &'a str,
        args: Value,
    ) -> BoxFuture<'a, Result<Value>> {
        Box::pin(async move {
            let w = caller
                .labels
                .get(WORKER)
                .and_then(|n| self.0.worker(n))
                .ok_or_else(|| anyhow!("you aren't on the roster"))?;
            crate::tools::answer(&self.0, w, op, args).await
        })
    }

    fn instructions(&self) -> Option<String> {
        Some(crate::tools::instructions())
    }

    fn serve<'a>(
        &'a self,
        caller: &'a Caller,
        path: &'a str,
        req: Request<Incoming>,
    ) -> Option<BoxFuture<'a, Response<Body>>> {
        let rest = path.strip_prefix("/drive")?;
        if !(rest.is_empty() || rest.starts_with('/')) {
            return None;
        }
        let who = caller.labels.get(WORKER)?.clone();
        // `/drive/mine/…` is the caller's own outbox: a worker needn't know
        // its name to upload.
        let path = drive_path(&who, rest);
        Some(Box::pin(async move {
            let method = req.method().clone();
            self.0
                .drive
                .serve(&who, &method, &path, req.into_body())
                .await
        }))
    }
}

/// A drive path at the edge (without `/drive`) as the drive takes it:
/// `/mine/…` is caller `who`'s outbox.
pub fn drive_path(who: &str, rest: &str) -> String {
    match rest.strip_prefix("/mine") {
        Some(r) if r.is_empty() || r.starts_with('/') => format!("/outbox/{who}{r}"),
        _ => rest.to_string(),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn mine_is_the_callers_outbox() {
        assert_eq!(super::drive_path("ada", "/mine/x.txt"), "/outbox/ada/x.txt");
        assert_eq!(super::drive_path("ada", "/mine"), "/outbox/ada");
        assert_eq!(super::drive_path("ada", "/mineral/x"), "/mineral/x");
        assert_eq!(super::drive_path("ada", "/inbox/bob/x"), "/inbox/bob/x");
        assert_eq!(
            super::drive_path("ada", "/outbox/bob/x"),
            "/outbox/bob/x",
            "the drive refuses it"
        );
    }
}
