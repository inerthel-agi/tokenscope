//! Token events from local transcripts + aggregation. Port of `backend/local_sources.py`
//! (Claude/Codex JSONL, Antigravity SQLite/protobuf, `aggregate`); keep both in sync.
use crate::paths;
use crate::pricing::*;
use chrono::{DateTime, Local, TimeZone, Utc};
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone, Debug)]
pub struct Event {
    pub tool: &'static str,
    pub model: String,
    pub project: String,
    pub project_name: String,
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_creation: u64,
    pub cost: f64,
    pub ts: Option<DateTime<Utc>>,
    pub day: String,
    pub dk: Option<String>,
    pub priced: bool, // false when no built-in rate or override knows this model
}

// ----- helpers -----
fn norm_path(p: &str) -> String {
    p.replace('\\', "/").trim_end_matches('/').to_lowercase()
}

fn basename(p: &str) -> String {
    p.replace('\\', "/").split('/').filter(|s| !s.is_empty()).last().unwrap_or(p).to_string()
}

pub fn parse_ts(raw: &Value) -> Option<DateTime<Utc>> {
    let s = raw.as_str()?;
    DateTime::parse_from_rfc3339(&s.replace('Z', "+00:00")).ok().map(|d| d.with_timezone(&Utc))
}

fn safe_int(v: &Value) -> u64 {
    match v.as_f64() {
        Some(f) if f.is_finite() && f > 0.0 => (f as u64).min(1_000_000_000_000),
        _ => 0,
    }
}

pub fn round_to(x: f64, n: i32) -> f64 {
    let p = 10f64.powi(n);
    (x * p).round() / p
}

fn local_day(ts: Option<DateTime<Utc>>) -> String {
    ts.unwrap_or_else(Utc::now).with_timezone(&Local).format("%Y-%m-%d").to_string()
}

fn safe_cost(c: f64) -> f64 {
    if c.is_finite() { round_to(c.max(0.0), 6) } else { 0.0 }
}

#[allow(clippy::too_many_arguments)]
fn make_event(tool: &'static str, model: Option<&str>, cwd: Option<&str>, ts: Option<DateTime<Utc>>,
              input: u64, output: u64, cache_read: u64, cache_creation: u64, cost: f64) -> Event {
    let cwd = cwd.filter(|c| !c.is_empty());
    Event {
        tool,
        model: model.filter(|m| !m.is_empty()).unwrap_or("unknown").to_string(),
        project: cwd.map(norm_path).unwrap_or_else(|| "unassigned".into()),
        project_name: cwd.map(basename).unwrap_or_else(|| "unassigned".into()),
        input, output, cache_read, cache_creation,
        cost: safe_cost(cost),
        // LOCAL day, consistent with the hourly buckets and the dashboard's "today".
        day: ts.map(|t| t.with_timezone(&Local).format("%Y-%m-%d").to_string()).unwrap_or_default(),
        ts,
        dk: None,
        priced: true,
    }
}

/// Opens a transcript; Codex now stores finished rollouts zstd-compressed (`.jsonl.zst`).
pub fn open_text(path: &Path) -> Option<Box<dyn std::io::Read>> {
    let file = fs::File::open(path).ok()?;
    if path.extension().is_some_and(|e| e == "zst") {
        return ruzstd::decoding::StreamingDecoder::new(file).ok().map(|d| Box::new(d) as Box<dyn std::io::Read>);
    }
    Some(Box::new(file))
}

fn read_json_lines(path: &Path, mut f: impl FnMut(&Map<String, Value>)) -> bool {
    let Some(reader) = open_text(path) else { return false };
    for line in BufReader::new(reader).split(b'\n') {
        let Ok(line) = line else { return false };
        let text = String::from_utf8_lossy(&line);
        let t = text.trim();
        if t.is_empty() {
            continue;
        }
        if let Ok(Value::Object(o)) = serde_json::from_str::<Value>(t) {
            f(&o);
        }
    }
    true
}

