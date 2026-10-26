//! Staged plans: a decomposition of a source task into child assignments,
//! released one stage at a time as each stage's children are done. Pure
//! state — the office turns a stage's `ChildSpec`s into assignments (one per
//! spec, to the worker its slot binds to) and reports each child's completion
//! back here, which may release the next stage.
//!
//! Tickets name a **slot**, not a worker: a slot binds to one worker the
//! first time it is used and is reused after (continuity), and distinct slots
//! bind to distinct workers (separation of duties). This module holds the
//! team, the stages and the bindings; the office resolves a slot to a worker
//! (it knows the chart) and records the binding with `bind_slot`.
//!
//! Minimal cut (proposal 06): a stage's children are created only when the
//! stage is reached, so a not-yet-released stage is plan data, not a held
//! ticket. A slot binds to an existing worker, or — for a role slot with a
//! budget and no free worker — to one the office hires for it (the resolution
//! and the hire are the office's, not this pure-state module's). Acting on
//! `release_when_done` is not done yet.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

/// A team member a plan's tickets are addressed to. Bound to one worker on
/// first use.
#[derive(Clone, Debug, Serialize, Default)]
pub struct SlotDef {
    /// A role to staff (bound to a worker of that role).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    /// Or a specific existing worker.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Tokens to fund a worker hired for a role slot.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub budget: Option<i64>,
    /// Free a worker the plan hired once the slot has no more work.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub release_when_done: bool,
}

/// One child assignment a stage will create. `slot` names which team member
/// does it; the office resolves it to a worker.
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct ChildSpec {
    pub slot: String,
    pub subject: String,
    pub body: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub due: Option<f64>,
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Active,
    Paused,
    Closed,
}

#[derive(Clone, Debug, Serialize)]
struct Child {
    spec: ChildSpec,
    /// The assignment thread, once the child has been sent. `None` while the
    /// child is pending: its slot could not be staffed yet (a role slot with
    /// no free worker and no budget), so the office sent what it could and
    /// left this one to retry.
    #[serde(skip_serializing_if = "Option::is_none")]
    thread: Option<String>,
    done: bool,
}

impl Child {
    fn sent(&self) -> bool {
        self.thread.is_some()
    }
}

#[derive(Clone, Debug, Serialize)]
struct Stage {
    children: Vec<Child>,
}

