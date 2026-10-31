//! Agent harnesses: the coding agent a session runs. A harness says where
//! and how the agent reads its configuration, and, for a driven session or
//! an office worker, how to hold a conversation with it headless. The rest
//! of zapper only sees these interfaces: another harness (opencode, say) is
//! one more implementation and a name in `agent.kind`
//! (zapper_policy::AGENT_KINDS).
//!
//! - `claude-code`: Claude Code (`claude -p`, stream-json in and out) — `claude`.
//! - `pi`: pi (`pi --mode rpc`, JSON commands in, session events out) — `pi`.
//! - `generic`: any command; zapper writes only `agent.files` and `env`.
//!
//! What harnesses have in common is in the profile's agent spec, in neutral
//! terms (instructions, a skills directory, MCP servers, a turn-end command,
//! model, settings, files and directories for the config directory); each
//! harness writes it in its own place and format. What differs — the config
//! directory, the instructions file, the MCP config's shape, and the protocol
//! — each `Harness`/`Conversation` hides.

mod claude;
mod pi;

use claude::ClaudeCode;
use pi::Pi;

#[cfg(test)]
pub(crate) use claude::MANAGED_SETTINGS;

use std::collections::BTreeMap;

use serde_json::Value;
use zapper_policy::proto::GuestFile;
use zapper_policy::{AgentSpec, Mount, MountMode, ResolvedProfile};

/// The coding agent a session runs.
pub trait Harness: Send + Sync {
    /// Its name in `agent.kind`.
    fn kind(&self) -> &'static str;
    /// Where the agent reads its configuration in the guest; the agent
    /// spec's `config_files` and `config_mounts` are relative to it.
    fn config_dir(&self, home: &str) -> String;
    /// Where it reads the instructions it starts with (`agent.instructions`),
    /// if it reads any: the file is written there, and kept read-only, so
    /// the workload can't change what it was told.
    fn instructions_file(&self, _home: &str) -> Option<String> {
        None
    }
    /// Where it reads skills (`<name>/SKILL.md` directories).
    fn skills_dir(&self, home: &str) -> String {
        format!("{}/skills", self.config_dir(home))
    }
    /// Files and environment the agent needs, from the neutral agent spec:
    /// its settings, instructions, MCP servers, turn-end command. `env`
    /// already holds the profile's variables.
    fn guest(
        &self,
        profile: &ResolvedProfile,
        home: &str,
        env: &mut BTreeMap<String, String>,
        files: &mut Vec<GuestFile>,
    );
    /// Host directories to mount: the skills directory and `config_mounts`,
    /// read-only unless a mount says otherwise.
    fn mounts(&self, agent: &AgentSpec, home: &str) -> Vec<Mount> {
        let mut v = Vec::new();
        if let Some(dir) = &agent.skills {
            v.push(Mount {
                host: dir.clone(),
                guest: self.skills_dir(home),
                mode: MountMode::Ro,
                extra: None,
            });
        }
        for m in &agent.config_mounts {
            v.push(Mount {
                guest: format!(
                    "{}/{}",
                    self.config_dir(home),
                    m.guest.trim_start_matches('/')
                ),
                ..m.clone()
            });
        }
        v
    }
    /// How to hold a conversation with it headless (a driven session, an
    /// office worker), if it can.
    fn headless(&self) -> Option<&dyn Headless>;
}

/// Starts conversations: one long one per driven session or office worker.
pub trait Headless: Send + Sync {
    /// A conversation, continuing `resume` (an id from `Event::Session`).
    fn conversation(&self, resume: Option<&str>) -> Box<dyn Conversation>;
}

/// One running agent process, spoken to in lines on its stdin and heard in
/// JSON lines on its stdout. Stateful: protocols with requests and replies
/// keep their ids and session here.
pub trait Conversation: Send {
    /// The command line.
    fn argv(&self) -> Vec<String>;
    /// The line that gives the agent a prompt (a task, or an office
    /// worker's wake), with its newline.
    fn prompt(&mut self, text: &str) -> Vec<u8>;
    /// What an output line means.
    fn read(&mut self, line: &Value) -> Vec<Event>;
    /// Whether `steer` hands messages over.
    fn steers(&self) -> bool {
        false
    }
    /// Whether an output line goes into the worker's transcript: not
    /// streaming deltas that a later line repeats whole.
    fn transcribe(&self, _line: &Value) -> bool {
        true
    }
    /// A line that hands the agent a message while it works, for harnesses
    /// that take one natively (pi's `follow_up`, opencode's `queue`
    /// delivery); `None` where the turn-end command does it (Claude Code).
    fn steer(&mut self, _text: &str) -> Option<Vec<u8>> {
        None
    }
}

/// Something the driver (or the office's foreman) needs from the agent's
/// output.
#[derive(Debug, PartialEq)]
pub enum Event {
    /// The conversation's id, for resuming it after the process restarts.
    Session(String),
    /// What the agent is doing now: `Edit pricing.md`.
    Activity(String),
    /// A turn ended.
    TurnEnd(TurnEnd),
}

#[derive(Debug, PartialEq)]
pub struct TurnEnd {
    /// Tokens the turn used, against budgets: input, output and cache
    /// writes (cache reads are not counted).
    pub tokens: u64,
    pub outcome: Outcome,
    /// For the journal's `turn end` record: usage and the like.
    pub detail: Value,
}

