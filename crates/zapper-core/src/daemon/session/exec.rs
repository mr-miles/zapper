//! Whether a program the guest runs may run: policy, the operator's
//! approval, and the counters and event of it.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Instant;

use zapper_policy::{evaluate_exec, Action, AnyRule, ExecRequest, ExecRule, Kind};

use super::Session;
use crate::daemon::{Daemon, EventBody};

pub(super) async fn decide_exec(
    daemon: &Arc<Daemon>,
    session: &Arc<Session>,
    pid: i32,
    uid: u32,
    req: ExecRequest,
) -> bool {
    let started = Instant::now();
    let decision = {
        let c = daemon.config.read().unwrap();
        let layers = session.layers(&c);
        evaluate_exec(&Session::layers_of(&layers), &req)
    };
    let allow = match decision.action {
        Action::Allow => true,
        Action::Deny => false,
        Action::Ask => {
            let suggestion = AnyRule::Exec(ExecRule {
                id: None,
                action: Action::Allow,
                path: req.path.clone(),
                args: None,
                final_: false,
                comment: Some("approved".into()),
            });
            daemon
                .ask(
                    session,
                    Kind::Exec,
                    format!("exec {}", req.argv.join(" ")),
                    suggestion,
                )
                .await
        }
    };
    let counter = if allow {
        &session.counters.exec_allowed
    } else {
        &session.counters.exec_denied
    };
    counter.fetch_add(1, Ordering::Relaxed);
    daemon.emit(
        Some(session),
        EventBody::Exec {
            pid,
            uid,
            path: req.path,
            argv: req.argv,
            cwd: req.cwd,
            action: if allow { Action::Allow } else { Action::Deny },
            rule: decision.rule,
            scope: decision.scope.map(|s| s.to_string()),
            wait_ms: started.elapsed().as_secs_f64() * 1000.0,
        },
    );
    allow
}
