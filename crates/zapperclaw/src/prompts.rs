//! What a worker is told: the wake that hands it its mail, and the notes
//! put before it.

use crate::journal::Journal;
use crate::mail::{PostOffice, BOARD};

/// How much of a message's body a wake shows; the rest is a `mail_read`
/// away.
const WAKE_BODY: usize = 16 * 1024;

/// The wake for worker `me`: the brief if `brief` is given (a conversation's
/// first turn), the notes, then each of `ids` in full.
pub fn wake(
    po: &PostOffice,
    journal: &Journal,
    me: &str,
    ids: &[String],
    notes: &[String],
    brief: Option<&str>,
) -> String {
    let mut p = String::new();
    if let Some(b) = brief {
        p.push_str(b);
        p.push_str("\n\n");
    }
    for n in notes {
        p.push_str(n.trim_end());
        p.push_str("\n\n");
    }
    if ids.is_empty() {
        p.push_str("No new mail. Look at your desk (the desk tool) and carry on with what you were doing, or stop.\n");
        return p;
    }
    p.push_str(&format!(
        "{} new message{} for you, {me}:\n",
        ids.len(),
        if ids.len() == 1 { "" } else { "s" }
    ));
    for id in ids {
        let Some(m) = po.messages.get(id) else {
            continue;
        };
        let body = po.body(journal, id);
        let shown = if body.len() > WAKE_BODY {
            let mut end = WAKE_BODY;
            while !body.is_char_boundary(end) {
                end -= 1;
            }
            format!(
                "{}\n[… {} more bytes: mail_read {id}]",
                &body[..end],
                body.len() - end
            )
        } else {
            body
        };
        let to = m.to.join(", ");
        let due = m
            .due
            .map(|d| format!("Answer due: in {}\n", until(d - crate::now())))
            .unwrap_or_default();
        // The headers the fake agent (zapper-fake) reads, as the office's.
        p.push_str(&format!(
            "\n---\nMessage: {id}\nThread: \"{}\" ({})\nFrom: {}\nTo: {to}\n{due}\n{}\n",
            m.subject, m.thread, m.from, shown
        ));
    }
    p.push_str(&format!(
        "\n---\nAnswer with mail_reply (the message's id); start a new thread with mail_send. Mail {BOARD} for the person who runs this office. Your desk (desk) shows what you wait for and what is awaited of you.\n"
    ));
    p
}

/// The brief a conversation starts with: who the worker is, their
/// instructions, and who else is here.
pub fn brief(me: &str, instructions: Option<&str>, directory: &[String]) -> String {
    let mut b = format!(
        "You are {me}, a worker in this office. You get mail as turns like this one; the office's tools (mail_*, desk, directory, drive_*) are how you answer and work with the others."
    );
    let others: Vec<&String> = directory.iter().filter(|n| *n != me).collect();
    if !others.is_empty() {
        b.push_str(&format!(
            " The others: {}.",
            others
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if let Some(i) = instructions {
        b.push_str("\n\nYour instructions:\n\n");
        b.push_str(i.trim());
    }
    b
}

/// After a turn that ran out of time.
pub fn timeout_note(limit: &str, subjects: &[String]) -> String {
    let on = if subjects.is_empty() {
        String::new()
    } else {
        format!(" It was on: {}.", subjects.join("; "))
    };
    format!("[Automatic note] Your last turn ran past the {limit} limit and was stopped.{on} Pick up where you left off, in smaller steps.")
}

/// After the agent stopped and was started again.
pub fn restart_note() -> String {
    "[Automatic note] Your agent stopped during the last turn and was started again; its mail is shown again below.".to_string()
}

/// A span of seconds as people say it.
pub fn until(secs: f64) -> String {
    let s = secs.max(0.0).round() as u64;
    match s {
        0..=89 => format!("{s}s"),
        90..=5399 => format!("{}m", (s + 30) / 60),
        _ => format!("{}h", (s + 1800) / 3600),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mail::Draft;

    #[test]
    fn a_wake_shows_its_mail_and_its_brief() {
        let dir = std::env::temp_dir().join(format!("zc-prompt-{}", zapper_util::random_id(8)));
        std::fs::create_dir_all(&dir).unwrap();
        let j = Journal::create(&dir.join("j.jsonl")).unwrap();
        let mut po = PostOffice::new(["ada".into(), "bob".into()], []);
        let d = Draft {
            from: "board".into(),
            to: vec!["ada".into()],
            subject: "Numbers".into(),
            body: "How many?".into(),
            ..Draft::default()
        };
        let s = po.send(&j, d, crate::now()).unwrap();
        let b = brief("ada", Some("Count things."), &["ada".into(), "bob".into()]);
        let text = wake(
            &po,
            &j,
            "ada",
            std::slice::from_ref(&s.id),
            &["A note.".into()],
            Some(&b),
        );
        assert!(text.starts_with("You are ada"), "{text}");
        assert!(text.contains("The others: bob."), "{text}");
        assert!(text.contains("Count things."), "{text}");
        assert!(text.contains("A note."), "{text}");
        assert!(
            text.contains(&format!(
                "Message: {}\nThread: \"Numbers\" ({})\nFrom: board\n",
                s.id, s.thread
            )),
            "{text}"
        );
        assert!(text.contains("How many?"), "{text}");
        let nothing = wake(&po, &j, "ada", &[], &[], None);
        assert!(nothing.starts_with("No new mail"), "{nothing}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn spans() {
        assert_eq!(until(30.0), "30s");
        assert_eq!(until(600.0), "10m");
        assert_eq!(until(7200.0), "2h");
        assert_eq!(until(-5.0), "0s");
    }
}
