//! The Office, left out of the build (`--no-default-features`): the names
//! the rest of the daemon uses, with nothing behind them. A session never
//! belongs to a firm and there are no runs, so the types are uninhabited and
//! every function is unreachable; the compiler checks it.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, RwLock};

use anyhow::Result;
use axum::Router;
use tokio::net::UnixListener;
use zapper_policy::{NetRequest, Policy};

use super::session::Session;
use super::Daemon;

/// No value: what a firm would be.
#[derive(Debug, Clone, Copy)]
pub enum Never {}

impl Never {
    fn absurd(&self) -> ! {
        match *self {}
    }
}

#[derive(Default)]
pub struct Office {
    pub runs: RwLock<BTreeMap<String, Arc<FirmRun>>>,
}

pub struct FirmRun {
    pub firm: String,
    pub roles: BTreeMap<String, Arc<RwLock<Policy>>>,
    pub security: Mutex<security::Security>,
    never: Never,
}

#[derive(Clone)]
pub struct FirmLink {
    pub firm: String,
    pub member: String,
    pub worker: bool,
    pub role: Option<(String, Arc<RwLock<Policy>>)>,
    never: Never,
}

impl FirmLink {
    pub fn run(&self) -> Option<Arc<FirmRun>> {
        self.never.absurd()
    }
}

pub async fn down(_d: &Arc<Daemon>, run: &Arc<FirmRun>, _reason: &str) {
    run.never.absurd()
}

pub fn close_orphans(_state: &std::path::Path) {}

pub mod security {
    use super::*;

    #[derive(Default)]
    pub struct Security {
        pub standing: BTreeMap<String, Never>,
    }

    pub fn ask(
        run: &FirmRun,
        _worker: &str,
        _req: &NetRequest,
        _methods: Option<Vec<String>>,
        _path: Option<String>,
        _reason: &str,
    ) -> Result<String> {
        run.never.absurd()
    }

    pub fn revoke(
        run: &FirmRun,
        _by: &str,
        _rule: &str,
        _reason: &str,
    ) -> Result<serde_json::Value> {
        run.never.absurd()
    }

    pub fn used(run: &FirmRun, _rule: &str) {
        run.never.absurd()
    }
}

pub mod facility {
    use super::*;
    use tokio::io::{AsyncRead, AsyncWrite};

    #[derive(Debug, Clone, Copy)]
    pub enum Internal {}

    pub trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
    impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}

    pub fn route(run: &FirmRun, _host: &str, _port: u16) -> Option<Result<Internal>> {
        run.never.absurd()
    }

    pub fn serves_name(run: &FirmRun, _name: &str) -> Option<bool> {
        run.never.absurd()
    }

    pub async fn connect(
        _run: &Arc<FirmRun>,
        _s: &Arc<Session>,
        target: Internal,
    ) -> Result<Box<dyn Io>> {
        match target {}
    }
}

pub mod ops {
    use super::*;

    pub async fn serve(_d: Arc<Daemon>, _s: Arc<Session>, link: FirmLink, _listener: UnixListener) {
        link.never.absurd()
    }
}

pub mod http {
    use super::*;

    pub fn read_routes() -> Router<Arc<Daemon>> {
        Router::new()
    }

    pub fn board_routes() -> Router<Arc<Daemon>> {
        Router::new()
    }
}
