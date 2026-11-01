//! Subscription utilization (Claude OAuth usage API + caches, Codex rate limits) and the
//! availability probe. Port of `backend/local_sources.py`; secrets are never logged.
use crate::events::parse_ts;
use crate::paths;
use base64::Engine;
use chrono::{DateTime, Utc};
use serde_json::{json, Map, Value};
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const OAUTH_USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
const OAUTH_BETA: &str = "oauth-2025-04-20";
const LIMITS_CACHE_TTL_S: f64 = 24.0 * 3600.0;
const UTIL_TTL_S: f64 = 60.0;

struct State {
    oauth_backoff_until: f64,
    cli_refresh_at: f64,
    util: Option<(f64, Value)>,
}
static STATE: Mutex<State> = Mutex::new(State { oauth_backoff_until: 0.0, cli_refresh_at: 0.0, util: None });

fn now() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

fn read_json(p: &Path) -> Option<Value> {
    let s = fs::read_to_string(p).ok()?;
    serde_json::from_str(s.trim_start_matches('\u{feff}')).ok()
}

fn iso(dt: DateTime<Utc>) -> String {
    dt.to_rfc3339_opts(chrono::SecondsFormat::AutoSi, false)
}

fn epoch_to_iso(secs: f64) -> Option<String> {
    DateTime::<Utc>::from_timestamp(secs.trunc() as i64, ((secs.fract()) * 1e9) as u32).map(iso)
}

/// Normalize a reset value: ISO strings pass through; epoch s/ms numbers are converted.
fn reset_to_iso(v: Option<&Value>) -> Value {
    let n = match v {
        None | Some(Value::Null) => return Value::Null,
        Some(Value::String(s)) => match s.trim().parse::<f64>() {
            Ok(n) => n,
            Err(_) => return Value::String(s.clone()),
        },
        Some(x) => match x.as_f64() {
            Some(n) => n,
            None => return Value::Null,
        },
    };
    if !n.is_finite() {
        return Value::Null;
    }
    epoch_to_iso(if n > 1e11 { n / 1000.0 } else { n }).map(Value::String).unwrap_or(Value::Null)
}

// ----- Claude -----
fn claude_oauth() -> Map<String, Value> {
    read_json(&paths::claude_dir().join(".credentials.json"))
        .and_then(|v| v.get("claudeAiOauth").and_then(|o| o.as_object().cloned()))
        .unwrap_or_default()
}

fn read_claude_oauth_token() -> Option<String> {
    let o = claude_oauth();
    let expires = o.get("expiresAt").and_then(|v| v.as_f64()).unwrap_or(0.0);
    if expires > 0.0 && expires < now() * 1000.0 {
        return None;
    }
    o.get("accessToken").and_then(|v| v.as_str()).filter(|s| !s.is_empty()).map(str::to_string)
}

fn find_on_path(names: &[&str]) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).flat_map(|d| names.iter().map(move |n| d.join(n))).find(|p| p.is_file())
}

/// The token expires after ~8h when the `claude` CLI is idle. `claude auth status` makes the
/// CLI renew and save it itself, so TokenScope never handles the refresh token. 1 try / 10 min.
fn refresh_claude_token_via_cli() -> Option<String> {
    {
        let mut s = STATE.lock().unwrap();
        if now() < s.cli_refresh_at {
            return None;
        }
        s.cli_refresh_at = now() + 600.0;
    }
    let exe = find_on_path(&["claude.exe", "claude.cmd", "claude"])?;
    let mut cmd = if exe.extension().is_some_and(|e| e.eq_ignore_ascii_case("cmd")) {
        let mut c = std::process::Command::new("cmd");
        c.arg("/C").arg(&exe);
        c
    } else {
        std::process::Command::new(&exe)
    };
    cmd.args(["auth", "status"]).stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let mut child = cmd.spawn().ok()?;
    let deadline = now() + 30.0;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if now() < deadline => std::thread::sleep(Duration::from_millis(200)),
            _ => {
                let _ = child.kill();
                return None;
            }
        }
    }
    read_claude_oauth_token()
}

