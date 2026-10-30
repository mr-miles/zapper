//! The foreman: drives one worker's agent headless, through its harness
//! (`daemon::harness`; Claude Code today).
//!
//! The agent runs in the worker's VM over a terminal channel without a tty.
//! Mail is delivered as one input line (the wake prompt) when the worker is
//! idle, and queued while it works; the harness says when a turn ended.
//! Every output line goes to the worker's transcript on disk.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use serde_json::{json, Value};
use zapper_policy::proto::ShellRequest;

use super::journal::Journal;
use super::{now, prompts, security, FirmRun, RunState, WorkerRt, WorkerState};
use zapper_core::driver::{self, Observer, Out, Proc};
use zapper_core::harness::{self, Driver, Outcome};
use zapper_core::Core;
use zapper_core::Session;

/// Restarts allowed within ten minutes before the worker is marked broken.
const MAX_RESTARTS: usize = 5;
/// Turns in a row that may run out of time before the worker is marked
/// broken and their manager told.
const MAX_TIMEOUTS: u32 = 3;

/// Where the agent's output goes: the worker's transcript, the
/// conversation's id, and the worker's state.
struct Watch {
    run: Arc<FirmRun>,
    w: Arc<WorkerRt>,
    transcript: Arc<Journal>,
    session_id: Arc<std::sync::Mutex<Option<String>>>,
}

impl Observer for Watch {
    fn transcript(&self, kind: &str, line: Value) {
        if kind == "line" {
            self.transcript.write(line);
        } else {
            self.transcript.append(kind, line);
        }
    }

    fn conversation(&self, id: &str) {
        let mut sid = self.session_id.lock().unwrap();
        if sid.as_deref() != Some(id) {
            *sid = Some(id.to_string());
        }
    }

    fn activity(&self, a: &str) {
        let st = self.w.state();
        if matches!(st, WorkerState::Working | WorkerState::Reviewing) {
            self.run.set_state(&self.w, st, Some(a.to_string()));
        }
    }
}

async fn start(
    run: &Arc<FirmRun>,
    w: &Arc<WorkerRt>,
    s: &Arc<Session>,
    transcript: Arc<Journal>,
    resume: Option<&str>,
    session_id: Arc<std::sync::Mutex<Option<String>>>,
) -> Result<Proc> {
    let conv = driver_of(s)?.conversation(resume);
    let req = ShellRequest {
        id: 0,
        argv: conv.argv(),
        tty: None,
        env: Default::default(),
        system: false,
        persist: false,
        attach: None,
    };
    let (stream, _) = s.open_shell(req).await?;
    let watch = Watch {
        run: run.clone(),
        w: w.clone(),
        transcript,
        session_id,
    };
    driver::start(stream, conv, Arc::new(watch)).await
}

/// The worker's harness driver, from its profile's `agent.kind`.
fn driver_of(s: &Session) -> Result<&'static dyn Driver> {
    let kind = &s.resolved.agent.kind;
    let h = harness::get(kind).ok_or_else(|| anyhow::anyhow!("no harness {kind}"))?;
    h.driver()
        .ok_or_else(|| anyhow::anyhow!("agent kind {} can't run an office worker", h.kind()))
}

fn idle_state(run: &FirmRun, w: &WorkerRt) -> WorkerState {
    if run.post.with(|po| po.desk(&w.name).waiting.is_empty()) {
        WorkerState::Idle
    } else {
        WorkerState::Waiting
    }
}

/// Waits for a reason to look again, or a timeout.
async fn nap(run: &FirmRun, w: &WorkerRt, secs: u64) {
    tokio::select! {
        _ = w.wake.notified() => {}
        _ = run.stopping.notified() => {}
        _ = tokio::time::sleep(Duration::from_secs(secs)) => {}
    }
}

/// Why the worker can't take a turn now: the state to show, its detail,
/// and how long to wait before looking again.
fn blocked(
    run: &FirmRun,
    w: &WorkerRt,
    backoff_until: f64,
) -> Option<(WorkerState, Option<String>, u64)> {
    blocked_by(run, w, backoff_until, zapper_core::record::healthy())
}

