use std::borrow::Cow;
use std::collections::{HashMap, VecDeque};
use std::io::Write;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::accounts::Provider;
use crate::router::Router;

pub const LOG_CAPACITY: usize = 500;
const MAX_LINE: usize = 16 * 1024 * 1024;
const MAX_WHOLE_BODY: usize = 8 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct TokenUsage {
    pub input: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub output: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RequestRecord {
    pub id: u64,
    pub at: DateTime<Utc>,
    pub machine: String,
    pub provider: Provider,
    pub account: String,
    pub email: Option<String>,
    pub model: Option<String>,
    pub session: Option<String>,
    pub transport: Cow<'static, str>,
    pub status: u16,
    pub latency_ms: u64,
    pub usage: TokenUsage,
}

#[derive(Default, Serialize, Deserialize)]
struct LogState {
    next_id: u64,
    records: VecDeque<RequestRecord>,
    #[serde(skip)]
    carried_over: u64,
    #[serde(skip)]
    ids_from_predecessor: HashMap<u64, u64>,
    #[serde(skip)]
    successor: Option<mpsc::UnboundedSender<Value>>,
}

impl LogState {
    fn push(&mut self, mut r: RequestRecord) -> u64 {
        self.next_id += 1;
        r.id = self.next_id;
        self.records.push_back(r);
        while self.records.len() > LOG_CAPACITY {
            self.records.pop_front();
        }
        self.tell_successor(self.next_id);
        self.next_id
    }

    fn change(&mut self, id: u64, change: impl FnOnce(&mut RequestRecord)) {
        if let Some(r) = self.records.iter_mut().rev().find(|r| r.id == id) {
            change(r);
            self.tell_successor(id);
        }
    }

    fn tell_successor(&self, id: u64) {
        if let Some(successor) = &self.successor
            && let Some(r) = self.records.iter().rev().find(|r| r.id == id)
        {
            let _ = successor.send(json!({ "request": r }));
        }
    }
}

#[derive(Default)]
pub struct RequestLog {
    state: Mutex<LogState>,
}

impl RequestLog {
    pub fn add(&self, r: RequestRecord) -> u64 {
        self.state.lock().unwrap().push(r)
    }

    pub fn set_usage(&self, id: u64, usage: TokenUsage) {
        self.state.lock().unwrap().change(id, |r| r.usage = usage);
    }

    pub fn set_transport(&self, id: u64, transport: &'static str) {
        self.state
            .lock()
            .unwrap()
            .change(id, |r| r.transport = transport.into());
    }

    pub fn export(&self, successor: mpsc::UnboundedSender<Value>) -> Value {
        let mut st = self.state.lock().unwrap();
        st.successor = Some(successor);
        serde_json::to_value(&*st).unwrap_or_default()
    }

    pub fn import(&self, state: &Value) {
        if let Ok(mut state) = serde_json::from_value::<LogState>(state.clone()) {
            state.carried_over = state.next_id;
            *self.state.lock().unwrap() = state;
        }
    }

    pub fn import_later(&self, record: &Value) {
        let Ok(later) = serde_json::from_value::<RequestRecord>(record.clone()) else {
            return;
        };
        let mut st = self.state.lock().unwrap();
        let known = if later.id <= st.carried_over {
            Some(later.id)
        } else {
            st.ids_from_predecessor.get(&later.id).copied()
        };
        match known {
            Some(id) => st.change(id, |r| *r = RequestRecord { id, ..later }),
            None => {
                let theirs = later.id;
                let ours = st.push(later);
                st.ids_from_predecessor.insert(theirs, ours);
            }
        }
    }

    pub fn recent(&self) -> Vec<RequestRecord> {
        self.state.lock().unwrap().records.iter().rev().cloned().collect()
    }
}

enum Decoder {
    Identity,
    Gzip(Box<flate2::write::GzDecoder<Vec<u8>>>),
    Deflate(Box<flate2::write::ZlibDecoder<Vec<u8>>>),
    Brotli(Box<brotli::DecompressorWriter<Vec<u8>>>),
    Zstd(Box<zstd::stream::write::Decoder<'static, Vec<u8>>>),
    Failed,
}

impl Decoder {
    fn new(encoding: Option<&str>) -> Self {
        match encoding.map(|e| e.trim().to_ascii_lowercase()).as_deref() {
            None | Some("") | Some("identity") => Self::Identity,
            Some("gzip") => Self::Gzip(Box::new(flate2::write::GzDecoder::new(Vec::new()))),
            Some("deflate") => Self::Deflate(Box::new(flate2::write::ZlibDecoder::new(Vec::new()))),
            Some("br") => Self::Brotli(Box::new(brotli::DecompressorWriter::new(Vec::new(), 4096))),
            Some("zstd") => match zstd::stream::write::Decoder::new(Vec::new()) {
                Ok(d) => Self::Zstd(Box::new(d)),
                Err(_) => Self::Failed,
            },
            Some(_) => Self::Failed,
        }
    }

    fn decode(&mut self, chunk: &[u8]) -> Option<Vec<u8>> {
        fn drain<W: Write>(w: &mut W, chunk: &[u8], inner: impl FnOnce(&mut W) -> &mut Vec<u8>) -> Option<Vec<u8>> {
            w.write_all(chunk).ok()?;
            w.flush().ok()?;
            Some(std::mem::take(inner(w)))
        }
        let out = match self {
            Self::Identity => Some(chunk.to_vec()),
            Self::Gzip(d) => drain(d.as_mut(), chunk, |d| d.get_mut()),
            Self::Deflate(d) => drain(d.as_mut(), chunk, |d| d.get_mut()),
            Self::Brotli(d) => drain(d.as_mut(), chunk, |d| d.get_mut()),
            Self::Zstd(d) => drain(d.as_mut(), chunk, |d| d.get_mut()),
            Self::Failed => None,
        };
        if out.is_none() {
            *self = Self::Failed;
        }
        out
    }
}

pub fn decompressed(encoding: Option<&str>, body: &[u8]) -> Vec<u8> {
    Decoder::new(encoding)
        .decode(body)
        .filter(|plain| !plain.is_empty())
        .unwrap_or_else(|| body.to_vec())
}

pub struct BodyScanner {
    decoder: Decoder,
    is_sse: bool,
    line: Vec<u8>,
    whole: Vec<u8>,
}

fn is_interesting(line: &[u8]) -> bool {
    fn has(line: &[u8], needle: &[u8]) -> bool {
        line.windows(needle.len()).any(|w| w == needle)
    }
    has(line, b"usage") || has(line, b"response.created")
}

impl BodyScanner {
    pub fn new(content_encoding: Option<&str>, content_type: Option<&str>) -> Self {
        Self {
            decoder: Decoder::new(content_encoding),
            is_sse: content_type.is_some_and(|c| c.contains("event-stream")),
            line: Vec::new(),
            whole: Vec::new(),
        }
    }

    pub fn feed(&mut self, chunk: &[u8], sink: &mut impl FnMut(Value)) {
        let Some(plain) = self.decoder.decode(chunk) else {
            return;
        };
        if !self.is_sse {
            if self.whole.len() + plain.len() <= MAX_WHOLE_BODY {
                self.whole.extend_from_slice(&plain);
            }
            return;
        }
        for b in plain {
            if b != b'\n' {
                if self.line.len() < MAX_LINE {
                    self.line.push(b);
                }
                continue;
            }
            if let Some(data) = self.line.strip_prefix(b"data:")
                && is_interesting(data)
                && let Ok(v) = serde_json::from_slice::<Value>(data.trim_ascii())
            {
                sink(v);
            }
            self.line.clear();
        }
    }

    pub fn finish(&mut self, sink: &mut impl FnMut(Value)) {
        if !self.whole.is_empty()
            && let Ok(v) = serde_json::from_slice::<Value>(&self.whole)
        {
            sink(v);
        }
    }
}

fn n(v: &Value) -> u64 {
    v.as_u64().unwrap_or(0)
}

fn apply_claude_usage(u: &mut TokenUsage, usage: &Value) {
    if usage["input_tokens"].is_u64() {
        u.input = n(&usage["input_tokens"]);
    }
    if usage["cache_read_input_tokens"].is_u64() {
        u.cache_read = n(&usage["cache_read_input_tokens"]);
    }
    if usage["cache_creation_input_tokens"].is_u64() {
        u.cache_write = n(&usage["cache_creation_input_tokens"]);
    }
    if usage["output_tokens"].is_u64() {
        u.output = n(&usage["output_tokens"]);
    }
}

fn apply_codex_usage(u: &mut TokenUsage, usage: &Value) {
    let cached = n(&usage["input_tokens_details"]["cached_tokens"]);
    u.input = n(&usage["input_tokens"]).saturating_sub(cached);
    u.cache_read = cached;
    u.output = n(&usage["output_tokens"]);
}

pub fn apply_event(provider: Provider, u: &mut TokenUsage, v: &Value) -> Option<String> {
    match (provider, v["type"].as_str()) {
        (Provider::Claude, Some("message_start")) => apply_claude_usage(u, &v["message"]["usage"]),
        (Provider::Claude, _) => apply_claude_usage(u, &v["usage"]),
        (Provider::Codex, Some("response.created")) => return v["response"]["id"].as_str().map(str::to_string),
        (Provider::Codex, Some("response.completed")) => apply_codex_usage(u, &v["response"]["usage"]),
        (Provider::Codex, _) => {
            if v["usage"].is_object() {
                apply_codex_usage(u, &v["usage"]);
            }
            if let Some(id) = v["id"].as_str().filter(|i| i.starts_with("resp_")) {
                return Some(id.to_string());
            }
        }
    }
    None
}

pub struct ResponseTap {
    scanner: BodyScanner,
    provider: Provider,
    usage: TokenUsage,
    record: Option<u64>,
    log: Arc<RequestLog>,
    router: Arc<Router>,
    account: String,
}

impl ResponseTap {
    pub fn new(
        scanner: BodyScanner,
        provider: Provider,
        record: Option<u64>,
        log: Arc<RequestLog>,
        router: Arc<Router>,
        account: String,
    ) -> Self {
        Self {
            scanner,
            provider,
            usage: TokenUsage::default(),
            record,
            log,
            router,
            account,
        }
    }

    fn take(&mut self, v: Value) {
        if let Some(id) = apply_event(self.provider, &mut self.usage, &v) {
            self.router.record_response_owner(&id, &self.account);
        }
    }

    pub fn feed(&mut self, chunk: &[u8]) {
        let mut events = Vec::new();
        self.scanner.feed(chunk, &mut |v| events.push(v));
        for v in events {
            self.take(v);
        }
    }
}

impl Drop for ResponseTap {
    fn drop(&mut self) {
        let mut events = Vec::new();
        self.scanner.finish(&mut |v| events.push(v));
        for v in events {
            self.take(v);
        }
        if let Some(record) = self.record {
            self.log.set_usage(record, self.usage);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn collect(s: &mut BodyScanner, chunk: &[u8]) -> Vec<Value> {
        let mut out = Vec::new();
        s.feed(chunk, &mut |v| out.push(v));
        out
    }

    #[test]
    fn scans_sse_across_chunks() {
        let mut s = BodyScanner::new(None, Some("text/event-stream"));
        let line = b"data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":7}}\n\n";
        let (a, b) = line.split_at(20);
        assert!(collect(&mut s, a).is_empty());
        assert_eq!(collect(&mut s, b)[0]["usage"]["output_tokens"], 7);
    }

    #[test]
    fn scans_gzip_sse() {
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(b"data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":9}}\n\n")
            .unwrap();
        let bytes = enc.finish().unwrap();
        let mut s = BodyScanner::new(Some("gzip"), Some("text/event-stream"));
        let got = collect(&mut s, &bytes);
        assert_eq!(got[0]["usage"]["output_tokens"], 9);
    }

    #[test]
    fn scans_zstd_and_brotli() {
        let line = b"data: {\"usage\":{\"output_tokens\":3}}\n\n";
        let z = zstd::encode_all(&line[..], 3).unwrap();
        let mut s = BodyScanner::new(Some("zstd"), Some("text/event-stream"));
        assert_eq!(collect(&mut s, &z).len(), 1);

        let mut br = brotli::CompressorWriter::new(Vec::new(), 4096, 5, 22);
        br.write_all(line).unwrap();
        let b = br.into_inner();
        let mut s = BodyScanner::new(Some("br"), Some("text/event-stream"));
        assert_eq!(collect(&mut s, &b).len(), 1);
    }

    #[test]
    fn decompresses_a_whole_body_and_passes_through_on_failure() {
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(b"{\"a\":1}").unwrap();
        let gz = enc.finish().unwrap();
        assert_eq!(decompressed(Some("gzip"), &gz), b"{\"a\":1}");
        assert_eq!(decompressed(None, b"plain"), b"plain");
        assert_eq!(decompressed(Some("gzip"), b"not gzip"), b"not gzip");
    }

    #[test]
    fn corrupt_stream_stops_quietly() {
        let mut s = BodyScanner::new(Some("gzip"), Some("text/event-stream"));
        assert!(collect(&mut s, b"this is not gzip at all, not even close").is_empty());
        assert!(collect(&mut s, b"more").is_empty());
    }

    #[test]
    fn codex_usage_events() {
        let mut u = TokenUsage::default();
        let id = apply_event(
            Provider::Codex,
            &mut u,
            &json!({"type": "response.created", "response": {"id": "resp_1"}}),
        );
        assert_eq!(id.as_deref(), Some("resp_1"));
        apply_event(
            Provider::Codex,
            &mut u,
            &json!({"type": "response.completed", "response": {"usage": {
                "input_tokens": 100, "input_tokens_details": {"cached_tokens": 80}, "output_tokens": 5
            }}}),
        );
        assert_eq!(
            u,
            TokenUsage {
                input: 20,
                cache_read: 80,
                cache_write: 0,
                output: 5
            }
        );
    }

    #[test]
    fn non_stream_json_body() {
        let mut s = BodyScanner::new(None, Some("application/json"));
        let mut out = Vec::new();
        s.feed(br#"{"usage":{"input_tokens":1,"#, &mut |v| out.push(v));
        s.feed(br#""output_tokens":2}}"#, &mut |v| out.push(v));
        s.finish(&mut |v| out.push(v));
        assert_eq!(out[0]["usage"]["output_tokens"], 2);
    }

    #[test]
    fn log_is_bounded_and_newest_first() {
        let log = RequestLog::default();
        let rec = |account: &str| RequestRecord {
            id: 0,
            at: Utc::now(),
            machine: "m".into(),
            provider: Provider::Claude,
            account: account.into(),
            email: None,
            model: None,
            session: None,
            transport: "http".into(),
            status: 200,
            latency_ms: 1,
            usage: TokenUsage::default(),
        };
        let first = log.add(rec("first"));
        log.set_usage(
            first,
            TokenUsage {
                input: 1,
                ..Default::default()
            },
        );
        assert_eq!(log.recent()[0].usage.input, 1);
        for _ in 0..LOG_CAPACITY {
            log.add(rec("later"));
        }
        let recent = log.recent();
        assert_eq!(recent.len(), LOG_CAPACITY);
        assert!(recent.iter().all(|r| r.account == "later"));
    }
}
