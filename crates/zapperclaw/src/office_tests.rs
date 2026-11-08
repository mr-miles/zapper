//! The office without sandboxes: the warden is `fake::FakeWarden`, and each
//! agent it starts is played by the test, in stream-json as Claude Code
//! speaks it. Mail wakes a worker, a turn's mail is read and journaled, mail
//! during a turn goes over through the Stop hook, an agent that exits or
//! runs too long is handled as a real one would be; the tools, the drive,
//! the edge's callers and the board.

use std::io::Write as _;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc::UnboundedReceiver;
use zapper_openshell::edge::Backend as _;

use crate::config::Config;
use crate::fake::{Agent, FakeWarden};
use crate::mail::{Draft, BOARD};
use crate::{tools, Office, State};

struct T {
    office: Arc<Office>,
    agents: UnboundedReceiver<Agent>,
    warden: Arc<FakeWarden>,
    dir: std::path::PathBuf,
}

impl Drop for T {
    fn drop(&mut self) {
        self.office.stop();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn office_with(extra: &str) -> T {
    let dir = std::env::temp_dir().join(format!("zc-office-{}", zapper_util::random_id(8)));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("ada.md"), "Count things.").unwrap();
    let yaml = format!(
        "workers:\n  ada: {{ profile: claude, instructions: ada.md, about: counts }}\n  bob: {{ profile: claude }}\nstate: {}\n{extra}",
        dir.join("state").display()
    );
    let config = Config::from_yaml(&yaml, &dir).unwrap();
    let (warden, agents) = FakeWarden::new();
    let warden = Arc::new(warden);
    let office = Office::new(config, warden.clone()).unwrap();
    T {
        office,
        agents,
        warden,
        dir,
    }
}

impl T {
    fn start(&self) {
        self.office.start();
    }

    fn board_mails(&self, to: &str, subject: &str) -> String {
        self.office
            .send(Draft {
                from: BOARD.into(),
                to: vec![to.into()],
                subject: subject.into(),
                body: "Please.".into(),
                ..Draft::default()
            })
            .unwrap()
            .id
    }

    async fn next(&mut self) -> Agent {
        tokio::time::timeout(Duration::from_secs(10), self.agents.recv())
            .await
            .expect("an agent started")
            .unwrap()
    }

    async fn tool(&self, who: &str, op: &str, args: Value) -> Result<Value, String> {
        let w = self.office.worker(who).unwrap();
        tools::answer(&self.office, w, op, args)
            .await
            .map_err(|e| format!("{e:#}"))
    }

