use std::collections::HashMap;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::{Config, Paths};
use crate::fsutil;
use crate::setup::ClientPaths;
use crate::transcripts::{Tool, Upload};

const SYNC_EVERY: Duration = Duration::from_secs(15);
const MAX_BATCH: usize = 4 * 1024 * 1024;
const MAX_LINE: usize = 16 * 1024 * 1024;
const MAX_BYTES_PER_PASS: u64 = 64 * 1024 * 1024;

const DROPPED_LINE_TYPES: &[&str] = &[
    "api-request",
    "api-request-blob",
    "api-request-shape",
    "attachment",
    "atis-latch",
    "cost-state",
    "queue-operation",
    "world_state",
    "turn_context",
    "token_usage_record",
];
const DROPPED_PAYLOAD_TYPES: &[&str] = &["reasoning", "token_count"];

#[derive(Deserialize)]
struct PayloadKind {
    #[serde(rename = "type")]
    kind: Option<String>,
}

#[derive(Deserialize)]
struct LineKind {
    #[serde(rename = "type")]
    kind: Option<String>,
    payload: Option<PayloadKind>,
}

pub fn keep_line(line: &[u8]) -> bool {
    let Ok(parsed) = serde_json::from_slice::<LineKind>(line) else {
        return false;
    };
    let dropped_type = parsed.kind.as_deref().is_some_and(|k| DROPPED_LINE_TYPES.contains(&k));
    let dropped_payload = parsed
        .payload
        .and_then(|p| p.kind)
        .is_some_and(|k| DROPPED_PAYLOAD_TYPES.contains(&k.as_str()));
    !dropped_type && !dropped_payload
}

#[derive(Debug, PartialEq)]
pub struct Batch {
    pub next_offset: u64,
    pub data: String,
}

pub fn read_batch(path: &Path, offset: u64, max_bytes: usize) -> Result<Batch> {
    let mut file = std::fs::File::open(path)?;
    file.seek(SeekFrom::Start(offset))?;
    let mut reader = BufReader::new(file);
    let mut next_offset = offset;
    let mut data = String::new();
    let mut line = Vec::new();
    while data.len() < max_bytes {
        line.clear();
        let n = reader.read_until(b'\n', &mut line)?;
        if n == 0 || line.last() != Some(&b'\n') {
            break;
        }
        next_offset += n as u64;
        if line.len() > MAX_LINE || !keep_line(&line) {
            continue;
        }
        if let Ok(text) = std::str::from_utf8(&line) {
            data.push_str(text);
        }
    }
    Ok(Batch { next_offset, data })
}

#[derive(Debug, PartialEq)]
pub struct SourceFile {
    pub tool: Tool,
    pub absolute: PathBuf,
    pub relative: String,
    pub size: u64,
    pub modified: SystemTime,
}

fn collect(root: &Path, dir: &Path, tool: Tool, out: &mut Vec<SourceFile>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let Ok(kind) = entry.file_type() else { continue };
        let path = entry.path();
        if kind.is_dir() {
            collect(root, &path, tool, out);
        } else if kind.is_file() && path.extension().is_some_and(|e| e == "jsonl") {
            let (Ok(meta), Ok(rel)) = (entry.metadata(), path.strip_prefix(root)) else {
                continue;
            };
            out.push(SourceFile {
                tool,
                relative: rel.to_string_lossy().replace('\\', "/"),
                size: meta.len(),
                modified: meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                absolute: path.clone(),
            });
        }
    }
}

pub fn find_sources(claude_projects: &Path, codex_sessions: &Path) -> Vec<SourceFile> {
    let mut out = Vec::new();
    collect(claude_projects, claude_projects, Tool::Claude, &mut out);
    collect(codex_sessions, codex_sessions, Tool::Codex, &mut out);
    out.sort_by_key(|f| std::cmp::Reverse(f.modified));
    out
}

pub type Destination = Arc<dyn Fn() -> String + Send + Sync>;

pub struct TranscriptSync {
    machine: String,
    paths: Paths,
    claude_projects: PathBuf,
    codex_sessions: PathBuf,
    http: reqwest::Client,
    destination: Destination,
    saved: SavedOffsets,
}

#[derive(Default, Serialize, Deserialize)]
struct SavedOffsets {
    destination: String,
    offsets: HashMap<String, u64>,
}

fn offsets_file(paths: &Paths) -> PathBuf {
    paths.data_dir.join("transcript-offsets.json")
}

fn key(file: &SourceFile) -> String {
    format!("{}/{}", file.tool.as_str(), file.relative)
}