fn extract_oauth_percent(b: &Map<String, Value>) -> Option<f64> {
    if let Some(u) = b.get("utilization").filter(|v| !v.is_null()) {
        return u.as_f64().map(|v| v.round().clamp(0.0, 100.0));
    }
    for k in ["percent_used", "used_percent", "usage", "value"] {
        if let Some(v) = b.get(k).and_then(|v| v.as_f64()) {
            return Some((if v <= 1.5 { v * 100.0 } else { v }).round().clamp(0.0, 100.0));
        }
    }
    None
}

fn title_case(s: &str) -> String {
    s.split(' ').map(|w| {
        let mut c = w.chars();
        c.next().map(|f| f.to_uppercase().collect::<String>() + &c.as_str().to_lowercase()).unwrap_or_default()
    }).collect::<Vec<_>>().join(" ")
}

fn fetch_claude_oauth_limits() -> Vec<Value> {
    if matches!(std::env::var("TOKENSCOPE_CLAUDE_OAUTH").unwrap_or_default().to_lowercase().as_str(), "0" | "false" | "no") {
        return vec![];
    }
    if now() < STATE.lock().unwrap().oauth_backoff_until {
        return vec![];
    }
    let Some(tok) = read_claude_oauth_token().or_else(refresh_claude_token_via_cli) else { return vec![] };
    let resp = ureq::AgentBuilder::new().timeout(Duration::from_secs(8)).build()
        .get(OAUTH_USAGE_URL)
        .set("Authorization", &format!("Bearer {tok}"))
        .set("anthropic-beta", OAUTH_BETA)
        .set("User-Agent", "tokenscope")
        .call();
    let body: Value = match resp {
        Ok(r) => match r.into_string().ok().and_then(|s| serde_json::from_str(&s).ok()) {
            Some(v) => v,
            None => return vec![],
        },
        Err(ureq::Error::Status(429, _)) => {
            STATE.lock().unwrap().oauth_backoff_until = now() + 300.0;
            return vec![];
        }
        Err(_) => return vec![],
    };
    // Buckets are parsed dynamically: Anthropic renames the model-specific weekly window.
    let mut out = vec![];
    for (key, bk) in body.as_object().into_iter().flatten() {
        let Some(bk) = bk.as_object() else { continue };
        let Some(pct) = extract_oauth_percent(bk) else { continue };
        let label = match key.as_str() {
            "five_hour" => "5h".to_string(),
            "seven_day" => "All".to_string(),
            k if k.starts_with("seven_day_") => title_case(&k["seven_day_".len()..].replace('_', " ")) + " only",
            _ => continue,
        };
        out.push(json!({ "label": label, "used_percent": pct,
                         "reset": reset_to_iso(bk.get("resets_at").or_else(|| bk.get("reset_at"))) }));
    }
    out
}

fn read_limits_cache(file: &Path, normalize: bool) -> Vec<Value> {
    let Some(v) = read_json(file) else { return vec![] };
    let updated = v.get("updatedAt").and_then(|x| x.as_f64()).unwrap_or(0.0);
    if now() * 1000.0 - updated > LIMITS_CACHE_TTL_S * 1000.0 {
        return vec![];
    }
    let items = v.get("limits").and_then(|l| l.as_array()).cloned().unwrap_or_default();
    items.into_iter().filter(|e| e.is_object()).filter_map(|e| {
        if !normalize {
            return Some(e);
        }
        let pct = e.get("used_percent").and_then(|p| p.as_f64())?.clamp(0.0, 100.0);
        Some(json!({ "label": e.get("label"), "used_percent": pct, "reset": reset_to_iso(e.get("reset")) }))
    }).collect()
}

