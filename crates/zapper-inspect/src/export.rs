//! A worker's transcript as Markdown: each wake with the mail that woke
//! them, then what the agent thought, said and called, and what came back.
//! Reads both shapes the foreman records: Claude Code's stream-json
//! (`assistant`, `user`, `result`) and pi's RPC events (`message_end`).

use crate::runs::{fmt_time, Person, Run};
use anyhow::{Context, Result};
use serde_json::Value;
use std::fmt::Write as _;
use std::io::{BufRead, BufReader};
use std::path::Path;

pub fn markdown(run: &Run, p: &Person, transcript: &Path) -> Result<String> {
    let f = std::fs::File::open(transcript)
        .with_context(|| format!("opening {}", transcript.display()))?;
    let mut md = String::new();
    header(&mut md, run, p);
    let mut w = Writer::default();
    for line in BufReader::new(f).split(b'\n').map_while(Result::ok) {
        if let Ok(v) = serde_json::from_slice::<Value>(&line) {
            w.event(&mut md, &v);
        }
    }
    Ok(md)
}

fn header(md: &mut String, run: &Run, p: &Person) {
    let _ = writeln!(md, "# {} ({}), run {}\n", p.name, title(p), run.id);
    let mut rows = vec![
        ("Company", run.company.clone()),
        ("Firm", run.firm.clone()),
        ("Role", p.role.clone()),
        (
            "Agent",
            format!("{} · {} · profile {}", p.agent, p.model, p.profile),
        ),
        ("Joined", fmt_time(p.joined)),
        (
            "Hired by",
            p.hired_by.clone().unwrap_or_else(|| "the board".into()),
        ),
        ("Status", p.status()),
    ];
    if let Some(r) = &p.reports_to {
        rows.push(("Reports to", r.clone()));
    }
    if let Some(c) = &p.contract {
        rows.push(("Contract", c.clone()));
    }
    if let Some(o) = &p.operative {
        let mut s = format!("scenario {}, slot {}", o.scenario, o.slot);
        if let Some(why) = &o.released {
            let _ = write!(s, " (released: {why})");
        }
        rows.push(("Operative", s));
        if let Some(why) = &o.debrief {
            rows.push(("Debrief turn", why.clone()));
        }
    }
    for (k, v) in rows {
        let _ = writeln!(md, "- **{k}**: {}", one_line(&v));
    }
    md.push('\n');
    // What the board told them in secret, which the transcript doesn't show.
    let prompt = p
        .operative
        .as_ref()
        .and_then(|o| run.slot_prompts.get(o.slot as usize))
        .filter(|s| !s.trim().is_empty());
    if let Some(prompt) = prompt {
        md.push_str("## The board's instructions\n\n");
        quote(md, prompt);
    }
}

fn title(p: &Person) -> &str {
    if p.title.is_empty() {
        &p.role
    } else {
        &p.title
    }
}

#[derive(Default)]
struct Writer {
    wakes: usize,
    /// What the last wake said: pi echoes it as a user message.
    wake_text: String,
}

impl Writer {
    fn event(&mut self, md: &mut String, v: &Value) {
        let ts = v["ts"].as_f64().unwrap_or(0.0);
        match v["type"].as_str().unwrap_or("") {
            "wake" => {
                self.wakes += 1;
                let text = v["text"].as_str().unwrap_or("");
                let _ = writeln!(md, "## Wake {} · {}\n", self.wakes, fmt_time(ts));
                quote(md, text);
                self.wake_text = text.trim().to_string();
            }
            // Claude Code.
            "assistant" => {
                for c in items(&v["message"]["content"]) {
                    match c["type"].as_str().unwrap_or("") {
                        "text" => text(md, &c["text"]),
                        "thinking" => thinking(md, &c["thinking"]),
                        "tool_use" => call(md, &c["name"], &c["input"]),
                        _ => {}
                    }
                }
            }
            "user" => match &v["message"]["content"] {
                Value::String(s) => self.user(md, s),
                content => {
                    for c in items(content) {
                        match c["type"].as_str().unwrap_or("") {
                            "tool_result" => result(
                                md,
                                &flatten(&c["content"]),
                                c["is_error"].as_bool().unwrap_or(false),
                            ),
                            "text" => self.user(md, c["text"].as_str().unwrap_or("")),
                            _ => {}
                        }
                    }
                }
            },
            "result" => {
                let _ = writeln!(
                    md,
                    "*Turn ended: {} steps, {:.1}s{}.*\n",
                    v["num_turns"].as_u64().unwrap_or(0),
                    v["duration_ms"].as_f64().unwrap_or(0.0) / 1000.0,
                    match v["total_cost_usd"].as_f64() {
                        Some(c) => format!(", ${c:.4}"),
                        None => String::new(),
                    }
                );
            }
            // pi.
            "message_end" => {
                let m = &v["message"];
                match m["role"].as_str().unwrap_or("") {
                    "assistant" => {
                        for c in items(&m["content"]) {
                            match c["type"].as_str().unwrap_or("") {
                                "text" => text(md, &c["text"]),
                                "thinking" => thinking(md, &c["thinking"]),
                                "toolCall" => call(md, &c["name"], &c["arguments"]),
                                _ => {}
                            }
                        }
                        if m["stopReason"] == "error" {
                            let err = m["errorMessage"].as_str().unwrap_or("error");
                            let _ = writeln!(md, "*The model failed: {}*\n", one_line(err));
                        }
                    }
                    "toolResult" => result(
                        md,
                        &flatten(&m["content"]),
                        m["isError"].as_bool().unwrap_or(false),
                    ),
                    "user" => self.user(md, &flatten(&m["content"])),
                    _ => {}
                }
            }
            _ => {}
        }
    }