// ----- Claude -----
fn parse_claude_file(path: &Path) -> Vec<Event> {
    let mut events = Vec::new();
    let mut file_cwd: Option<String> = None;
    let mut seen: HashSet<String> = HashSet::new(); // usage is repeated on every content-block line
    let ok = read_json_lines(path, |o| {
        if let Some(c) = o.get("cwd").and_then(|v| v.as_str()).filter(|c| !c.is_empty()) {
            file_cwd = Some(c.to_string());
        }
        if o.get("type").and_then(|v| v.as_str()) != Some("assistant") {
            return;
        }
        let Some(msg) = o.get("message").and_then(|v| v.as_object()) else { return };
        let Some(usage) = msg.get("usage").and_then(|v| v.as_object()) else { return };
        let dk = msg.get("id").filter(|v| !v.is_null() && v.as_str() != Some("")).map(|id| {
            format!("{}\0{}", id.as_str().map(str::to_string).unwrap_or_else(|| id.to_string()),
                    o.get("requestId").and_then(|v| v.as_str()).unwrap_or("None"))
        });
        if let Some(k) = &dk {
            if !seen.insert(k.clone()) {
                return;
            }
        }
        let model = msg.get("model").and_then(|v| v.as_str());
        let u = |k: &str| usage.get(k).map(safe_int).unwrap_or(0);
        let (inp, out, cread, ccreate) =
            (u("input_tokens"), u("output_tokens"), u("cache_read_input_tokens"), u("cache_creation_input_tokens"));
        let cc = usage.get("cache_creation").and_then(|v| v.as_object());
        let cg = |k: &str| cc.and_then(|c| c.get(k)).map(safe_int).unwrap_or(0);
        let (mut w5, mut w1) = (cg("ephemeral_5m_input_tokens"), cg("ephemeral_1h_input_tokens"));
        if w5 + w1 == 0 {
            (w5, w1) = (ccreate, 0);
        }
        if inp == 0 && out == 0 && cread == 0 && ccreate == 0 {
            return; // synthetic / empty assistant turn
        }
        let m = model.unwrap_or("");
        let ts = o.get("timestamp").and_then(parse_ts);
        let (in_rate, out_rate, priced) = resolve(m, &local_day(ts), &claude_pricing(m, inp + cread + ccreate));
        let cost = inp as f64 / 1e6 * in_rate
            + out as f64 / 1e6 * out_rate
            + (cread as f64 * claude_cache_read_mult(m)
                + w5 as f64 * CLAUDE_CACHE_WRITE_5M_MULT
                + w1 as f64 * CLAUDE_CACHE_WRITE_1H_MULT) / 1e6 * in_rate;
        let cwd = o.get("cwd").and_then(|v| v.as_str()).filter(|c| !c.is_empty()).map(str::to_string).or(file_cwd.clone());
        let mut ev = make_event("claude_api", model, cwd.as_deref(), ts, inp, out, cread, ccreate, cost);
        ev.dk = dk; // resumed sessions replay messages into new files: dedup across files
        ev.priced = priced;
        events.push(ev);
    });
    if ok { events } else { Vec::new() }
}

// ----- Codex -----
pub fn read_codex_config_model() -> Option<String> {
    let text = fs::read_to_string(paths::codex_dir().join("config.toml")).ok()?;
    for raw in text.lines() {
        let s = raw.trim();
        if s.starts_with('[') {
            break;
        }
        let Some(rest) = s.strip_prefix("model") else { continue };
        let rest = rest.trim_start();
        if let Some(v) = rest.strip_prefix('=') {
            let v = v.trim().trim_matches('"').trim_matches('\'');
            if !v.is_empty() {
                return Some(v.to_string());
            }
        }
    }
    None
}

fn strip_unc(c: &str) -> String {
    c.strip_prefix("\\\\?\\").unwrap_or(c).to_string()
}

