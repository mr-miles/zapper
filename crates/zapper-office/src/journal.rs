//! Append-only JSON-lines files of a run: the firm's journal and the workers'
//! transcripts. The daemon keeps offsets into them, not their contents. They
//! are part of the record: written through `record`, and so through the
//! keeper when there is one.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use anyhow::{Context, Result};
use serde_json::{json, Value};

use zapper_core::util::now;

/// Longest line kept; longer ones are cut and marked.
pub const MAX_LINE: usize = 256 * 1024;

pub struct Journal {
    /// Where it is read from.
    path: PathBuf,
    file: Mutex<zapper_core::record::Writer>,
    offset: AtomicU64,
    seq: AtomicU64,
}

/// Where an entry is in its file.
#[derive(Debug, Clone, Copy, Default)]
pub struct At {
    pub offset: u64,
    pub len: u32,
}

impl Journal {
    pub fn create(path: &Path) -> Result<Journal> {
        let file = zapper_core::record::Writer::open(path)
            .with_context(|| format!("creating {}", path.display()))?;
        let path = zapper_core::record::view(path);
        let offset = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        // An existing journal continues its sequence.
        let last = std::fs::read(&path).ok().and_then(|b| {
            let tail = &b[b.len().saturating_sub(MAX_LINE + 1)..];
            String::from_utf8_lossy(tail).lines().rev().find_map(|l| {
                serde_json::from_str::<Value>(l)
                    .ok()
                    .and_then(|v| v["seq"].as_u64())
            })
        });
        Ok(Journal {
            path,
            file: Mutex::new(file),
            offset: AtomicU64::new(offset),
            seq: AtomicU64::new(last.unwrap_or(0) + 1),
        })
    }

    /// Appends `{seq, ts, type, …fields}` and returns where it went.
    pub fn append(&self, kind: &str, fields: Value) -> At {
        let mut entry = json!({ "seq": 0, "ts": now(), "type": kind });
        if let (Some(e), Value::Object(f)) = (entry.as_object_mut(), fields) {
            for (k, v) in f {
                e.insert(k, v);
            }
        }
        self.write(entry)
    }

    /// Appends an arbitrary JSON value as one line, adding `seq` and `ts` if
    /// it is an object without them.
    pub fn write(&self, mut entry: Value) -> At {
        let mut file = self.file.lock().unwrap();
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        if let Some(o) = entry.as_object_mut() {
            o.insert("seq".into(), json!(seq));
            o.entry("ts").or_insert(json!(now()));
        }
        let mut line = serde_json::to_string(&entry).unwrap_or_default();
        if line.len() > MAX_LINE {
            line = json!({ "seq": seq, "ts": now(), "type": "truncated", "bytes": line.len() })
                .to_string();
        }
        // Offsets count only what was written.
        if let Err(e) = file.line(line.as_bytes()) {
            tracing::warn!("journal {}: {e:#}", self.path.display());
            return At {
                offset: self.offset.load(Ordering::Relaxed),
                len: 0,
            };
        }
        let len = line.len() as u64 + 1;
        let offset = self.offset.fetch_add(len, Ordering::Relaxed);
        At {
            offset,
            len: len as u32,
        }
    }

    /// Reads the entry at `at`.
    pub fn read(&self, at: At) -> Result<Value> {
        read_at(&self.path, at)
    }
}

pub fn read_at(path: &Path, at: At) -> Result<Value> {
    let mut f = File::open(path)?;
    f.seek(SeekFrom::Start(at.offset))?;
    let mut buf = vec![0u8; at.len as usize];
    f.read_exact(&mut buf)?;
    Ok(serde_json::from_slice(&buf)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_and_read_back() {
        let dir = std::env::temp_dir().join(format!(
            "zapper-journal-{}",
            zapper_core::util::random_id(6)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let j = Journal::create(&dir.join("j.jsonl")).unwrap();
        let a = j.append("mail", json!({ "subject": "hi" }));
        let b = j.append("worker", json!({ "worker": "ada", "state": "idle" }));
        assert_eq!(j.read(a).unwrap()["subject"], "hi");
        assert_eq!(j.read(b).unwrap()["state"], "idle");
        assert!(b.offset > a.offset);
        let big = "x".repeat(MAX_LINE + 1);
        let c = j.append("mail", json!({ "body": big }));
        assert_eq!(j.read(c).unwrap()["type"], "truncated");
        drop(j);
        let j = Journal::create(&dir.join("j.jsonl")).unwrap();
        let d = j.append("run", json!({ "state": "ended" }));
        assert_eq!(
            j.read(d).unwrap()["seq"],
            4,
            "a reopened journal continues its sequence"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
}
