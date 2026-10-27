//! pi (github.com/earendil-works/pi, npm `@earendil-works/pi-coding-agent`):
//! configuration in `~/.pi/agent`, driven over `pi --mode rpc` — a `prompt`
//! command per wake, `follow_up` for mail during a turn; `agent_settled` ends
//! the turn, and the assistant messages before it carry the usage and how
//! they stopped.

use std::collections::BTreeMap;

use serde_json::{json, Value};
use zapper_policy::proto::GuestFile;
use zapper_policy::{merge, ResolvedProfile};

use super::claude::claude_activity;
use super::{Conversation, Driver, Event, Harness, Outcome, TurnEnd};

/// pi: configuration in `~/.pi/agent`, driven over `pi --mode rpc`.
pub struct Pi;

impl Harness for Pi {
    fn kind(&self) -> &'static str {
        "pi"
    }

    fn config_dir(&self, home: &str) -> String {
        format!("{home}/.pi/agent")
    }

    fn guest(
        &self,
        profile: &ResolvedProfile,
        home: &str,
        env: &mut BTreeMap<String, String>,
        files: &mut Vec<GuestFile>,
    ) {
        // No update checks, catalog refreshes or telemetry: the network is
        // for the model.
        for k in ["PI_OFFLINE", "PI_SKIP_VERSION_CHECK"] {
            env.insert(k.into(), "1".into());
        }
        env.insert("PI_TELEMETRY".into(), "0".into());
        files.extend(pi_files(profile, &self.config_dir(home)));
    }

    fn driver(&self) -> Option<&dyn Driver> {
        Some(self)
    }
}

impl Driver for Pi {
    fn conversation(&self, resume: Option<&str>) -> Box<dyn Conversation> {
        // pi opens a session by an id the host picks, or creates it: one id
        // per worker, reported once so a restart resumes it.
        let (session, told) = match resume {
            Some(id) => (id.to_string(), true),
            None => (format!("w-{}", crate::daemon::random_id(12)), false),
        };
        Box::new(PiConversation {
            session,
            told,
            next: 0,
            tokens: 0,
            usage: json!({}),
            messages: 0,
            stop: None,
            error: None,
        })
    }
}

/// `pi --mode rpc`: a `prompt` command per wake, `follow_up` for mail during
/// a turn; `agent_settled` ends the turn, and the assistant messages before
/// it carry the usage and how they stopped.
struct PiConversation {
    session: String,
    /// Whether `Event::Session` was reported.
    told: bool,
    next: u64,
    tokens: u64,
    usage: Value,
    messages: u64,
    stop: Option<String>,
    error: Option<String>,
}

impl PiConversation {
    fn command(&mut self, kind: &str, text: &str) -> Vec<u8> {
        self.next += 1;
        let mut line = serde_json::to_vec(
            &json!({ "id": format!("z-{}", self.next), "type": kind, "message": text }),
        )
        .expect("JSON");
        line.push(b'\n');
        line
    }

    fn end(&mut self, aborted: bool) -> TurnEnd {
        let outcome = match (&self.stop, &self.error) {
            (Some(s), why) if s == "error" => {
                let why = why
                    .clone()
                    .unwrap_or_else(|| "the model request failed".into());
                if rate_limited(&why) {
                    Outcome::RateLimited
                } else {
                    Outcome::Failed(why.chars().take(200).collect())
                }
            }
            _ => Outcome::Done,
        };
        let t = TurnEnd {
            tokens: self.tokens,
            outcome,
            detail: json!({ "usage": self.usage, "messages": self.messages, "stop": self.stop, "aborted": aborted }),
        };
        self.tokens = 0;
        self.usage = json!({});
        self.messages = 0;
        self.stop = None;
        self.error = None;
        t
    }
}

/// An error message that means "later": the provider's limits or load.
fn rate_limited(why: &str) -> bool {
    let w = why.to_ascii_lowercase();
    [
        "rate limit",
        "rate_limit",
        "429",
        "overloaded",
        "529",
        "too many requests",
    ]
    .iter()
    .any(|k| w.contains(k))
}