fn parse_codex_file(path: &Path, default_model: Option<&str>) -> Vec<Event> {
    let mut events = Vec::new();
    let mut cwd: Option<String> = None;
    let mut model: Option<String> = default_model.map(str::to_string);
    let ok = read_json_lines(path, |o| {
        let t = o.get("type").and_then(|v| v.as_str()).unwrap_or("");
        let empty = Map::new();
        let p = o.get("payload").and_then(|v| v.as_object()).unwrap_or(&empty);
        match t {
            "session_meta" | "turn_context" => {
                if let Some(c) = p.get("cwd").and_then(|v| v.as_str()).filter(|c| !c.is_empty()) {
                    cwd = Some(strip_unc(c));
                }
                if t == "turn_context" {
                    if let Some(m) = p.get("model").and_then(|v| v.as_str()).filter(|m| !m.is_empty()) {
                        model = Some(m.to_string());
                    }
                }
            }
            "event_msg" if p.get("type").and_then(|v| v.as_str()) == Some("token_count") => {
                let Some(d) = p.get("info").and_then(|i| i.get("last_token_usage")).and_then(|v| v.as_object()) else { return };
                let g = |k: &str| d.get(k).map(safe_int).unwrap_or(0);
                let (inp, out) = (g("input_tokens"), g("output_tokens"));
                let cached = g("cached_input_tokens").min(inp);
                if inp == 0 && out == 0 {
                    return;
                }
                let m = model.as_deref().unwrap_or("");
                let ts = o.get("timestamp").and_then(parse_ts);
                let builtin = codex_pricing(m);
                let (in_rate, out_rate, priced) = resolve(m, &local_day(ts), &builtin);
                let label = builtin.0;
                let uncached = inp - cached;
                // output_tokens already includes reasoning_output_tokens.
                let cost = (uncached as f64 * in_rate + cached as f64 * in_rate * codex_cached_mult(&label)) / 1e6
                    + out as f64 * out_rate / 1e6;
                let mut ev = make_event("codex", model.as_deref(), cwd.as_deref(), ts, uncached, out, cached, 0, cost);
                ev.priced = priced;
                events.push(ev);
            }
            _ => {}
        }
    });
    if ok { events } else { Vec::new() }
}

// ----- Antigravity (protobuf in SQLite) -----
type Field<'a> = (u64, u8, PbVal<'a>);
#[derive(Clone, Copy)]
enum PbVal<'a> {
    Int(u64),
    Bytes(&'a [u8]),
}

fn pb_varint(b: &[u8], i: &mut usize) -> Option<u64> {
    let (mut shift, mut result) = (0u32, 0u64);
    loop {
        let x = *b.get(*i)?;
        *i += 1;
        if shift < 64 {
            result |= ((x & 0x7f) as u64) << shift;
        }
        if x & 0x80 == 0 {
            return Some(result);
        }
        shift += 7;
    }
}

fn pb_fields(b: &[u8]) -> Vec<Field<'_>> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let Some(tag) = pb_varint(b, &mut i) else { break };
        let (f, wt) = (tag >> 3, (tag & 7) as u8);
        if f == 0 {
            break;
        }
        let take = |i: &mut usize, n: usize| -> Option<&[u8]> {
            let s = b.get(*i..*i + n)?;
            *i += n;
            Some(s)
        };
        let v = match wt {
            0 => pb_varint(b, &mut i).map(PbVal::Int),
            2 => pb_varint(b, &mut i).and_then(|n| take(&mut i, n as usize)).map(PbVal::Bytes),
            5 => take(&mut i, 4).map(PbVal::Bytes),
            1 => take(&mut i, 8).map(PbVal::Bytes),
            _ => None,
        };
        match v {
            Some(v) => out.push((f, wt, v)),
            None => break,
        }
    }
    out
}

fn pb_bytes<'a>(fields: &[Field<'a>], num: u64) -> Option<&'a [u8]> {
    fields.iter().find_map(|(f, w, v)| match v {
        PbVal::Bytes(b) if *f == num && *w == 2 => Some(*b),
        _ => None,
    })
}

