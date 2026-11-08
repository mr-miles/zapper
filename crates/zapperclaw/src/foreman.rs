//! The foreman: drives one worker's agent headless, through its harness
//! (`zapper_driver::harness`), over the stream the warden connects to the
//! office's agent port.
//!
//! Mail is delivered as one input line (the wake) when the worker is idle
//! and a turn is free, and queued while it works; the harness says when a
//! turn ended. Every output line goes to the worker's transcript. As the
//! office's foreman, less the budgets and the officers: it keeps the turn
//! limit, the Stop hook's extensions and `--resume`.

use std::collections::VecDeque;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use serde_json::{json, Value};
use zapper_driver::harness::{self, Outcome};
use zapper_driver::process::{self as driver, Observer, Out, Proc};

use crate::journal::Journal;
use crate::{now, prompts, Office, State, Worker};

/// Restarts allowed within ten minutes before the worker is marked broken.
const MAX_RESTARTS: usize = 5;
/// Turns in a row that may run out of time before the worker rests.
const MAX_TIMEOUTS: u32 = 3;
/// Extensions of one turn by the Stop hook.
const MAX_EXTENSIONS: u32 = 3;

/// Where the agent's output goes.
struct Watch {
    office: Arc<Office>,
    w: Arc<Worker>,
    transcript: Arc<Journal>,
}

impl Observer for Watch {
    fn line(&self, line: Value) {
        self.transcript.write(line);
    }

    fn note(&self, kind: &str, detail: Value) {
        self.transcript.append(kind, detail);
    }

    fn conversation(&self, id: &str) {
        let mut c = self.w.conversation.lock().unwrap();
        if c.as_deref() != Some(id) {
            *c = Some(id.to_string());
            drop(c);
            // Kept, for a restarted office to resume it.
            self.office
                .journal
                .append("conversation", json!({ "worker": self.w.name, "id": id }));
        }
    }

    fn activity(&self, a: &str) {
        if self.w.state() == State::Working {
            self.office
                .set_state(&self.w, State::Working, Some(a.to_string()));
        }
    }
}

async fn start(office: &Arc<Office>, w: &Arc<Worker>, transcript: Arc<Journal>) -> Result<Proc> {
    // The harness the warden runs for the worker's profile.
    let kind = w
        .sandbox
        .lock()
        .unwrap()
        .as_ref()
        .map(|s| s.kind.clone())
        .unwrap_or_default();
    let resume = w.conversation.lock().unwrap().clone();
    let conv = harness::headless(&kind)?.conversation(resume.as_deref());
    let stream = office.backend.agent(&w.name, resume.as_deref()).await?;
    if resume.is_none() {
        w.briefed.store(false, Ordering::Relaxed);
    }
    let watch = Watch {
        office: office.clone(),
        w: w.clone(),
        transcript,
    };
    Ok(driver::start(stream, conv, Arc::new(watch)))
}

fn idle_state(office: &Office, w: &Worker) -> State {
    if office.post.lock().unwrap().desk(&w.name).waiting.is_empty() {
        State::Idle
    } else {
        State::Waiting
    }
}

/// Waits for a reason to look again, or `secs`.
async fn nap(office: &Office, w: &Worker, secs: u64) {
    tokio::select! {
        _ = w.wake.notified() => {}
        _ = office.stopping.notified() => {}
        _ = tokio::time::sleep(Duration::from_secs(secs)) => {}
    }
}

/// The brief, the first time a conversation is woken.
fn brief_for(office: &Office, w: &Worker) -> Option<String> {
    if w.briefed.swap(true, Ordering::Relaxed) {
        return None;
    }
    let instructions = match office.config.instructions(&w.name) {
        Ok(i) => i,
        Err(e) => {
            tracing::warn!("{e:#}");
            None
        }
    };
    let names: Vec<String> = office.workers.keys().cloned().collect();
    Some(prompts::brief(&w.name, instructions.as_deref(), &names))
}

