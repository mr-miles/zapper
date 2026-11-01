//! What clients built into the process (the office) register in the core:
//! their own state, checks of the config, the rules they keep, and what to
//! do when the daemon stops.

use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};

use anyhow::Result;
use futures_util::future::BoxFuture;
use zapper_policy::Config;

/// A check every config change must pass, besides the config's own.
pub type ConfigCheck = Arc<dyn Fn(&Config) -> Result<()> + Send + Sync>;

/// Run when the daemon stops, before its sessions are stopped.
pub type ShutdownHook = Arc<dyn Fn(crate::Core) -> BoxFuture<'static, ()> + Send + Sync>;

#[derive(Default)]
pub struct Clients {
    state: Mutex<HashMap<TypeId, Arc<dyn Any + Send + Sync>>>,
    pub config_checks: RwLock<Vec<ConfigCheck>>,
    pub shutdown: RwLock<Vec<ShutdownHook>>,
}

impl Clients {
    /// The client state of type `T`, made on first use.
    pub fn state<T: Default + Send + Sync + 'static>(&self) -> Arc<T> {
        self.state
            .lock()
            .unwrap()
            .entry(TypeId::of::<T>())
            .or_insert_with(|| Arc::new(T::default()))
            .clone()
            .downcast()
            .expect("client state keyed by its type")
    }

    pub fn check(&self, c: &Config) -> Result<()> {
        for check in self.config_checks.read().unwrap().iter() {
            check(c)?;
        }
        Ok(())
    }
}
