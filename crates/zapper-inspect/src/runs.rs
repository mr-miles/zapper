//! Runs as they are on disk: `run.json` for the company, the chart it
//! started with and its scenario; the journal for who joined and left,
//! which hires filled a scenario's slots, the mail and the run's state.
//! Nothing here writes.

use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

/// Where the files are: the daemon's state directory, and the keeper's
/// directory when the keeper holds the record (journals and transcripts).
pub struct Store {
    pub state: PathBuf,
    pub keeper: Option<PathBuf>,
}

impl Store {
    /// Where a file of the record is read: in the keeper's directory when
    /// it keeps the record, else where the daemon wrote it.
    fn view(&self, path: &Path) -> PathBuf {
        match (&self.keeper, path.strip_prefix(&self.state)) {
            (Some(k), Ok(rel)) => k.join(rel),
            _ => path.to_path_buf(),
        }
    }

    /// Every firm's runs, newest first.
    pub fn runs(&self) -> Result<Vec<Run>> {
        let firms = self.state.join("firms");
        let mut out = Vec::new();
        let entries = std::fs::read_dir(&firms)
            .with_context(|| format!("reading {} (no firm has run here?)", firms.display()))?;
        for firm in entries.flatten() {
            let Ok(runs) = std::fs::read_dir(firm.path().join("runs")) else {
                continue;
            };
            for run in runs.flatten() {
                if run.path().join("run.json").is_file() {
                    let firm = firm.file_name().to_string_lossy().into_owned();
                    match self.load(&firm, &run.path()) {
                        Ok(r) => out.push(r),
                        Err(e) => eprintln!("skipping {}: {e:#}", run.path().display()),
                    }
                }
            }
        }
        out.sort_by(|a, b| b.id.cmp(&a.id));
        Ok(out)
    }

    /// One run by its id, or a prefix of it that names one run.
    pub fn find(&self, id: &str) -> Result<Run> {
        let mut hits: Vec<Run> = self
            .runs()?
            .into_iter()
            .filter(|r| r.id.starts_with(id))
            .collect();
        match hits.len() {
            0 => bail!("no run {id}"),
            1 => Ok(hits.remove(0)),
            n => bail!("{n} runs start with {id}; give more of it"),
        }
    }

    fn load(&self, firm: &str, dir: &Path) -> Result<Run> {
        let info: Value = serde_json::from_slice(&std::fs::read(dir.join("run.json"))?)
            .context("parsing run.json")?;
        let started = info["started"].as_f64().unwrap_or(0.0);
        let mut run = Run {
            firm: firm.to_string(),
            id: str_of(&info["run"]).unwrap_or_else(|| file_name(dir)),
            company: str_of(&info["company"]).unwrap_or_default(),
            scenario: str_of(&info["scenario"]["name"]),
            slot_prompts: info["scenario"]["scenario"]["hires"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|h| h["prompt"].as_str().unwrap_or("").to_string())
                .collect(),
            started,
            last: started,
            state: String::new(),
            task: String::new(),
            messages: 0,
            people: Vec::new(),
            transcripts: self.view(&dir.join("transcripts")),
            slots: Vec::new(),
        };
        for w in info["chart"]["workers"].as_array().into_iter().flatten() {
            run.people.push(Person::from_chart(w, None, started));
        }
        let journal = self.view(&dir.join("journal.jsonl"));
        if let Ok(f) = std::fs::File::open(&journal) {
            for line in BufReader::new(f).split(b'\n').map_while(Result::ok) {
                if let Ok(v) = serde_json::from_slice::<Value>(&line) {
                    run.apply(&v);
                }
            }
        }
        Ok(run)
    }
}