fn pb_int(fields: &[Field<'_>], num: u64) -> Option<u64> {
    fields.iter().find_map(|(f, w, v)| match v {
        PbVal::Int(x) if *f == num && *w == 0 => Some(*x),
        _ => None,
    })
}

fn pb_str(fields: &[Field<'_>], num: u64) -> Option<String> {
    let s = std::str::from_utf8(pb_bytes(fields, num)?).ok()?;
    s.chars().all(|c| !c.is_control()).then(|| s.to_string())
}

/// (input, output, model, ts_secs) from one gen_metadata blob.
fn ag_parse_gen(blob: &[u8]) -> Option<(u64, u64, String, Option<i64>)> {
    let f1 = pb_fields(pb_bytes(&pb_fields(blob), 1)?);
    let (mut inp, mut out) = (0, 0);
    if let Some(f4) = pb_bytes(&f1, 4).map(pb_fields) {
        inp = pb_int(&f4, 2).unwrap_or(0);
        out = pb_int(&f4, 3).unwrap_or(0);
    }
    if inp == 0 && out == 0 {
        return None;
    }
    let model = pb_str(&f1, 21)
        .or_else(|| pb_str(&f1, 19))
        .or_else(|| pb_bytes(&f1, 20).and_then(|b| pb_str(&pb_fields(b), 2)));
    let ts = pb_bytes(&f1, 9)
        .and_then(|b| pb_bytes(&pb_fields(b), 4).map(|x| x.to_vec()))
        .and_then(|b| pb_fields(&b).iter().find_map(|(_, w, v)| match v {
            PbVal::Int(x) if *w == 0 && *x > 1_600_000_000 && *x < 2_000_000_000 => Some(*x as i64),
            _ => None,
        }));
    Some((inp.min(1_000_000_000_000), out.min(1_000_000_000_000), model.unwrap_or_else(|| "Gemini".into()), ts))
}

fn ag_workspace(blob: &[u8]) -> Option<String> {
    static RE: OnceLock<regex::bytes::Regex> = OnceLock::new();
    let re = RE.get_or_init(|| regex::bytes::Regex::new(r#"file://[^\x00-\x1f"']{3,400}"#).unwrap());
    let m = re.find(blob)?;
    let uri = percent_encoding::percent_decode(m.as_bytes()).decode_utf8_lossy().to_string();
    let rest = uri.strip_prefix("file:")?;
    Some(rest.trim_start_matches('/').to_string()) // file:///d:/path -> d:/path
}

/// Reads a private copy (db + -wal/-shm): the live DB may be mid-write.
fn parse_antigravity_db(path: &Path) -> Vec<Event> {
    let mtime_fallback = fs::metadata(path).and_then(|m| m.modified()).ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_secs() as i64);
    let tmp = std::env::temp_dir().join(format!("tokenscope-ag-{}-{}", std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0)));
    if fs::create_dir_all(&tmp).is_err() {
        return Vec::new();
    }
    let dst = tmp.join(path.file_name().unwrap_or_default());
    let mut events = Vec::new();
    if fs::copy(path, &dst).is_ok() {
        for suffix in ["-wal", "-shm"] {
            let side = PathBuf::from(format!("{}{}", path.display(), suffix));
            if side.exists() {
                let _ = fs::copy(&side, format!("{}{}", dst.display(), suffix));
            }
        }
        if let Ok(con) = rusqlite::Connection::open(&dst) {
            let cwd: Option<String> = con
                .query_row("SELECT data FROM trajectory_metadata_blob LIMIT 1", [], |r| r.get::<_, Vec<u8>>(0))
                .ok()
                .and_then(|b| ag_workspace(&b));
            if let Ok(mut st) = con.prepare("SELECT data FROM gen_metadata") {
                if let Ok(rows) = st.query_map([], |r| r.get::<_, Option<Vec<u8>>>(0)) {
                    for blob in rows.flatten().flatten() {
                        let Some((inp, out, model, ts_secs)) = ag_parse_gen(&blob) else { continue };
                        let ts = ts_secs.or(mtime_fallback).and_then(|s| Utc.timestamp_opt(s, 0).single());
                        let day = local_day(ts);
                        let (ir, or, _) = resolve(&model, &day, &antigravity_pricing(&model, &day));
                        let cost = inp as f64 / 1e6 * ir + out as f64 / 1e6 * or;
                        events.push(make_event("antigravity", Some(&model), cwd.as_deref(), ts, inp, out, 0, 0, cost));
                    }
                }
            }
        }
    }
    let _ = fs::remove_dir_all(&tmp);
    events
}

// ----- file walk + per-file mtime cache -----
type Cache = HashMap<PathBuf, (SystemTime, Arc<Vec<Event>>)>;
fn cache() -> &'static Mutex<Cache> {
    static C: OnceLock<Mutex<Cache>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(HashMap::new()))
}