pub async fn run(office: Arc<Office>, w: Arc<Worker>) {
    let path = office
        .dir
        .join("transcripts")
        .join(format!("{}.jsonl", w.name));
    let transcript = match Journal::create(&path) {
        Ok(j) => Arc::new(j),
        Err(e) => {
            office.set_state(&w, State::Error, Some(format!("transcript: {e:#}")));
            return;
        }
    };
    let turn_timeout = office
        .config
        .turn_timeout()
        .unwrap_or(Duration::from_secs(1800));
    let limit = office.config.turn_timeout.clone();
    let mut proc: Option<Proc> = None;
    let mut restarts: VecDeque<f64> = VecDeque::new();
    let mut backoff_until = 0.0;
    let mut timeouts = 0;
    office.set_state(&w, idle_state(&office, &w), None);
    loop {
        if office.stopped() {
            return;
        }
        let has_mail = !w.queue.lock().unwrap().is_empty() || !w.notes.lock().unwrap().is_empty();
        if !has_mail || now() < backoff_until {
            let st = match w.state() {
                State::Error => State::Error,
                _ => idle_state(&office, &w),
            };
            let detail = (st == State::Error)
                .then(|| w.state.lock().unwrap().1.clone())
                .flatten();
            office.set_state(&w, st, detail);
            nap(&office, &w, 5).await;
            continue;
        }
        let permit = match office.turns.clone().try_acquire_owned() {
            Ok(p) => p,
            Err(_) => {
                office.set_state(&w, State::Queued, None);
                tokio::select! {
                    p = office.turns.clone().acquire_owned() => match p { Ok(p) => p, Err(_) => return },
                    _ = office.stopping.notified() => return,
                }
            }
        };
        if office.stopped() {
            return;
        }
        if proc.is_none() {
            match start(&office, &w, transcript.clone()).await {
                Ok(p) => proc = Some(p),
                Err(e) => {
                    office.set_state(&w, State::Error, Some(format!("starting the agent: {e:#}")));
                    drop(permit);
                    backoff_until = now() + 30.0;
                    continue;
                }
            }
        }
        let ids: Vec<String> = std::mem::take(&mut *w.queue.lock().unwrap());
        let notes: Vec<String> = std::mem::take(&mut *w.notes.lock().unwrap());
        let brief = brief_for(&office, &w);
        let prompt = {
            let mut po = office.post.lock().unwrap();
            po.mark_read(&w.name, &ids);
            prompts::wake(
                &po,
                &office.journal,
                &w.name,
                &ids,
                &notes,
                brief.as_deref(),
            )
        };
        w.extended.store(0, Ordering::Relaxed);
        w.handed.lock().unwrap().clear();
        office.set_state(&w, State::Working, None);
        office.journal.append(
            "turn",
            json!({ "worker": w.name, "event": "start", "messages": ids }),
        );
        transcript.append("wake", json!({ "text": prompt }));
        let started = Instant::now();
        let deadline = tokio::time::Instant::now() + turn_timeout;
        let p = proc.as_mut().unwrap();
        let more = || hand_over(&office, &w, "steered");
        let Some(outcome) = p
            .turn(&prompt, deadline, &w.wake, &office.stopping, more)
            .await
        else {
            return;
        };
        let ms = started.elapsed().as_millis() as u64;
        // The mail handed over during the turn is the turn's too: shown
        // again with the rest if the turn has to be.
        let mut ids = ids;
        ids.extend(std::mem::take(&mut *w.handed.lock().unwrap()));
        match outcome {
            Out::End(end) => match end.outcome {
                Outcome::RateLimited => {
                    requeue(&w, ids);
                    backoff_until = now() + 60.0;
                    office.journal.append(
                        "turn",
                        json!({ "worker": w.name, "event": "rate_limited", "ms": ms }),
                    );
                }
                Outcome::Failed(why) => {
                    requeue(&w, ids);
                    backoff_until = now() + 60.0;
                    office.journal.append(
                        "turn",
                        json!({ "worker": w.name, "event": "failed", "ms": ms, "error": why }),
                    );
                    office.set_state(&w, State::Error, Some(why));
                }
                _ => {
                    let mut rec =
                        json!({ "worker": w.name, "event": "end", "ms": ms, "tokens": end.tokens });
                    zapper_policy::merge(&mut rec, &end.detail);
                    office.journal.append("turn", rec);
                    timeouts = 0;
                    if w.state() == State::Error {
                        office.set_state(&w, idle_state(&office, &w), None);
                    }
                }
            },
            Out::Late => {
                // Dropping the process hangs the agent up; the next turn
                // resumes the conversation, with a note of what this was on.
                proc = None;
                let subjects: Vec<String> = {
                    let po = office.post.lock().unwrap();
                    ids.iter()
                        .filter_map(|id| po.messages.get(id))
                        .map(|m| format!("{} from {}", m.subject, m.from))
                        .collect()
                };
                w.notes
                    .lock()
                    .unwrap()
                    .push(prompts::timeout_note(&limit, &subjects));
                office.journal.append(
                    "turn",
                    json!({ "worker": w.name, "event": "timed_out", "ms": ms, "messages": ids }),
                );
                timeouts += 1;
                if timeouts >= MAX_TIMEOUTS {
                    timeouts = 0;
                    let why = format!("{MAX_TIMEOUTS} turns in a row ran past {limit}");
                    let rest = turn_timeout.as_secs_f64().min(600.0);
                    office.set_state(&w, State::Error, Some(why.clone()));
                    let notice = crate::mail::Draft {
                        from: crate::mail::OFFICE.into(),
                        to: vec![crate::mail::BOARD.into()],
                        subject: format!("{} is stuck", w.name),
                        body: format!("[Automatic notice] {}: {why}. They rest for {} minutes. Ask less of them at a time.", w.name, (rest / 60.0).ceil()),
                        ..Default::default()
                    };
                    if let Err(e) = office.send(notice) {
                        tracing::warn!("{e:#}");
                    }
                    backoff_until = now() + rest;
                }
            }
            Out::Exit(code) => {
                proc = None;
                requeue(&w, ids);
                let at = now();
                w.notes.lock().unwrap().push(prompts::restart_note());
                office.journal.append(
                    "turn",
                    json!({ "worker": w.name, "event": "interrupted", "ms": ms, "exit_code": code }),
                );
                restarts.push_back(at);
                while restarts.front().is_some_and(|t| at - t > 600.0) {
                    restarts.pop_front();
                }
                if restarts.len() > MAX_RESTARTS {
                    office.set_state(
                        &w,
                        State::Error,
                        Some(format!(
                            "the agent exited {} times in ten minutes (last code {code:?})",
                            restarts.len()
                        )),
                    );
                    backoff_until = now() + 120.0;
                    restarts.clear();
                }
            }
        }
        drop(permit);
    }
}