    /// Mail handed over mid-turn, or the wake echoed back (skipped).
    fn user(&mut self, md: &mut String, s: &str) {
        let s = s.trim();
        if s.is_empty() || s == self.wake_text {
            return;
        }
        md.push_str("**Handed over during the turn:**\n\n");
        quote(md, s);
    }
}

fn items(v: &Value) -> impl Iterator<Item = &Value> {
    v.as_array().into_iter().flatten()
}

/// A tool result's or message's content: a string, or text blocks.
fn flatten(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Array(a) => a
            .iter()
            .filter_map(|c| c["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn text(md: &mut String, v: &Value) {
    let s = v.as_str().unwrap_or("").trim();
    if !s.is_empty() {
        let _ = writeln!(md, "{s}\n");
    }
}

fn thinking(md: &mut String, v: &Value) {
    let s = v.as_str().unwrap_or("").trim();
    if !s.is_empty() {
        md.push_str("<details><summary>Thinking</summary>\n\n");
        quote(md, s);
        md.push_str("</details>\n\n");
    }
}

fn call(md: &mut String, name: &Value, input: &Value) {
    let name = name.as_str().unwrap_or("?");
    let name = zapper_policy::proto::mcp_tool_name(name);
    let _ = writeln!(md, "**→ `{name}`**\n");
    let Some(args) = input.as_object() else {
        fenced(
            md,
            "json",
            &serde_json::to_string_pretty(input).unwrap_or_default(),
        );
        return;
    };
    // Short arguments inline; long or multi-line text as a block.
    let mut blocks = Vec::new();
    for (k, v) in args {
        match v.as_str() {
            Some(s) if s.contains('\n') || s.len() > 100 => blocks.push((k, s)),
            Some(s) => {
                let _ = writeln!(md, "- {k}: `{}`", s.replace('`', "'"));
            }
            None => {
                let _ = writeln!(md, "- {k}: `{v}`");
            }
        }
    }
    if blocks.len() < args.len() {
        md.push('\n');
    }
    for (k, s) in blocks {
        let lang = if matches!(k.as_str(), "command" | "script") {
            "sh"
        } else {
            ""
        };
        let _ = writeln!(md, "{k}:\n");
        fenced(md, lang, s);
    }
}

fn result(md: &mut String, s: &str, error: bool) {
    md.push_str(if error {
        "**← error**\n\n"
    } else {
        "**←**\n\n"
    });
    fenced(md, "", s.trim_end());
}

/// A code block whose fence is longer than any run of backticks inside.
fn fenced(md: &mut String, lang: &str, s: &str) {
    let (mut run, mut longest) = (0, 0);
    for c in s.chars() {
        run = if c == '`' { run + 1 } else { 0 };
        longest = longest.max(run);
    }
    let fence = "`".repeat((longest + 1).max(3));
    let _ = writeln!(md, "{fence}{lang}\n{s}\n{fence}\n");
}

fn quote(md: &mut String, s: &str) {
    for line in s.trim().lines() {
        md.push_str(if line.is_empty() { ">" } else { "> " });
        md.push_str(line);
        md.push('\n');
    }
    md.push('\n');
}

fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn render(events: &[Value]) -> String {
        let mut md = String::new();
        let mut w = Writer::default();
        for e in events {
            w.event(&mut md, e);
        }
        md
    }

    #[test]
    fn claude_code() {
        let md = render(&[
            json!({ "type": "wake", "text": "New mail: 1", "ts": 1.0 }),
            json!({ "type": "assistant", "message": { "content": [
                { "type": "thinking", "thinking": "Plan it" },
                { "type": "tool_use", "name": "mcp__office__mail_send", "input": { "to": ["bob"] } },
            ] } }),
            json!({ "type": "user", "message": { "content": [
                { "type": "tool_result", "content": [{ "type": "text", "text": "sent m-2" }] },
            ] } }),
            json!({ "type": "assistant", "message": { "content": [{ "type": "text", "text": "Done." }] } }),
        ]);
        assert!(md.contains("## Wake 1"));
        assert!(md.contains("> New mail: 1"));
        assert!(md.contains("<summary>Thinking</summary>\n\n> Plan it"));
        assert!(md.contains("**→ `mail_send`**\n\n- to: `[\"bob\"]`"));
        assert!(md.contains("```\nsent m-2\n```"));
        assert!(md.contains("Done.\n"));
    }

    #[test]
    fn pi_skips_the_echoed_wake() {
        let md = render(&[
            json!({ "type": "wake", "text": "New mail: 1" }),
            json!({ "type": "message_end", "message": { "role": "user", "content": [{ "type": "text", "text": "New mail: 1" }] } }),
            json!({ "type": "message_end", "message": { "role": "assistant", "content": [
                { "type": "toolCall", "name": "mcp__office__desk", "arguments": {} },
            ] } }),
            json!({ "type": "message_end", "message": { "role": "toolResult", "isError": true, "content": [{ "type": "text", "text": "no" }] } }),
            json!({ "type": "message_end", "message": { "role": "user", "content": [{ "type": "text", "text": "More mail" }] } }),
        ]);
        assert_eq!(md.matches("New mail: 1").count(), 1);
        assert!(md.contains("**→ `desk`**"));
        assert!(md.contains("**← error**"));
        assert!(md.contains("Handed over during the turn:**\n\n> More mail"));
    }

    #[test]
    fn fences_outgrow_the_content() {
        let mut md = String::new();
        fenced(&mut md, "", "a ```` b");
        assert!(md.starts_with("`````\n"));
    }
}
