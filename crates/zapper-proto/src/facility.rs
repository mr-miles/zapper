//! What the daemon tells a facility, a program on the host serving guests
//! from a socket in the daemon's `state/services/` directory: who calls a
//! socket service, in the first line of each connection
//! ([`ServiceCaller`]); and the requests of an MCP server registered on a
//! socket (`POST /v1/mcp`), for which the daemon is the MCP endpoint the
//! guest talks to, asking the facility one request per connection, one line
//! each way, naming the caller itself.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The first line on a connection to a socket service: who is calling,
/// from the VM the connection came from. The guest's own bytes follow it
/// (with TLS terminated, for a service the daemon terminates it for), so
/// nothing the guest sends can pose as it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ServiceCaller {
    pub session: String,
    pub labels: BTreeMap<String, String>,
    /// The name the guest asked for, and the port.
    pub name: String,
    pub port: u16,
}

/// A request to an MCP server on a socket: who calls, from the VM the
/// guest's connection came from (never from what the guest sent), and what
/// for.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpCall {
    /// The calling session's id, and its labels.
    pub session: String,
    pub labels: BTreeMap<String, String>,
    /// The server's name, as it is registered.
    pub server: String,
    pub method: McpMethod,
    /// For `tools/call`: the tool, and its arguments as the guest sent them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arguments: Option<Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum McpMethod {
    /// The server's tools for the caller: a JSON array of MCP tool
    /// definitions.
    #[serde(rename = "tools/list")]
    ToolsList,
    /// A tool called: its result, any JSON, goes to the model as text.
    #[serde(rename = "tools/call")]
    ToolsCall,
}

/// A facility's answer: its result, or why not (an error result to the
/// model, for a tool call).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct McpReply {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// A request to a decider on a socket (`POST /v1/deciders`): an `ask` a rule
/// referred, or an `allowed` note that a rule the decider set was used. Who
/// calls is named from the VM the guest's connection came from, never from
/// what the guest sent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AskCall {
    pub session: String,
    pub labels: BTreeMap<String, String>,
    pub method: AskMethod,
    /// For `ask`: the request's host and port, and, when a rule inspects it,
    /// the HTTP method and path.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub host: String,
    #[serde(default)]
    pub port: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http_method: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// For `allowed`: the rule that allowed the request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rule: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AskMethod {
    /// A request a rule asked about: the reply is a referral, or an error
    /// (the request is refused as a denial).
    #[serde(rename = "ask")]
    Ask,
    /// A request a rule allowed, for the decider's records (a once-only
    /// approval is used up); the reply is ignored.
    #[serde(rename = "allowed")]
    Allowed,
}

/// A decider's answer to an `ask`: where the request went, or why it is
/// refused outright (no referral).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct AskReply {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub referral: Option<Referral>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Where an `ask` went, as the guest is told it: "access to … requested from
/// `to` (request `id`); `then`".
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Referral {
    pub id: String,
    pub to: String,
    pub then: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_call_and_a_reply_are_one_json_line_each() {
        let call = McpCall {
            session: "abc234".into(),
            labels: [("firm".to_string(), "acme".to_string())].into(),
            server: "office".into(),
            method: McpMethod::ToolsCall,
            tool: Some("mail_send".into()),
            arguments: Some(json!({ "to": ["pat"] })),
        };
        let line = serde_json::to_string(&call).unwrap();
        assert!(!line.contains('\n'));
        assert!(line.contains(r#""method":"tools/call""#), "{line}");
        assert_eq!(serde_json::from_str::<McpCall>(&line).unwrap(), call);
        let list: McpCall = serde_json::from_value(json!({
            "session": "s", "labels": {}, "server": "office", "method": "tools/list"
        }))
        .unwrap();
        assert_eq!((list.method, list.tool), (McpMethod::ToolsList, None));
        let reply: McpReply = serde_json::from_str(r#"{"error":"no"}"#).unwrap();
        assert_eq!(reply.error.as_deref(), Some("no"));
    }

    #[test]
    fn an_ask_call_and_a_referral_are_one_json_line_each() {
        let call = AskCall {
            session: "s".into(),
            labels: [("member".to_string(), "pat".to_string())].into(),
            method: AskMethod::Ask,
            host: "api.example".into(),
            port: 443,
            http_method: None,
            path: None,
            rule: None,
        };
        let line = serde_json::to_string(&call).unwrap();
        assert!(!line.contains('\n'));
        assert!(line.contains(r#""method":"ask""#), "{line}");
        assert_eq!(serde_json::from_str::<AskCall>(&line).unwrap(), call);
        // An `allowed` note carries the rule, not a request.
        let note: AskCall = serde_json::from_value(json!({
            "session": "s", "labels": {}, "method": "allowed", "rule": "r-3"
        }))
        .unwrap();
        assert_eq!((note.method, note.rule.as_deref()), (AskMethod::Allowed, Some("r-3")));
        let reply: AskReply =
            serde_json::from_str(r#"{"referral":{"id":"r-1","to":"security","then":"wait"}}"#)
                .unwrap();
        assert_eq!(reply.referral.unwrap().id, "r-1");
    }
}