impl Conversation for PiConversation {
    fn argv(&self) -> Vec<String> {
        ["pi", "--mode", "rpc", "--session-id", &self.session]
            .iter()
            .map(|s| s.to_string())
            .collect()
    }

    fn prompt(&mut self, text: &str) -> Vec<u8> {
        self.command("prompt", text)
    }

    fn steers(&self) -> bool {
        true
    }

    /// `message_update` carries deltas; `message_end` has the message whole.
    fn transcribe(&self, line: &Value) -> bool {
        line["type"] != "message_update"
    }

    /// Delivered when the agent would otherwise stop, like Claude Code's
    /// Stop hook.
    fn steer(&mut self, text: &str) -> Option<Vec<u8>> {
        Some(self.command("follow_up", text))
    }

    fn read(&mut self, v: &Value) -> Vec<Event> {
        let mut out = Vec::new();
        if !self.told {
            self.told = true;
            out.push(Event::Session(self.session.clone()));
        }
        match v["type"].as_str() {
            // What the model is producing now, while it streams: the
            // deltas themselves stay out of the transcript.
            Some("message_update") => {
                let ev = &v["assistantMessageEvent"];
                match ev["type"].as_str() {
                    Some("thinking_start") => out.push(Event::Activity("thinking".into())),
                    Some("text_start") => out.push(Event::Activity("writing".into())),
                    Some("toolcall_start") => {
                        let name = ev["toolName"].as_str().unwrap_or("a tool");
                        let name = zapper_policy::proto::mcp_tool_name(name);
                        out.push(Event::Activity(format!("preparing {name}")));
                    }
                    _ => {}
                }
            }
            Some("tool_execution_start") => {
                let call = json!({ "message": { "content": [{ "type": "tool_use", "name": v["toolName"], "input": v["args"] }] } });
                out.extend(claude_activity(&call).map(Event::Activity));
            }
            Some("message_end") if v["message"]["role"] == "assistant" => {
                let m = &v["message"];
                let u = &m["usage"];
                self.tokens += ["input", "output", "cacheWrite"]
                    .iter()
                    .map(|k| u[*k].as_u64().unwrap_or(0))
                    .sum::<u64>();
                for k in ["input", "output", "cacheRead", "cacheWrite"] {
                    let n = self.usage[k].as_u64().unwrap_or(0) + u[k].as_u64().unwrap_or(0);
                    self.usage[k] = json!(n);
                }
                self.messages += 1;
                self.stop = m["stopReason"].as_str().map(str::to_string);
                self.error = m["errorMessage"].as_str().map(str::to_string);
            }
            Some("auto_retry_end") if v["success"] == json!(false) => {
                self.stop = Some("error".into());
                self.error = v["finalError"].as_str().map(str::to_string);
            }
            // A wake pi refused outright (it never started a run).
            Some("response") if v["command"] == "prompt" && v["success"] == json!(false) => {
                self.stop = Some("error".into());
                self.error = v["error"].as_str().map(str::to_string);
                out.push(Event::TurnEnd(self.end(false)));
            }
            Some("agent_settled") => {
                out.push(Event::TurnEnd(self.end(v["aborted"] == json!(true))));
            }
            _ => {}
        }
        out
    }
}