fn write_tokenscope_limits_cache(limits: &[Value]) {
    let d = paths::tokenscope_dir();
    let _ = fs::create_dir_all(&d);
    let _ = fs::write(d.join("claude-limits-cache.json"),
                      json!({ "updatedAt": (now() * 1000.0) as i64, "limits": limits }).to_string());
}

/// (mode, plan): subscription (claude.ai OAuth) | api | none.
fn detect_claude_account() -> (&'static str, Option<String>) {
    let o = claude_oauth();
    let sub = o.get("subscriptionType").and_then(|v| v.as_str()).map(str::to_string);
    let has_token = o.get("accessToken").and_then(|v| v.as_str()).is_some_and(|s| !s.is_empty());
    if read_claude_oauth_token().is_some() {
        return ("subscription", Some(sub.unwrap_or_else(|| "subscription".into())));
    }
    if has_token && sub.is_some() {
        return ("subscription", sub); // expired token: still a subscriber
    }
    let env = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
    let mut base = env("ANTHROPIC_BASE_URL").unwrap_or_default().to_lowercase();
    let mut has_key = env("ANTHROPIC_API_KEY").is_some() || env("ANTHROPIC_AUTH_TOKEN").is_some();
    if let Some(s) = read_json(&paths::claude_dir().join("settings.json")) {
        if s.get("apiKeyHelper").is_some_and(|v| !v.is_null() && v != "") {
            has_key = true;
        }
        let e = s.get("env").cloned().unwrap_or_default();
        if e.get("ANTHROPIC_API_KEY").is_some_and(|v| v != "") || e.get("ANTHROPIC_AUTH_TOKEN").is_some_and(|v| v != "") {
            has_key = true;
        }
        if base.is_empty() {
            if let Some(b) = e.get("ANTHROPIC_BASE_URL").and_then(|v| v.as_str()) {
                base = b.to_lowercase();
            }
        }
    }
    if read_json(&paths::home().join(".claude.json")).is_some_and(|c| c.get("primaryApiKey").is_some_and(|v| v != "" && !v.is_null())) {
        has_key = true;
    }
    if base.contains("bedrock") {
        return ("api", Some("bedrock".into()));
    }
    if base.contains("aiplatform") || base.contains("vertex") {
        return ("api", Some("vertex".into()));
    }
    if base.contains("azure") || base.contains("foundry") {
        return ("api", Some("foundry".into()));
    }
    if has_key || !base.is_empty() {
        return ("api", Some("Claude Platform".into()));
    }
    ("none", None)
}

fn read_claude_utilization() -> Value {
    let (mode, plan) = detect_claude_account();
    let mut limits = vec![];
    let mut source = "none";
    if mode == "subscription" {
        // live OAuth -> claude-rpc cache -> our own persisted last-good cache
        limits = fetch_claude_oauth_limits();
        if !limits.is_empty() {
            source = "oauth";
            write_tokenscope_limits_cache(&limits);
        } else {
            limits = read_limits_cache(&paths::claude_rpc_dir().join("limits-cache.json"), true);
            if limits.is_empty() {
                limits = read_limits_cache(&paths::tokenscope_dir().join("claude-limits-cache.json"), false);
            }
            if !limits.is_empty() {
                source = "cache";
            }
        }
        let order = |l: &Value| match l.get("label").and_then(|v| v.as_str()) {
            Some("5h") => (0, String::new()),
            Some("All") => (1, String::new()),
            other => (9, other.unwrap_or("").to_string()),
        };
        limits.sort_by_key(order);
    }
    json!({ "available": !limits.is_empty(), "mode": mode, "source": source, "plan": plan, "limits": limits })
}

