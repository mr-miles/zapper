//! The driver against a scripted Claude Code on the guest's side of the
//! terminal channel, in memory: no VM.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;
use tokio::sync::mpsc;
use zapper_policy::proto::Frame;

use super::{run, Driver, Report, Status};
use crate::daemon::harness;
use crate::daemon::session::read_frame;

#[derive(Default)]
struct Rec {
    events: Mutex<Vec<(String, Value)>>,
}

impl Report for Rec {
    fn event(&self, event: &str, detail: Value) {
        self.events.lock().unwrap().push((event.into(), detail));
    }
}

impl Rec {
    fn named(&self, name: &str) -> Vec<Value> {
        let ev = self.events.lock().unwrap();
        ev.iter()
            .filter(|(n, _)| n == name)
            .map(|(_, d)| d.clone())
            .collect()
    }
}

/// What the scripted Claude saw: each process's argv and the prompts.
#[derive(Default)]
struct Seen {
    argv: Mutex<Vec<Vec<String>>>,
    prompts: Mutex<Vec<String>>,
}

async fn send(g: &mut UnixStream, frame: Frame) {
    let mut buf = Vec::new();
    frame.write_to(&mut buf).unwrap();
    let _ = g.write_all(&buf).await;
}

async fn line(g: &mut UnixStream, v: Value) {
    let mut l = serde_json::to_vec(&v).unwrap();
    l.push(b'\n');
    send(g, Frame::Data(l)).await;
}