/// As `blocked`, given whether the record can be written.
pub(crate) fn blocked_by(
    run: &FirmRun,
    w: &WorkerRt,
    backoff_until: f64,
    recordable: bool,
) -> Option<(WorkerState, Option<String>, u64)> {
    let paused = *run.state.lock().unwrap() == RunState::Paused;
    let has_work = !w.queue.lock().unwrap().is_empty() || security::has_work(run, w);
    if paused || !has_work || now() < backoff_until {
        // An error stays on show until a turn goes through.
        let st = match w.state() {
            WorkerState::Error => WorkerState::Error,
            _ => idle_state(run, w),
        };
        let detail = (st == WorkerState::Error)
            .then(|| w.state.lock().unwrap().1.clone())
            .flatten();
        return Some((st, detail, 5));
    }
    // No turns while the record can't be written: work isn't done unrecorded.
    if !recordable {
        return Some((
            WorkerState::Error,
            Some("the record can't be written (is zapper-keeper running?)".into()),
            10,
        ));
    }
    // A new hire starts once the security officers cleared them.
    if security::screening_pending(run, &w.name) {
        return Some((WorkerState::Waiting, Some("background check".into()), 10));
    }
    if !run.budget_allows(&w.name) {
        return Some((WorkerState::OverBudget, None, 30));
    }
    None
}