// ----- Codex -----
fn parse_codex_credits(c: Option<&Value>) -> Value {
    match c {
        Some(v) if v.is_number() => v.clone(),
        Some(Value::Object(o)) => {
            if o.get("unlimited").is_some_and(|u| u.as_bool() == Some(true)) {
                return Value::Null;
            }
            for k in ["remaining", "balance"] {
                match o.get(k) {
                    Some(v) if v.is_number() => return v.clone(),
                    Some(Value::String(s)) => {
                        if let Ok(f) = s.parse::<f64>() {
                            return json!(f);
                        }
                    }
                    _ => {}
                }
            }
            Value::Null
        }
        _ => Value::Null,
    }
}

fn codex_limit(d: Option<&Value>) -> Value {
    let Some(d) = d.and_then(|v| v.as_object()) else { return Value::Null };
    let Some(used) = d.get("used_percent").filter(|v| !v.is_null()) else { return Value::Null };
    json!({ "used_percent": crate::events::round_to(used.as_f64().unwrap_or(0.0), 1),
            "window_minutes": d.get("window_minutes"),
            "reset": reset_to_iso(d.get("resets_at")) })
}

/// A window whose reset passed after we observed it has rolled over: 0% used.
fn apply_rollover(limit: Value, observed: Option<&str>) -> Value {
    let Some(reset) = limit.get("reset").and_then(|r| r.as_str()).and_then(|r| parse_ts(&json!(r))) else { return limit };
    let obs = observed.and_then(|o| parse_ts(&json!(o)));
    if reset <= Utc::now() && obs.is_none_or(|o| o < reset) {
        let mut l = limit;
        l["used_percent"] = json!(0.0);
        return l;
    }
    limit
}

fn read_codex_plan() -> (Value, Value) {
    let Some(data) = read_json(&paths::codex_dir().join("auth.json")) else { return (Value::Null, Value::Null) };
    let plan = data.pointer("/tokens/id_token").and_then(|v| v.as_str())
        .and_then(|t| t.split('.').nth(1))
        .and_then(|p| base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(p.trim_end_matches('=')).ok())
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .and_then(|c| c.get("https://api.openai.com/auth").and_then(|a| a.get("chatgpt_plan_type")).cloned())
        .unwrap_or(Value::Null);
    (data.get("auth_mode").cloned().unwrap_or(Value::Null), plan)
}

fn tail(path: &Path, max: u64) -> Option<String> {
    if path.extension().is_some_and(|e| e == "zst") {
        let mut buf = Vec::new();
        crate::events::open_text(path)?.read_to_end(&mut buf).ok()?;
        let start = buf.len().saturating_sub(max as usize);
        return Some(String::from_utf8_lossy(&buf[start..]).into_owned());
    }
    let mut f = fs::File::open(path).ok()?;
    let size = f.metadata().ok()?.len();
    f.seek(SeekFrom::Start(size.saturating_sub(max))).ok()?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).ok()?;
    Some(String::from_utf8_lossy(&buf).into_owned())
}

