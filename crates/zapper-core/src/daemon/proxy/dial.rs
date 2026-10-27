//! Where a connection goes, and dialing it: a service the daemon connects
//! itself by name, or an address resolved on the host and checked against the
//! address policy.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio::net::TcpStream;

use crate::daemon::session::Session;
use crate::daemon::{services, Daemon};

/// Where a connection goes: an address on the network, or a service the
/// daemon connects itself, by the name asked for.
#[derive(Clone)]
pub(super) enum Target {
    Addr(SocketAddr),
    Service(String, services::Target),
}

/// The target for `host:port`: a service by name, anything else resolved on
/// the host and checked against the address policy.
pub(super) async fn target(
    d: &Daemon,
    s: &Session,
    host: &str,
    port: u16,
) -> Result<Target, String> {
    match d.services.route(s, host, port) {
        Some(services::Route::Service(t)) => Ok(Target::Service(host.to_string(), t)),
        Some(services::Route::Unserved) => Err(format!("nothing serves {host}:{port}")),
        None => resolve(d, host, port).await.map(Target::Addr),
    }
}

pub(super) async fn dial_target(
    d: &Daemon,
    s: &Arc<Session>,
    t: &Target,
) -> std::io::Result<Box<dyn services::Io>> {
    match t {
        Target::Addr(a) => Ok(Box::new(dial(*a).await?)),
        Target::Service(host, t) => services::connect(d, s, host, t)
            .await
            .map_err(|e| std::io::Error::other(format!("{e:#}"))),
    }
}

/// Resolves on the host and returns the first address sessions may reach.
async fn resolve(d: &Daemon, host: &str, port: u16) -> Result<SocketAddr, String> {
    let addrs: Vec<SocketAddr> = match tokio::time::timeout(
        Duration::from_secs(10),
        tokio::net::lookup_host((host, port)),
    )
    .await
    {
        Ok(Ok(a)) => a.collect(),
        Ok(Err(e)) => return Err(format!("resolving {host}: {e}")),
        Err(_) => return Err(format!("resolving {host}: timed out")),
    };
    let policy = d.addrs.read().unwrap();
    let mut why = "no addresses";
    for a in &addrs {
        match policy.check(a.ip()) {
            Ok(()) => return Ok(*a),
            Err(e) => why = e,
        }
    }
    Err(format!("{host} resolves to a forbidden address: {why}"))
}

pub(super) async fn dial(addr: SocketAddr) -> std::io::Result<TcpStream> {
    match tokio::time::timeout(Duration::from_secs(15), TcpStream::connect(addr)).await {
        Ok(r) => r,
        Err(_) => Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "connect timed out",
        )),
    }
}