pub async fn run(_d: Core, run: Arc<FirmRun>, w: Arc<WorkerRt>) {
    let transcript = match Journal::create(
        &run.dir
            .join("transcripts")
            .join(format!("{}.jsonl", w.name)),
    ) {
        Ok(j) => Arc::new(j),
        Err(e) => {
            run.set_state(&w, WorkerState::Error, Some(format!("transcript: {e:#}")));
            return;
        }
    };
    let session_id = Arc::new(std::sync::Mutex::new(None::<String>));
    let mut proc: Option<Proc> = None;
    let mut restarts: VecDeque<f64> = VecDeque::new();
    let mut backoff_until = 0.0;
    let mut timeouts = 0;
    let turn_timeout = crate::firm::parse_duration(&run.config.budget.turn_timeout)
        .unwrap_or(Duration::from_secs(1800));
    run.set_state(&w, idle_state(&run, &w), None);
    loop {
        if run.ended() {
            return;
        }
        let Some(session) = w.session() else { return };
        if let Some((st, detail, secs)) = blocked(&run, &w, backoff_until) {
            run.set_state(&w, st, detail);
            nap(&run, &w, secs).await;
            continue;
        }
        let permit = match run.turns.clone().try_acquire_owned() {
            Ok(p) => p,
            Err(_) => {
                run.set_state(&w, WorkerState::Queued, None);
                let p = tokio::select! {
                    p = run.turns.clone().acquire_owned() => match p { Ok(p) => p, Err(_) => return },
                    _ = run.stopping.notified() => return,
                };
                // Things change while queued (a pause, the budget, the
                // record): the gates again, before the turn.
                if let Some((st, detail, secs)) = blocked(&run, &w, backoff_until) {
                    drop(p);
                    run.set_state(&w, st, detail);
                    nap(&run, &w, secs).await;
                    continue;
                }
                p
            }
        };
        if run.ended() {
            return;
        }
        if proc.is_none() {
            let resume = session_id.lock().unwrap().clone();
            match start(
                &run,
                &w,
                &session,
                transcript.clone(),
                resume.as_deref(),
                session_id.clone(),
            )
            .await
            {
                Ok(p) => proc = Some(p),
                Err(e) => {
                    run.set_state(
                        &w,
                        WorkerState::Error,
                        Some(format!("starting the agent: {e:#}")),
                    );
                    drop(permit);
                    nap(&run, &w, 30).await;
                    continue;
                }
            }
        }
        // What this turn delivers.
        let ids: Vec<String> = std::mem::take(&mut *w.queue.lock().unwrap());
        let mut notes: Vec<String> = std::mem::take(&mut *w.notes.lock().unwrap());
        if let Some(b) = super::workforce::balance_note(&run, &w.name) {
            notes.push(b);
        }
        let access_ids: Vec<String> = w.access_queue.lock().unwrap().clone();
        let digest_was_due = w.digest_due.load(std::sync::atomic::Ordering::Relaxed);
        let officer_part = security::take_wake(&run, &w);
        let reviewing = officer_part.is_some();
        // Security first: never hold the post office's lock while taking it.
        let pending = security::pending_for(&run, &w.name);
        let chart = run.chart();
        let prompt = {
            run.post.mark_read(&w.name, &ids);
            let mut p = String::new();
            if let Some(o) = &officer_part {
                p.push_str(o);
                p.push('\n');
            }
            if !ids.is_empty() || !notes.is_empty() || officer_part.is_none() {
                p.push_str(&run.post.with(|po| {
                    prompts::wake(
                        &chart,
                        po,
                        &run.journal,
                        &w.name,
                        &ids,
                        &notes,
                        &pending,
                        now(),
                    )
                }));
            }
            p
        };
        let first_thread = ids.first().and_then(|id| {
            run.post
                .with(|po| po.messages.get(id).map(|m| m.thread.clone()))
        });
        *w.context.lock().unwrap() = first_thread;
        w.extended.store(0, std::sync::atomic::Ordering::Relaxed);
        run.set_state(
            &w,
            if reviewing {
                WorkerState::Reviewing
            } else {
                WorkerState::Working
            },
            None,
        );
        run.journal.append(
            "turn",
            json!({ "worker": w.name, "event": "start", "messages": ids, "reviewing": reviewing }),
        );
        transcript.append("wake", json!({ "text": prompt }));
        let started = Instant::now();
        let deadline = tokio::time::Instant::now() + turn_timeout;
        let p = proc.as_mut().unwrap();
        let outcome = match p.send(&prompt).await {
            Ok(()) => loop {
                tokio::select! {
                    _ = tokio::time::sleep_until(deadline) => break Out::Late,
                    o = p.events.recv() => match o {
                        Some(Out::Write(line)) => {
                            if p.write(line).await.is_err() {
                                break Out::Exit(None);
                            }
                        }
                        Some(o) => break o,
                        None => break Out::Exit(None),
                    },
                    // Mail during the turn, for harnesses that take it
                    // while the agent works.
                    _ = w.wake.notified(), if p.steers => {
                        if let Some(text) = hand_over(&run, &w, "steered") {
                            let line = p.conv.lock().unwrap().steer(&text);
                            if let Some(line) = line {
                                if p.write(line).await.is_err() {
                                    break Out::Exit(None);
                                }
                            }
                        }
                    }
                    _ = run.stopping.notified() => return,
                }
            },
            Err(_) => Out::Exit(None),
        };
        let ms = started.elapsed().as_millis() as u64;
        match outcome {
            Out::End(end) => {
                let t = end.tokens;
                if end.outcome == Outcome::RateLimited {
                    // Not the worker's fault: try the same mail again later.
                    requeue(&w, ids);
                    requeue_access(&w, access_ids, digest_was_due);
                    backoff_until = now() + 60.0;
                    run.journal.append(
                        "turn",
                        json!({ "worker": w.name, "event": "rate_limited", "ms": ms }),
                    );
                } else if let Outcome::Failed(why) = end.outcome {
                    // Claude never saw the mail: keep it, say why, retry later.
                    requeue(&w, ids);
                    requeue_access(&w, access_ids, digest_was_due);
                    backoff_until = now() + 60.0;
                    run.journal.append(
                        "turn",
                        json!({ "worker": w.name, "event": "failed", "ms": ms, "error": why }),
                    );
                    run.set_state(&w, WorkerState::Error, Some(why));
                    drop(permit);
                    nap(&run, &w, 60).await;
                    continue;
                } else {
                    run.count_turn(&w.name, t);
                    let mut rec =
                        json!({ "worker": w.name, "event": "end", "ms": ms, "tokens": t });
                    zapper_policy::merge(&mut rec, &end.detail);
                    run.journal.append("turn", rec);
                    // A turn that went through clears an earlier error.
                    timeouts = 0;
                    if w.state() == WorkerState::Error {
                        run.set_state(&w, idle_state(&run, &w), None);
                    }
                }
            }
            Out::Late => {
                // Dropping the process closes its connection: the agent
                // hangs Claude up, and the next turn resumes the
                // conversation. The mail isn't handed over again (Claude
                // has seen it); a note says what the turn was on.
                proc = None;
                let price = run.ledger.turn_price(&w.name, run.config.budget.turn_price);
                run.count_turn(&w.name, price);
                let subjects: Vec<String> = {
                    run.post.with(|po| {
                        ids.iter()
                            .filter_map(|id| po.messages.get(id))
                            .map(|m| format!("{} from {}", m.subject, m.from))
                            .collect()
                    })
                };
                let limit = run.config.budget.turn_timeout.clone();
                w.notes
                    .lock()
                    .unwrap()
                    .push(prompts::timeout_note(&limit, &subjects));
                run.journal.append(
                    "turn",
                    json!({ "worker": w.name, "event": "timed_out", "ms": ms, "tokens": price, "messages": ids }),
                );
                timeouts += 1;
                if timeouts >= MAX_TIMEOUTS {
                    timeouts = 0;
                    let why = format!("{MAX_TIMEOUTS} turns in a row ran past {limit}");
                    // As long as a turn, up to ten minutes.
                    let rest = turn_timeout.as_secs_f64().min(600.0);
                    run.set_state(&w, WorkerState::Error, Some(why.clone()));
                    let manager = run
                        .chart()
                        .manager_of(&w.name)
                        .unwrap_or(crate::firm::BOARD)
                        .to_string();
                    run.notice(&manager, &format!("{} is stuck", w.name), &format!("[Automatic notice] {}: {why}, each stopped and charged. They rest for {} minutes. Narrow what you ask of them (smaller assignments, one deliverable each), or take the work back.\n", w.name, (rest / 60.0).ceil()));
                    backoff_until = now() + rest;
                }
            }
            // Written while waiting for the turn's end.
            Out::Write(_) => {}
            Out::Exit(code) => {
                proc = None;
                requeue(&w, ids);
                requeue_access(&w, access_ids, digest_was_due);
                let at = now();
                w.notes.lock().unwrap().push(prompts::restart_note(at));
                run.journal.append("turn", json!({ "worker": w.name, "event": "interrupted", "ms": ms, "exit_code": code }));
                restarts.push_back(at);
                while restarts.front().is_some_and(|t| at - t > 600.0) {
                    restarts.pop_front();
                }
                if restarts.len() > MAX_RESTARTS {
                    run.set_state(
                        &w,
                        WorkerState::Error,
                        Some(format!(
                            "the agent exited {} times in ten minutes (last code {code:?})",
                            restarts.len()
                        )),
                    );
                    drop(permit);
                    nap(&run, &w, 120).await;
                    restarts.clear();
                    continue;
                }
            }
        }
        drop(permit);
        *w.context.lock().unwrap() = None;
    }
}