/// pi's files in its config directory: settings, instructions, MCP servers.
/// Models and providers (`models.json`) are the profile's `config_files`.
fn pi_files(profile: &ResolvedProfile, dir: &str) -> Vec<GuestFile> {
    let agent = &profile.agent;
    // Quiet, and no project trust: a worker loads no `.pi/` from its work.
    let mut settings = json!({ "quietStartup": true, "defaultProjectTrust": "never" });
    if let Some(s) = &agent.settings {
        merge(&mut settings, s);
    }
    if let Some(m) = &agent.model {
        merge(&mut settings, &json!({ "defaultModel": m }));
    }
    let pretty = |v: &Value| serde_json::to_string_pretty(v).unwrap();
    let mut files = vec![GuestFile {
        path: format!("{dir}/settings.json"),
        content: pretty(&settings),
        mode: 0o644,
        user_owned: true,
    }];
    if let Some(md) = &agent.instructions {
        files.push(GuestFile {
            path: format!("{dir}/AGENTS.md"),
            content: md.clone(),
            mode: 0o644,
            user_owned: true,
        });
    }
    // MCP tools are declared to the model directly: pi's default reaches
    // them only from codemode scripts, which the skills don't use.
    if let Some(Value::Object(servers)) = &agent.mcp_servers {
        let mut servers = servers.clone();
        for s in servers.values_mut() {
            if s.get("exposure").is_none() {
                s["exposure"] = json!("direct");
            }
        }
        files.push(GuestFile {
            path: format!("{dir}/mcp.json"),
            content: pretty(&json!({ "mcpServers": servers })),
            mode: 0o644,
            user_owned: true,
        });
    }
    files
}

#[cfg(test)]
mod tests {
    use super::*;
    use zapper_policy::AgentSpec;

    #[test]
    fn pi_setup() {
        let profile = ResolvedProfile {
            name: "p".into(),
            engine: Default::default(),
            agent: AgentSpec {
                kind: "pi".into(),
                instructions: Some("Be brief.".into()),
                skills: Some("/host/skills".into()),
                model: Some("qwen".into()),
                settings: Some(json!({ "defaultProvider": "lab" })),
                mcp_servers: Some(
                    json!({ "office": { "command": "/a", "args": ["office", "mcp"] } }),
                ),
                turn_end: Some("hook".into()),
                ..Default::default()
            },
            mounts: vec![],
            setup: vec![],
            layers: vec![],
        };
        let (mut env, mut files) = (BTreeMap::new(), Vec::new());
        Pi.guest(&profile, "/home/a", &mut env, &mut files);
        let file = |p: &str| {
            files
                .iter()
                .find(|f| f.path == p)
                .map(|f| serde_json::from_str::<Value>(&f.content).unwrap_or(json!(f.content)))
        };
        let s = file("/home/a/.pi/agent/settings.json").unwrap();
        assert_eq!(
            (s["defaultModel"].as_str(), s["defaultProvider"].as_str()),
            (Some("qwen"), Some("lab"))
        );
        assert_eq!(s["defaultProjectTrust"], "never");
        assert_eq!(file("/home/a/.pi/agent/AGENTS.md").unwrap(), "Be brief.");
        let mcp = file("/home/a/.pi/agent/mcp.json").unwrap();
        assert_eq!(
            mcp["mcpServers"]["office"]["exposure"], "direct",
            "the office tools are declared"
        );
        assert_eq!(env["PI_OFFLINE"], "1");
        assert_eq!(
            Pi.mounts(&profile.agent, "/home/a")[0].guest,
            "/home/a/.pi/agent/skills"
        );
    }

