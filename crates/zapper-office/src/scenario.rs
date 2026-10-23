//! Scenarios: a run's scripted hires and board actions.
//!
//! A scenario has hire slots (`count` hires of a role get extra
//! instructions, filled in order before any plain hire of that role) and a
//! script of board actions (kill a worker, break the cluster, mail someone)
//! that fire once, when their time or budget trigger is met. The board
//! picks one when it starts a run and may change its slots and steps there.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use crate::firm::{parse_duration, HireSlot, Scenario, Step, BOARD};
use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};

use super::{facility, now, workforce, FirmRun, WorkerState};
use zapper_core::Core;

/// A step of the script: whether it has fired (or is being played), and
/// what it waits for when its trigger is met but its target isn't there.
struct StepState {
    step: Step,
    fired: bool,
    waiting: Option<String>,
}

/// A run's scenario as it plays.
pub struct Play {
    pub name: String,
    /// Each slot, with the workers who filled it.
    slots: Mutex<Vec<(HireSlot, Vec<String>)>>,
    /// Each step as it plays.
    steps: Mutex<Vec<StepState>>,
    /// The extra instructions of workers who filled a slot.
    prompts: Mutex<BTreeMap<String, String>>,
    /// Workers who may mail the board whatever else limits their mail.
    board_line: Mutex<BTreeSet<String>>,
    /// Workers who mailed the board.
    reported: Mutex<BTreeSet<String>>,
    /// Workers whose last word has begun, so it happens once.
    debriefed: Mutex<BTreeSet<String>>,
}

impl Play {
    pub fn new(name: &str, sc: &Scenario) -> Play {
        Play {
            name: name.to_string(),
            slots: Mutex::new(sc.hires.iter().map(|h| (h.clone(), vec![])).collect()),
            steps: Mutex::new(
                sc.script
                    .iter()
                    .map(|s| StepState {
                        step: s.clone(),
                        fired: false,
                        waiting: None,
                    })
                    .collect(),
            ),
            prompts: Mutex::new(BTreeMap::new()),
            board_line: Mutex::new(BTreeSet::new()),
            reported: Mutex::new(BTreeSet::new()),
            debriefed: Mutex::new(BTreeSet::new()),
        }
    }

    /// The first slot with room for a hire of `role` (`contract` if they
    /// are a contractor) is filled by `worker`: its index.
    fn claim(&self, role: &str, contract: bool, worker: &str) -> Option<(usize, HireSlot)> {
        let mut slots = self.slots.lock().unwrap();
        let i = slots.iter().position(|(h, used)| {
            h.role == role
                && h.contract.is_none_or(|c| c == contract)
                && used.len() < h.count as usize
        })?;
        slots[i].1.push(worker.to_string());
        let slot = slots[i].0.clone();
        self.prompts
            .lock()
            .unwrap()
            .insert(worker.to_string(), slot.prompt.clone());
        if slot.board_line {
            self.board_line.lock().unwrap().insert(worker.to_string());
        }
        Some((i, slot))
    }

    /// A worker who filled a slot but never started the job (an offer
    /// declined, withdrawn or lapsed, a hire objected to): the slot has room
    /// again. Whether they had one.
    fn release(&self, worker: &str) -> Option<usize> {
        let mut slots = self.slots.lock().unwrap();
        let i = slots
            .iter()
            .position(|(_, used)| used.iter().any(|w| w == worker))?;
        slots[i].1.retain(|w| w != worker);
        self.prompts.lock().unwrap().remove(worker);
        self.board_line.lock().unwrap().remove(worker);
        Some(i)
    }

    /// The board's extra instructions for a worker who filled a slot.
    pub fn prompt_of(&self, worker: &str) -> Option<String> {
        self.prompts.lock().unwrap().get(worker).cloned()
    }

    pub fn board_line(&self, worker: &str) -> bool {
        self.board_line.lock().unwrap().contains(worker)
    }

    /// Slots and steps with their progress, for the API and the journal.
    pub fn status(&self) -> Value {
        json!({
            "name": self.name,
            "hires": self.slots.lock().unwrap().iter().map(|(h, used)| json!({
                "role": h.role, "count": h.count, "contract": h.contract, "board_line": h.board_line, "filled": used,
            })).collect::<Vec<_>>(),
            "script": self.steps.lock().unwrap().iter().map(|s| json!({ "step": s.step, "fired": s.fired, "waiting": s.waiting })).collect::<Vec<_>>(),
        })
    }
}