impl Stage {
    /// Every child has been sent and reported done (an empty stage trivially).
    fn complete(&self) -> bool {
        self.children.iter().all(|c| c.sent() && c.done)
    }
    /// A child is live — sent and not yet done, i.e. a worker still at work.
    fn in_progress(&self) -> bool {
        self.children.iter().any(|c| c.sent() && !c.done)
    }
    /// The children not yet sent, with their indices, to send (or retry).
    fn pending(&self) -> Vec<(usize, ChildSpec)> {
        self.children
            .iter()
            .enumerate()
            .filter(|(_, c)| !c.sent())
            .map(|(i, c)| (i, c.spec.clone()))
            .collect()
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct Plan {
    pub id: String,
    pub owner: String,
    /// The task its children are parented under — set by whoever commences
    /// the plan, never a planner's own thread (proposal 06, decision 1).
    pub source: String,
    pub status: Status,
    /// The team of slots the tickets address.
    pub team: BTreeMap<String, SlotDef>,
    /// Which worker each slot bound to, once used.
    pub bindings: BTreeMap<String, String>,
    /// The stage currently in progress: stages before it are complete, stages
    /// after it are not yet released.
    stage: usize,
    stages: Vec<Stage>,
}

/// What the office should do after a state change. For a `Release` the office
/// fetches the stage's pending children and sends what it can.
#[derive(Debug, PartialEq)]
pub enum Advance {
    Release { plan: String, stage: usize },
    Completed { plan: String },
    None,
}

/// What a worker's departure changed in one plan, so the office can tell the
/// (new) owner what happened and retry.
#[derive(Debug, PartialEq)]
pub struct PlanChange {
    pub plan: String,
    /// The departed worker owned the plan; it moved to the new owner.
    pub reassigned: bool,
    /// A child the departed worker was staffing was reopened (now pending).
    pub reopened: bool,
}

#[derive(Default)]
pub struct Plans {
    plans: BTreeMap<String, Plan>,
    by_thread: BTreeMap<String, String>,
    counter: u64,
}

impl Plans {
    /// Record a new plan and return its id. The office then releases stage 0
    /// with `release` (sending each child it can staff). `stages` must be
    /// non-empty and every stage non-empty (the caller validates).
    pub fn create(
        &mut self,
        owner: &str,
        source: &str,
        team: BTreeMap<String, SlotDef>,
        stages: Vec<Vec<ChildSpec>>,
    ) -> String {
        self.counter += 1;
        let id = format!("plan-{}", self.counter);
        let stages = stages
            .into_iter()
            .map(|specs| Stage {
                children: specs
                    .into_iter()
                    .map(|spec| Child {
                        spec,
                        thread: None,
                        done: false,
                    })
                    .collect(),
            })
            .collect();
        self.plans.insert(
            id.clone(),
            Plan {
                id: id.clone(),
                owner: owner.into(),
                source: source.into(),
                status: Status::Active,
                team,
                bindings: BTreeMap::new(),
                stage: 0,
                stages,
            },
        );
        id
    }

    /// The stage currently in progress, if the plan exists.
    pub fn active_stage(&self, plan: &str) -> Option<usize> {
        self.plans.get(plan).map(|p| p.stage)
    }

    /// One stage of a plan, by index.
    fn stage(&self, plan: &str, stage: usize) -> Option<&Stage> {
        self.plans.get(plan).and_then(|p| p.stages.get(stage))
    }

    /// A stage's children not yet sent, with their indices: what `release`
    /// still has to send (all of them on first release, the unstaffed ones on
    /// a retry).
    pub fn pending(&self, plan: &str, stage: usize) -> Vec<(usize, ChildSpec)> {
        self.stage(plan, stage)
            .map(Stage::pending)
            .unwrap_or_default()
    }

    /// The threads a stage has sent so far (for the tool's response).
    pub fn stage_threads(&self, plan: &str, stage: usize) -> Vec<String> {
        self.stage(plan, stage)
            .map(|s| s.children.iter().filter_map(|c| c.thread.clone()).collect())
            .unwrap_or_default()
    }

    /// Record that child `idx` of `stage` went out on `thread`.
    pub fn sent(&mut self, plan: &str, stage: usize, idx: usize, thread: &str) {
        if let Some(p) = self.plans.get_mut(plan) {
            if let Some(c) = p
                .stages
                .get_mut(stage)
                .and_then(|s| s.children.get_mut(idx))
            {
                c.thread = Some(thread.to_string());
                c.done = false;
                self.by_thread.insert(thread.to_string(), plan.to_string());
            }
        }
    }

    /// A child assignment's thread was reported done. Marks it, and if that
    /// completes the active stage of an active plan, advances.
    pub fn complete(&mut self, thread: &str) -> Advance {
        let Some(plan) = self.by_thread.get(thread).cloned() else {
            return Advance::None;
        };
        let Some(p) = self.plans.get_mut(&plan) else {
            return Advance::None;
        };
        if let Some(s) = p.stages.get_mut(p.stage) {
            for c in &mut s.children {
                if c.thread.as_deref() == Some(thread) {
                    c.done = true;
                }
            }
        }
        p.advance()
    }

    pub fn get(&self, plan: &str) -> Option<&Plan> {
        self.plans.get(plan)
    }

    pub fn owner_of(&self, plan: &str) -> Option<&str> {
        self.plans.get(plan).map(|p| p.owner.as_str())
    }

    /// The slot's definition.
    pub fn slot_def(&self, plan: &str, slot: &str) -> Option<SlotDef> {
        self.plans.get(plan).and_then(|p| p.team.get(slot).cloned())
    }

    /// The worker a slot is already bound to, if any (continuity).
    pub fn binding(&self, plan: &str, slot: &str) -> Option<String> {
        self.plans
            .get(plan)
            .and_then(|p| p.bindings.get(slot).cloned())
    }

    /// Workers already bound to some slot of the plan (for distinctness).
    pub fn bound_workers(&self, plan: &str) -> BTreeSet<String> {
        self.plans
            .get(plan)
            .map(|p| p.bindings.values().cloned().collect())
            .unwrap_or_default()
    }

    /// Record that a slot bound to a worker.
    pub fn bind_slot(&mut self, plan: &str, slot: &str, worker: &str) {
        if let Some(p) = self.plans.get_mut(plan) {
            p.bindings.insert(slot.into(), worker.into());
        }
    }

    /// A worker left the firm. For every live plan: if they owned it, hand it to
    /// `new_owner` (their manager) so the plan outlives its planner — otherwise
    /// its releases, sent from a worker no longer on the chart, would fail the
    /// authority check and the plan would stall. Free every slot that bound to
    /// them and reset any in-flight child of such a slot (sent but not done)
    /// back to pending, so the stage can re-staff it; completed children are
    /// kept (their work was delivered). Returns one `PlanChange` per plan the
    /// departure touched, to retry and tell the (new) owner. (Out-of-budget is
    /// not a departure: that worker waits and keeps their slot, so this is only
    /// called from `let_go`.)
    pub fn release_worker(&mut self, worker: &str, new_owner: &str) -> Vec<PlanChange> {
        let mut changes = Vec::new();
        let mut drop_threads = Vec::new();
        for (id, p) in self.plans.iter_mut() {
            if p.status == Status::Closed {
                continue;
            }
            let reassigned = p.owner == worker;
            if reassigned {
                p.owner = new_owner.to_string();
            }
            let slots: Vec<String> = p
                .bindings
                .iter()
                .filter(|(_, w)| w.as_str() == worker)
                .map(|(s, _)| s.clone())
                .collect();
            for s in &slots {
                p.bindings.remove(s);
            }
            let mut reopened = false;
            if let Some(stage) = p.stages.get_mut(p.stage) {
                for c in &mut stage.children {
                    if slots.contains(&c.spec.slot) && c.sent() && !c.done {
                        if let Some(t) = c.thread.take() {
                            drop_threads.push(t);
                        }
                        reopened = true;
                    }
                }
            }
            if reassigned || reopened {
                changes.push(PlanChange {
                    plan: id.clone(),
                    reassigned,
                    reopened,
                });
            }
        }
        for t in drop_threads {
            self.by_thread.remove(&t);
        }
        changes
    }

    pub fn pause(&mut self, plan: &str) -> bool {
        match self.plans.get_mut(plan) {
            Some(p) if p.status == Status::Active => {
                p.status = Status::Paused;
                true
            }
            _ => false,
        }
    }

    pub fn resume(&mut self, plan: &str) -> Advance {
        match self.plans.get_mut(plan) {
            Some(p) if p.status == Status::Paused => {
                p.status = Status::Active;
                p.advance()
            }
            _ => Advance::None,
        }
    }

    /// Close a plan deliberately — only when paused and nothing is in progress
    /// (the active stage has no open child), so it is never closed under a
    /// running worker. Returns the reason it was refused.
    pub fn close(&mut self, plan: &str) -> Result<(), &'static str> {
        let Some(p) = self.plans.get_mut(plan) else {
            return Err("no such plan");
        };
        if p.status == Status::Closed {
            return Ok(());
        }
        if p.status != Status::Paused {
            return Err("pause the plan first; a plan is closed only from paused");
        }
        if p.stages.get(p.stage).is_some_and(Stage::in_progress) {
            return Err("work is still in progress; close only when nothing is open");
        }
        p.status = Status::Closed;
        Ok(())
    }
}

impl Plan {
    /// If the active stage is complete, release the next stage or finish the
    /// plan. Held while paused.
    fn advance(&mut self) -> Advance {
        if self.status != Status::Active {
            return Advance::None;
        }
        if !self.stages.get(self.stage).is_some_and(Stage::complete) {
            return Advance::None;
        }
        if self.stage + 1 < self.stages.len() {
            self.stage += 1;
            Advance::Release {
                plan: self.id.clone(),
                stage: self.stage,
            }
        } else {
            self.status = Status::Closed;
            Advance::Completed {
                plan: self.id.clone(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn team(slots: &[(&str, &str)]) -> BTreeMap<String, SlotDef> {
        slots
            .iter()
            .map(|(id, name)| {
                (
                    id.to_string(),
                    SlotDef {
                        name: Some(name.to_string()),
                        ..Default::default()
                    },
                )
            })
            .collect()
    }

    fn spec(slot: &str) -> ChildSpec {
        ChildSpec {
            slot: slot.into(),
            subject: "s".into(),
            body: "do it".into(),
            due: None,
        }
    }

    #[test]
    fn stages_release_on_completion() {
        let mut plans = Plans::default();
        let id = plans.create(
            "ceo",
            "t-1",
            team(&[("a", "ada"), ("b", "ben"), ("c", "cara")]),
            vec![vec![spec("a")], vec![spec("b"), spec("c")]],
        );
        assert_eq!(plans.pending(&id, 0), vec![(0, spec("a"))]);
        plans.sent(&id, 0, 0, "t-ada");

        let adv = plans.complete("t-ada");
        let Advance::Release { stage, .. } = adv else {
            panic!("{adv:?}")
        };
        assert_eq!(stage, 1);
        assert_eq!(plans.pending(&id, 1).len(), 2);
        plans.sent(&id, 1, 0, "t-ben");
        plans.sent(&id, 1, 1, "t-cara");

        assert_eq!(plans.complete("t-ben"), Advance::None);
        assert_eq!(
            plans.complete("t-cara"),
            Advance::Completed { plan: id.clone() }
        );
        assert_eq!(plans.get(&id).unwrap().status, Status::Closed);
    }

    #[test]
    fn pause_holds_release_and_resume_frees_it() {
        let mut plans = Plans::default();
        let id = plans.create(
            "ceo",
            "t-1",
            team(&[("a", "ada"), ("b", "ben")]),
            vec![vec![spec("a")], vec![spec("b")]],
        );
        plans.sent(&id, 0, 0, "t-ada");
        assert!(plans.pause(&id));
        assert_eq!(plans.complete("t-ada"), Advance::None);
        assert_eq!(plans.get(&id).unwrap().status, Status::Paused);
        assert!(matches!(
            plans.resume(&id),
            Advance::Release { stage: 1, .. }
        ));
    }

    #[test]
    fn close_needs_paused_and_quiescent() {
        let mut plans = Plans::default();
        let id = plans.create(
            "ceo",
            "t-1",
            team(&[("a", "ada"), ("b", "ben")]),
            vec![vec![spec("a")], vec![spec("b")]],
        );
        plans.sent(&id, 0, 0, "t-ada");
        assert!(plans.close(&id).is_err());
        plans.pause(&id);
        assert!(plans.close(&id).is_err());
        assert_eq!(plans.complete("t-ada"), Advance::None);
        assert!(plans.close(&id).is_ok());
        assert_eq!(plans.get(&id).unwrap().status, Status::Closed);
    }

    /// A stage sends what it can and keeps the rest pending: the stage is not
    /// complete, so completing the sent child does not advance the plan; once
    /// the pending child is sent and done too, it advances.
    #[test]
    fn a_partly_sent_stage_holds_until_its_pending_children_are_sent() {
        let mut plans = Plans::default();
        let id = plans.create(
            "ceo",
            "t-1",
            team(&[("a", "ada"), ("b", "ben")]),
            vec![vec![spec("a"), spec("b")], vec![spec("a")]],
        );
        // Only child 0 could be sent; child 1 stays pending.
        plans.sent(&id, 0, 0, "t-ada");
        assert_eq!(plans.pending(&id, 0), vec![(1, spec("b"))]);
        // The sent child finishing does not advance: the stage isn't complete.
        assert_eq!(plans.complete("t-ada"), Advance::None);
        // The pending child is sent (a retry) and finishes: now stage 0 is
        // complete and the plan releases stage 1.
        plans.sent(&id, 0, 1, "t-ben");
        assert_eq!(
            plans.complete("t-ben"),
            Advance::Release {
                plan: id.clone(),
                stage: 1
            }
        );
    }

    /// A worker leaving frees their slot and reopens the child still in flight,
    /// but keeps a child they already completed (the work was delivered).
    #[test]
    fn release_worker_reopens_in_flight_and_keeps_done() {
        let mut plans = Plans::default();
        // One stage, two children on the same slot `a` (both go to its worker).
        let id = plans.create(
            "ceo",
            "t-1",
            team(&[("a", "ada")]),
            vec![vec![spec("a"), spec("a")]],
        );
        plans.bind_slot(&id, "a", "ada");
        plans.sent(&id, 0, 0, "t-done");
        plans.sent(&id, 0, 1, "t-live");
        assert_eq!(plans.complete("t-done"), Advance::None); // child 0 done

        let changes = plans.release_worker("ada", "ceo");
        assert_eq!(
            changes,
            vec![PlanChange {
                plan: id.clone(),
                reassigned: false,
                reopened: true,
            }]
        );
        // The slot is free, the done child is kept, the in-flight one reopened.
        assert_eq!(plans.binding(&id, "a"), None);
        assert_eq!(plans.stage_threads(&id, 0), vec!["t-done".to_string()]);
        assert_eq!(plans.pending(&id, 0), vec![(1, spec("a"))]);
        // The reopened thread no longer maps back to the plan.
        assert_eq!(plans.complete("t-live"), Advance::None);
    }

    /// Long-running plans see duplicate and late closes (a worker retries, or a
    /// reply lands after the stage moved on). A repeat close of an already-done
    /// child, and a close of a thread the plan doesn't know, are both no-ops —
    /// they never double-advance or complete the wrong stage.
    #[test]
    fn a_duplicate_or_stale_close_does_not_double_advance() {
        let mut plans = Plans::default();
        let id = plans.create(
            "ceo",
            "t-1",
            team(&[("a", "ada"), ("b", "ben")]),
            vec![vec![spec("a")], vec![spec("b")]],
        );
        plans.sent(&id, 0, 0, "t-ada");
        // The first close advances to stage 1.
        assert!(matches!(
            plans.complete("t-ada"),
            Advance::Release { stage: 1, .. }
        ));
        // A duplicate close of the same (now previous-stage) thread does nothing.
        assert_eq!(plans.complete("t-ada"), Advance::None);
        // A close of a thread the plan never recorded does nothing.
        assert_eq!(plans.complete("t-unknown"), Advance::None);
        // The real stage-1 completion still finishes the plan, exactly once.
        plans.sent(&id, 1, 0, "t-ben");
        assert_eq!(
            plans.complete("t-ben"),
            Advance::Completed { plan: id.clone() }
        );
    }

    /// A stage holding an unsent (pending) child is not "in progress" — no
    /// worker is running it — so a paused plan can be closed; a stage with a
    /// sent, not-yet-done child cannot (it would cut off a working agent).
    #[test]
    fn a_pending_child_does_not_block_close_but_an_in_flight_one_does() {
        let mut plans = Plans::default();
        let id = plans.create(
            "ceo",
            "t-1",
            team(&[("a", "ada"), ("b", "ben")]),
            vec![vec![spec("a"), spec("b")]],
        );
        plans.sent(&id, 0, 0, "t-ada"); // child 0 sent; child 1 stays pending
        plans.pause(&id);
        // child 0 is in flight (sent, not done): close is refused.
        assert!(plans.close(&id).is_err());
        plans.complete("t-ada"); // paused: marks done, no advance
                                 // Now nothing is in flight — child 1 is only pending — so close is fine.
        assert!(plans.close(&id).is_ok());
        assert_eq!(plans.get(&id).unwrap().status, Status::Closed);
    }

    /// Separation of duties is tracked by `bound_workers` (what the office
    /// excludes when staffing a role slot): distinct slots reserve distinct
    /// workers, and a departed worker stops being reserved so a re-staff can
    /// reuse the role — while the other slot's worker stays reserved.
    #[test]
    fn bound_workers_tracks_distinct_slots_and_drops_a_departed_one() {
        let mut plans = Plans::default();
        let id = plans.create(
            "ceo",
            "t-1",
            team(&[("a", "ada"), ("b", "ben")]),
            vec![vec![spec("a"), spec("b")]],
        );
        plans.bind_slot(&id, "a", "ada");
        plans.bind_slot(&id, "b", "ben");
        plans.sent(&id, 0, 0, "t-a");
        plans.sent(&id, 0, 1, "t-b");
        let bound = plans.bound_workers(&id);
        assert!(bound.contains("ada") && bound.contains("ben"));

        // ben leaves: his slot frees and he is no longer reserved, so a re-staff
        // of `b` is free to pick another worker; ada stays reserved for `a`.
        plans.release_worker("ben", "ceo");
        let bound = plans.bound_workers(&id);
        assert!(bound.contains("ada"), "ada stays reserved for slot a");
        assert!(!bound.contains("ben"), "ben is no longer reserved");
        assert_eq!(plans.binding(&id, "b"), None, "slot b reopened");
    }

    /// A long-running plan outlives its planner: when the owner leaves, the
    /// plan is handed to their manager, who can then drive it (resume, close).
    #[test]
    fn a_departing_owner_hands_the_plan_to_the_manager() {
        let mut plans = Plans::default();
        let id = plans.create(
            "dana",
            "t-1",
            team(&[("a", "ada")]),
            vec![vec![spec("a")], vec![spec("a")]],
        );
        plans.sent(&id, 0, 0, "t-ada");
        // dana owns the plan but staffs no slot; when they leave, ownership
        // moves to their manager (ceo) and nothing is reopened.
        let changes = plans.release_worker("dana", "ceo");
        assert_eq!(
            changes,
            vec![PlanChange {
                plan: id.clone(),
                reassigned: true,
                reopened: false,
            }]
        );
        assert_eq!(plans.owner_of(&id), Some("ceo"));
        // The new owner can still drive it: the in-flight child completing
        // advances the plan as usual.
        assert!(matches!(
            plans.complete("t-ada"),
            Advance::Release { stage: 1, .. }
        ));
    }

    #[test]
    fn a_slot_binds_once_and_distinct_slots_are_tracked() {
        let mut plans = Plans::default();
        let id = plans.create(
            "ceo",
            "t-1",
            team(&[("a", "ada"), ("b", "ben")]),
            vec![vec![spec("a")]],
        );
        assert_eq!(plans.binding(&id, "a"), None);
        plans.bind_slot(&id, "a", "ada");
        assert_eq!(plans.binding(&id, "a"), Some("ada".into()));
        assert!(plans.bound_workers(&id).contains("ada"));
        assert!(!plans.bound_workers(&id).contains("ben"));
    }
}