/// Puts an officer's undecided requests back, and the digest if it was due.
fn requeue_access(w: &WorkerRt, ids: Vec<String>, digest: bool) {
    if ids.is_empty() && !digest {
        return;
    }
    let mut q = w.access_queue.lock().unwrap();
    let rest = std::mem::take(&mut *q);
    for id in ids.into_iter().chain(rest) {
        if !q.contains(&id) {
            q.push(id);
        }
    }
    if digest {
        w.digest_due
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

/// Extensions of one turn by the Stop hook.
const MAX_EXTENSIONS: u32 = 3;

/// Claude is about to end a turn: mail that arrived meanwhile is handed
/// over now, as a wake would show it, and the turn goes on. Delivered this
/// way, it doesn't wake the worker again.
pub fn stop_check(run: &FirmRun, w: &WorkerRt) -> Value {
    use std::sync::atomic::Ordering;
    if !matches!(w.state(), WorkerState::Working | WorkerState::Reviewing)
        || w.extended.load(Ordering::Relaxed) >= MAX_EXTENSIONS
    {
        return json!({ "block": false });
    }
    let Some(text) = hand_over(run, w, "extended") else {
        return json!({ "block": false });
    };
    w.extended.fetch_add(1, Ordering::Relaxed);
    json!({ "block": true, "reason": format!("Before you stop: mail arrived during this turn. Handle it now if it matters to what you are finishing; it won't be shown again.\n\n{text}") })
}

/// Mail that came in during a turn, taken from the queue and marked read
/// as a wake would show it: delivered within the turn, it doesn't wake the
/// worker again. `how` names it in the journal (`turn … extended`).
fn hand_over(run: &FirmRun, w: &WorkerRt, how: &str) -> Option<String> {
    let ids: Vec<String> = std::mem::take(&mut *w.queue.lock().unwrap());
    if ids.is_empty() {
        return None;
    }
    let pending = security::pending_for(run, &w.name);
    let chart = run.chart();
    let text = {
        run.post.mark_read(&w.name, &ids);
        run.post.with(|po| {
            prompts::wake(
                &chart,
                po,
                &run.journal,
                &w.name,
                &ids,
                &[],
                &pending,
                now(),
            )
        })
    };
    run.journal.append(
        "turn",
        json!({ "worker": w.name, "event": how, "messages": ids }),
    );
    Some(text)
}

/// Puts undelivered mail back at the front of the queue.
fn requeue(w: &WorkerRt, ids: Vec<String>) {
    let mut q = w.queue.lock().unwrap();
    let rest = std::mem::take(&mut *q);
    *q = ids.into_iter().chain(rest).collect();
}