fn walk(dir: &Path, recursive: bool, keep: &dyn Fn(&str) -> bool, out: &mut Vec<PathBuf>) {
    let Ok(rd) = fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        let name = e.file_name().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue; // like Python glob
        }
        match e.file_type() {
            Ok(t) if t.is_dir() && recursive => walk(&p, true, keep, out),
            Ok(t) if t.is_file() && keep(&name) => out.push(p),
            _ => {}
        }
    }
}

// One extra day of slack so the mtime prefilter never drops an in-window event.
const MTIME_MARGIN_S: f64 = 86400.0;

fn collect(root: &Path, recursive: bool, keep: &dyn Fn(&str) -> bool, days: i64,
           parser: &dyn Fn(&Path) -> Vec<Event>) -> Vec<(PathBuf, Arc<Vec<Event>>)> {
    let mut files = Vec::new();
    walk(root, recursive, keep, &mut files);
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0);
    let cutoff = now - days as f64 * 86400.0 - MTIME_MARGIN_S;
    let mut out = Vec::new();
    let seen: HashSet<&PathBuf> = files.iter().collect();
    for fp in &files {
        let Ok(mt) = fs::metadata(fp).and_then(|m| m.modified()) else { continue };
        let secs = mt.duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0);
        if secs < cutoff {
            continue;
        }
        let hit = cache().lock().unwrap().get(fp).filter(|(m, _)| *m == mt).map(|(_, e)| e.clone());
        let evs = hit.unwrap_or_else(|| {
            let e = Arc::new(parser(fp));
            cache().lock().unwrap().insert(fp.clone(), (mt, e.clone()));
            e
        });
        out.push((fp.clone(), evs));
    }
    // prune vanished files under this root
    cache().lock().unwrap().retain(|k, _| !k.starts_with(root) || seen.contains(k));
    out
}

pub fn read_claude_events(days: i64) -> Vec<Event> {
    let files = collect(&paths::claude_dir().join("projects"), true, &|n| n.ends_with(".jsonl"), days,
                        &|p| parse_claude_file(p));
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for (_, evs) in files {
        for e in evs.iter() {
            if let Some(k) = &e.dk {
                if !seen.insert(k.clone()) {
                    continue;
                }
            }
            out.push(e.clone());
        }
    }
    out
}

pub fn is_rollout(n: &str) -> bool {
    n.starts_with("rollout-") && (n.ends_with(".jsonl") || n.ends_with(".jsonl.zst"))
}

/// Live rollouts are `.jsonl`; finished ones get compressed to `.jsonl.zst` and may move to
/// `archived_sessions/`. One session can briefly exist in both forms: count it once.
pub fn read_codex_events(days: i64) -> Vec<Event> {
    let default_model = read_codex_config_model();
    let parse = |p: &Path| parse_codex_file(p, default_model.as_deref());
    let mut files = Vec::new();
    for dir in ["sessions", "archived_sessions"] {
        files.extend(collect(&paths::codex_dir().join(dir), true, &|n| is_rollout(n), days, &parse));
    }
    files.sort_by_key(|(p, _)| p.extension().is_some_and(|e| e == "zst")); // plain .jsonl first
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for (p, evs) in files {
        let name = p.file_name().unwrap_or_default().to_string_lossy().trim_end_matches(".zst").to_string();
        if seen.insert(name) {
            out.extend(evs.iter().cloned());
        }
    }
    out
}

