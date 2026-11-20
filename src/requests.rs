use std::borrow::Cow;
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::accounts::Provider;
use crate::analytics::{Ledger, Row};

const RECENT_ROWS: usize = 500;
const MAX_RECORDS: usize = 20_000;
const HOUR: Duration = Duration::hours(1);
const MAX_LINE: usize = 16 * 1024 * 1024;
const MAX_WHOLE_BODY: usize = 8 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Speed {
    #[default]
    Standard,
    Fast,
    Ultrafast,
}

impl Speed {
    fn parse(v: &Value) -> Option<Self> {
        Some(match v.as_str()? {
            "fast" | "priority" => Self::Fast,
            "ultrafast" => Self::Ultrafast,
            _ => Self::Standard,
        })
    }
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct TokenUsage {
    pub input: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    #[serde(default)]
    pub cache_write_1h: u64,
    pub output: u64,
    #[serde(default)]
    pub speed: Speed,
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
    #[serde(default)]
    newest_dropped: Option<DateTime<Utc>>,
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
        let hour_ago = Utc::now() - HOUR;
        while self.records.len() > MAX_RECORDS
            || (self.records.len() > RECENT_ROWS && self.records.front().is_some_and(|r| r.at < hour_ago))
        {
            let dropped = self.records.pop_front().map(|r| r.at);
            self.newest_dropped = self.newest_dropped.max(dropped);
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

pub struct RequestLog {
    state: Mutex<LogState>,
    ledger: Arc<Ledger>,
}

impl RequestLog {
    pub fn new(ledger: Arc<Ledger>) -> Self {
        Self {
            state: Mutex::default(),
            ledger,
        }
    }

    pub fn ledger(&self) -> Arc<Ledger> {
        self.ledger.clone()
    }

    pub fn add(&self, mut r: RequestRecord) -> RequestRecord {
        r.id = self.state.lock().unwrap().push(r.clone());
        r
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

    pub fn finish(&self, record: &RequestRecord) {
        self.set_usage(record.id, record.usage);
        let u = &record.usage;
        if record.status < 400 && u.input + u.cache_read + u.cache_write + u.output > 0 {
            self.ledger.append(&Row::from(record));
        }
    }

    pub fn export(&self, successor: mpsc::UnboundedSender<Value>) -> Value {
        let mut st = self.state.lock().unwrap();
        st.successor = Some(successor);
        serde_json::to_value(&*st).unwrap_or_default()
    }

    pub fn import(&self, state: &Value) {
        if let Ok(mut state) = serde_json::from_value::<LogState>(state.clone()) {
            state.carried_over = state.next_id;
            if state.newest_dropped.is_none() && state.next_id > state.records.len() as u64 {
                state.newest_dropped = state.records.front().map(|r| r.at);
            }
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
        let st = self.state.lock().unwrap();
        st.records.iter().rev().take(RECENT_ROWS).cloned().collect()
    }

    pub fn last_hour(&self) -> Value {
        let now = Utc::now();
        let this_minute = now.timestamp().div_euclid(60);
        let hour_ago = DateTime::from_timestamp((this_minute - 59) * 60, 0).unwrap_or(now - HOUR);
        let mut minutes: Vec<HashMap<&str, i64>> = (0..60)
            .map(|i| HashMap::from([("minute", this_minute - 59 + i)]))
            .collect();
        let mut accounts = HashSet::new();
        let mut latencies = Vec::new();
        let (mut total, mut failed, mut input, mut cache_read, mut cache_write) = (0, 0, 0, 0, 0);
        let st = self.state.lock().unwrap();
        let capped = st.newest_dropped.is_some_and(|at| at >= hour_ago);
        let hour = st.records.iter().filter(|r| r.at >= hour_ago && r.at <= now);
        for r in hour {
            total += 1;
            accounts.insert(r.account.as_str());
            input += r.usage.input;
            cache_read += r.usage.cache_read;
            cache_write += r.usage.cache_write;
            let lane = if r.status >= 400 {
                failed += 1;
                "failed"
            } else {
                latencies.push(r.latency_ms);
                r.provider.as_str()
            };
            let index = 59 - (this_minute - r.at.timestamp().div_euclid(60));
            if let Some(minute) = usize::try_from(index).ok().and_then(|i| minutes.get_mut(i)) {
                *minute.entry(lane).or_default() += 1;
            }
        }
        let accounts = accounts.len();
        drop(st);
        latencies.sort_unstable();
        let percentile = |p: f64| {
            let i = ((latencies.len() as f64 * p) as usize).min(latencies.len().saturating_sub(1));
            latencies.get(i).copied()
        };
        json!({
            "minutes": minutes,
            "total": total,
            "capped": capped,
            "failed": failed,
            "accounts": accounts,
            "latency_p50_ms": percentile(0.5),
            "latency_p95_ms": percentile(0.95),
            "input": input,
            "cache_read": cache_read,
            "cache_write": cache_write,
        })
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

pub fn decompressed<'a>(encoding: Option<&str>, body: &'a [u8], limit: usize) -> Cow<'a, [u8]> {
    let reader: Box<dyn Read + 'a> = match encoding.map(|e| e.trim().to_ascii_lowercase()).as_deref() {
        Some("gzip") => Box::new(flate2::read::GzDecoder::new(body)),
        Some("deflate") => Box::new(flate2::read::ZlibDecoder::new(body)),
        Some("br") => Box::new(brotli::Decompressor::new(body, 4096)),
        Some("zstd") => match zstd::stream::read::Decoder::new(body) {
            Ok(d) => Box::new(d),
            Err(_) => return Cow::Borrowed(body),
        },
        _ => return Cow::Borrowed(body),
    };
    let mut plain = Vec::new();
    let _ = reader.take(limit as u64).read_to_end(&mut plain);
    if plain.is_empty() {
        Cow::Borrowed(body)
    } else {
        Cow::Owned(plain)
    }
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
    has(line, b"usage")
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
            self.take_line(sink);
        }
    }

    fn take_line(&mut self, sink: &mut impl FnMut(Value)) {
        if let Some(data) = self.line.strip_prefix(b"data:")
            && is_interesting(data)
            && let Ok(v) = serde_json::from_slice::<Value>(data.trim_ascii())
        {
            sink(v);
        }
        self.line.clear();
    }

    pub fn finish(&mut self, sink: &mut impl FnMut(Value)) {
        self.take_line(sink);
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
    if usage["cache_creation"]["ephemeral_1h_input_tokens"].is_u64() {
        u.cache_write_1h = n(&usage["cache_creation"]["ephemeral_1h_input_tokens"]);
    }
    if usage["output_tokens"].is_u64() {
        u.output = n(&usage["output_tokens"]);
    }
    if let Some(speed) = Speed::parse(&usage["speed"]) {
        u.speed = speed;
    }
}

fn apply_codex_usage(u: &mut TokenUsage, response: &Value) {
    let usage = &response["usage"];
    if usage.is_object() {
        let cached = n(&usage["input_tokens_details"]["cached_tokens"]);
        let written = n(&usage["input_tokens_details"]["cache_write_tokens"]);
        u.input = n(&usage["input_tokens"]).saturating_sub(cached + written);
        u.cache_read = cached;
        u.cache_write = written;
        u.output = n(&usage["output_tokens"]);
    }
    if let Some(speed) = Speed::parse(&response["service_tier"]) {
        u.speed = speed;
    }
}

pub fn is_response_end(kind: Option<&str>) -> bool {
    matches!(
        kind,
        Some("response.completed" | "response.incomplete" | "response.failed" | "response.cancelled")
    )
}

pub fn apply_event(provider: Provider, u: &mut TokenUsage, v: &Value) {
    match (provider, v["type"].as_str()) {
        (Provider::Claude, Some("message_start")) => apply_claude_usage(u, &v["message"]["usage"]),
        (Provider::Claude, _) => apply_claude_usage(u, &v["usage"]),
        (_, kind) if is_response_end(kind) => apply_codex_usage(u, &v["response"]),
        _ if v["usage"].is_object() => apply_codex_usage(u, v),
        _ => {}
    }
}

pub struct ResponseTap {
    scanner: BodyScanner,
    provider: Provider,
    usage: TokenUsage,
    record: Option<RequestRecord>,
    log: Arc<RequestLog>,
    quota: Option<(Arc<crate::usage::Usage>, String)>,
}

impl ResponseTap {
    pub fn new(scanner: BodyScanner, provider: Provider, record: Option<RequestRecord>, log: Arc<RequestLog>) -> Self {
        Self {
            scanner,
            provider,
            usage: TokenUsage::default(),
            record,
            log,
            quota: None,
        }
    }

    pub fn watching_quota(mut self, usage: Arc<crate::usage::Usage>, account: String) -> Self {
        self.quota = Some((usage, account));
        self
    }

    fn take(&mut self, v: Value) {
        apply_event(self.provider, &mut self.usage, &v);
        if let Some((usage, account)) = &self.quota {
            usage.observe_muse_event(account, &v);
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
        if let Some(mut record) = self.record.take() {
            record.usage = self.usage;
            self.log.finish(&record);
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
        assert!(collect(&mut s, b"data: {\"usage\":{\"output_tokens\":8}}").is_empty());
        let mut last = Vec::new();
        s.finish(&mut |v| last.push(v));
        assert_eq!(
            last[0]["usage"]["output_tokens"], 8,
            "a stream that ends without a newline keeps its last event"
        );
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
        assert_eq!(&*decompressed(Some("gzip"), &gz, 1024), b"{\"a\":1}");
        assert_eq!(&*decompressed(Some("gzip"), &gz, 3), b"{\"a");
        assert_eq!(&*decompressed(None, b"plain", 1024), b"plain");
        assert_eq!(&*decompressed(Some("gzip"), b"not gzip", 1024), b"not gzip");
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
        apply_event(
            Provider::Codex,
            &mut u,
            &json!({"type": "response.completed", "response": {"usage": {
                "input_tokens": 100, "input_tokens_details": {"cached_tokens": 80, "cache_write_tokens": 15}, "output_tokens": 5
            }}}),
        );
        assert_eq!(
            u,
            TokenUsage {
                input: 5,
                cache_read: 80,
                cache_write: 15,
                cache_write_1h: 0,
                output: 5,
                speed: Speed::Standard
            }
        );
        apply_event(
            Provider::Codex,
            &mut u,
            &json!({"type": "response.cancelled", "response": {"usage": null, "service_tier": "priority"}}),
        );
        assert_eq!((u.input, u.cache_read, u.output, u.speed), (5, 80, 5, Speed::Fast));
        apply_event(
            Provider::Codex,
            &mut u,
            &json!({"type": "response.completed", "response": {"usage": null, "service_tier": "ultrafast"}}),
        );
        assert_eq!(u.speed, Speed::Ultrafast);
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
        let dir = tempfile::tempdir().unwrap();
        let log = RequestLog::new(Ledger::open(dir.path().to_path_buf()));
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
        let first = log.add(rec("first")).id;
        log.set_usage(
            first,
            TokenUsage {
                input: 1,
                ..Default::default()
            },
        );
        assert_eq!(log.recent()[0].usage.input, 1);
        for _ in 0..RECENT_ROWS {
            log.add(rec("later"));
        }
        let recent = log.recent();
        assert_eq!(recent.len(), RECENT_ROWS);
        assert!(recent.iter().all(|r| r.account == "later"));
    }
}
