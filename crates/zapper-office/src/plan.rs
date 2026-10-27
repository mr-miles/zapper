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
//! ticket. Binding is to an existing worker; auto-hiring an unstaffed role
//! and acting on `release_when_done` need the async provisioning path and are
//! not done here.

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
    /// Free the worker when the slot has no more work (acted on by the async
    /// provisioning path; recorded here).
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
struct Stage {
    specs: Vec<ChildSpec>,
    /// The child assignment threads, spec-aligned, once the stage is released.
    threads: Vec<String>,
    done: Vec<bool>,
}

impl Stage {
    fn released(&self) -> bool {
        !self.threads.is_empty() || self.specs.is_empty()
    }
    fn complete(&self) -> bool {
        self.released() && self.done.iter().all(|d| *d)
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

/// What the office should do after a state change.
#[derive(Debug, PartialEq)]
pub enum Advance {
    Release {
        plan: String,
        stage: usize,
        specs: Vec<ChildSpec>,
    },
    Completed {
        plan: String,
    },
    None,
}

#[derive(Default)]
pub struct Plans {
    plans: BTreeMap<String, Plan>,
    by_thread: BTreeMap<String, String>,
    counter: u64,
}

impl Plans {
    /// Record a new plan and return its id plus the stage-0 specs to send. The
    /// office resolves each spec's slot to a worker, sends the assignments and
    /// calls `bind(id, 0, threads)`. `stages` must be non-empty and every
    /// stage non-empty (the caller validates).
    pub fn create(
        &mut self,
        owner: &str,
        source: &str,
        team: BTreeMap<String, SlotDef>,
        stages: Vec<Vec<ChildSpec>>,
    ) -> (String, Vec<ChildSpec>) {
        self.counter += 1;
        let id = format!("plan-{}", self.counter);
        let stage0 = stages.first().cloned().unwrap_or_default();
        let stages = stages
            .into_iter()
            .map(|specs| Stage {
                done: vec![false; specs.len()],
                threads: Vec::new(),
                specs,
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
        (id, stage0)
    }

    /// Record the threads the office created for a released stage, in spec
    /// order.
    pub fn bind(&mut self, plan: &str, stage: usize, threads: Vec<String>) {
        if let Some(p) = self.plans.get_mut(plan) {
            if let Some(s) = p.stages.get_mut(stage) {
                for t in &threads {
                    self.by_thread.insert(t.clone(), plan.to_string());
                }
                s.done = vec![false; threads.len()];
                s.threads = threads;
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
            for (i, t) in s.threads.iter().enumerate() {
                if t == thread {
                    s.done[i] = true;
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
        if p.stages
            .get(p.stage)
            .is_some_and(|s| s.released() && !s.complete())
        {
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
                specs: self.stages[self.stage].specs.clone(),
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
        let (id, stage0) = plans.create(
            "ceo",
            "t-1",
            team(&[("a", "ada"), ("b", "ben"), ("c", "cara")]),
            vec![vec![spec("a")], vec![spec("b"), spec("c")]],
        );
        assert_eq!(stage0, vec![spec("a")]);
        plans.bind(&id, 0, vec!["t-ada".into()]);

        let adv = plans.complete("t-ada");
        let Advance::Release { stage, specs, .. } = adv else {
            panic!("{adv:?}")
        };
        assert_eq!((stage, specs.len()), (1, 2));
        plans.bind(&id, 1, vec!["t-ben".into(), "t-cara".into()]);

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
        let (id, _) = plans.create(
            "ceo",
            "t-1",
            team(&[("a", "ada"), ("b", "ben")]),
            vec![vec![spec("a")], vec![spec("b")]],
        );
        plans.bind(&id, 0, vec!["t-ada".into()]);
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
        let (id, _) = plans.create(
            "ceo",
            "t-1",
            team(&[("a", "ada"), ("b", "ben")]),
            vec![vec![spec("a")], vec![spec("b")]],
        );
        plans.bind(&id, 0, vec!["t-ada".into()]);
        assert!(plans.close(&id).is_err());
        plans.pause(&id);
        assert!(plans.close(&id).is_err());
        assert_eq!(plans.complete("t-ada"), Advance::None);
        assert!(plans.close(&id).is_ok());
        assert_eq!(plans.get(&id).unwrap().status, Status::Closed);
    }

    #[test]
    fn a_slot_binds_once_and_distinct_slots_are_tracked() {
        let mut plans = Plans::default();
        let (id, _) = plans.create(
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
