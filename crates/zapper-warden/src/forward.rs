//! The warden's two forwards into the office sandbox: each connection
//! accepted on a host address is piped, as bytes, to a port on the office
//! sandbox's loopback (`ForwardTcp`). The warden reads none of it: the
//! office checks who is calling (the workers' edge checks their tokens; the
//! board needs the office's own token).

use std::sync::Arc;
use std::time::Duration;

use tokio::net::TcpListener;

use crate::gateway::Gateway;
use crate::names;

/// Pipes every connection on `listener` to port `port` of the office
/// sandbox, until the task is dropped. `what` names it in the logs.
pub async fn serve(listener: TcpListener, gw: Arc<dyn Gateway>, port: u16, what: &'static str) {
    loop {
        let (mut conn, from) = match listener.accept().await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("warden: {what}: accepting: {e}");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        let gw = gw.clone();
        tokio::spawn(async move {
            let mut office = match gw.forward(names::OFFICE, port).await {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!("warden: {what}: forwarding {from} to the office: {e:#}");
                    return;
                }
            };
            if let Err(e) = tokio::io::copy_bidirectional(&mut conn, &mut office).await {
                tracing::debug!("warden: {what}: {from}: {e}");
            }
        });
    }
}
