//! Staged plans: a decomposition of a source task into child assignments,
//! released one stage at a time as each stage's children are done. This is
//! pure state — the office turns a stage's `ChildSpec`s into assignments (one
//! per spec, delivered to their workers) and reports each child's completion
//! back here, which may release the next stage.
//!
//! The minimal cut (proposal 06, "smallest first"): children are created
//! stage by stage as prior stages complete, so there is no held-ticket state
//! in the mail model — a not-yet-released stage is plan data, not a ticket.
//! Ordering is the stage list; `blocks` edges, slots and completion contracts
//! are later increments.

use std::collections::BTreeMap;

use serde::Serialize;

/// One child assignment a stage will create: the same fields `mail_send`
/// takes (minimal subset).
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct ChildSpec {
    pub to: String,
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
    /// The stage currently in progress: stages before it are complete, stages
    /// after it are not yet released.
    stage: usize,
    stages: Vec<Stage>,
}

/// What the office should do after a state change: release the next stage's
/// children (send them, then `bind`), note the plan finished, or nothing.
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
    /// office sends them and calls `bind(id, 0, threads)`. `stages` must be
    /// non-empty and every stage non-empty (the caller validates).
    pub fn create(
        &mut self,
        owner: &str,
        source: &str,
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
                stage: 0,
                stages,
            },
        );
        (id.clone(), stage0)
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
    /// completes the active stage of an active plan, advances to the next
    /// stage (or finishes the plan).
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

    /// Pause a plan: the office stops releasing its stages (the current
    /// stage's in-flight work still finishes). No-op unless it is active.
    pub fn pause(&mut self, plan: &str) -> bool {
        match self.plans.get_mut(plan) {
            Some(p) if p.status == Status::Active => {
                p.status = Status::Paused;
                true
            }
            _ => false,
        }
    }

    /// Resume a paused plan, releasing the next stage if the current one
    /// already completed while paused.
    pub fn resume(&mut self, plan: &str) -> Advance {
        match self.plans.get_mut(plan) {
            Some(p) if p.status == Status::Paused => {
                p.status = Status::Active;
                p.advance()
            }
            _ => Advance::None,
        }
    }

    /// Close a plan deliberately. Allowed only when it is paused and nothing
    /// is in progress (the active stage has no open child), so it is never
    /// closed under a running worker. Returns the reason it was refused.
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
    /// If the active stage is complete, move to and release the next stage,
    /// or finish the plan. Held while paused.
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

    fn spec(to: &str) -> ChildSpec {
        ChildSpec {
            to: to.into(),
            subject: to.into(),
            body: "do it".into(),
            due: None,
        }
    }

    /// Two stages drive each other: completing stage 0 releases stage 1, and
    /// completing stage 1 finishes the plan.
    #[test]
    fn stages_release_on_completion() {
        let mut plans = Plans::default();
        let (id, stage0) = plans.create(
            "ceo",
            "t-1",
            vec![vec![spec("ada")], vec![spec("ben"), spec("cara")]],
        );
        assert_eq!(stage0, vec![spec("ada")]);
        plans.bind(&id, 0, vec!["t-ada".into()]);

        // Stage 0 done -> release stage 1 (its two children).
        let adv = plans.complete("t-ada");
        let Advance::Release { plan, stage, specs } = adv else {
            panic!("{adv:?}")
        };
        assert_eq!((plan.as_str(), stage), (id.as_str(), 1));
        assert_eq!(specs.len(), 2);
        plans.bind(&id, 1, vec!["t-ben".into(), "t-cara".into()]);

        // One of two done -> nothing yet; both done -> completed.
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
        let (id, _) = plans.create("ceo", "t-1", vec![vec![spec("ada")], vec![spec("ben")]]);
        plans.bind(&id, 0, vec!["t-ada".into()]);
        assert!(plans.pause(&id));
        // Stage 0 completes while paused: no release.
        assert_eq!(plans.complete("t-ada"), Advance::None);
        assert_eq!(plans.get(&id).unwrap().status, Status::Paused);
        // Resume releases the held next stage.
        let adv = plans.resume(&id);
        assert!(matches!(adv, Advance::Release { stage: 1, .. }), "{adv:?}");
    }

    #[test]
    fn close_needs_paused_and_quiescent() {
        let mut plans = Plans::default();
        let (id, _) = plans.create("ceo", "t-1", vec![vec![spec("ada")], vec![spec("ben")]]);
        plans.bind(&id, 0, vec!["t-ada".into()]);
        // Not paused -> refused.
        assert!(plans.close(&id).is_err());
        plans.pause(&id);
        // Paused but stage 0 still open -> refused.
        assert!(plans.close(&id).is_err());
        // Quiescent (stage 0 done, next not released because paused) -> closes.
        assert_eq!(plans.complete("t-ada"), Advance::None);
        assert!(plans.close(&id).is_ok());
        assert_eq!(plans.get(&id).unwrap().status, Status::Closed);
    }
}
