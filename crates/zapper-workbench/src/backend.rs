//! The workbench as an edge's backend (`zapper_openshell::edge`): its
//! sandboxes are the gateway's, each known by the name its SVID's sandbox
//! has there; it serves the drive at `/drive/` and the repository at
//! `/git/`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::future::BoxFuture;
use hyper::body::Incoming;
use hyper::{Request, Response};
use zapper_openshell::edge::{Backend, Body, Caller};
use zapper_openshell::OpenShell;

use crate::drive::Drive;
use crate::repo::Repo;

/// How long a sandbox's name is taken as known, before it is asked again
/// (a sandbox deleted since is no longer served).
const KNOWN_FOR: Duration = Duration::from_secs(60);

pub struct Workbench {
    pub os: Arc<OpenShell>,
    pub drive: Drive,
    pub repo: Repo,
    known: Mutex<HashMap<String, (Caller, Instant)>>,
}

impl Workbench {
    pub fn new(os: Arc<OpenShell>, drive: Drive, repo: Repo) -> Workbench {
        Workbench {
            os,
            drive,
            repo,
            known: Mutex::default(),
        }
    }
}

impl Backend for Workbench {
    fn name(&self) -> &str {
        "workbench"
    }

    fn caller(&self, uuid: &str) -> Option<Caller> {
        if let Some((c, at)) = self.known.lock().unwrap().get(uuid) {
            if at.elapsed() < KNOWN_FOR {
                return Some(c.clone());
            }
        }
        let found = zapper_openshell::client::block(self.os.sandbox_by_id(uuid));
        let caller = match found {
            Ok(Some((session, labels))) => Caller { session, labels },
            Ok(None) => {
                self.known.lock().unwrap().remove(uuid);
                return None;
            }
            Err(e) => {
                tracing::warn!("looking sandbox {uuid} up: {e:#}");
                return None;
            }
        };
        self.known
            .lock()
            .unwrap()
            .insert(uuid.to_string(), (caller.clone(), Instant::now()));
        Some(caller)
    }

    fn git<'a>(
        &'a self,
        caller: &'a Caller,
        req: Request<Incoming>,
    ) -> BoxFuture<'a, Response<Body>> {
        Box::pin(async move { self.repo.serve(&caller.session, req).await })
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
        Some(Box::pin(async move {
            let method = req.method().clone();
            self.drive
                .serve(&caller.session, &method, rest, req.into_body())
                .await
        }))
    }
}