pub fn read_antigravity_events(days: i64) -> Vec<Event> {
    collect(&paths::antigravity_conv_dir(), false, &|n| n.ends_with(".db"), days, &|p| parse_antigravity_db(p))
        .into_iter().flat_map(|(_, v)| v.iter().cloned().collect::<Vec<_>>()).collect()
}

// ----- aggregation (same JSON shape as the Python `aggregate`) -----
#[derive(Default)]
struct Acc {
    input: u64,
    output: u64,
    cache_read: u64,
    cost: f64,
    entries: u64,
    tools: Vec<(&'static str, u64)>,
}

fn bucket<'a, K: std::hash::Hash + Eq + Clone>(order: &mut Vec<K>, map: &'a mut HashMap<K, Acc>, k: K) -> &'a mut Acc {
    if !map.contains_key(&k) {
        order.push(k.clone());
    }
    map.entry(k).or_default()
}

fn add_tool(a: &mut Acc, t: &'static str, n: u64) {
    match a.tools.iter_mut().find(|(k, _)| *k == t) {
        Some((_, v)) => *v += n,
        None => a.tools.push((t, n)),
    }
}

fn tool_of(a: &Acc, t: &str) -> u64 {
    a.tools.iter().find(|(k, _)| *k == t).map(|(_, v)| *v).unwrap_or(0)
}

pub fn aggregate(events: &[Event], days: i64, tool: Option<&str>, project: Option<&str>) -> Value {
    let since = Utc::now() - chrono::Duration::days(days.min(1_000_000));
    let want_hours = days <= 2;
    let (mut tin, mut tout, mut tcr, mut tcc, mut tcost, mut kept) = (0u64, 0u64, 0u64, 0u64, 0f64, 0u64);
    let (mut o_tool, mut o_model, mut o_proj, mut o_day, mut o_hour) = (vec![], vec![], vec![], vec![], vec![]);
    let (mut m_tool, mut m_model, mut m_proj, mut m_day, mut m_hour): (HashMap<&str, Acc>, HashMap<(&str, String), Acc>,
        HashMap<String, Acc>, HashMap<String, Acc>, HashMap<String, Acc>) = Default::default();
    let mut proj_names: HashMap<String, String> = HashMap::new();
    let mut unpriced: HashSet<(&str, String)> = HashSet::new();
    let mut unpriced_entries = 0u64;
    for e in events {
        if tool.is_some_and(|t| t != e.tool) || project.is_some_and(|p| p != e.project) {
            continue; // project = dashboard drill-down (normalized path)
        }
        if e.ts.is_some_and(|t| t < since) {
            continue;
        }
        kept += 1;
        tin += e.input;
        tout += e.output;
        tcr += e.cache_read;
        tcc += e.cache_creation;
        tcost += e.cost;
        let io = e.input + e.output;
        for a in [bucket(&mut o_tool, &mut m_tool, e.tool),
                  bucket(&mut o_model, &mut m_model, (e.tool, e.model.clone())),
                  bucket(&mut o_proj, &mut m_proj, e.project.clone())] {
            a.input += e.input;
            a.output += e.output;
            a.cache_read += e.cache_read;
            a.cost += e.cost;
            a.entries += 1;
            add_tool(a, e.tool, io);
        }
        proj_names.entry(e.project.clone()).or_insert_with(|| e.project_name.clone());
        if !e.priced {
            unpriced.insert((e.tool, e.model.clone()));
            unpriced_entries += 1;
        }
        let tb = |a: &mut Acc| {
            a.input += e.input;
            a.output += e.output;
            a.cost += e.cost;
            add_tool(a, e.tool, io);
        };
        tb(bucket(&mut o_day, &mut m_day, e.day.clone()));
        if want_hours {
            if let Some(ts) = e.ts {
                tb(bucket(&mut o_hour, &mut m_hour, ts.with_timezone(&Local).format("%Y-%m-%dT%H").to_string()));
            }
        }
    }
    let r4 = |c: f64| round_to(c, 4);
    let by_tool: Vec<Value> = o_tool.iter().map(|t| { let a = &m_tool[t]; json!({
        "tool": t, "input_tokens": a.input, "output_tokens": a.output, "cache_read_tokens": a.cache_read,
        "cost_usd": r4(a.cost), "entries": a.entries }) }).collect();
    let mut models: Vec<&(&str, String)> = o_model.iter().collect();
    models.sort_by_key(|k| std::cmp::Reverse(m_model[*k].input + m_model[*k].output));
    let by_model: Vec<Value> = models.iter().map(|k| { let a = &m_model[*k]; json!({
        "tool": k.0, "model": k.1, "underlying_model": null, "input_tokens": a.input, "output_tokens": a.output,
        "cache_read_tokens": a.cache_read, "cost_usd": r4(a.cost), "entries": a.entries,
        "unpriced": unpriced.contains(*k) }) }).collect();
    let mut projs: Vec<&String> = o_proj.iter().collect();
    projs.sort_by_key(|k| std::cmp::Reverse(m_proj[*k].input + m_proj[*k].output));
    let by_project: Vec<Value> = projs.iter().map(|k| { let a = &m_proj[*k];
        let tools: Map<String, Value> = a.tools.iter().map(|(t, n)| (t.to_string(), json!(n))).collect();
        json!({ "project": k, "project_name": proj_names[*k], "input_tokens": a.input, "output_tokens": a.output,
        "cache_read_tokens": a.cache_read, "cost_usd": r4(a.cost), "entries": a.entries, "tools": tools }) }).collect();
    let series = |order: &mut Vec<String>, map: &HashMap<String, Acc>, key: &str| -> Vec<Value> {
        order.sort();
        order.iter().map(|k| { let a = &map[k]; json!({
            key: k, "input_tokens": a.input, "output_tokens": a.output, "cost_usd": r4(a.cost),
            "claude_api": tool_of(a, "claude_api"), "codex": tool_of(a, "codex"), "antigravity": tool_of(a, "antigravity") }) }).collect()
    };
    json!({
        "totals": { "input_tokens": tin, "output_tokens": tout, "total_tokens": tin + tout,
                    "cache_read_tokens": tcr, "cache_creation_tokens": tcc, "cost_usd": r4(tcost), "entries": kept,
                    "unpriced_entries": unpriced_entries },
        "by_tool": by_tool,
        "by_model": by_model,
        "by_underlying_model": [],
        "by_project": by_project,
        "by_day": series(&mut o_day, &m_day, "day"),
        "by_hour": series(&mut o_hour, &m_hour, "hour"),
    })
}

