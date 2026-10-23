//! The files a firm's run leaves in its directory, and what its journal says
//! about it: the office lists past runs from them, zapper-inspect reads them
//! whole. Nothing here writes.

use std::io::{BufRead, BufReader};
use std::path::Path;

use serde_json::Value;

/// The run's company, the chart it started with and its scenario.
pub const INFO: &str = "run.json";
/// Everything that happened, a JSON object a line.
pub const JOURNAL: &str = "journal.jsonl";
/// Each worker's agent transcript, `<worker>.jsonl`.
pub const TRANSCRIPTS: &str = "transcripts";

/// A journal's lines, as read; a line that isn't whole ends it.
pub fn lines(journal: &Path) -> impl Iterator<Item = Vec<u8>> {
    std::fs::File::open(journal)
        .ok()
        .into_iter()
        .flat_map(|f| BufReader::new(f).split(b'\n').map_while(Result::ok))
}

/// What a journal says about its run.
#[derive(Default)]
pub struct Summary {
    /// The board's first mail, as journaled: what the firm was asked to
    /// do. A run without one was never kicked off.
    pub kickoff: Option<Value>,
    pub messages: u64,
    /// When the last thing happened.
    pub last: Option<f64>,
    /// The run's last state (running, paused, ended…).
    pub state: Option<String>,
}

impl Summary {
    /// Takes in one journal entry.
    pub fn apply(&mut self, v: &Value) {
        if let Some(ts) = v["ts"].as_f64() {
            self.last = Some(ts);
        }
        match v["type"].as_str() {
            Some("mail") => {
                self.messages += 1;
                if self.kickoff.is_none() && v["from"] == "board" {
                    self.kickoff = Some(v.clone());
                }
            }
            Some("run") => {
                if let Some(s) = v["state"].as_str() {
                    self.state = Some(s.to_string());
                }
            }
            _ => {}
        }
    }

    pub fn ended(&self) -> bool {
        matches!(self.state.as_deref(), Some("ended" | "failed"))
    }

    /// A journal's summary, parsing only the lines it needs.
    pub fn read(journal: &Path) -> Summary {
        let mut sum = Summary::default();
        for line in lines(journal) {
            // Most lines are not needed whole: look before parsing.
            let needed = has(&line, br#""type":"mail""#) || has(&line, br#""type":"run""#);
            if !needed && sum.last.is_some() && line.len() > 64 {
                if let Some(ts) = ts_of(&line) {
                    sum.last = Some(ts);
                }
                continue;
            }
            if let Ok(v) = serde_json::from_slice::<Value>(&line) {
                sum.apply(&v);
            }
        }
        sum
    }
}

fn has(line: &[u8], pat: &[u8]) -> bool {
    line.windows(pat.len()).any(|w| w == pat)
}

/// `"ts":<number>` from a journal line, without parsing it all.
fn ts_of(line: &[u8]) -> Option<f64> {
    let key = br#""ts":"#;
    let at = line.windows(key.len()).position(|w| w == key)? + key.len();
    let rest = &line[at..];
    let end = rest
        .iter()
        .position(|b| {
            !(b.is_ascii_digit()
                || *b == b'.'
                || *b == b'-'
                || *b == b'e'
                || *b == b'E'
                || *b == b'+')
        })
        .unwrap_or(rest.len());
    std::str::from_utf8(&rest[..end]).ok()?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn journal(name: &str, text: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("zapper-runs-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(JOURNAL);
        std::fs::write(&path, text).unwrap();
        path
    }

    #[test]
    fn a_journal_summed_up() {
        let worker = format!(
            r#"{{"seq":3,"ts":20.5,"type":"worker","worker":"ada","detail":"{}"}}"#,
            "x".repeat(100)
        );
        let lines = [
            json!({ "seq": 1, "ts": 10.0, "type": "run", "state": "running" }).to_string(),
            json!({ "seq": 2, "ts": 11.0, "type": "mail", "from": "ceo", "subject": "Hi" })
                .to_string(),
            worker,
            json!({ "seq": 4, "ts": 30.0, "type": "mail", "from": "board", "subject": "Build it" })
                .to_string(),
            json!({ "seq": 5, "ts": 31.0, "type": "mail", "from": "board", "subject": "And this" })
                .to_string(),
            json!({ "seq": 6, "ts": 40.0, "type": "run" }).to_string(),
            "not json".to_string(),
            // The last line, without its newline, still counts.
            json!({ "seq": 7, "ts": 50.25, "type": "run", "state": "failed" }).to_string(),
        ];
        let path = journal("sum", &lines.join("\n"));
        let s = Summary::read(&path);
        assert_eq!(s.messages, 3);
        assert_eq!(s.kickoff.as_ref().unwrap()["subject"], "Build it");
        assert_eq!(s.last, Some(50.25));
        // A run line without a state keeps the last one.
        assert_eq!(s.state.as_deref(), Some("failed"));
        assert!(s.ended());
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    /// Long lines that aren't mail or the run's are only scanned for their
    /// time; a short one is parsed.
    #[test]
    fn the_last_time_comes_from_lines_never_parsed_whole() {
        let long = format!(
            r#"{{"seq":2,"ts":1.5e2,"type":"turn","detail":"{}"}}"#,
            "y".repeat(200)
        );
        let text = format!(
            "{}\n{long}\n",
            json!({ "ts": 100.0, "type": "run", "state": "running" })
        );
        let path = journal("fast", &text);
        let s = Summary::read(&path);
        assert_eq!((s.last, s.ended()), (Some(150.0), false));
        assert_eq!(s.state.as_deref(), Some("running"));
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn no_journal_is_an_empty_summary() {
        let s = Summary::read(Path::new("/nonexistent/journal.jsonl"));
        assert_eq!((s.messages, s.last, s.kickoff.is_none()), (0, None, true));
        assert_eq!(lines(Path::new("/nonexistent")).count(), 0);
    }
}
