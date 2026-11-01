//! The foreman without VMs: a worker's VM is played by `shell_vm`, and its
//! agent by a fake Claude Code that answers each wake in stream-json. Mail
//! wakes the worker, the turn's mail is read and journaled, mail during a
//! turn goes over through the stop hook, and an agent that exits, runs too
//! long or is rate-limited is handled as a real one would be.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use zapper_core::testing::{shell_vm, Shell, TestCore};
use zapper_core::Session;

use crate::{test_run, FirmRun, WorkerState};

const FIRM: &str = "
profiles:
  w: { agent: { kind: claude-code } }
firms:
  acme:
    roles:
      ceo: { profile: w, agent: ceo }
      eng: { profile: w, agent: software-engineer }
    workers:
      - { name: ceo, role: ceo }
      - { name: pat, role: eng, reports_to: ceo }
";

fn run_with(yaml: &str) -> (TestCore, Arc<FirmRun>) {
    let t = crate::test_core(yaml);
    let run = test_run(&t.core, "acme");
    (t, run)
}

fn session_of(run: &FirmRun, worker: &str) -> Arc<Session> {
    run.rt(worker)
        .unwrap()
        .session
        .lock()
        .unwrap()
        .clone()
        .unwrap()
}

/// Pat's VM, and pat's foreman started on it.
fn pats_foreman(run: &Arc<FirmRun>) -> tokio::sync::mpsc::UnboundedReceiver<Shell> {
    let vm = shell_vm(&session_of(run, "pat"));
    tokio::spawn(crate::foreman::run(run.clone(), run.rt("pat").unwrap()));
    vm
}