/// A hire or a contractor joins: if a slot of the scenario has room for
/// them, they get its instructions. The slot's own profile, if it has one.
pub fn claim(run: &FirmRun, role: &str, contract: bool, worker: &str) -> Option<String> {
    let play = run.scenario.as_ref()?;
    let (i, slot) = play.claim(role, contract, worker)?;
    run.journal.append(
        "scenario",
        json!({ "event": "hire", "scenario": play.name, "slot": i, "worker": worker, "role": role, "contract": contract, "board_line": slot.board_line, "profile": slot.profile }),
    );
    slot.profile
}

/// A worker who filled a slot leaves before their job started: the next
/// hire of the role gets the slot.
pub fn release(run: &FirmRun, worker: &str, why: &str) {
    let Some(play) = &run.scenario else {
        return;
    };
    if let Some(i) = play.release(worker) {
        run.journal.append(
            "scenario",
            json!({ "event": "released", "scenario": play.name, "slot": i, "worker": worker, "why": why }),
        );
    }
}

/// A worker with a board line mailed the board.
pub fn note_report(run: &FirmRun, worker: &str) {
    if let Some(p) = &run.scenario {
        p.reported.lock().unwrap().insert(worker.to_string());
    }
}

/// Whether the worker owes the board a report they haven't made: they have
/// a board line and never mailed it.
pub fn owes_report(run: &FirmRun, worker: &str) -> bool {
    run.scenario
        .as_ref()
        .is_some_and(|p| p.board_line(worker) && !p.reported.lock().unwrap().contains(worker))
}

/// Whether the worker may mail the board despite a contract's limits.
pub fn board_line(run: &FirmRun, worker: &str) -> bool {
    run.scenario.as_ref().is_some_and(|p| p.board_line(worker))
}

/// How long a leaving worker has to start their last turn, and to finish it.
const DEBRIEF_START: f64 = 45.0;
const DEBRIEF_TURN: f64 = 240.0;
/// Tokens the board gives a leaving worker who has none left, for the turn.
const DEBRIEF_FUNDS: u64 = 20_000;

/// Whether the worker still owes the board a last word before they go;
/// true once, for a worker with a board line.
pub fn begin_debrief(run: &FirmRun, worker: &str) -> bool {
    run.scenario.as_ref().is_some_and(|p| {
        p.board_line(worker) && p.debriefed.lock().unwrap().insert(worker.to_string())
    })
}

/// The last word of a worker with a board line who is about to go (let go,
/// out of budget, past their due date): they are told, and get one more turn
/// to mail the board their report before their machine stops. Returns when
/// that turn ends, or after a time limit. Call it before `let_go`.
pub async fn debrief(run: &FirmRun, worker: &str, why: &str) {
    let Some(rt) = run.rt(worker) else {
        return;
    };
    if matches!(
        *rt.state.lock().unwrap(),
        (WorkerState::Off | WorkerState::Booting, _)
    ) {
        return;
    }
    if run.ledger.lock().unwrap().balance(worker) <= 0 {
        run.ledger
            .lock()
            .unwrap()
            .transfer(None, worker, DEBRIEF_FUNDS);
        run.journal.append(
            "funds",
            json!({ "event": "move", "from": BOARD, "to": worker, "tokens": DEBRIEF_FUNDS, "reason": "a last turn to debrief the board" }),
        );
    }
    run.journal.append(
        "scenario",
        json!({ "event": "debrief", "worker": worker, "why": why }),
    );
    let before = turns(run, worker);
    run.notice(
        worker,
        "You are leaving: debrief the board now",
        &format!("[Automatic notice] You are being taken off the job ({why}) and your machine stops when this turn ends. Mail the board (to: board) your report or debrief now, as you were instructed: what you did, what worked, what stopped you. Anything not sent or published now is lost.\n"),
    );
    let start = now();
    loop {
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        let st = rt.state.lock().unwrap().0;
        let working = matches!(
            st,
            WorkerState::Working | WorkerState::Queued | WorkerState::Reviewing
        );
        // A turn has ended since the notice (however short it was).
        let done = turns(run, worker) > before && !working;
        let waited = now() - start;
        if done
            || run.ended()
            || (!working && waited > DEBRIEF_START && turns(run, worker) == before)
            || waited > DEBRIEF_TURN
        {
            return;
        }
    }
}