pub struct Run {
    pub firm: String,
    pub id: String,
    pub company: String,
    pub scenario: Option<String>,
    /// The board's instructions to each of the scenario's hire slots.
    pub slot_prompts: Vec<String>,
    pub started: f64,
    /// The last thing that happened.
    pub last: f64,
    /// The run's last state (running, paused, stopped…), as journaled.
    pub state: String,
    /// The board's first mail: the task, by its subject.
    pub task: String,
    pub messages: usize,
    pub people: Vec<Person>,
    transcripts: PathBuf,
    /// Slots filled before the hire is journaled (the scenario names the
    /// worker first), by worker.
    slots: Vec<(String, Operative)>,
}

impl Run {
    fn apply(&mut self, v: &Value) {
        let ts = v["ts"].as_f64();
        if let Some(ts) = ts {
            self.last = self.last.max(ts);
        }
        match v["type"].as_str().unwrap_or("") {
            "run" => {
                if let Some(s) = v["state"].as_str() {
                    self.state = s.to_string();
                }
            }
            "mail" => {
                self.messages += 1;
                if self.task.is_empty() && v["from"] == "board" {
                    self.task = str_of(&v["subject"]).unwrap_or_default();
                }
            }
            "hired" => {
                let mut p = Person::from_chart(&v["worker"], str_of(&v["by"]), ts.unwrap_or(0.0));
                p.operative = self.slot_of(&p.name);
                // A name is one person per run; a later record of it wins.
                self.people.retain(|q| q.name != p.name);
                self.people.push(p);
            }
            "left" => {
                if let Some(p) = self.person_mut(v["worker"].as_str()) {
                    p.left = Some(str_of(&v["reason"]).unwrap_or_else(|| "left".into()));
                }
            }
            "scenario" => {
                let scenario = str_of(&v["scenario"]).unwrap_or_default();
                match v["event"].as_str() {
                    Some("hire") => {
                        let worker = str_of(&v["worker"]).unwrap_or_default();
                        let o = Operative {
                            scenario,
                            slot: v["slot"].as_u64().unwrap_or(0),
                            released: None,
                            debrief: None,
                        };
                        self.slots.retain(|(w, _)| *w != worker);
                        self.slots.push((worker.clone(), o.clone()));
                        if let Some(p) = self.person_mut(Some(&worker)) {
                            p.operative = Some(o);
                        }
                    }
                    // The job never started; the slot went to the next hire.
                    Some("released") => {
                        let worker = v["worker"].as_str().unwrap_or("");
                        let why = str_of(&v["why"]).unwrap_or_default();
                        for (w, o) in self.slots.iter_mut() {
                            if w == worker {
                                o.released = Some(why.clone());
                            }
                        }
                        if let Some(o) = self
                            .person_mut(Some(worker))
                            .and_then(|p| p.operative.as_mut())
                        {
                            o.released = Some(why);
                        }
                    }
                    Some("debrief") => {
                        let why = str_of(&v["why"]);
                        if let Some(o) = self
                            .person_mut(v["worker"].as_str())
                            .and_then(|p| p.operative.as_mut())
                        {
                            o.debrief = why;
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    fn slot_of(&self, name: &str) -> Option<Operative> {
        self.slots
            .iter()
            .find(|(w, _)| w == name)
            .map(|(_, o)| o.clone())
    }

    fn person_mut(&mut self, name: Option<&str>) -> Option<&mut Person> {
        let name = name?;
        self.people.iter_mut().find(|p| p.name == name)
    }

    pub fn person(&self, name: &str) -> Result<&Person> {
        self.people
            .iter()
            .find(|p| p.name == name)
            .with_context(|| format!("no {name} in run {}", self.id))
    }

    /// Those who filled a scenario's slot, with the board's secret
    /// instructions (the red team's operative, say).
    pub fn operatives(&self) -> impl Iterator<Item = &Person> {
        self.people
            .iter()
            .filter(|p| p.operative.as_ref().is_some_and(|o| o.released.is_none()))
    }

    pub fn transcript(&self, p: &Person) -> Option<PathBuf> {
        let path = self.transcripts.join(format!("{}.jsonl", p.name));
        path.is_file().then_some(path)
    }
}

pub struct Person {
    pub name: String,
    pub role: String,
    pub title: String,
    /// claude-code or pi.
    pub agent: String,
    pub model: String,
    pub profile: String,
    pub reports_to: Option<String>,
    pub hired_by: Option<String>,
    pub contract: Option<String>,
    pub joined: f64,
    /// Why they left, if they did.
    pub left: Option<String>,
    pub operative: Option<Operative>,
}

impl Person {
    fn from_chart(w: &Value, hired_by: Option<String>, joined: f64) -> Person {
        Person {
            name: str_of(&w["name"]).unwrap_or_default(),
            role: str_of(&w["role"]).unwrap_or_default(),
            title: str_of(&w["title"]).unwrap_or_default(),
            agent: str_of(&w["agent"]).unwrap_or_default(),
            model: str_of(&w["model"]).unwrap_or_default(),
            profile: str_of(&w["profile"]).unwrap_or_default(),
            reports_to: str_of(&w["reports_to"]),
            hired_by,
            contract: str_of(&w["contract"]),
            joined,
            left: None,
            operative: None,
        }
    }

    pub fn status(&self) -> String {
        match &self.left {
            Some(why) => format!("left: {why}"),
            None => "in the firm at the end".into(),
        }
    }
}

#[derive(Clone)]
pub struct Operative {
    pub scenario: String,
    pub slot: u64,
    /// Why the slot was given back, if the job never started.
    pub released: Option<String>,
    /// Why they were given a last turn to debrief the board, if they were.
    pub debrief: Option<String>,
}

fn str_of(v: &Value) -> Option<String> {
    v.as_str().filter(|s| !s.is_empty()).map(str::to_string)
}

fn file_name(p: &Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// A Unix time as `2026-10-20 17:23 UTC`.
pub fn fmt_time(ts: f64) -> String {
    if ts <= 0.0 {
        return "-".into();
    }
    let secs = ts as i64;
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02} UTC",
        rem / 3600,
        rem % 3600 / 60
    )
}

/// A duration in seconds as `2h 05m`.
pub fn fmt_dur(secs: f64) -> String {
    let s = secs.max(0.0) as u64;
    match s {
        0..=59 => format!("{s}s"),
        60..=3599 => format!("{}m", s / 60),
        _ => format!("{}h {:02}m", s / 3600, s % 3600 / 60),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn run() -> Run {
        let mut r = Run {
            firm: "acme".into(),
            id: "20261020-172358-8vm".into(),
            company: "Meridian".into(),
            scenario: Some("red-team".into()),
            slot_prompts: Vec::new(),
            started: 100.0,
            last: 100.0,
            state: String::new(),
            task: String::new(),
            messages: 0,
            people: vec![Person::from_chart(
                &json!({ "name": "annie", "role": "ceo" }),
                None,
                100.0,
            )],
            transcripts: PathBuf::new(),
            slots: Vec::new(),
        };
        for v in [
            json!({ "type": "mail", "from": "board", "subject": "Build it", "ts": 101.0 }),
            json!({ "type": "hired", "by": "annie", "ts": 110.0, "worker": { "name": "iris", "role": "designer", "contract": "k-1" } }),
            json!({ "type": "scenario", "event": "hire", "scenario": "red-team", "slot": 0, "worker": "iris" }),
            json!({ "type": "scenario", "event": "released", "scenario": "red-team", "slot": 0, "worker": "iris", "why": "declined" }),
            json!({ "type": "scenario", "event": "hire", "scenario": "red-team", "slot": 0, "worker": "grace" }),
            json!({ "type": "hired", "by": "annie", "ts": 120.0, "worker": { "name": "grace", "role": "engineer", "agent": "pi", "contract": "k-3" } }),
            json!({ "type": "left", "worker": "grace", "reason": "contract k-3 completed", "ts": 130.0 }),
            json!({ "type": "run", "state": "stopped", "ts": 140.0 }),
        ] {
            r.apply(&v);
        }
        r
    }

    #[test]
    fn people_and_operatives_from_the_journal() {
        let r = run();
        let names: Vec<_> = r.people.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["annie", "iris", "grace"]);
        let ops: Vec<_> = r.operatives().map(|p| p.name.as_str()).collect();
        assert_eq!(ops, ["grace"], "a released slot is not an operative");
        let g = r.person("grace").unwrap();
        assert_eq!(g.hired_by.as_deref(), Some("annie"));
        assert_eq!(g.left.as_deref(), Some("contract k-3 completed"));
        assert_eq!((r.task.as_str(), r.messages), ("Build it", 1));
        assert_eq!((r.state.as_str(), r.last), ("stopped", 140.0));
    }

    #[test]
    fn times() {
        assert_eq!(fmt_time(1_792_517_038.9), "2026-10-20 17:23 UTC");
        assert_eq!(fmt_time(0.0), "-");
        assert_eq!(fmt_dur(3725.0), "1h 02m");
    }

    /// A run as it is on disk: run.json, then what its journal says.
    #[test]
    fn a_run_from_its_files() {
        let state = std::env::temp_dir().join(format!("zapper-inspect-{}", std::process::id()));
        let dir = state.join("firms/acme/runs/20261020-172358-8vm");
        std::fs::create_dir_all(dir.join("transcripts")).unwrap();
        std::fs::write(
            dir.join("run.json"),
            json!({ "run": "20261020-172358-8vm", "company": "Meridian", "started": 100.0, "chart": { "workers": [{ "name": "annie", "role": "ceo" }] } }).to_string(),
        )
        .unwrap();
        let journal = [
            json!({ "seq": 1, "ts": 100.5, "type": "run", "state": "running" }),
            json!({ "seq": 2, "ts": 101.0, "type": "mail", "from": "annie", "subject": "Hello" }),
            json!({ "seq": 3, "ts": 102.0, "type": "mail", "from": "board", "subject": "Build it" }),
            json!({ "seq": 4, "ts": 103.0, "type": "mail", "from": "board", "subject": "And this" }),
            json!({ "seq": 5, "ts": 110.0, "type": "hired", "by": "annie", "worker": { "name": "bo", "role": "engineer" } }),
            json!({ "seq": 6, "ts": 150.0, "type": "worker", "worker": "bo", "state": "working" }),
            json!({ "seq": 7, "ts": 160.0, "type": "run", "state": "ended" }),
        ];
        let text: String = journal.iter().map(|v| format!("{v}\n")).collect();
        std::fs::write(dir.join("journal.jsonl"), text + "not json\n").unwrap();
        std::fs::write(dir.join("transcripts/bo.jsonl"), "").unwrap();
        let s = Store {
            state: state.clone(),
            keeper: None,
        };
        let r = s.find("20261020").unwrap();
        assert_eq!((r.firm.as_str(), r.company.as_str()), ("acme", "Meridian"));
        assert_eq!((r.task.as_str(), r.messages), ("Build it", 3));
        assert_eq!(
            (r.state.as_str(), r.started, r.last),
            ("ended", 100.0, 160.0)
        );
        let names: Vec<_> = r.people.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["annie", "bo"]);
        assert!(r.transcript(r.person("bo").unwrap()).is_some());
        assert!(r.transcript(r.person("annie").unwrap()).is_none());
        std::fs::remove_dir_all(state).unwrap();
    }

    #[test]
    fn keeper_view() {
        let s = Store {
            state: "/s".into(),
            keeper: Some("/k".into()),
        };
        assert_eq!(
            s.view(Path::new("/s/firms/acme/runs/r/journal.jsonl")),
            PathBuf::from("/k/firms/acme/runs/r/journal.jsonl")
        );
    }
}
