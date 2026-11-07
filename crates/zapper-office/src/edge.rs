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
    fn serve<'a>(
        &'a self,
        caller: &'a Caller,
        path: &'a str,
        req: Request<Incoming>,
    ) -> Option<BoxFuture<'a, Response<Body>>> {
        let (api, rest) = if let Some(r) = path.strip_prefix("/cluster") {
            (true, r)
        } else {
            (false, path.strip_prefix("/apps")?)
        };
        if !(rest.is_empty() || rest.starts_with('/')) {
            return None;
        }
        Some(Box::pin(async move {
            let Some(run) = self.run(caller) else {
                return plain(StatusCode::NOT_FOUND, "your firm isn't running");
            };
            if api {
                let member = caller.labels.get("member").cloned().unwrap_or_default();
                cluster::api(&run, &member, rest, req).await
            } else {
                cluster::apps(&run, rest, req).await
            }
        }))
    }
}

/// The firm's cluster as the edge serves it to workers: its API at
/// `/cluster/` (each worker as their own service account, whose token the
/// office keeps and adds here), and its ingress at `/apps/` (as
/// `apps.<domain>`). Both reach the cluster's sandbox over the gateway.
pub(crate) mod cluster {
    use std::sync::Arc;

    use base64::Engine as _;
    use http_body_util::BodyExt;
    use hyper::body::Incoming;
    use hyper::{header, Request, Response, StatusCode};
    use hyper_util::rt::TokioIo;
    use zapper_openshell::edge::{plain, Body};

    use crate::facility::{API_PORT, HTTP_PORT};
    use crate::FirmRun;

    /// Headers that are the connection's, not the request's.
    const HOP: [&str; 7] = [
        "connection",
        "keep-alive",
        "proxy-authorization",
        "proxy-connection",
        "te",
        "trailer",
        "transfer-encoding",
    ];

    /// `req` (its path under `/cluster`) to the cluster's API as `member`.
    pub async fn api(
        run: &Arc<FirmRun>,
        member: &str,
        rest: &str,
        mut req: Request<Incoming>,
    ) -> Response<Body> {
        let token = run
            .facility
            .cluster_tokens
            .lock()
            .unwrap()
            .get(member)
            .cloned();
        let Some(token) = token else {
            return plain(
                StatusCode::FORBIDDEN,
                "you hold no grant on the cluster: ask with credentials_request(resource: cluster, level, reason)",
            );
        };
        if req.headers().contains_key(header::UPGRADE) {
            return plain(
                StatusCode::NOT_IMPLEMENTED,
                "exec, attach and port-forward aren't served through the office edge",
            );
        }
        let Some(cluster) = run.facility.cluster.lock().unwrap().clone() else {
            return plain(StatusCode::SERVICE_UNAVAILABLE, "the cluster isn't running");
        };
        let Some(ca) = ca_of(run) else {
            return plain(
                StatusCode::SERVICE_UNAVAILABLE,
                "the cluster isn't ready yet",
            );
        };
        if let Err(why) = retarget(&mut req, rest, "127.0.0.1:6443") {
            return plain(StatusCode::BAD_REQUEST, why);
        }
        let headers = req.headers_mut();
        headers.remove(header::AUTHORIZATION);
        match format!("Bearer {token}").parse() {
            Ok(v) => {
                headers.insert(header::AUTHORIZATION, v);
            }
            Err(_) => return plain(StatusCode::INTERNAL_SERVER_ERROR, "a bad token"),
        }
        let stream = match run.client.connect_port(cluster.id(), API_PORT).await {
            Ok(s) => s,
            Err(e) => {
                return plain(
                    StatusCode::BAD_GATEWAY,
                    &format!("reaching the cluster: {e:#}"),
                )
            }
        };
        let tls = match tls(&ca, stream).await {
            Ok(t) => t,
            Err(e) => {
                return plain(
                    StatusCode::BAD_GATEWAY,
                    &format!("the cluster's TLS: {e:#}"),
                )
            }
        };
        send(TokioIo::new(tls), req).await
    }

