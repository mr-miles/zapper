//! The Office, left out of the build (`--no-default-features`): the names
//! the rest of the daemon uses, with nothing behind them. A session never
//! belongs to a firm and there are no runs, so the types are uninhabited and
//! every function is unreachable; the compiler checks it.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, RwLock};

use anyhow::Result;
use axum::Router;
use zapper_policy::Policy;

use crate::Core;

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

pub async fn down(_d: &Core, run: &Arc<FirmRun>, _reason: &str) {
    run.never.absurd()
}

pub fn close_orphans(_state: &std::path::Path) {}

pub mod security {
    use super::*;

    #[derive(Default)]
    pub struct Security {
        pub standing: BTreeMap<String, Never>,
    }

    pub fn revoke(
        run: &FirmRun,
        _by: &str,
        _rule: &str,
        _reason: &str,
    ) -> Result<serde_json::Value> {
        run.never.absurd()
    }
}

pub mod http {
    use super::*;

    pub fn read_routes() -> Router<Core> {
        Router::new()
    }

    pub fn board_routes() -> Router<Core> {
        Router::new()
    }
}
