use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path as FsPath, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::app::App;
use crate::fsutil;

const STATE_DIR: &str = ".state";
const INDEX_FILE: &str = "index.json";
const MAX_EVENT_TEXT: usize = 6000;
const MAX_EVENTS: usize = 2000;
const MAX_LISTED: usize = 300;
const MAX_SEARCH_HITS: usize = 50;
const SEARCH_BUDGET: Duration = Duration::from_secs(15);
const INDEX_FLUSH_EVERY: Duration = Duration::from_secs(60);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Tool {
    Claude,
    Codex,
}

impl Tool {
    pub fn as_str(self) -> &'static str {
        match self {
            Tool::Claude => "claude",
            Tool::Codex => "codex",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    User,
    Assistant,
    ToolCall,
    ToolResult,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Event {
    pub kind: EventKind,
    pub name: Option<String>,
    pub text: String,
    pub at: Option<DateTime<Utc>>,
}

fn text_of(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts
            .iter()
            .map(|p| match p["type"].as_str() {
                Some("image" | "input_image") => "[image]".to_string(),
                _ => p["text"].as_str().map(str::to_string).unwrap_or_default(),
            })
            .filter(|t| !t.is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn timestamp(v: &Value) -> Option<DateTime<Utc>> {
    v["timestamp"]
        .as_str()
        .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
        .map(|d| d.with_timezone(&Utc))
}

fn event(kind: EventKind, name: Option<&str>, text: String, at: Option<DateTime<Utc>>) -> Event {
    Event {
        kind,
        name: name.map(str::to_string),
        text,
        at,
    }
}

fn claude_events(v: &Value) -> Vec<Event> {
    let at = timestamp(v);
    if v["isMeta"].as_bool() == Some(true) {
        return Vec::new();
    }
    let content = &v["message"]["content"];
    match v["type"].as_str() {
        Some("user") => {
            let Some(parts) = content.as_array() else {
                let text = text_of(content);
                return if text.is_empty() {
                    Vec::new()
                } else {
                    vec![event(EventKind::User, None, text, at)]
                };
            };
            parts
                .iter()
                .filter_map(|p| match p["type"].as_str() {
                    Some("tool_result") => {
                        let kind = if p["is_error"].as_bool() == Some(true) {
                            EventKind::Failed
                        } else {
                            EventKind::ToolResult
                        };
                        Some(event(kind, None, text_of(&p["content"]), at))
                    }
                    Some("text") => Some(event(EventKind::User, None, p["text"].as_str()?.to_string(), at)),
                    Some("image") => Some(event(EventKind::User, None, "[image]".into(), at)),
                    _ => None,
                })
                .collect()
        }
        Some("assistant") => content
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|p| match p["type"].as_str() {
                Some("text") => Some(event(EventKind::Assistant, None, p["text"].as_str()?.to_string(), at)),
                Some("tool_use") => Some(event(
                    EventKind::ToolCall,
                    p["name"].as_str(),
                    p["input"].to_string(),
                    at,
                )),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

pub fn codex_output_failed(text: &str) -> bool {
    let after_digit = |marker: &str| {
        text.match_indices(marker).any(|(i, _)| {
            let digits: String = text[i + marker.len()..]
                .trim_start()
                .chars()
                .take_while(char::is_ascii_digit)
                .collect();
            digits.parse::<u32>().is_ok_and(|n| n != 0)
        })
    };
    after_digit("Process exited with code") || after_digit("\"exit_code\":")
}

fn codex_events(v: &Value) -> Vec<Event> {
    let at = timestamp(v);
    let p = &v["payload"];
    let one = |e: Event| vec![e];
    match (v["type"].as_str(), p["type"].as_str()) {
        (Some("event_msg"), Some("user_message")) => one(event(
            EventKind::User,
            None,
            p["message"].as_str().unwrap_or("").to_string(),
            at,
        )),
        (Some("event_msg"), Some("agent_message")) => one(event(
            EventKind::Assistant,
            None,
            p["message"].as_str().unwrap_or("").to_string(),
            at,
        )),
        (Some("response_item"), Some("function_call")) => one(event(
            EventKind::ToolCall,
            p["name"].as_str(),
            p["arguments"].as_str().unwrap_or("").to_string(),
            at,
        )),
        (Some("response_item"), Some("custom_tool_call")) => one(event(
            EventKind::ToolCall,
            p["name"].as_str(),
            p["input"].as_str().unwrap_or("").to_string(),
            at,
        )),
        (Some("response_item"), Some("local_shell_call")) => one(event(
            EventKind::ToolCall,
            Some("shell"),
            p["action"]["command"].to_string(),
            at,
        )),
        (Some("response_item"), Some("function_call_output" | "custom_tool_call_output")) => {
            let text = text_of(&p["output"]);
            let failed = p["success"].as_bool() == Some(false) || codex_output_failed(&text);
            one(event(
                if failed {
                    EventKind::Failed
                } else {
                    EventKind::ToolResult
                },
                None,
                text,
                at,
            ))
        }
        _ => Vec::new(),
    }
}

pub fn events_from_line(tool: Tool, v: &Value) -> Vec<Event> {
    match tool {
        Tool::Claude => claude_events(v),
        Tool::Codex => codex_events(v),
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ThreadSummary {
    pub id: String,
    pub machine: String,
    pub tool: Tool,
    pub path: String,
    pub project: Option<String>,
    pub title: Option<String>,
    pub started: Option<DateTime<Utc>>,
    pub last_active: Option<DateTime<Utc>>,
    pub turns: u32,
    pub errors: u32,
    pub subagent: bool,
    pub bytes: u64,
    #[serde(default)]
    pub titled: bool,
}

fn thread_id(rel: &str) -> String {
    URL_SAFE_NO_PAD.encode(rel)
}

fn one_line(text: &str, max: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        flat
    } else {
        format!("{}…", flat.chars().take(max).collect::<String>())
    }
}

impl ThreadSummary {
    fn new(machine: &str, tool: Tool, path: &str, rel: &str) -> Self {
        Self {
            id: thread_id(rel),
            machine: machine.to_string(),
            tool,
            path: path.to_string(),
            project: None,
            title: None,
            started: None,
            last_active: None,
            turns: 0,
            errors: 0,
            subagent: path.contains("/subagents/"),
            bytes: 0,
            titled: false,
        }
    }

    pub fn apply(&mut self, v: &Value) {
        if let Some(t) = v["aiTitle"].as_str().or_else(|| v["customTitle"].as_str()) {
            self.title = Some(one_line(t, 120));
            self.titled = true;
        }
        if self.project.is_none() {
            self.project = v["cwd"]
                .as_str()
                .or_else(|| v["payload"]["cwd"].as_str())
                .map(str::to_string);
        }
        if v["isSidechain"].as_bool() == Some(true) || v["payload"]["source"]["subagent"].is_object() {
            self.subagent = true;
        }
        if let Some(t) = timestamp(v) {
            self.started = Some(self.started.map_or(t, |s| s.min(t)));
            self.last_active = Some(self.last_active.map_or(t, |s| s.max(t)));
        }
        for e in events_from_line(self.tool, v) {
            match e.kind {
                EventKind::User => {
                    self.turns += 1;
                    if self.title.is_none() {
                        self.title = Some(one_line(&e.text, 120));
                    }
                }
                EventKind::Failed => self.errors += 1,
                _ => {}
            }
        }
    }
}

#[derive(Default)]
struct Index {
    threads: HashMap<String, ThreadSummary>,
    dirty: bool,
}

pub struct Transcripts {
    root: PathBuf,
    index: Mutex<Index>,
    uploads: tokio::sync::Mutex<()>,
}

fn is_safe_segment(s: &str) -> bool {
    !s.is_empty() && s.len() <= 255 && s != "." && s != ".." && !s.contains(['\\', ':', '\0'])
}

pub fn valid_machine(machine: &str) -> bool {
    !machine.is_empty()
        && machine.len() <= 128
        && !machine.starts_with('.')
        && machine
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

pub fn valid_path(path: &str) -> bool {
    path.ends_with(".jsonl") && !path.starts_with('/') && path.split('/').all(is_safe_segment)
}

fn walk_jsonl(dir: &FsPath, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let Ok(kind) = entry.file_type() else { continue };
        let path = entry.path();
        if kind.is_dir() && entry.file_name() != STATE_DIR {
            walk_jsonl(&path, out);
        } else if kind.is_file() && path.extension().is_some_and(|e| e == "jsonl") {
            out.push(path);
        }
    }
}

impl Transcripts {
    pub fn open(root: &FsPath) -> Result<Arc<Self>> {
        fsutil::ensure_private_dir(root)?;
        let saved: Vec<ThreadSummary> = fsutil::read_json(&root.join(STATE_DIR).join(INDEX_FILE))?.unwrap_or_default();
        let threads = saved.into_iter().map(|t| (Self::rel(&t), t)).collect();
        Ok(Arc::new(Self {
            root: root.to_path_buf(),
            index: Mutex::new(Index { threads, dirty: false }),
            uploads: tokio::sync::Mutex::new(()),
        }))
    }

    fn rel(t: &ThreadSummary) -> String {
        format!("{}/{}/{}", t.machine, t.tool.as_str(), t.path)
    }

    fn offset_file(&self, rel: &str) -> PathBuf {
        self.root.join(STATE_DIR).join(format!("{rel}.offset"))
    }

    fn read_offset(&self, rel: &str) -> u64 {
        std::fs::read_to_string(self.offset_file(rel))
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0)
    }

    pub fn append(&self, up: &Upload) -> Result<u64> {
        let rel = format!("{}/{}/{}", up.machine, up.tool.as_str(), up.path);
        let current = self.read_offset(&rel);
        if up.offset != current {
            return Ok(current);
        }
        let target = self.root.join(&rel);
        if let Some(dir) = target.parent() {
            fsutil::ensure_private_dir_all(dir, &self.root)?;
        }
        if !up.data.is_empty() {
            let mut file = fsutil::open_private_append(&target)?;
            file.write_all(up.data.as_bytes())?;
        }
        let offset_file = self.offset_file(&rel);
        if let Some(dir) = offset_file.parent() {
            fsutil::ensure_private_dir_all(dir, &self.root)?;
        }
        std::fs::write(offset_file, up.next_offset.to_string())?;

        let mut index = self.index.lock().unwrap();
        let summary = index
            .threads
            .entry(rel.clone())
            .or_insert_with(|| ThreadSummary::new(&up.machine, up.tool, &up.path, &rel));
        summary.bytes += up.data.len() as u64;
        for line in up.data.lines() {
            if let Ok(v) = serde_json::from_str::<Value>(line) {
                summary.apply(&v);
            }
        }
        index.dirty = true;
        Ok(up.next_offset)
    }

    fn summarize_file(&self, rel: &str, machine: &str, tool: Tool, path: &str, file: &FsPath) -> Option<ThreadSummary> {
        let reader = BufReader::new(std::fs::File::open(file).ok()?);
        let mut summary = ThreadSummary::new(machine, tool, path, rel);
        for line in reader.lines().map_while(Result::ok) {
            summary.bytes += line.len() as u64 + 1;
            if let Ok(v) = serde_json::from_str::<Value>(&line) {
                summary.apply(&v);
            }
        }
        Some(summary)
    }

    pub fn reindex(&self) {
        let mut files = Vec::new();
        walk_jsonl(&self.root, &mut files);
        for file in files {
            let Ok(rel_path) = file.strip_prefix(&self.root) else {
                continue;
            };
            let rel = rel_path.to_string_lossy().replace('\\', "/");
            let mut parts = rel.splitn(3, '/');
            let (Some(machine), Some(tool), Some(path)) = (parts.next(), parts.next(), parts.next()) else {
                continue;
            };
            let tool = match tool {
                "claude" => Tool::Claude,
                "codex" => Tool::Codex,
                _ => continue,
            };
            let size = file.metadata().map(|m| m.len()).unwrap_or(0);
            let known = self.index.lock().unwrap().threads.get(&rel).map(|t| t.bytes);
            if known == Some(size) {
                continue;
            }
            if let Some(summary) = self.summarize_file(&rel, machine, tool, path, &file) {
                let mut index = self.index.lock().unwrap();
                index.threads.insert(rel, summary);
                index.dirty = true;
            }
        }
    }

    pub fn flush(&self) -> Result<()> {
        let snapshot: Vec<ThreadSummary> = {
            let mut index = self.index.lock().unwrap();
            if !index.dirty {
                return Ok(());
            }
            index.dirty = false;
            index.threads.values().cloned().collect()
        };
        let dir = self.root.join(STATE_DIR);
        fsutil::ensure_private_dir(&dir)?;
        fsutil::write_json_durable(&dir.join(INDEX_FILE), &snapshot)
    }

    pub async fn run_maintenance(self: Arc<Self>) {
        let this = self.clone();
        let _ = tokio::task::spawn_blocking(move || this.reindex()).await;
        loop {
            let this = self.clone();
            match tokio::task::spawn_blocking(move || this.flush()).await {
                Ok(Err(e)) => tracing::warn!("could not save the transcript index: {e:#}"),
                Err(e) => tracing::warn!("transcript index task failed: {e}"),
                Ok(Ok(())) => {}
            }
            tokio::time::sleep(INDEX_FLUSH_EVERY).await;
        }
    }

    pub fn list(&self) -> Vec<ThreadSummary> {
        let mut all: Vec<_> = self.index.lock().unwrap().threads.values().cloned().collect();
        all.sort_by_key(|t| std::cmp::Reverse(t.last_active));
        all
    }

    fn find(&self, id: &str) -> Option<ThreadSummary> {
        self.index
            .lock()
            .unwrap()
            .threads
            .values()
            .find(|t| t.id == id)
            .cloned()
    }

    fn file_of(&self, t: &ThreadSummary) -> PathBuf {
        self.root.join(Self::rel(t))
    }

    pub fn read_events(&self, t: &ThreadSummary) -> Result<(Vec<Event>, usize)> {
        let reader = BufReader::new(std::fs::File::open(self.file_of(t))?);
        let mut events = VecDeque::new();
        let mut dropped = 0;
        for line in reader.lines().map_while(Result::ok) {
            let Ok(v) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            for mut e in events_from_line(t.tool, &v) {
                if e.text.chars().count() > MAX_EVENT_TEXT {
                    e.text = format!("{}…", e.text.chars().take(MAX_EVENT_TEXT).collect::<String>());
                }
                events.push_back(e);
                if events.len() > MAX_EVENTS {
                    events.pop_front();
                    dropped += 1;
                }
            }
        }
        Ok((events.into(), dropped))
    }

    pub fn search(&self, query: &str) -> Vec<Value> {
        let needle = query.to_lowercase();
        let started = Instant::now();
        let mut hits = Vec::new();
        if needle.is_empty() {
            return hits;
        }
        for t in self.list() {
            if hits.len() >= MAX_SEARCH_HITS || started.elapsed() > SEARCH_BUDGET {
                break;
            }
            let Ok(file) = std::fs::File::open(self.file_of(&t)) else {
                continue;
            };
            for line in BufReader::new(file).lines().map_while(Result::ok) {
                let lower = line.to_lowercase();
                if !lower.contains(&needle) {
                    continue;
                }
                let Ok(v) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                let matched = events_from_line(t.tool, &v)
                    .into_iter()
                    .find(|e| e.text.to_lowercase().contains(&needle));
                let Some(e) = matched else { continue };
                hits.push(json!({
                    "thread": t.id,
                    "title": t.title,
                    "machine": t.machine,
                    "kind": e.kind,
                    "at": e.at,
                    "snippet": snippet(&e.text, &needle),
                }));
                break;
            }
        }
        hits
    }
}

fn snippet(text: &str, lowered_needle: &str) -> String {
    let lower = text.to_lowercase();
    let chars_before = lower
        .find(lowered_needle)
        .map(|i| lower[..i].chars().count())
        .unwrap_or(0);
    let from = chars_before.saturating_sub(80);
    one_line(&text.chars().skip(from).take(240).collect::<String>(), 240)
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Upload {
    pub machine: String,
    pub tool: Tool,
    pub path: String,
    pub offset: u64,
    pub next_offset: u64,
    pub data: String,
}

fn fail(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(json!({ "error": message.into() }))).into_response()
}

pub async fn upload(State(app): State<Arc<App>>, Json(up): Json<Upload>) -> Response {
    if !valid_machine(&up.machine) || !valid_path(&up.path) || up.next_offset < up.offset {
        return fail(StatusCode::BAD_REQUEST, "invalid transcript upload");
    }
    let _serialized = app.transcripts.uploads.lock().await;
    match app.transcripts.append(&up) {
        Ok(expected) => Json(json!({ "expected": expected })).into_response(),
        Err(e) => fail(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
}

#[derive(Deserialize)]
pub struct ListQuery {
    machine: Option<String>,
    errors: Option<bool>,
    subagents: Option<bool>,
}

pub async fn list(State(app): State<Arc<App>>, Query(q): Query<ListQuery>) -> Json<Value> {
    let all = app.transcripts.list();
    let mut machines: Vec<String> = all.iter().map(|t| t.machine.clone()).collect();
    machines.sort();
    machines.dedup();
    let filtered: Vec<_> = all
        .into_iter()
        .filter(|t| q.machine.as_deref().is_none_or(|m| m.is_empty() || m == t.machine))
        .filter(|t| q.errors != Some(true) || t.errors > 0)
        .filter(|t| q.subagents == Some(true) || !t.subagent)
        .collect();
    let total = filtered.len();
    let threads: Vec<_> = filtered.into_iter().take(MAX_LISTED).collect();
    Json(json!({ "threads": threads, "total": total, "machines": machines }))
}

pub async fn thread(State(app): State<Arc<App>>, Path(id): Path<String>) -> Response {
    let Some(summary) = app.transcripts.find(&id) else {
        return fail(StatusCode::NOT_FOUND, "thread not found");
    };
    let transcripts = app.transcripts.clone();
    let read_summary = summary.clone();
    let result = tokio::task::spawn_blocking(move || transcripts.read_events(&read_summary)).await;
    match result {
        Ok(Ok((events, omitted))) => {
            Json(json!({ "thread": summary, "events": events, "omitted": omitted })).into_response()
        }
        Ok(Err(e)) => fail(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
        Err(e) => fail(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

#[derive(Deserialize)]
pub struct SearchQuery {
    q: String,
}

pub async fn search(State(app): State<Arc<App>>, Query(q): Query<SearchQuery>) -> Response {
    let transcripts = app.transcripts.clone();
    match tokio::task::spawn_blocking(move || transcripts.search(q.q.trim())).await {
        Ok(hits) => Json(json!({ "hits": hits })).into_response(),
        Err(e) => fail(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn up(machine: &str, path: &str, offset: u64, data: &str) -> Upload {
        Upload {
            machine: machine.into(),
            tool: Tool::Claude,
            path: path.into(),
            offset,
            next_offset: offset + data.len() as u64,
            data: data.into(),
        }
    }

    const USER: &str = r#"{"type":"user","timestamp":"2026-11-07T10:00:00Z","cwd":"/srv/x","message":{"role":"user","content":"Fix the flaky test"}}"#;
    const FAILED: &str = r#"{"type":"user","timestamp":"2026-11-07T10:01:00Z","message":{"content":[{"type":"tool_result","is_error":true,"content":"Exit code 1"}]}}"#;
    const CALL: &str = r#"{"type":"assistant","timestamp":"2026-11-07T10:00:30Z","message":{"content":[{"type":"text","text":"Running it"},{"type":"tool_use","name":"Bash","input":{"command":"cargo test"}}]}}"#;

    #[test]
    fn claude_lines_normalize() {
        let v: Value = serde_json::from_str(CALL).unwrap();
        let e = events_from_line(Tool::Claude, &v);
        assert_eq!(e[0].kind, EventKind::Assistant);
        assert_eq!(e[1].kind, EventKind::ToolCall);
        assert_eq!(e[1].name.as_deref(), Some("Bash"));
        let v: Value = serde_json::from_str(FAILED).unwrap();
        assert_eq!(events_from_line(Tool::Claude, &v)[0].kind, EventKind::Failed);
        let meta = json!({"type": "user", "isMeta": true, "message": {"content": "injected"}});
        assert!(events_from_line(Tool::Claude, &meta).is_empty());
    }

    #[test]
    fn codex_lines_normalize() {
        let user = json!({"type": "event_msg", "payload": {"type": "user_message", "message": "hello"}});
        assert_eq!(events_from_line(Tool::Codex, &user)[0].kind, EventKind::User);
        let call =
            json!({"type": "response_item", "payload": {"type": "custom_tool_call", "name": "exec", "input": "ls"}});
        assert_eq!(events_from_line(Tool::Codex, &call)[0].name.as_deref(), Some("exec"));
        let ok = json!({"type": "response_item", "payload": {"type": "function_call_output", "output": "Process exited with code 0"}});
        assert_eq!(events_from_line(Tool::Codex, &ok)[0].kind, EventKind::ToolResult);
        let bad = json!({"type": "response_item", "payload": {"type": "custom_tool_call_output",
            "output": [{"type": "input_text", "text": "Process exited with code 101"}]}});
        assert_eq!(events_from_line(Tool::Codex, &bad)[0].kind, EventKind::Failed);
        let reasoning = json!({"type": "response_item", "payload": {"type": "reasoning"}});
        assert!(events_from_line(Tool::Codex, &reasoning).is_empty());
    }

    #[test]
    fn failure_detection() {
        assert!(codex_output_failed("Process exited with code 2\nOutput"));
        assert!(codex_output_failed(r#"{"exit_code":1}"#));
        assert!(!codex_output_failed("Process exited with code 0"));
        assert!(!codex_output_failed("it worked"));
    }

    #[test]
    fn paths_are_validated() {
        assert!(valid_path("-srv-x/abc.jsonl"));
        assert!(valid_path("2026/11/07/rollout.jsonl"));
        assert!(!valid_path("../evil.jsonl"));
        assert!(!valid_path("/etc/evil.jsonl"));
        assert!(!valid_path("a/../../b.jsonl"));
        assert!(!valid_path("a/b.txt"));
        assert!(!valid_path(r"a\b.jsonl"));
        assert!(valid_machine("node-one"));
        assert!(!valid_machine(".state"));
        assert!(!valid_machine("a/b"));
    }

    #[test]
    fn append_checks_offsets_and_summarizes() {
        let d = tempfile::tempdir().unwrap();
        let t = Transcripts::open(d.path()).unwrap();
        let first = format!("{USER}\n{CALL}\n");
        assert_eq!(
            t.append(&up("hub-box", "p/a.jsonl", 0, &first)).unwrap(),
            first.len() as u64
        );
        assert_eq!(
            t.append(&up("hub-box", "p/a.jsonl", 0, &first)).unwrap(),
            first.len() as u64,
            "replay is refused"
        );
        let second = format!("{FAILED}\n");
        t.append(&up("hub-box", "p/a.jsonl", first.len() as u64, &second))
            .unwrap();
        let stored = std::fs::read_to_string(d.path().join("hub-box/claude/p/a.jsonl")).unwrap();
        assert_eq!(stored, format!("{first}{second}"));

        let s = &t.list()[0];
        assert_eq!(s.turns, 1);
        assert_eq!(s.errors, 1);
        assert_eq!(s.title.as_deref(), Some("Fix the flaky test"));
        assert_eq!(s.project.as_deref(), Some("/srv/x"));
        let (events, omitted) = t.read_events(s).unwrap();
        assert_eq!(events.len(), 4);
        assert_eq!(omitted, 0);
    }

    #[test]
    fn a_new_hub_asks_for_everything_again() {
        let d = tempfile::tempdir().unwrap();
        let t = Transcripts::open(d.path()).unwrap();
        assert_eq!(t.append(&up("hub-box", "a.jsonl", 500, "x\n")).unwrap(), 0);
        assert!(!d.path().join("hub-box/claude/a.jsonl").exists());
    }

    #[test]
    fn index_survives_a_restart_and_reindex_finds_new_files() {
        let d = tempfile::tempdir().unwrap();
        let line = format!("{USER}\n");
        {
            let t = Transcripts::open(d.path()).unwrap();
            t.append(&up("hub-box", "a.jsonl", 0, &line)).unwrap();
            t.flush().unwrap();
        }
        let dir = d.path().join("node-two/codex/2026");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("r.jsonl"),
            "{\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\",\"message\":\"hi\"}}\n",
        )
        .unwrap();
        let t = Transcripts::open(d.path()).unwrap();
        assert_eq!(t.list().len(), 1);
        t.reindex();
        assert_eq!(t.list().len(), 2);
        assert!(
            t.list()
                .iter()
                .any(|s| s.machine == "node-two" && s.tool == Tool::Codex && s.turns == 1)
        );
    }

    #[test]
    fn search_finds_message_text() {
        let d = tempfile::tempdir().unwrap();
        let t = Transcripts::open(d.path()).unwrap();
        t.append(&up("hub-box", "a.jsonl", 0, &format!("{USER}\n{FAILED}\n")))
            .unwrap();
        let hits = t.search("EXIT CODE");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0]["kind"], "failed");
        assert!(hits[0]["snippet"].as_str().unwrap().contains("Exit code 1"));
        assert!(t.search("no such thing").is_empty());
        assert!(t.search("").is_empty());
    }

    #[test]
    fn long_events_are_cut_in_the_view_only() {
        let d = tempfile::tempdir().unwrap();
        let t = Transcripts::open(d.path()).unwrap();
        let long = "y".repeat(MAX_EVENT_TEXT + 50);
        let line = json!({"type": "user", "message": {"content": long}}).to_string() + "\n";
        t.append(&up("hub-box", "a.jsonl", 0, &line)).unwrap();
        let (events, _) = t.read_events(&t.list()[0]).unwrap();
        assert_eq!(events[0].text.chars().count(), MAX_EVENT_TEXT + 1);
        assert_eq!(
            std::fs::read_to_string(d.path().join("hub-box/claude/a.jsonl")).unwrap(),
            line
        );
    }
}