    /// `req` (its path under `/apps`) to the cluster's ingress, as a request
    /// for `apps.<domain>`.
    pub async fn apps(
        run: &Arc<FirmRun>,
        rest: &str,
        mut req: Request<Incoming>,
    ) -> Response<Body> {
        let Some(cluster) = run.facility.cluster.lock().unwrap().clone() else {
            return plain(StatusCode::SERVICE_UNAVAILABLE, "the cluster isn't running");
        };
        if let Err(why) = retarget(&mut req, rest, &format!("apps.{}", run.domain)) {
            return plain(StatusCode::BAD_REQUEST, why);
        }
        req.headers_mut().remove(header::AUTHORIZATION);
        let stream = match run.client.connect_port(cluster.id(), HTTP_PORT).await {
            Ok(s) => s,
            Err(e) => {
                return plain(
                    StatusCode::BAD_GATEWAY,
                    &format!("reaching the cluster: {e:#}"),
                )
            }
        };
        send(TokioIo::new(stream), req).await
    }

    /// Points `req` at `rest` (`/` if empty, its query kept) on `host`, the
    /// connection's own headers dropped.
    fn retarget(req: &mut Request<Incoming>, rest: &str, host: &str) -> Result<(), &'static str> {
        let path = if rest.is_empty() { "/" } else { rest };
        let uri = match req.uri().query() {
            Some(q) => format!("{path}?{q}"),
            None => path.to_string(),
        };
        *req.uri_mut() = uri.parse().map_err(|_| "bad path")?;
        let headers = req.headers_mut();
        for h in HOP {
            headers.remove(h);
        }
        headers.insert(header::HOST, host.parse().map_err(|_| "bad host")?);
        Ok(())
    }

    /// Sends `req` over `io` (HTTP/1.1) and streams the answer back.
    async fn send<I>(io: I, req: Request<Incoming>) -> Response<Body>
    where
        I: hyper::rt::Read + hyper::rt::Write + Unpin + Send + 'static,
    {
        let (mut sender, conn) = match hyper::client::conn::http1::handshake(io).await {
            Ok(c) => c,
            Err(e) => return plain(StatusCode::BAD_GATEWAY, &format!("the cluster: {e}")),
        };
        tokio::spawn(async move {
            let _ = conn.await;
        });
        match sender.send_request(req).await {
            Ok(resp) => resp.map(|b| b.map_err(std::io::Error::other).boxed()),
            Err(e) => plain(StatusCode::BAD_GATEWAY, &format!("the cluster: {e}")),
        }
    }

    /// The cluster's CA certificate (DER), from k3s's kubeconfig.
    fn ca_of(run: &FirmRun) -> Option<Vec<u8>> {
        let kube = run.facility.kubeconfig.lock().unwrap().clone()?;
        let b64 = kube.lines().find_map(|l| {
            l.trim()
                .strip_prefix("certificate-authority-data:")
                .map(|v| v.trim().to_string())
        })?;
        let pem = base64::engine::general_purpose::STANDARD.decode(b64).ok()?;
        let pem = String::from_utf8(pem).ok()?;
        let body: String = pem.lines().filter(|l| !l.starts_with("-----")).collect();
        base64::engine::general_purpose::STANDARD.decode(body).ok()
    }

    /// TLS to k3s's API over `stream`, trusting only the cluster's CA; its
    /// certificate names 127.0.0.1, the address it listens on in its sandbox.
    async fn tls(
        ca: &[u8],
        stream: Box<dyn zapper_driver::process::DriveStream>,
    ) -> anyhow::Result<tokio_rustls::client::TlsStream<Box<dyn zapper_driver::process::DriveStream>>>
    {
        let mut roots = rustls::RootCertStore::empty();
        roots.add(rustls::pki_types::CertificateDer::from(ca.to_vec()))?;
        let config = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()?
        .with_root_certificates(roots)
        .with_no_client_auth();
        let name = rustls::pki_types::ServerName::IpAddress(std::net::Ipv4Addr::LOCALHOST.into());
        Ok(tokio_rustls::TlsConnector::from(Arc::new(config))
            .connect(name, stream)
            .await?)
    }
}