    #[test]
    fn pi_output() {
        // Events as pi 1.3 printed them against an OpenAI-compatible endpoint.
        let mut d = Pi.driver().unwrap().conversation(None);
        let argv = d.argv();
        assert_eq!(&argv[..4], ["pi", "--mode", "rpc", "--session-id"]);
        let id = argv[4].clone();
        let first = d.read(&json!({ "id": "z-1", "type": "response", "command": "prompt", "success": true, "data": { "disposition": "started" } }));
        assert_eq!(
            first,
            vec![Event::Session(id.clone())],
            "the id is reported once"
        );
        assert!(d.read(&json!({ "type": "agent_start" })).is_empty());
        // While it streams: thinking, writing, a tool call taking shape.
        let up = |t: &str, extra: Value| {
            let mut e = json!({ "type": t });
            zapper_policy::merge(&mut e, &extra);
            json!({ "type": "message_update", "usage": {}, "assistantMessageEvent": e })
        };
        assert_eq!(
            d.read(&up("thinking_start", json!({ "contentIndex": 0 }))),
            vec![Event::Activity("thinking".into())]
        );
        assert!(d
            .read(&up("thinking_delta", json!({ "delta": "hm" })))
            .is_empty());
        assert_eq!(
            d.read(&up("text_start", json!({}))),
            vec![Event::Activity("writing".into())]
        );
        assert_eq!(
            d.read(&up(
                "toolcall_start",
                json!({ "id": "c", "toolName": "mcp__office__mail_send" })
            )),
            vec![Event::Activity("preparing mail_send".into())]
        );
        let e = json!({ "type": "tool_execution_start", "toolCallId": "c-1", "toolName": "mcp__office__mail_send", "args": { "subject": "Plan" } });
        assert_eq!(d.read(&e), vec![Event::Activity("mail_send Plan".into())]);
        let e = json!({ "type": "tool_execution_start", "toolCallId": "c-2", "toolName": "read", "args": { "path": "notes/note.txt" } });
        assert_eq!(d.read(&e), vec![Event::Activity("read note.txt".into())]);
        let usage = |i: u64, o: u64| json!({ "input": i, "output": o, "cacheRead": 7, "cacheWrite": 1, "reasoning": 48, "totalTokens": i + o });
        d.read(&json!({ "type": "message_end", "message": { "role": "assistant", "stopReason": "toolUse", "usage": usage(893, 75), "content": [] } }));
        d.read(
            &json!({ "type": "message_end", "message": { "role": "toolResult", "content": [] } }),
        );
        d.read(&json!({ "type": "message_end", "message": { "role": "assistant", "stopReason": "stop", "usage": usage(988, 32), "content": [] } }));
        assert!(
            d.read(&json!({ "type": "agent_end", "willRetry": false }))
                .is_empty(),
            "not final"
        );
        let end = |d: &mut Box<dyn Conversation>, v: Value| match d.read(&v).pop() {
            Some(Event::TurnEnd(t)) => t,
            other => panic!("{other:?}"),
        };
        let t = end(&mut d, json!({ "type": "agent_settled", "aborted": false }));
        assert_eq!(
            (t.tokens, t.outcome),
            (893 + 75 + 1 + 988 + 32 + 1, Outcome::Done)
        );
        assert_eq!(t.detail["usage"]["cacheRead"], 14);
        // A refused key: the request fails at once.
        d.read(&json!({ "type": "message_end", "message": { "role": "assistant", "stopReason": "error", "errorMessage": "401: {\"message\":\"token validation failed\"}", "usage": usage(0, 0), "content": [] } }));
        let t = end(&mut d, json!({ "type": "agent_settled", "aborted": false }));
        assert!(
            matches!(t.outcome, Outcome::Failed(ref w) if w.contains("401")),
            "{t:?}"
        );
        // pi's own retries gave up on the provider's limits.
        d.read(&json!({ "type": "auto_retry_end", "success": false, "attempt": 3, "finalError": "429 Too Many Requests" }));
        assert_eq!(
            end(&mut d, json!({ "type": "agent_settled", "aborted": false })).outcome,
            Outcome::RateLimited
        );
        // A prompt pi refused never starts a run: the turn ends there.
        assert!(matches!(end(&mut d, json!({ "type": "response", "command": "prompt", "success": false, "error": "busy" })).outcome, Outcome::Failed(_)));
        let p: Value = serde_json::from_slice(&d.prompt("hello")).unwrap();
        assert_eq!(
            (p["type"].as_str(), p["message"].as_str()),
            (Some("prompt"), Some("hello"))
        );
        assert!(d.steers());
        let f: Value = serde_json::from_slice(&d.steer("mail").unwrap()).unwrap();
        assert_eq!(f["type"], "follow_up");
        assert_ne!(p["id"], f["id"]);
        // Resumed: the same session, not reported again.
        let mut r = Pi.driver().unwrap().conversation(Some(&id));
        assert_eq!(r.argv()[4], id);
        assert!(r.read(&json!({ "type": "agent_start" })).is_empty());
    }
}