impl TranscriptSync {
    pub fn new(machine: String, paths: Paths, clients: &ClientPaths, destination: Destination) -> Self {
        let parent = |p: &Path| p.parent().map(Path::to_path_buf).unwrap_or_default();
        let saved = fsutil::read_json(&offsets_file(&paths))
            .ok()
            .flatten()
            .unwrap_or_default();
        Self {
            machine,
            claude_projects: parent(&clients.claude_settings).join("projects"),
            codex_sessions: parent(&clients.codex_config).join("sessions"),
            paths,
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(60))
                .build()
                .expect("build sync client"),
            destination,
            saved,
        }
    }

    pub async fn send(&self, up: &Upload) -> Result<u64> {
        let resp = self
            .http
            .post(format!("{}/conflux/transcripts", (self.destination)()))
            .json(up)
            .send()
            .await?
            .error_for_status()?;
        let v: Value = resp.json().await?;
        v["expected"]
            .as_u64()
            .ok_or_else(|| anyhow::anyhow!("the hub sent no offset"))
    }

    pub async fn sync_file(&mut self, file: &SourceFile, budget: &mut u64) -> Result<()> {
        let k = key(file);
        let mut offset = self.saved.offsets.get(&k).copied().unwrap_or(0);
        while offset < file.size && *budget > 0 {
            let path = file.absolute.clone();
            let batch = tokio::task::spawn_blocking(move || read_batch(&path, offset, MAX_BATCH)).await??;
            if batch.next_offset == offset {
                break;
            }
            let up = Upload {
                machine: self.machine.clone(),
                tool: file.tool,
                path: file.relative.clone(),
                offset,
                next_offset: batch.next_offset,
                data: batch.data,
            };
            let expected = self.send(&up).await?;
            *budget = budget.saturating_sub(up.data.len() as u64 + 1);
            let accepted = expected == batch.next_offset;
            self.saved.offsets.insert(k.clone(), expected);
            if !accepted && expected == offset {
                break;
            }
            offset = expected;
        }
        Ok(())
    }

    pub async fn pass(&mut self) -> Result<()> {
        let destination = (self.destination)();
        if destination != self.saved.destination {
            self.saved.offsets.clear();
            self.saved.destination = destination;
        }
        let (claude, codex) = (self.claude_projects.clone(), self.codex_sessions.clone());
        let files = tokio::task::spawn_blocking(move || find_sources(&claude, &codex)).await?;
        let mut budget = MAX_BYTES_PER_PASS;
        let mut result = Ok(());
        for file in &files {
            if budget == 0 {
                break;
            }
            if self.saved.offsets.get(&key(file)).copied().unwrap_or(0) >= file.size {
                continue;
            }
            if let Err(e) = self.sync_file(file, &mut budget).await {
                result = Err(e);
                break;
            }
        }
        fsutil::ensure_private_dir(&self.paths.data_dir)?;
        fsutil::write_json_durable(&offsets_file(&self.paths), &self.saved)?;
        result
    }

    pub async fn run(mut self) {
        let mut last_error = None;
        loop {
            let enabled = Config::load(&self.paths).map(|c| c.transcript_sync).unwrap_or(false);
            if enabled {
                match self.pass().await {
                    Ok(()) => last_error = None,
                    Err(e) => {
                        let text = format!("{e:#}");
                        if last_error.as_ref() != Some(&text) {
                            tracing::warn!("transcript sync failed: {text}");
                        }
                        last_error = Some(text);
                    }
                }
            }
            tokio::time::sleep(SYNC_EVERY).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_lines_are_dropped() {
        assert!(keep_line(br#"{"type":"user","message":{"content":"hi"}}"#));
        assert!(keep_line(br#"{"type":"ai-title","aiTitle":"x"}"#));
        assert!(!keep_line(br#"{"type":"api-request-blob","message":{}}"#));
        assert!(!keep_line(br#"{"type":"attachment"}"#));
        assert!(!keep_line(
            br#"{"type":"response_item","payload":{"type":"reasoning"}}"#
        ));
        assert!(keep_line(
            br#"{"type":"response_item","payload":{"type":"function_call"}}"#
        ));
        assert!(!keep_line(b"not json"));
    }

    #[test]
    fn batches_stop_at_complete_lines() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("a.jsonl");
        let line = "{\"type\":\"user\"}\n";
        std::fs::write(&p, format!("{line}{line}{{\"type\":\"us")).unwrap();
        let b = read_batch(&p, 0, MAX_BATCH).unwrap();
        assert_eq!(b.next_offset, (line.len() * 2) as u64);
        assert_eq!(b.data, format!("{line}{line}"));
        let again = read_batch(&p, b.next_offset, MAX_BATCH).unwrap();
        assert_eq!(
            again.next_offset, b.next_offset,
            "a partial last line waits for its newline"
        );
    }

    #[test]
    fn dropped_lines_still_advance_the_offset() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("a.jsonl");
        let dropped = "{\"type\":\"api-request\"}\n";
        std::fs::write(&p, dropped).unwrap();
        let b = read_batch(&p, 0, MAX_BATCH).unwrap();
        assert_eq!(b.next_offset, dropped.len() as u64);
        assert!(b.data.is_empty());
    }

    #[test]
    fn batch_size_is_bounded() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("a.jsonl");
        let line = "{\"type\":\"user\"}\n";
        std::fs::write(&p, line.repeat(100)).unwrap();
        let b = read_batch(&p, 0, line.len() * 3).unwrap();
        assert_eq!(b.next_offset, (line.len() * 3) as u64);
    }

    #[test]
    fn sources_use_forward_slashes_and_newest_first() {
        let d = tempfile::tempdir().unwrap();
        let claude = d.path().join("claude/projects");
        let codex = d.path().join("codex/sessions");
        std::fs::create_dir_all(claude.join("p/s/subagents")).unwrap();
        std::fs::create_dir_all(&codex).unwrap();
        std::fs::write(claude.join("p/s/subagents/a.jsonl"), "x").unwrap();
        std::fs::write(claude.join("p/ignored.txt"), "x").unwrap();
        std::fs::write(codex.join("b.jsonl"), "y").unwrap();
        let found = find_sources(&claude, &codex);
        assert_eq!(found.len(), 2);
        assert!(
            found
                .iter()
                .any(|f| f.relative == "p/s/subagents/a.jsonl" && f.tool == Tool::Claude)
        );
        assert!(found.iter().any(|f| f.relative == "b.jsonl" && f.tool == Tool::Codex));
    }
}
