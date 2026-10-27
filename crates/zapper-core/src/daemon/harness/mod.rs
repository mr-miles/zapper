//! Agent harnesses: the coding agent a session runs. A harness says where
//! and how the agent reads its configuration, and, for office workers, how
//! to hold a conversation with it headless. The rest of zapper only sees
//! these interfaces: another harness (pi, opencode) is one more
//! implementation and a name in `agent.kind` (zapper_policy::AGENT_KINDS).
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

pub use claude::ClaudeCode;
pub use pi::Pi;

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
    /// How to hold a conversation with it as an office worker, if it can.
    fn driver(&self) -> Option<&dyn Driver>;
}

/// Starts conversations: one long one per office worker.
pub trait Driver: Send + Sync {
    /// A conversation, continuing `resume` (an id from `Event::Session`).
    fn conversation(&self, resume: Option<&str>) -> Box<dyn Conversation>;
}

/// One running agent process, spoken to in lines on its stdin and heard in
/// JSON lines on its stdout. Stateful: protocols with requests and replies
/// keep their ids and session here.
pub trait Conversation: Send {
    /// The command line.
    fn argv(&self) -> Vec<String>;
    /// Lines to send first (a protocol's handshake); none for Claude Code.
    fn opening(&mut self) -> Vec<Vec<u8>> {
        Vec::new()
    }
    /// The line that gives the agent a prompt (a wake), with its newline.
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

/// Something the foreman needs from the agent's output.
#[derive(Debug, PartialEq)]
pub enum Event {
    /// The conversation's id, for resuming it after the process restarts.
    Session(String),
    /// What the agent is doing now: `Edit pricing.md`.
    Activity(String),
    /// A turn ended.
    TurnEnd(TurnEnd),
    /// A line to write back (a reply the protocol expects; Claude Code's
    /// stream-json has none, opencode's ACP does).
    #[allow(dead_code)]
    Send(Vec<u8>),
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

/// The harness of an `agent.kind`.
pub fn get(kind: &str) -> Option<&'static dyn Harness> {
    match kind {
        "claude-code" => Some(&ClaudeCode),
        "pi" => Some(&Pi),
        "generic" => Some(&Generic),
        _ => None,
    }
}

/// Any command; nothing to set up beyond `agent.files` and `env`.
pub struct Generic;

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
    fn driver(&self) -> Option<&dyn Driver> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds() {
        for k in zapper_policy::AGENT_KINDS {
            assert_eq!(get(k).unwrap().kind(), *k);
        }
        assert!(get("opencode").is_none());
        assert!(get("claude-code").unwrap().driver().is_some());
        assert!(get("pi").unwrap().driver().is_some());
        assert!(
            get("generic").unwrap().driver().is_none(),
            "a generic command can't be driven as a worker"
        );
    }
}