fn read_codex_utilization() -> Value {
    let root = paths::codex_dir().join("sessions");
    let (auth_mode, plan) = read_codex_plan();
    let mut result = json!({ "available": false, "auth_mode": auth_mode, "plan_type": plan,
        "primary": null, "secondary": null, "spark_primary": null, "spark_secondary": null,
        "spark_label": null, "credits": null, "observed_at": null });
    if !root.exists() {
        return result;
    }
    let cutoff = now() - 30.0 * 86400.0;
    let mut files: Vec<(PathBuf, f64)> = vec![];
    fn walk(d: &Path, cutoff: f64, out: &mut Vec<(PathBuf, f64)>) {
        let Ok(rd) = fs::read_dir(d) else { return };
        for e in rd.flatten() {
            let p = e.path();
            let name = e.file_name().to_string_lossy().to_string();
            if p.is_dir() {
                walk(&p, cutoff, out);
            } else if crate::events::is_rollout(&name) {
                if let Some(mt) = e.metadata().ok().and_then(|m| m.modified().ok())
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_secs_f64()) {
                    if mt >= cutoff {
                        out.push((p, mt));
                    }
                }
            }
        }
    }
    walk(&root, cutoff, &mut files);
    files.sort_by(|a, b| b.1.total_cmp(&a.1));

    let (mut codex_snap, mut spark_snap): (Option<Value>, Option<Value>) = (None, None);
    'files: for (fp, mt) in &files {
        let Some(text) = tail(fp, 256 * 1024) else { continue };
        for line in text.split('\n').rev() {
            let Ok(o) = serde_json::from_str::<Value>(line.trim()) else { continue };
            if o.get("type").and_then(|v| v.as_str()) != Some("event_msg") {
                continue;
            }
            let p = &o["payload"];
            if p.get("type").and_then(|v| v.as_str()) != Some("token_count") {
                continue;
            }
            let Some(rl) = p.get("rate_limits").filter(|v| v.is_object()) else { continue };
            let lid = rl.get("limit_id").and_then(|v| v.as_str()).unwrap_or("");
            let lname = rl.get("limit_name").and_then(|v| v.as_str()).unwrap_or("");
            let is_spark = lid.starts_with("codex_") || lid.to_lowercase().contains("spark") || lname.to_lowercase().contains("spark");
            let observed = o.get("timestamp").and_then(parse_ts).map(iso).or_else(|| epoch_to_iso(*mt));
            let snap = json!({ "primary": codex_limit(rl.get("primary")), "secondary": codex_limit(rl.get("secondary")),
                "credits": parse_codex_credits(rl.get("credits")), "plan_type": rl.get("plan_type"),
                "limit_name": if lname.is_empty() { Value::Null } else { json!(lname) }, "observed_at": observed });
            if is_spark {
                spark_snap.get_or_insert(snap);
            } else {
                codex_snap.get_or_insert(snap);
            }
            if codex_snap.is_some() && spark_snap.is_some() {
                break 'files;
            }
        }
    }
    if let Some(base) = codex_snap.as_ref().or(spark_snap.as_ref()) {
        result["available"] = json!(true);
        for k in ["primary", "secondary", "credits", "observed_at"] {
            result[k] = base[k].clone();
        }
        if result["plan_type"].is_null() {
            result["plan_type"] = base["plan_type"].clone();
        }
    }
    if let Some(s) = &spark_snap {
        result["spark_primary"] = s["primary"].clone();
        result["spark_secondary"] = s["secondary"].clone();
        result["spark_label"] = s["limit_name"].clone();
    }
    let obs = result["observed_at"].as_str().map(str::to_string);
    for k in ["primary", "secondary", "spark_primary", "spark_secondary"] {
        let v = result[k].take();
        result[k] = apply_rollover(v, obs.as_deref());
    }
    result
}

/// Throttled: the Claude OAuth endpoint is rate-limited, the dashboard polls every 10 s.
pub fn read_local_utilization() -> Value {
    if let Some((ts, v)) = &STATE.lock().unwrap().util {
        if now() - ts < UTIL_TTL_S {
            return v.clone();
        }
    }
    let v = json!({ "claude": read_claude_utilization(), "codex": read_codex_utilization() });
    STATE.lock().unwrap().util = Some((now(), v.clone()));
    v
}

pub fn read_local_status() -> Value {
    let (c, x) = (paths::claude_dir(), paths::codex_dir());
    json!({
        "claude": { "available": c.join("projects").exists(), "dir": "~/.claude",
                    "has_oauth": read_claude_oauth_token().is_some(),
                    "has_limits_cache": paths::claude_rpc_dir().join("limits-cache.json").exists() },
        "codex": { "available": x.join("sessions").exists(), "dir": "~/.codex", "has_auth": x.join("auth.json").exists() },
        "antigravity": { "available": paths::antigravity_conv_dir().exists(), "dir": "~/.gemini/antigravity",
                         "has_state": paths::antigravity_state_db().exists() },
    })
}
