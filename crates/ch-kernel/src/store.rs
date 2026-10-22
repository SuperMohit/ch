//! Session persistence: one JSONL file per session.
//!
//! Line 1 is a `meta` record, then one `event` record per history event, then a final
//! `totals` record. The file is rewritten after every turn.

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use anyhow::Context as _;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::history::{Event, History};
use crate::types::Usage;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionMeta {
    pub id: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub workspace: String,
    pub model: String,
    /// Id of the model provider `model` ran on; `None` in files saved before it was kept.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// First user prompt, shortened.
    pub title: String,
    pub turns: u32,
}

#[derive(Debug, Clone)]
pub struct SessionSnapshot {
    pub meta: SessionMeta,
    pub history: History,
    pub usage: Usage,
    pub cost: f64,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Line {
    Meta(SessionMeta),
    Event(Event),
    Totals { usage: Usage, cost: f64 },
}

/// A directory of session files for one workspace.
#[derive(Debug, Clone)]
pub struct SessionStore {
    dir: PathBuf,
}

impl SessionStore {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// `<base>/<name>-<hash>` for a workspace path, so each project gets its own list.
    pub fn for_workspace(base: &Path, workspace: &Path) -> Self {
        let s = workspace.to_string_lossy();
        // FNV-1a, stable across runs and platforms.
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for b in s.as_bytes() {
            h ^= u64::from(*b);
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
        let name: String = workspace
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "root".into())
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        Self::new(base.join(format!("{name}-{h:016x}")))
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn path_for(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.jsonl"))
    }

    /// Per-session directory for auxiliary files (elided tool outputs, ...).
    pub fn aux_dir(&self, id: &str) -> PathBuf {
        self.dir.join(id)
    }

    pub fn save(&self, snap: &SessionSnapshot) -> anyhow::Result<()> {
        fs::create_dir_all(&self.dir)
            .with_context(|| format!("creating {}", self.dir.display()))?;
        let path = self.path_for(&snap.meta.id);
        let tmp = unique_temp_path(&path);
        let written = Self::write(&tmp, snap);
        if written.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        written?;
        fs::rename(&tmp, &path)?;
        Ok(())
    }

    fn write(tmp: &Path, snap: &SessionSnapshot) -> anyhow::Result<()> {
        let mut f = std::io::BufWriter::new(fs::File::create(tmp)?);
        writeln!(
            f,
            "{}",
            serde_json::to_string(&Line::Meta(snap.meta.clone()))?
        )?;
        for ev in snap.history.events() {
            writeln!(f, "{}", serde_json::to_string(&Line::Event(ev.clone()))?)?;
        }
        writeln!(
            f,
            "{}",
            serde_json::to_string(&Line::Totals {
                usage: snap.usage,
                cost: snap.cost
            })?
        )?;
        // On disk before the rename, so a crash can't leave an empty session file.
        f.into_inner()?.sync_all()?;
        Ok(())
    }

    pub fn load(&self, id: &str) -> anyhow::Result<SessionSnapshot> {
        let path = self.path_for(id);
        let f = fs::File::open(&path).with_context(|| format!("opening {}", path.display()))?;
        let mut meta = None;
        let mut events = Vec::new();
        let mut usage = Usage::default();
        let mut cost = 0.0;
        for (n, line) in BufReader::new(f).lines().enumerate() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<Line>(&line)
                .with_context(|| format!("{}:{}", path.display(), n + 1))?
            {
                Line::Meta(m) => meta = Some(m),
                Line::Event(e) => events.push(e),
                Line::Totals { usage: u, cost: c } => {
                    usage = u;
                    cost = c;
                }
            }
        }
        let meta = meta.with_context(|| format!("{} has no meta line", path.display()))?;
        Ok(SessionSnapshot {
            meta,
            history: History::from_events(events),
            usage,
            cost,
        })
    }

    /// Sessions in this store, most recently updated first.
    pub fn list(&self) -> anyhow::Result<Vec<SessionMeta>> {
        let mut out = Vec::new();
        let Ok(rd) = fs::read_dir(&self.dir) else {
            return Ok(out);
        };
        for entry in rd.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let Ok(f) = fs::File::open(&path) else {
                continue;
            };
            let mut first = String::new();
            if BufReader::new(f).read_line(&mut first).is_err() {
                continue;
            }
            if let Ok(Line::Meta(m)) = serde_json::from_str::<Line>(&first) {
                out.push(m);
            }
        }
        out.sort_by_key(|m| std::cmp::Reverse(m.updated_at));
        Ok(out)
    }

    pub fn latest(&self) -> anyhow::Result<Option<SessionMeta>> {
        Ok(self.list()?.into_iter().next())
    }
}

/// A temp path next to `path` that no other save uses, so two processes saving the
/// same session can't write into one file.
fn unique_temp_path(path: &Path) -> PathBuf {
    path.with_extension(format!("jsonl.{}.tmp", uuid::Uuid::new_v4().simple()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Message;

    #[test]
    fn save_load_list_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::for_workspace(dir.path(), Path::new("/tmp/my proj"));
        assert!(store.dir().to_string_lossy().contains("my_proj-"));
        let mut history = History::new();
        history.push(1, Message::user("hi"));
        history.push(1, Message::assistant("hello"));
        let now = Utc::now();
        let snap = SessionSnapshot {
            meta: SessionMeta {
                id: "s1".into(),
                created_at: now,
                updated_at: now,
                workspace: "/tmp/my proj".into(),
                model: "m".into(),
                provider: Some("p".into()),
                title: "hi".into(),
                turns: 1,
            },
            history,
            usage: Usage {
                input_tokens: 5,
                ..Default::default()
            },
            cost: 0.5,
        };
        store.save(&snap).unwrap();
        let loaded = store.load("s1").unwrap();
        assert_eq!(loaded.history.len(), 2);
        assert_eq!(loaded.usage.input_tokens, 5);
        assert_eq!(loaded.meta, snap.meta);
        assert_eq!(store.list().unwrap().len(), 1);
        assert_eq!(store.latest().unwrap().unwrap().id, "s1");
    }
}