pub fn read_local_summary(days: i64, tool: Option<&str>, project: Option<&str>) -> Value {
    // Costs are computed at parse time: drop cached files when pricing.json changed.
    static SEEN_VERSION: Mutex<u64> = Mutex::new(0);
    let v = refresh_overrides();
    if std::mem::replace(&mut *SEEN_VERSION.lock().unwrap(), v) != v {
        cache().lock().unwrap().clear();
    }
    let mut events = Vec::new();
    if tool.is_none() || tool == Some("claude_api") {
        events.extend(read_claude_events(days));
    }
    if tool.is_none() || tool == Some("codex") {
        events.extend(read_codex_events(days));
    }
    if tool.is_none() || tool == Some("antigravity") {
        events.extend(read_antigravity_events(days));
    }
    aggregate(&events, days, tool, project)
}

#[cfg(test)]
mod tests {
    /// Parity check against the Python backend: `TS_DUMP=<dir> cargo test -- --ignored dump`.
    #[test]
    #[ignore]
    fn dump() {
        let Ok(dir) = std::env::var("TS_DUMP") else { return };
        for days in [1, 30, 100_000] {
            let v = super::read_local_summary(days, None, None);
            std::fs::write(format!("{dir}/rust_{days}.json"), v.to_string()).unwrap();
        }
        std::fs::write(format!("{dir}/rust_util.json"), crate::util::read_local_utilization().to_string()).unwrap();
    }
}