    fn journal(&self) -> Vec<Value> {
        std::fs::read_to_string(self.office.dir.join("journal.jsonl"))
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    fn turns(&self, worker: &str) -> Vec<Value> {
        self.journal()
            .into_iter()
            .filter(|e| e["type"] == "turn" && e["worker"] == worker)
            .collect()
    }

    async fn turn_event(&self, worker: &str, event: &str) -> Value {
        for _ in 0..500 {
            if let Some(e) = self.turns(worker).into_iter().find(|e| e["event"] == event) {
                return e;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("no turn {event}: {:?}", self.turns(worker));
    }

    async fn state(&self, worker: &str, want: State) {
        let w = self.office.worker(worker).unwrap();
        for _ in 0..500 {
            if w.state() == want {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("{worker} is {:?}, not {want:?}", w.state());
    }
}

/// The text of the next wake the agent is sent.
async fn wake(agent: &mut Agent) -> String {
    let line = tokio::time::timeout(Duration::from_secs(10), agent.line())
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

/// The fake Claude Code ends its turn.
async fn end_turn(agent: &mut Agent) {
    agent
        .say(&json!({ "type": "system", "subtype": "init", "session_id": "c-1", "tools": [], "model": "fake" }).to_string())
        .await;
    agent
        .say(&json!({ "type": "assistant", "session_id": "c-1", "message": { "role": "assistant", "content": [{ "type": "text", "text": "Done." }] } }).to_string())
        .await;
    agent
        .say(&json!({ "type": "result", "subtype": "success", "is_error": false, "session_id": "c-1", "result": "Done.", "usage": { "input_tokens": 100, "output_tokens": 10 } }).to_string())
        .await;
}

#[tokio::test]
async fn mail_wakes_the_worker_whose_turn_reads_it_and_is_journaled() {
    let mut t = office_with("");
    t.start();
    t.board_mails("ada", "The plan");
    let mut agent = t.next().await;
    assert_eq!(agent.name, "ada");
    assert_eq!(agent.resume, None);
    let text = wake(&mut agent).await;
    assert!(
        text.starts_with("You are ada"),
        "the first wake briefs: {text}"
    );
    assert!(text.contains("Count things."), "{text}");
    assert!(text.contains("The plan"), "{text}");
    assert_eq!(t.office.post.lock().unwrap().unread("ada").len(), 0);
    t.state("ada", State::Working).await;
    end_turn(&mut agent).await;
    let end = t.turn_event("ada", "end").await;
    assert!(end["tokens"].as_u64().unwrap() > 0, "{end}");
    t.state("ada", State::Idle).await;
    assert_eq!(
        t.office
            .worker("ada")
            .unwrap()
            .conversation
            .lock()
            .unwrap()
            .as_deref(),
        Some("c-1")
    );
    assert!(t
        .journal()
        .iter()
        .any(|e| e["type"] == "conversation" && e["id"] == "c-1"));
    // The next mail goes to the same agent, without the brief.
    t.board_mails("ada", "More");
    let text = wake(&mut agent).await;
    assert!(!text.contains("You are ada"), "{text}");
    assert!(text.contains("More"), "{text}");
}

#[tokio::test]
async fn mail_during_a_turn_goes_over_through_the_stop_hook() {
    let mut t = office_with("");
    t.start();
    t.board_mails("ada", "First");
    let mut agent = t.next().await;
    wake(&mut agent).await;
    t.state("ada", State::Working).await;
    t.board_mails("ada", "Second");
    let hook = t.tool("ada", "stop_check", json!({})).await.unwrap();
    assert_eq!(hook["block"], true, "{hook}");
    assert!(
        hook["reason"].as_str().unwrap().contains("Second"),
        "{hook}"
    );
    let again = t.tool("ada", "stop_check", json!({})).await.unwrap();
    assert_eq!(again["block"], false, "handed over once");
    end_turn(&mut agent).await;
    t.turn_event("ada", "extended").await;
    t.state("ada", State::Idle).await;
    assert!(t
        .office
        .worker("ada")
        .unwrap()
        .queue
        .lock()
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn an_agent_that_exits_is_restarted_resuming_and_shown_its_mail_again() {
    let mut t = office_with("");
    t.start();
    t.board_mails("ada", "First");
    let mut agent = t.next().await;
    wake(&mut agent).await;
    end_turn(&mut agent).await;
    t.turn_event("ada", "end").await;
    t.board_mails("ada", "Second");
    wake(&mut agent).await;
    agent.exit(1).await;
    t.turn_event("ada", "interrupted").await;
    let mut again = t.next().await;
    assert_eq!(
        again.resume.as_deref(),
        Some("c-1"),
        "it resumes the conversation"
    );
    let text = wake(&mut again).await;
    assert!(text.contains("started again"), "{text}");
    assert!(text.contains("Second"), "the mail is shown again: {text}");
}

#[tokio::test]
async fn a_turn_that_runs_too_long_is_stopped_and_noted() {
    let mut t = office_with("turn_timeout: 1s\n");
    t.start();
    t.board_mails("ada", "Slow");
    let mut agent = t.next().await;
    wake(&mut agent).await;
    let late = t.turn_event("ada", "timed_out").await;
    assert_eq!(late["messages"].as_array().unwrap().len(), 1);
    t.board_mails("ada", "Next");
    let mut again = t.next().await;
    let text = wake(&mut again).await;
    assert!(text.contains("ran past the 1s limit"), "{text}");
    assert!(text.contains("Slow from board"), "{text}");
}

#[tokio::test]
async fn turns_wait_for_a_free_one() {
    let mut t = office_with("max_concurrent: 1\n");
    t.start();
    t.board_mails("ada", "One");
    let mut first = t.next().await;
    wake(&mut first).await;
    let busy = first.name.clone();
    let other = if busy == "ada" { "bob" } else { "ada" };
    t.board_mails(other, "Two");
    t.state(other, State::Queued).await;
    end_turn(&mut first).await;
    let mut second = t.next().await;
    assert_eq!(second.name, other);
    assert!(wake(&mut second).await.contains("Two"));
}

#[tokio::test]
async fn workers_mail_each_other_and_reply_in_the_thread() {
    let t = office_with("");
    let sent = t
        .tool(
            "ada",
            "mail_send",
            json!({ "to": "bob", "subject": "Numbers", "body": "How many?", "due": "in 2h" }),
        )
        .await
        .unwrap();
    let id = sent["sent"].as_str().unwrap().to_string();
    assert_eq!(
        *t.office.worker("bob").unwrap().queue.lock().unwrap(),
        vec![id.clone()]
    );
    let desk = t.tool("bob", "desk", json!({})).await.unwrap();
    assert_eq!(desk["owed"].as_array().unwrap().len(), 1);
    let reply = t
        .tool("bob", "mail_reply", json!({ "message": id, "body": "42" }))
        .await
        .unwrap();
    assert_eq!(reply["thread"], sent["thread"]);
    let inbox = t.tool("ada", "mail_inbox", json!({})).await.unwrap();
    assert_eq!(inbox["messages"][0]["subject"], "Re: Numbers");
    assert!(t.tool("ada", "desk", json!({})).await.unwrap()["waiting"]
        .as_array()
        .unwrap()
        .is_empty());
    let read = t
        .tool("ada", "mail_read", json!({ "message": reply["sent"] }))
        .await
        .unwrap();
    assert_eq!(read["body"], "42");
    let dir = t.tool("bob", "directory", json!({})).await.unwrap();
    assert_eq!(dir["workers"][0]["about"], "counts");
    // The board's mail to ada is hers alone.
    let private = t.board_mails("ada", "Just you");
    let e = t
        .tool("bob", "mail_read", json!({ "message": private }))
        .await
        .unwrap_err();
    assert!(e.contains("no message"), "{e}");
    let e = t
        .tool("ada", "mail_send", json!({ "to": "eve", "body": "x" }))
        .await
        .unwrap_err();
    assert!(e.contains("nobody is"), "{e}");
    assert!(t.journal().iter().any(|e| e["type"] == "op_error"));
}

#[tokio::test]
async fn a_file_put_in_an_outbox_is_published_to_the_shared_drive() {
    let t = office_with("");
    let parts: Vec<String> = ["outbox", "ada", "report.md"].map(String::from).to_vec();
    let mut w = t.office.files.begin_write(&parts).unwrap();
    w.file().write_all(b"# Report").unwrap();
    w.finish().unwrap();
    let p = t
        .tool(
            "ada",
            "drive_publish",
            json!({ "path": "report.md", "as": "q3/report.md" }),
        )
        .await
        .unwrap();
    assert_eq!(p["published"], "ada/q3/report.md");
    let list = t
        .tool("bob", "drive_list", json!({ "path": "ada/q3" }))
        .await
        .unwrap();
    assert_eq!(list["entries"][0]["name"], "report.md");
    let f = t
        .tool("bob", "drive_fetch", json!({ "path": "ada/q3/report.md" }))
        .await
        .unwrap();
    assert_eq!(
        f["url"],
        "http://host.openshell.internal:8970/drive/inbox/ada/q3/report.md"
    );
    assert_eq!(f["size"], 8);
    let e = t
        .tool("bob", "drive_publish", json!({ "path": "report.md" }))
        .await
        .unwrap_err();
    assert!(e.contains("no report.md in your outbox"), "{e}");
    let e = t
        .tool("ada", "drive_publish", json!({ "path": "../bob/x" }))
        .await
        .unwrap_err();
    assert!(e.contains("isn't a name"), "{e}");
}

#[tokio::test]
async fn the_edges_callers_are_the_sandboxes_the_warden_made_for_the_roster() {
    let mut t = office_with("");
    let backend = crate::edge::OfficeBackend(t.office.clone());
    assert!(backend.caller("id-ada").is_none(), "no sandbox yet");
    t.start();
    t.board_mails("ada", "Hi");
    let _agent = t.next().await;
    let c = backend.caller("id-ada").expect("ada's sandbox");
    assert_eq!(c.labels[crate::edge::WORKER], "ada");
    assert!(backend.caller("id-eve").is_none());
    assert!(t.warden.made.lock().unwrap().contains(&"ada".to_string()));
}

#[tokio::test]
async fn a_sandbox_the_warden_wont_make_is_an_error_until_it_does() {
    let t = office_with("");
    t.warden
        .fail_ensure
        .store(1, std::sync::atomic::Ordering::Relaxed);
    t.start();
    t.state("ada", State::Error).await;
    let detail = t
        .office
        .worker("ada")
        .unwrap()
        .state
        .lock()
        .unwrap()
        .1
        .clone();
    assert!(detail.unwrap().contains("429"));
}

/// A raw HTTP request to the board at `addr`: the status and the body.
async fn http(addr: std::net::SocketAddr, req: String) -> (u16, String) {
    let mut c = tokio::net::TcpStream::connect(addr).await.unwrap();
    c.write_all(req.as_bytes()).await.unwrap();
    let mut out = String::new();
    c.read_to_string(&mut out).await.unwrap();
    let status = out[9..12].parse().unwrap();
    let body = out
        .split_once("\r\n\r\n")
        .map(|(_, b)| b.to_string())
        .unwrap_or_default();
    (status, body)
}

#[tokio::test]
async fn the_board_needs_its_token_and_its_writes_their_header() {
    let t = office_with("");
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let board = crate::board::Board::new(t.office.clone(), "tok".into());
    tokio::spawn(board.serve(l));
    let get = |auth: &str| {
        format!("GET /api/workers HTTP/1.1\r\nHost: {addr}\r\n{auth}Connection: close\r\n\r\n")
    };
    assert_eq!(http(addr, get("")).await.0, 401);
    assert_eq!(
        http(addr, get("Authorization: Bearer nope\r\n")).await.0,
        401
    );
    let (s, body) = http(addr, get("Authorization: Bearer tok\r\n")).await;
    assert_eq!(s, 200);
    assert!(body.contains("\"ada\""), "{body}");
    let mail = r#"{"to":["ada"],"subject":"Hi","body":"Hello"}"#;
    let post = |host: &str, extra: &str| {
        format!("POST /api/mail HTTP/1.1\r\nHost: {host}\r\nCookie: zc_board=tok\r\n{extra}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{mail}", mail.len())
    };
    let host = addr.to_string();
    assert_eq!(http(addr, post(&host, "")).await.0, 403, "no X-Zapper-UI");
    assert_eq!(
        http(addr, post("evil.example", "X-Zapper-UI: 1\r\n"))
            .await
            .0,
        403
    );
    let other_origin = "X-Zapper-UI: 1\r\nOrigin: http://evil.example\r\n";
    assert_eq!(http(addr, post(&host, other_origin)).await.0, 403);
    let ok = format!("X-Zapper-UI: 1\r\nOrigin: http://{host}\r\n");
    let (s, body) = http(addr, post(&host, &ok)).await;
    assert_eq!(s, 200, "{body}");
    assert_eq!(
        *t.office.worker("ada").unwrap().queue.lock().unwrap(),
        vec!["m-1".to_string()]
    );
    let (s, _) = http(
        addr,
        format!("GET /?token=tok HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n"),
    )
    .await;
    assert_eq!(s, 303, "the token sets the cookie");
}

#[tokio::test]
async fn an_office_started_again_numbers_on_from_its_journal() {
    let t = office_with("");
    let first = t.board_mails("ada", "One");
    let config = t.office.config.clone();
    let (warden, _agents) = FakeWarden::new();
    let again = Office::new(config, Arc::new(warden)).unwrap();
    let sent = again
        .send(Draft {
            from: BOARD.into(),
            to: vec!["bob".into()],
            body: "Two".into(),
            ..Draft::default()
        })
        .unwrap();
    assert_eq!(first, "m-1");
    assert_eq!(sent.id, "m-2", "not m-1 again");
    assert_eq!(sent.thread, "t-2");
}

#[tokio::test]
async fn mail_handed_over_in_a_turn_that_then_fails_is_shown_again() {
    let mut t = office_with("");
    t.start();
    t.board_mails("ada", "First");
    let mut agent = t.next().await;
    wake(&mut agent).await;
    t.state("ada", State::Working).await;
    t.board_mails("ada", "Second");
    let hook = t.tool("ada", "stop_check", json!({})).await.unwrap();
    assert_eq!(hook["block"], true);
    agent.exit(1).await;
    let mut again = t.next().await;
    let text = wake(&mut again).await;
    assert!(text.contains("First") && text.contains("Second"), "{text}");
}

#[tokio::test]
async fn an_answer_to_a_reminder_goes_to_who_asked_and_settles_it() {
    let t = office_with("");
    let asked = t
        .tool(
            "ada",
            "mail_send",
            json!({ "to": ["bob"], "subject": "Numbers", "body": "How many?", "due": "in 2m" }),
        )
        .await
        .unwrap();
    let due = t.office.post.lock().unwrap().messages[asked["sent"].as_str().unwrap()]
        .due
        .unwrap();
    let reminders = t
        .office
        .post
        .lock()
        .unwrap()
        .tick(&t.office.journal, due + 1.0);
    assert_eq!(reminders.len(), 1);
    let answer = t
        .tool(
            "bob",
            "mail_reply",
            json!({ "message": reminders[0].id, "body": "42" }),
        )
        .await
        .unwrap();
    let m = t.office.post.lock().unwrap().messages[answer["sent"].as_str().unwrap()].clone();
    assert_eq!(m.to, vec!["ada"], "to who asked, not the board");
    assert_eq!(m.in_reply_to.as_deref(), asked["sent"].as_str());
    assert!(
        t.office.post.lock().unwrap().desk("ada").waiting.is_empty(),
        "settled"
    );
    // Mail to someone else in the thread settles nothing.
    let again = t
        .tool(
            "ada",
            "mail_send",
            json!({ "to": ["bob"], "body": "And colours?", "due": "in 2m" }),
        )
        .await
        .unwrap();
    t.tool(
        "bob",
        "mail_reply",
        json!({ "message": again["sent"], "to": ["board"], "body": "ask the board" }),
    )
    .await
    .unwrap();
    assert_eq!(t.office.post.lock().unwrap().desk("ada").waiting.len(), 1);
}