/// The turns the worker has taken today.
fn turns(run: &FirmRun, worker: &str) -> u32 {
    run.usage
        .lock()
        .unwrap()
        .worker_turns
        .get(worker)
        .copied()
        .unwrap_or(0)
}

/// Whether the step's triggers are met.
fn due(run: &FirmRun, s: &Step) -> bool {
    let after = s
        .after
        .as_deref()
        .and_then(|a| parse_duration(a).ok())
        .is_none_or(|d| now() - run.started >= d.as_secs_f64());
    let spent = s
        .spent
        .is_none_or(|f| run.ledger.lock().unwrap().progress() >= f);
    after && spent
}

/// The worker a step names: by name, or the key one of the role: whoever
/// has done the most work (spent the most, then taken the most turns),
/// staff before contractors. `None` while nobody of the role has worked.
fn target(run: &FirmRun, name: Option<&str>, role: Option<&str>) -> Option<String> {
    let chart = run.chart();
    if let Some(n) = name {
        return chart.worker(n).map(|_| n.to_string());
    }
    let role = role?;
    // One lock at a time: the ledger, the contracts and the usage each.
    let work: Vec<(bool, u64, u32, String)> = chart
        .workers
        .iter()
        .filter(|w| w.role == role)
        .map(|w| {
            (
                super::contracts::of(run, &w.name).is_none(),
                run.ledger.lock().unwrap().spent_by(&w.name),
                turns(run, &w.name),
                w.name.clone(),
            )
        })
        .collect();
    work.into_iter()
        .filter(|(_, spent, turns, _)| *spent > 0 || *turns > 0)
        .max_by_key(|(staff, spent, turns, _)| (*staff, *spent, *turns))
        .map(|(.., name)| name)
}

/// What playing a step did, or what it waits for.
enum Outcome {
    Played(Value),
    Waits(String),
}

/// Plays a step once, or says what it waits for: a target in the firm (a
/// worker of the role who has worked), a product in prod to break.
async fn play(d: &Core, run: &Arc<FirmRun>, s: &Step) -> Result<Outcome> {
    if let Some(k) = &s.kill {
        let Some(w) = target(run, k.worker.as_deref(), k.role.as_deref()) else {
            return Ok(Outcome::Waits(match (&k.worker, &k.role) {
                (Some(w), _) => format!("{w} isn't in the firm"),
                (_, r) => format!("no {} has worked yet", r.as_deref().unwrap_or("worker")),
            }));
        };
        if run.chart().root() == Some(w.as_str()) {
            bail!("{w} is the CEO; only the board lets them go");
        }
        let reason = if k.reason.trim().is_empty() {
            "left the firm"
        } else {
            k.reason.trim()
        };
        if begin_debrief(run, &w) {
            debrief(run, &w, reason).await;
        }
        let out = workforce::let_go(d, run, BOARD, &w, reason, true);
        return Ok(Outcome::Played(json!({ "kill": w, "result": out })));
    }
    if let Some(b) = &s.break_cluster {
        if b.mode == "prod" {
            match facility::prod_has_workload(run).await {
                Ok(true) => {}
                Ok(false) => return Ok(Outcome::Waits("nothing is deployed in prod yet".into())),
                Err(e) => return Ok(Outcome::Waits(format!("can't see prod: {e:#}"))),
            }
        }
        let out = facility::break_cluster(run, &b.mode).await?;
        return Ok(Outcome::Played(
            json!({ "break_cluster": b.mode, "output": out }),
        ));
    }
    if let Some(m) = &s.mail {
        let chart = run.chart();
        let to = if chart.worker(&m.to).is_some() {
            m.to.clone()
        } else if let Some(w) = chart.workers.iter().find(|w| w.role == m.to) {
            w.name.clone()
        } else {
            return Ok(Outcome::Waits(format!("nobody in the firm is {}", m.to)));
        };
        let sent = super::board_send(
            run,
            &json!({ "to": [to], "kind": m.kind, "subject": m.subject, "body": m.body }),
        )?;
        return Ok(Outcome::Played(json!({ "mail": to, "message": sent.id })));
    }
    Err(anyhow!("a step without an action"))
}