/// Claude is about to end a turn: mail that came meanwhile is handed over
/// now, and the turn goes on. Delivered so, it doesn't wake the worker
/// again.
pub fn stop_check(office: &Office, w: &Worker) -> Value {
    if w.state() != State::Working || w.extended.load(Ordering::Relaxed) >= MAX_EXTENSIONS {
        return json!({ "block": false });
    }
    let Some(text) = hand_over(office, w, "extended") else {
        return json!({ "block": false });
    };
    w.extended.fetch_add(1, Ordering::Relaxed);
    json!({ "block": true, "reason": format!("Before you stop: mail arrived during this turn. Handle it now if it matters to what you are finishing; it won't be shown again.\n\n{text}") })
}

/// Mail that came during a turn, taken from the queue and marked read.
fn hand_over(office: &Office, w: &Worker, how: &str) -> Option<String> {
    let ids: Vec<String> = std::mem::take(&mut *w.queue.lock().unwrap());
    if ids.is_empty() {
        return None;
    }
    let text = {
        let mut po = office.post.lock().unwrap();
        po.mark_read(&w.name, &ids);
        prompts::wake(&po, &office.journal, &w.name, &ids, &[], None)
    };
    w.handed.lock().unwrap().extend(ids.iter().cloned());
    office.journal.append(
        "turn",
        json!({ "worker": w.name, "event": how, "messages": ids }),
    );
    Some(text)
}

/// Puts undelivered mail back at the front of the queue.
fn requeue(w: &Worker, ids: Vec<String>) {
    let mut q = w.queue.lock().unwrap();
    let rest = std::mem::take(&mut *q);
    *q = ids.into_iter().chain(rest).collect();
}