/// A tool called as `s`'s VM does, through the gateway: the tool's answer,
/// or `{"error": ...}`.
async fn tool(t: &TestCore, s: &Arc<Session>, name: &str, args: Value) -> Value {
    let (host, mut guest) = UnixStream::pair().unwrap();
    let (d, vm) = (t.core.clone(), s.clone());
    tokio::spawn(async move { zapper_core::testing::mcp(&d, &vm, host).await });
    let call = json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": { "name": name, "arguments": args } });
    guest
        .write_all(format!("office\n{call}\n").as_bytes())
        .await
        .unwrap();
    let mut reply = String::new();
    BufReader::new(guest).read_line(&mut reply).await.unwrap();
    let v: Value = serde_json::from_str(&reply).unwrap();
    let text = v["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default();
    if v["result"]["isError"] == json!(true) {
        return json!({ "error": text });
    }
    serde_json::from_str(text).unwrap()
}

/// The CEO mails pat.
async fn mail_pat(t: &TestCore, run: &FirmRun, subject: &str) {
    let sent = tool(
        t,
        &session_of(run, "ceo"),
        "mail_send",
        json!({ "to": ["pat"], "subject": subject, "body": "Please.", "kind": "fyi" }),
    )
    .await;
    assert!(sent.get("error").is_none(), "{sent}");
}

/// The next command started in the VM, within ten seconds.
async fn next(vm: &mut tokio::sync::mpsc::UnboundedReceiver<Shell>) -> Shell {
    tokio::time::timeout(Duration::from_secs(10), vm.recv())
        .await
        .expect("an agent started")
        .unwrap()
}

/// The text of the next wake the agent is sent.
async fn wake(shell: &mut Shell) -> String {
    let line = tokio::time::timeout(Duration::from_secs(10), shell.line())
        .await
        .expect("a wake")
        .expect("stdin open");
    let v: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(v["type"], "user", "{v}");
    v["message"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .to_string()
}

/// The fake Claude Code ends its turn: its init, a line of text, and the
/// result with its usage.
async fn end_turn(shell: &mut Shell, result: Value) {
    shell
        .say(&json!({ "type": "system", "subtype": "init", "session_id": "c-1", "tools": [], "model": "fake" }).to_string())
        .await;
    shell
        .say(&json!({ "type": "assistant", "session_id": "c-1", "message": { "role": "assistant", "content": [{ "type": "text", "text": "Done." }] } }).to_string())
        .await;
    shell.say(&result.to_string()).await;
}

fn done() -> Value {
    json!({ "type": "result", "subtype": "success", "is_error": false, "session_id": "c-1", "result": "Done.", "usage": { "input_tokens": 100, "output_tokens": 10 } })
}

/// Pat's turn events in the journal, in order.
fn turns(run: &FirmRun) -> Vec<Value> {
    std::fs::read_to_string(run.dir.join("journal.jsonl"))
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str::<Value>(l).unwrap())
        .filter(|e| e["type"] == "turn" && e["worker"] == "pat")
        .collect()
}

/// Waits up to ten seconds for pat's journal to have a turn event `event`.
async fn turn_event(run: &FirmRun, event: &str) -> Value {
    for _ in 0..500 {
        if let Some(e) = turns(run).into_iter().find(|e| e["event"] == event) {
            return e;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("no turn {event}: {:?}", turns(run));
}

async fn unread(t: &TestCore, run: &FirmRun) -> u64 {
    let inbox = tool(t, &session_of(run, "pat"), "mail_inbox", json!({})).await;
    inbox["unread"].as_u64().unwrap()
}

#[tokio::test]
async fn mail_wakes_the_worker_whose_turn_reads_it_and_is_journaled() {
    let (t, run) = run_with(FIRM);
    let mut vm = pats_foreman(&run);
    mail_pat(&t, &run, "The plan").await;

    let mut agent = next(&mut vm).await;
    assert_eq!(agent.request.argv[0], "claude", "{:?}", agent.request.argv);
    let text = wake(&mut agent).await;
    assert!(text.contains("The plan"), "{text}");
    // What a wake shows is read.
    assert_eq!(unread(&t, &run).await, 0);
    let w = run.rt("pat").unwrap();
    assert_eq!(w.state(), WorkerState::Working);

    end_turn(&mut agent, done()).await;
    let end = turn_event(&run, "end").await;
    assert!(end["tokens"].as_u64().unwrap() > 0, "{end}");
    let start = &turns(&run)[0];
    assert_eq!(start["event"], "start");
    assert_eq!(start["messages"].as_array().unwrap().len(), 1, "{start}");
    for _ in 0..100 {
        if w.state() == WorkerState::Idle {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(w.state(), WorkerState::Idle);
    // The wake and the agent's lines are the worker's transcript.
    let transcript =
        std::fs::read_to_string(run.dir.join("transcripts").join("pat.jsonl")).unwrap();
    assert!(
        transcript.contains("The plan") && transcript.contains("Done."),
        "{transcript}"
    );
}

/// The desk names the message to reply to for each piece of work owed, so
/// a report can go to its assignment's thread rather than a later one.
#[tokio::test]
async fn the_desk_names_what_to_reply_to() {
    let (t, run) = run_with(FIRM);
    let mut vm = pats_foreman(&run);
    let sent = tool(
        &t,
        &session_of(&run, "ceo"),
        "mail_send",
        json!({ "to": ["pat"], "subject": "Build it", "body": "Please.", "kind": "assignment" }),
    )
    .await;
    let (id, thread) = (
        sent["id"].as_str().unwrap(),
        sent["thread"].as_str().unwrap(),
    );
    let mut agent = next(&mut vm).await;
    let text = wake(&mut agent).await;
    let line = format!("\"Build it\" from ceo (assignment, reply to {id} in {thread})");
    assert!(text.contains(&line), "{text}");
}

#[tokio::test]
async fn mail_that_comes_during_a_turn_goes_over_through_the_stop_hook() {
    let (t, run) = run_with(FIRM);
    let mut vm = pats_foreman(&run);
    mail_pat(&t, &run, "First").await;
    let mut agent = next(&mut vm).await;
    wake(&mut agent).await;

    mail_pat(&t, &run, "Second").await;
    // Claude Code's Stop hook asks; the new mail is the reason to go on.
    let check = tool(&t, &session_of(&run, "pat"), "stop_check", json!({})).await;
    assert_eq!(check["block"], true, "{check}");
    assert!(
        check["reason"].as_str().unwrap().contains("Second"),
        "{check}"
    );
    assert_eq!(
        turn_event(&run, "extended").await["messages"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(unread(&t, &run).await, 0);
    // Handed over, it wakes nobody again.
    let again = tool(&t, &session_of(&run, "pat"), "stop_check", json!({})).await;
    assert_eq!(again["block"], false, "{again}");

    end_turn(&mut agent, done()).await;
    turn_event(&run, "end").await;
    assert!(
        tokio::time::timeout(Duration::from_millis(700), agent.line())
            .await
            .is_err(),
        "no second wake"
    );
}

#[tokio::test]
async fn an_agent_that_exits_mid_turn_is_restarted_with_its_mail_again() {
    let (t, run) = run_with(FIRM);
    let mut vm = pats_foreman(&run);
    mail_pat(&t, &run, "Keep this").await;
    let mut agent = next(&mut vm).await;
    wake(&mut agent).await;
    agent.exit(1).await;

    let gone = turn_event(&run, "interrupted").await;
    assert_eq!(gone["exit_code"], 1, "{gone}");
    let mut again = next(&mut vm).await;
    let text = wake(&mut again).await;
    assert!(text.contains("Keep this"), "the same mail: {text}");
    assert!(text.contains("previous turn was interrupted"), "{text}");
    end_turn(&mut again, done()).await;
    turn_event(&run, "end").await;
}

#[tokio::test]
async fn a_turn_that_runs_too_long_is_cut_charged_and_noted() {
    let firm = FIRM.replace("  acme:\n", "  acme:\n    budget: { turn_timeout: 1s }\n");
    let (t, run) = run_with(&firm);
    let mut vm = pats_foreman(&run);
    mail_pat(&t, &run, "Slow work").await;
    let mut agent = next(&mut vm).await;
    wake(&mut agent).await;

    // No answer: after a second the foreman hangs the agent up.
    let late = turn_event(&run, "timed_out").await;
    assert_eq!(late["messages"].as_array().unwrap().len(), 1, "{late}");
    let closed = tokio::time::timeout(Duration::from_secs(5), agent.line()).await;
    assert_eq!(closed, Ok(None), "the agent's stdin is closed");
    let note = run.rt("pat").unwrap().notes.lock().unwrap().clone();
    assert!(note.iter().any(|n| n.contains("Slow work")), "{note:?}");
}

#[tokio::test]
async fn a_rate_limited_turn_keeps_its_mail_for_later() {
    let (t, run) = run_with(FIRM);
    let mut vm = pats_foreman(&run);
    mail_pat(&t, &run, "Later").await;
    let mut agent = next(&mut vm).await;
    wake(&mut agent).await;
    end_turn(
        &mut agent,
        json!({ "type": "result", "is_error": true, "result": "API Error: 429 rate_limit_error", "session_id": "c-1" }),
    )
    .await;

    turn_event(&run, "rate_limited").await;
    let queued = run.rt("pat").unwrap().queue.lock().unwrap().clone();
    assert_eq!(queued.len(), 1, "the mail waits for the next turn");
}

/// No turns while the record can't be written: with mail waiting, a
/// worker shows the error and waits; once the record is back, nothing
/// holds them. (The record's health is the daemon's, shared by every test
/// in this process, so the gate is given it rather than it being broken.)
#[tokio::test]
async fn no_turns_while_the_record_cant_be_written() {
    let (t, run) = run_with(FIRM);
    mail_pat(&t, &run, "The plan").await;
    let w = run.rt("pat").unwrap();
    for _ in 0..200 {
        if !w.queue.lock().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let (state, detail, _) = crate::foreman::blocked_by(&run, &w, 0.0, false).expect("held");
    assert_eq!(state, WorkerState::Error);
    assert!(detail.unwrap().contains("record can't be written"));
    assert!(
        crate::foreman::blocked_by(&run, &w, 0.0, true).is_none(),
        "free once it's back"
    );
}