#[derive(Debug, PartialEq)]
pub enum Outcome {
    Done,
    /// The provider's limits: try the same wake again later.
    RateLimited,
    /// The turn failed before the agent could work (a refused token, an
    /// outage): what it was handed must not be lost.
    Failed(String),
}

/// Sums `keys` from a usage object; a missing key counts as 0. Shared by the
/// harnesses, which differ only in the key spellings.
fn sum_u64(usage: &Value, keys: &[&str]) -> u64 {
    keys.iter().map(|k| usage[*k].as_u64().unwrap_or(0)).sum()
}

/// An error message that means "later": the provider's limits or load. Shared
/// by both harnesses (Claude adds its own is-error guard at the call site).
/// The status codes count only as numbers of their own, not inside others
/// (`line 1429`, `port 4290`).
fn rate_limited(text: &str) -> bool {
    let w = text.to_ascii_lowercase();
    let words = [
        "rate limit",
        "rate_limit",
        "usage limit",
        "overloaded",
        "too many requests",
    ];
    words.iter().any(|k| w.contains(k))
        || w.split(|c: char| !c.is_ascii_digit())
            .any(|n| n == "429" || n == "529")
}

/// `v` as one JSON line, newline included: what the agents read on stdin.
fn json_line(v: &Value) -> Vec<u8> {
    let mut line = serde_json::to_vec(v).expect("JSON");
    line.push(b'\n');
    line
}

/// A file of the agent's configuration, in its user's home, theirs and
/// readable.
fn user_file(path: String, content: String) -> GuestFile {
    GuestFile {
        path,
        content,
        mode: 0o644,
        user_owned: true,
    }
}

/// What the agent is doing, from a tool call's name and input: `Edit
/// pricing.md`.
fn tool_activity(name: &str, input: &Value) -> String {
    let name = zapper_policy::proto::mcp_tool_name(name);
    let arg = [
        "file_path",
        "path",
        "command",
        "pattern",
        "url",
        "subject",
        "query",
        "description",
    ]
    .iter()
    .find_map(|k| input[*k].as_str())
    .map(|s| {
        let s = s.lines().next().unwrap_or("");
        // Show a path's basename, but keep a value with spaces whole — it is a
        // command or prose, not a path.
        let s = s
            .rsplit('/')
            .next()
            .filter(|_| !s.contains(' '))
            .unwrap_or(s);
        s.chars().take(60).collect::<String>()
    })
    .unwrap_or_default();
    format!("{name} {arg}").trim().to_string()
}

/// Pretty-prints JSON for a config file.
fn pretty(v: &Value) -> String {
    serde_json::to_string_pretty(v).unwrap()
}

/// The harness of an `agent.kind`.
/// How to run an agent of `kind` headless (a driven session, an office
/// worker); an error naming the kinds that can be, if this one can't.
pub fn headless(kind: &str) -> anyhow::Result<&'static dyn Headless> {
    get(kind).and_then(|h| h.headless()).ok_or_else(|| {
        anyhow::anyhow!(
            "agent kind {kind} can't be run headless (those that can: {})",
            zapper_policy::DRIVABLE_KINDS.join(", ")
        )
    })
}

pub fn get(kind: &str) -> Option<&'static dyn Harness> {
    match kind {
        "claude-code" => Some(&ClaudeCode),
        "pi" => Some(&Pi),
        "generic" => Some(&Generic),
        _ => None,
    }
}

/// Any command; nothing to set up beyond `agent.files` and `env`.
pub(crate) struct Generic;

impl Harness for Generic {
    fn kind(&self) -> &'static str {
        "generic"
    }
    fn config_dir(&self, home: &str) -> String {
        home.into()
    }
    fn guest(
        &self,
        _: &ResolvedProfile,
        _: &str,
        _: &mut BTreeMap<String, String>,
        _: &mut Vec<GuestFile>,
    ) {
    }
    fn mounts(&self, _: &AgentSpec, _: &str) -> Vec<Mount> {
        Vec::new()
    }
    fn headless(&self) -> Option<&dyn Headless> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_rate_limit_is_said_in_words_or_its_own_status() {
        for t in [
            "API Error: 429 rate_limit_error",
            "HTTP 429",
            "status=529",
            "Overloaded",
            "429 Too Many Requests",
        ] {
            assert!(rate_limited(t), "{t}");
        }
        for t in [
            "error at line 1429",
            "connect to port 4290",
            "id 5291 failed",
        ] {
            assert!(!rate_limited(t), "{t}");
        }
    }

    #[test]
    fn kinds() {
        for k in zapper_policy::AGENT_KINDS {
            assert_eq!(get(k).unwrap().kind(), *k);
        }
        assert!(get("opencode").is_none());
        assert!(get("claude-code").unwrap().headless().is_some());
        assert!(get("pi").unwrap().headless().is_some());
        assert!(
            get("generic").unwrap().headless().is_none(),
            "a generic command can't be driven as a worker"
        );
        // The config's list of what can be driven is these harnesses'.
        let drivable: Vec<&str> = zapper_policy::AGENT_KINDS
            .iter()
            .copied()
            .filter(|k| get(k).unwrap().headless().is_some())
            .collect();
        assert_eq!(drivable, zapper_policy::DRIVABLE_KINDS);
    }
}
