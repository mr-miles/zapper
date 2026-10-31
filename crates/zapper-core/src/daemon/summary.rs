//! Events as people read them: a verdict (`allow`, `deny` or `ask`, when the
//! event decided something) and a line saying what happened, which the API
//! adds to the live stream for `zapper events` and the UI to show as they are.

use serde_json::Value;
use zapper_policy::proto::GuestEvent;

use super::{Event, EventBody};

impl Event {
    /// The event as the live stream carries it: its fields, `summary`, and
    /// `verdict` when it has one.
    pub fn view(&self) -> Value {
        let mut v = serde_json::to_value(self).unwrap_or_default();
        v["summary"] = self.body.summary().into();
        if let Some(verdict) = self.body.verdict() {
            v["verdict"] = verdict.into();
        }
        v
    }
}

fn opt<T>(v: &Option<T>, f: impl Fn(&T) -> String) -> String {
    v.as_ref().map(f).unwrap_or_default()
}

/// `""` for an empty list, else `{prefix}a,b,c{suffix}`.
fn joined(prefix: &str, items: &[String], suffix: &str) -> String {
    if items.is_empty() {
        String::new()
    } else {
        format!("{prefix}{}{suffix}", items.join(","))
    }
}

impl EventBody {
    fn verdict(&self) -> Option<&'static str> {
        let yes = |b: bool| Some(if b { "allow" } else { "deny" });
        match self {
            EventBody::Net { action, .. } | EventBody::Exec { action, .. } => Some(match action {
                zapper_policy::Action::Allow => "allow",
                zapper_policy::Action::Deny => "deny",
                zapper_policy::Action::Ask => "ask",
            }),
            EventBody::Ask { .. } => Some("ask"),
            EventBody::Answer { allow, .. } => yes(*allow),
            EventBody::Dns { allowed, .. } => yes(*allowed),
            EventBody::Mcp { ok: false, .. } => Some("deny"),
            EventBody::Ssh { state, .. } if state == "refused" => Some("deny"),
            EventBody::Disks { state, .. } if state == "changed" => Some("deny"),
            EventBody::Driver { event, detail }
                if event == "error" || detail["outcome"] == "failed" =>
            {
                Some("deny")
            }
            _ => None,
        }
    }

    fn summary(&self) -> String {
        match self {
            EventBody::Net {
                host,
                port,
                method,
                path,
                inspected,
                status,
                secrets,
                error,
                ..
            } => format!(
                "{} {host}:{port}{}{}{}{}{}",
                method.as_deref().unwrap_or("connect"),
                path.as_deref().unwrap_or(""),
                opt(status, |s| format!(" → {s}")),
                joined("  [secrets: ", secrets, "]"),
                opt(error, |e| format!("  {e}")),
                if *inspected { "" } else { "  (tunnel)" },
            ),
            EventBody::Exec { argv, wait_ms, .. } => {
                format!("{}  ({wait_ms:.1} ms)", argv.join(" "))
            }
            EventBody::Ask { id, subject, .. } => format!("{id}: {subject}"),
            EventBody::Answer { id, remember, .. } => {
                format!("{id}{}", opt(remember, |r| format!(" · {r}")))
            }
            EventBody::Dns {
                name,
                rcode,
                answer,
                reason,
                ..
            } => format!(
                "{rcode} {}{}{}",
                if name.is_empty() {
                    "(invalid query)"
                } else {
                    name
                },
                opt(answer, |a| format!(" → {a}")),
                opt(reason, |r| format!("  ({r})")),
            ),
            EventBody::Guest { event } => match event {
                GuestEvent::Exit {
                    pid,
                    comm,
                    exit_code,
                    signal,
                    elapsed_ms,
                    ..
                } => format!(
                    "{comm} pid {pid} exit {} · {elapsed_ms} ms",
                    match signal {
                        Some(s) => format!("signal {s}"),
                        None => exit_code.to_string(),
                    }
                ),
                GuestEvent::Setup { step, ok, .. } => {
                    format!("setup {}: {step}", if *ok { "ok" } else { "FAILED" })
                }
                GuestEvent::ShellExit { id, exit_code } => {
                    format!("shell {id} exited with {exit_code}")
                }
                other => serde_json::to_string(other).unwrap_or_default(),
            },
            EventBody::Shell {
                id,
                state,
                argv,
                exit_code,
                client,
            } => format!(
                "shell {id} {state}{}{}{}",
                opt(client, |c| format!(" by {c}")),
                opt(&argv.as_ref().map(|a| a.join(" ")), |a| format!(": {a}")),
                opt(exit_code, |c| format!(" exit {c}")),
            ),
            EventBody::Session { state, detail } => {
                format!("{state}{}", opt(detail, |d| format!(": {d}")))
            }
            EventBody::Config { change } => change.clone(),
            EventBody::Ssh {
                state,
                client,
                reason,
            } => format!(
                "ssh {state}{}{}",
                opt(client, |c| format!(" for {c}")),
                opt(reason, |r| format!(": {r}")),
            ),
            EventBody::Service { change, service } => format!(
                "{change} {} {}{} → {}{} ({})",
                service.kind,
                service.name,
                opt(&service.port, |p| format!(":{p}")),
                service.to,
                joined(
                    " for ",
                    &service
                        .sessions
                        .iter()
                        .map(|(k, v)| format!("{k}={v}"))
                        .collect::<Vec<_>>(),
                    "",
                ),
                service.owner,
            ),
            EventBody::Driver { event, detail } => {
                let s = |k: &str| detail[k].as_str().map(|v| format!(" {v}"));
                let n = |k: &str, unit: &str| detail[k].as_u64().map(|v| format!(" · {v} {unit}"));
                format!(
                    "{event}{}{}{}{}{}",
                    s("activity").unwrap_or_default(),
                    s("outcome").unwrap_or_default(),
                    n("tokens", "tokens").unwrap_or_default(),
                    n("prompts", "prompts").unwrap_or_default(),
                    detail["error"]
                        .as_str()
                        .map(|e| format!("  {e}"))
                        .unwrap_or_default(),
                )
            }
            EventBody::Disks {
                state,
                disks,
                detail,
            } => format!(
                "disks {state}: {}{}",
                disks
                    .iter()
                    .map(|(name, hash)| format!("{name} {}", &hash[..hash.len().min(12)]))
                    .collect::<Vec<_>>()
                    .join(", "),
                opt(detail, |d| format!("  {d}"))
            ),
            EventBody::Image {
                name,
                build,
                state,
                detail,
            } => format!(
                "{state} image {name} (build {build}){}",
                opt(detail, |d| format!("  {d}"))
            ),
            EventBody::Mcp {
                server,
                tool,
                ms,
                error,
                ..
            } => format!(
                "{server}.{tool} · {ms:.1} ms{}",
                opt(error, |e| format!("  {e}"))
            ),
        }
    }
}