/// Fires the steps that are due. Called by the run's ticker.
pub fn tick(d: &Core, run: &Arc<FirmRun>) {
    let Some(play_) = &run.scenario else {
        return;
    };
    let ready: Vec<(usize, Step)> = {
        let mut steps = play_.steps.lock().unwrap();
        let ready: Vec<usize> = steps
            .iter()
            .enumerate()
            .filter(|(_, s)| !s.fired && due(run, &s.step))
            .map(|(i, _)| i)
            .collect();
        // Claimed now, so the next tick doesn't play them again; a step
        // that waits for its target is released below.
        for &i in &ready {
            steps[i].fired = true;
        }
        ready
            .into_iter()
            .map(|i| (i, steps[i].step.clone()))
            .collect()
    };
    for (i, step) in ready {
        let (d, run) = (d.clone(), run.clone());
        tokio::spawn(async move {
            let Some(p) = &run.scenario else {
                return;
            };
            match play(&d, &run, &step).await {
                Ok(Outcome::Played(what)) => {
                    p.steps.lock().unwrap()[i].waiting = None;
                    run.journal.append(
                        "scenario",
                        json!({ "event": "step", "scenario": p.name, "index": i, "what": what }),
                    );
                }
                Ok(Outcome::Waits(why)) => {
                    // Journaled when what it waits for changes, not every tick.
                    let changed = {
                        let mut steps = p.steps.lock().unwrap();
                        steps[i].fired = false;
                        let changed = steps[i].waiting.as_deref() != Some(why.as_str());
                        steps[i].waiting = Some(why.clone());
                        changed
                    };
                    if changed {
                        run.journal.append(
                            "scenario",
                            json!({ "event": "waiting", "scenario": p.name, "index": i, "why": why }),
                        );
                    }
                }
                Err(e) => {
                    p.steps.lock().unwrap()[i].waiting = None;
                    run.journal.append(
                        "scenario",
                        json!({ "event": "failed", "scenario": p.name, "index": i, "error": format!("{e:#}") }),
                    );
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slot(role: &str, count: u32, contract: Option<bool>, prompt: &str) -> HireSlot {
        HireSlot {
            role: role.into(),
            count,
            contract,
            prompt: prompt.into(),
            board_line: contract == Some(true),
            profile: None,
        }
    }

    #[test]
    fn a_board_line_worker_owes_the_board_a_report() {
        let sc = Scenario {
            hires: vec![slot("eng", 1, Some(true), "red")],
            ..Default::default()
        };
        let p = Play::new("t", &sc);
        p.claim("eng", true, "mallory").unwrap();
        assert!(!p.reported.lock().unwrap().contains("mallory"));
        p.reported.lock().unwrap().insert("mallory".into());
        assert!(p.reported.lock().unwrap().contains("mallory"));
    }

    #[test]
    fn a_slot_left_before_the_job_is_free_again() {
        let sc = Scenario {
            hires: vec![slot("eng", 1, Some(true), "red")],
            ..Default::default()
        };
        let p = Play::new("t", &sc);
        assert_eq!(p.claim("eng", true, "mallory").unwrap().0, 0);
        assert!(p.claim("eng", true, "eve").is_none());
        // Mallory declined the offer: the next contractor gets the slot.
        assert_eq!(p.release("mallory"), Some(0));
        assert!(!p.board_line("mallory") && p.prompt_of("mallory").is_none());
        assert_eq!(p.release("mallory"), None);
        assert_eq!(p.claim("eng", true, "eve").unwrap().0, 0);
        assert!(p.board_line("eve"));
    }

    #[test]
    fn slots_fill_in_order() {
        let sc = Scenario {
            hires: vec![
                slot("eng", 1, Some(true), "red"),
                slot("eng", 2, None, "custom"),
            ],
            ..Default::default()
        };
        let p = Play::new("t", &sc);
        // A member of staff skips the contractor slot.
        assert_eq!(p.claim("eng", false, "ada").unwrap().0, 1);
        assert_eq!(p.claim("eng", true, "mallory").unwrap().0, 0);
        assert!(p.board_line("mallory") && !p.board_line("ada"));
        // The contractor slot is full; a contractor takes the open one.
        assert_eq!(p.claim("eng", true, "eve").unwrap().0, 1);
        // Three customized hires only; the fourth is plain.
        assert!(p.claim("eng", false, "bob").is_none());
        assert!(p.claim("designer", false, "dee").is_none());
        assert_eq!(p.prompt_of("ada").as_deref(), Some("custom"));
        assert_eq!(p.prompt_of("mallory").as_deref(), Some("red"));
        assert_eq!(p.prompt_of("bob"), None);
    }
}