/// Claude Code as the driver sees it, by what each prompt says: `hang`
/// never answers, `crash` exits, `ratelimit` hits the limit once, `fail`
/// gets an API error, `always-exit` exits as soon as it starts, anything
/// else works for a moment and ends the turn.
async fn claude(
    mut g: UnixStream,
    seen: Arc<Seen>,
    limited: Arc<Mutex<bool>>,
    crashed: Arc<Mutex<bool>>,
    exit_at_once: bool,
) {
    if exit_at_once {
        send(&mut g, Frame::Exit(1)).await;
        return;
    }
    let mut buf = Vec::new();
    while let Ok(Some(f)) = read_frame(&mut g).await {
        let Frame::Data(d) = f else { continue };
        buf.extend_from_slice(&d);
        while let Some(i) = buf.iter().position(|c| *c == b'\n') {
            let l: Vec<u8> = buf.drain(..=i).collect();
            let v: Value = serde_json::from_slice(&l).unwrap();
            let text = v["message"]["content"][0]["text"]
                .as_str()
                .unwrap_or("")
                .to_string();
            seen.prompts.lock().unwrap().push(text.clone());
            if text.contains("hang") {
                continue;
            }
            if text.contains("crash") && !std::mem::replace(&mut *crashed.lock().unwrap(), true) {
                send(&mut g, Frame::Exit(1)).await;
                return;
            }
            line(
                &mut g,
                json!({ "type": "system", "subtype": "init", "session_id": "conv-1" }),
            )
            .await;
            if text.contains("slow") {
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
            line(&mut g, json!({ "type": "assistant", "message": { "content": [{ "type": "tool_use", "name": "Edit", "input": { "file_path": "/work/a.txt" } }] } })).await;
            let result = if text.contains("ratelimit")
                && !std::mem::replace(&mut *limited.lock().unwrap(), true)
            {
                json!({ "type": "result", "is_error": true, "result": "Claude AI usage limit reached", "usage": {} })
            } else if text.contains("fail") {
                json!({ "type": "result", "is_error": true, "result": "API Error: 401 invalid token", "usage": {} })
            } else {
                json!({ "type": "result", "is_error": false, "result": "ok", "usage": { "input_tokens": 10, "output_tokens": 5 } })
            };
            line(&mut g, result).await;
        }
    }
}

struct Rig {
    driver: Arc<Driver>,
    rec: Arc<Rec>,
    seen: Arc<Seen>,
    transcript: Arc<Mutex<Vec<Value>>>,
    task: tokio::task::JoinHandle<()>,
}

fn rig(turn_timeout: Option<Duration>, exit_at_once: bool) -> Rig {
    let driver = Driver::new();
    let rec = Arc::new(Rec::default());
    let seen = Arc::new(Seen::default());
    let transcript = Arc::new(Mutex::new(Vec::new()));
    let limited = Arc::new(Mutex::new(false));
    let crashed = Arc::new(Mutex::new(false));
    let (tx, mut rx) = mpsc::unbounded_channel::<UnixStream>();
    {
        let (seen, limited, crashed) = (seen.clone(), limited.clone(), crashed.clone());
        tokio::spawn(async move {
            while let Some(g) = rx.recv().await {
                tokio::spawn(claude(
                    g,
                    seen.clone(),
                    limited.clone(),
                    crashed.clone(),
                    exit_at_once,
                ));
            }
        });
    }
    let spawn_seen = seen.clone();
    let spawn: super::Spawn = Box::new(move |argv| {
        spawn_seen.argv.lock().unwrap().push(argv);
        let tx = tx.clone();
        Box::pin(async move {
            let (host, guest) = UnixStream::pair()?;
            tx.send(guest).unwrap();
            Ok(host)
        })
    });
    let conversations: super::Conversations = Box::new(|resume| {
        harness::get("claude-code")
            .unwrap()
            .driver()
            .unwrap()
            .conversation(resume)
    });
    let t = transcript.clone();
    let task = tokio::spawn(run(
        driver.clone(),
        conversations,
        spawn,
        rec.clone(),
        Box::new(move |v| t.lock().unwrap().push(v)),
        turn_timeout,
    ));
    Rig {
        driver,
        rec,
        seen,
        transcript,
        task,
    }
}

/// Waits (in the test's paused time) until `ok` holds.
async fn until(what: &str, ok: impl Fn() -> bool) {
    for _ in 0..20_000 {
        if ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("timed out waiting for {what}");
}

#[tokio::test(start_paused = true)]
async fn a_prompt_is_a_turn() {
    let r = rig(None, false);
    assert_eq!(r.driver.prompt("hello").unwrap(), 1);
    until("the turn's end", || r.rec.named("turn_end").len() == 1).await;
    let end = &r.rec.named("turn_end")[0];
    assert_eq!(end["outcome"], "done");
    assert_eq!(end["tokens"], 15);
    assert_eq!(r.rec.named("turn_start")[0]["prompts"], 1);
    assert!(
        !r.rec.named("activity").is_empty(),
        "what the agent was doing"
    );
    let info = r.driver.info();
    assert_eq!(info["status"], "idle");
    assert_eq!(info["turns"], 1);
    assert_eq!(info["tokens"], 15);
    assert_eq!(info["conversation"], "conv-1");
    let argv = r.seen.argv.lock().unwrap()[0].clone();
    assert_eq!(&argv[..2], ["claude", "-p"]);
    assert!(
        !argv.contains(&"--resume".to_string()),
        "a new conversation"
    );
    let types: Vec<Value> = r
        .transcript
        .lock()
        .unwrap()
        .iter()
        .map(|v| v["type"].clone())
        .collect();
    assert_eq!(
        types,
        [json!("system"), json!("assistant"), json!("result")]
    );
    r.driver.stop();
    r.task.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn prompts_during_a_turn_wait_for_the_next_one_together() {
    let r = rig(None, false);
    r.driver.prompt("slow work").unwrap();
    until("the slow turn", || r.driver.status() == Status::Working).await;
    assert_eq!(r.driver.prompt("a").unwrap(), 1);
    assert_eq!(r.driver.prompt("b").unwrap(), 2);
    assert_eq!(r.driver.info()["queued"], 2);
    until("two turns", || r.rec.named("turn_end").len() == 2).await;
    assert_eq!(*r.seen.prompts.lock().unwrap(), ["slow work", "a\n\nb"]);
    assert_eq!(r.rec.named("turn_start")[1]["prompts"], 2);
    assert_eq!(r.seen.argv.lock().unwrap().len(), 1, "one process for both");
    r.driver.stop();
    r.task.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_rate_limit_retries_the_same_prompt_later() {
    let r = rig(None, false);
    r.driver.prompt("ratelimit me").unwrap();
    until("the retry", || r.rec.named("turn_end").len() == 2).await;
    let ends = r.rec.named("turn_end");
    assert_eq!(ends[0]["outcome"], "rate_limited");
    assert_eq!(ends[1]["outcome"], "done");
    assert_eq!(
        *r.seen.prompts.lock().unwrap(),
        ["ratelimit me", "ratelimit me"]
    );
    r.driver.stop();
    r.task.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn an_api_failure_keeps_the_prompt_and_retries_later() {
    let r = rig(None, false);
    r.driver.prompt("fail").unwrap();
    until("the failed turn", || r.rec.named("turn_end").len() == 1).await;
    assert_eq!(r.rec.named("turn_end")[0]["outcome"], "failed");
    let info = r.driver.info();
    assert_eq!(info["status"], "error");
    assert!(
        info["error"].as_str().unwrap().contains("API Error"),
        "{info}"
    );
    assert_eq!(info["queued"], 1, "the agent never saw it");
    until("the retry, a minute on", || {
        r.rec.named("turn_end").len() == 2
    })
    .await;
    assert_eq!(*r.seen.prompts.lock().unwrap(), ["fail", "fail"]);
    r.driver.stop();
    r.task.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn after_a_crash_the_prompt_goes_to_the_resumed_conversation() {
    let r = rig(None, false);
    r.driver.prompt("hello").unwrap();
    until("the first turn", || r.rec.named("turn_end").len() == 1).await;
    r.driver.prompt("crash now").unwrap();
    until("the exit", || r.rec.named("exited").len() == 1).await;
    until("the prompt again", || r.rec.named("turn_end").len() == 2).await;
    assert_eq!(
        *r.seen.prompts.lock().unwrap(),
        ["hello", "crash now", "crash now"]
    );
    let argv = r.seen.argv.lock().unwrap().clone();
    assert_eq!(argv.len(), 2, "a new process");
    let resume = argv[1]
        .iter()
        .position(|a| a == "--resume")
        .expect("--resume");
    assert_eq!(argv[1][resume + 1], "conv-1");
    r.driver.stop();
    r.task.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_turn_that_runs_too_long_drops_the_process() {
    let r = rig(Some(Duration::from_secs(60)), false);
    r.driver.prompt("hello").unwrap();
    until("the first turn", || r.rec.named("turn_end").len() == 1).await;
    r.driver.prompt("hang").unwrap();
    until("the timeout", || r.rec.named("turn_end").len() == 2).await;
    assert_eq!(r.rec.named("turn_end")[1]["outcome"], "timed_out");
    r.driver.prompt("after").unwrap();
    until("the turn after", || r.rec.named("turn_end").len() == 3).await;
    let argv = r.seen.argv.lock().unwrap().clone();
    assert_eq!(argv.len(), 2, "the hung process was dropped");
    assert!(argv[1].contains(&"--resume".to_string()));
    r.driver.stop();
    r.task.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn an_agent_that_keeps_exiting_rests_then_tries_again() {
    let r = rig(None, true);
    r.driver.prompt("hello").unwrap();
    until("resting", || r.driver.status() == Status::Error).await;
    assert_eq!(
        r.rec.named("exited").len(),
        6,
        "more than five in ten minutes"
    );
    let info = r.driver.info();
    assert_eq!(info["queued"], 1, "the prompt waits");
    assert!(
        info["error"].as_str().unwrap().contains("exited 6 times"),
        "{info}"
    );
    until("trying again after the rest", || {
        r.rec.named("exited").len() > 6
    })
    .await;
    r.driver.stop();
    r.task.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn prompts_are_bounded_and_end_with_the_session() {
    let r = rig(None, false);
    assert!(r.driver.prompt("  ").is_err());
    assert!(r.driver.prompt(&"x".repeat(super::MAX_PROMPT + 1)).is_err());
    r.driver.prompt("slow").unwrap();
    until("working", || r.driver.status() == Status::Working).await;
    for i in 0..super::MAX_QUEUED {
        r.driver.prompt(&format!("p{i}")).unwrap();
    }
    assert!(r.driver.prompt("one too many").is_err());
    r.driver.stop();
    r.task.await.unwrap();
    assert_eq!(r.driver.status(), Status::Stopped);
    assert!(r.driver.prompt("late").is_err());
}
