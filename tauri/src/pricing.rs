//! Per-1M-token list rates. Port of the pricing functions in `backend/local_sources.py`
//! (rates checked 2026-11-01); keep both in sync.
use regex::Regex;
use std::sync::OnceLock;

pub const CLAUDE_CACHE_READ_MULT: f64 = 0.1;
pub const CLAUDE_CACHE_WRITE_5M_MULT: f64 = 1.25;
pub const CLAUDE_CACHE_WRITE_1H_MULT: f64 = 2.0;
pub const CODEX_CACHED_INPUT_MULT: f64 = 0.1;

pub type Rate = (String, f64, f64);

fn r(label: &str, i: f64, o: f64) -> Rate {
    (label.to_string(), i, o)
}

/// 1–2 digits at `i` that are not followed by another digit (Python `\d{1,2}(?!\d)`).
fn small_num(b: &[u8], i: usize) -> Option<(u32, usize)> {
    let mut j = i;
    while j < b.len() && j - i < 3 && b[j].is_ascii_digit() {
        j += 1;
    }
    let n = j - i;
    if n == 0 || n > 2 {
        return None;
    }
    Some((std::str::from_utf8(&b[i..j]).ok()?.parse().ok()?, j))
}

/// (major, minor) after the family word: `opus-4-1`, `opus-4-20250514` -> (4, 0),
/// `Opus 4.6`; legacy `claude-3-5-haiku` -> (3, 5). (0, 0) when absent.
pub fn claude_version(b: &str, family: &str) -> (u32, u32) {
    let bytes = b.as_bytes();
    let mut from = 0;
    while let Some(pos) = b[from..].find(family) {
        let mut i = from + pos + family.len();
        if i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'-') {
            i += 1;
        }
        if let Some((major, j)) = small_num(bytes, i) {
            let minor = if j < bytes.len() && (bytes[j] == b'.' || bytes[j] == b'-') {
                small_num(bytes, j + 1).map(|(m, _)| m).unwrap_or(0)
            } else {
                0
            };
            return (major, minor);
        }
        from += pos + family.len();
    }
    // legacy ids: claude-3-5-haiku-20241022
    static LEGACY: OnceLock<Regex> = OnceLock::new();
    let re = LEGACY.get_or_init(|| Regex::new(r"claude-(\d+)(?:-(\d{1,2}))?-([a-z]+)").unwrap());
    for c in re.captures_iter(b) {
        if &c[3] == family {
            let major = c[1].parse().unwrap_or(0);
            let minor = c.get(2).and_then(|m| m.as_str().parse().ok()).unwrap_or(0);
            return (major, minor);
        }
    }
    (0, 0)
}

pub fn claude_pricing(model_id: &str, prompt_tokens: u64) -> Rate {
    let lower = model_id.to_lowercase();
    let b = lower.split('[').next().unwrap_or("");
    if b.contains("fable") || b.contains("mythos") {
        return r("Fable", 10.0, 50.0);
    }
    if b.contains("haiku") {
        let v = claude_version(b, "haiku");
        if v >= (5, 5) {
            return if prompt_tokens > 100_000 { r("Haiku 5.5", 0.50, 2.50) } else { r("Haiku 5.5", 0.10, 0.50) };
        }
        if v.0 == 3 {
            return r("Haiku", 0.8, 4.0);
        }
        return r("Haiku", 1.0, 5.0);
    }
    if b.contains("sonnet") {
        if claude_version(b, "sonnet").0 >= 5 {
            return r("Sonnet 5", 2.0, 10.0);
        }
        return r("Sonnet", 3.0, 15.0);
    }
    if b.contains("opus") {
        let v = claude_version(b, "opus");
        if v >= (5, 5) {
            return r("Opus 5.5", 4.0, 20.0);
        }
        if v == (4, 0) || v == (4, 1) {
            return r("Opus 4.1", 15.0, 75.0);
        }
        return r("Opus", 5.0, 25.0);
    }
    r(if model_id.is_empty() { "unknown" } else { model_id }, 0.0, 0.0)
}

/// Cache-hit multiplier of the input rate: 0.025x Fable/Mythos 5.1, 0.05x Opus/Sonnet 5.5.
pub fn claude_cache_read_mult(model_id: &str) -> f64 {
    let b = model_id.to_lowercase();
    for fam in ["fable", "mythos"] {
        if b.contains(fam) && claude_version(&b, fam) >= (5, 1) {
            return 0.025;
        }
    }
    for fam in ["opus", "sonnet"] {
        if b.contains(fam) && claude_version(&b, fam) >= (5, 5) {
            return 0.05;
        }
    }
    CLAUDE_CACHE_READ_MULT
}

/// Python `gpt-5(?:[.-]1)?(?![.\d-])`: bare GPT-5 / 5.1.
fn is_gpt5_base(b: &str) -> bool {
    let bad = |c: Option<u8>| matches!(c, Some(x) if x == b'.' || x == b'-' || x.is_ascii_digit());
    let bytes = b.as_bytes();
    let mut from = 0;
    while let Some(pos) = b[from..].find("gpt-5") {
        let i = from + pos + 5;
        if i + 1 < bytes.len() && (bytes[i] == b'.' || bytes[i] == b'-') && bytes[i + 1] == b'1' && !bad(bytes.get(i + 2).copied()) {
            return true;
        }
        if !bad(bytes.get(i).copied()) {
            return true;
        }
        from = i;
    }
    false
}

pub fn codex_pricing(model_id: &str) -> Rate {
    static G6: OnceLock<Regex> = OnceLock::new();
    static G56: OnceLock<Regex> = OnceLock::new();
    let b = model_id.to_lowercase();
    let g6 = G6.get_or_init(|| Regex::new(r"gpt[ .-]?6(?:[ .-](\d+))?").unwrap());
    if let Some(c) = g6.captures(&b) {
        let minor = if c.get(1).map(|m| m.as_str()) == Some("1") { ".1" } else { "" };
        if b.contains("astra") {
            return r("GPT-6 Astra", 10.0, 50.0);
        }
        if b.contains("sol") {
            return r(&format!("GPT-6{minor} Sol"), 2.0, 10.0);
        }
        if b.contains("terra") {
            return r("GPT-6.1 Terra", 1.0, 5.0);
        }
        if b.contains("luna") {
            return r("GPT-6 Luna", 0.10, 0.50);
        }
    }
    if G56.get_or_init(|| Regex::new(r"gpt[ .-]?5[ .-]?6").unwrap()).is_match(&b) {
        if b.contains("luna") {
            return r("GPT-5.6 Luna", 0.20, 1.20);
        }
        if b.contains("terra") {
            return r("GPT-5.6 Terra", 2.0, 12.0);
        }
        if b.contains("sol") {
            return r("GPT-5.6 Sol", 4.0, 20.0);
        }
    }
    if b.contains("codex") {
        return r("Codex", 1.75, 14.0);
    }
    if b.contains("gpt-5.5") || b.contains("gpt-5-5") {
        return r("GPT-5.5", 5.0, 30.0);
    }
    if b.contains("nano") {
        return r("Nano", 0.20, 1.25);
    }
    if b.contains("mini") {
        return r("Mini", 0.75, 4.50);
    }
    if b.contains("gpt-5.4") || b.contains("gpt-5-4") {
        return r("GPT-5.4", 2.50, 15.0);
    }
    if b.contains("gpt-5.2") || b.contains("gpt-5-2") {
        return r("GPT-5.2", 1.75, 14.0);
    }
    if is_gpt5_base(&b) {
        return r("GPT-5", 1.25, 10.0);
    }
    if b.contains("gpt-4o") {
        return r("GPT-4o", 2.50, 10.0);
    }
    r(if model_id.is_empty() { "unknown" } else { model_id }, 0.0, 0.0)
}

pub fn codex_cached_mult(label: &str) -> f64 {
    if label.starts_with("GPT-6.1") { 0.05 } else { CODEX_CACHED_INPUT_MULT }
}

/// Antigravity: subscription usage, so these public list rates are an ESTIMATE.
/// `day` (YYYY-MM-DD, local) applies Google's announced price changes on their date.
pub fn antigravity_pricing(model_id: &str, day: &str) -> Rate {
    static GV: OnceLock<Regex> = OnceLock::new();
    let b = model_id.to_lowercase();
    if b.contains("claude") {
        return claude_pricing(model_id, 0);
    }
    if b.contains("gpt") || b.contains("oss") {
        return r("GPT-OSS", 0.15, 0.60);
    }
    let ver = GV
        .get_or_init(|| Regex::new(r"gemini[ \-]?(\d+(?:[.\-]\d)?)").unwrap())
        .captures(&b)
        .map(|c| c[1].replace('-', "."))
        .unwrap_or_default();
    if b.contains("argon") {
        return if day >= "2026-12-01" { r("Gemini 4 Argon", 4.0, 20.0) } else { r("Gemini 4 Argon", 2.0, 10.0) };
    }
    if b.contains("flash") {
        if b.contains("lite") {
            return match ver.as_str() {
                "3.5" => r("Gemini Flash Lite", 0.30, 2.50),
                "2.5" => r("Gemini Flash Lite", 0.10, 0.40),
                _ => r("Gemini Flash Lite", 0.25, 1.50),
            };
        }
        return match ver.as_str() {
            "3.6" | "3.8" if day >= "2027-01-01" => r("Gemini Flash", 1.50, 7.50),
            "3.6" | "3.8" => r("Gemini Flash", 0.75, 3.75),
            "3.5" => r("Gemini Flash", 1.50, 9.00),
            "2.5" => r("Gemini Flash", 0.30, 2.50),
            _ => r("Gemini Flash", 0.50, 3.00),
        };
    }
    if b.contains("pro") {
        return if ver == "2.5" { r("Gemini Pro", 1.25, 10.0) } else { r("Gemini Pro", 2.0, 12.0) };
    }
    r("Gemini", 0.50, 3.00)
}

// ----- user overrides: ~/.tokenscope/pricing.json, no rebuild needed -----
// {"models": [{"match": "gpt-6-luna", "input": 0.1, "output": 0.5, "from": "2026-11-01", "until": "2027-01-01"}]}
// `match` is a case-insensitive substring of the model id (longest match wins); `from` is
// inclusive, `until` exclusive, both optional (YYYY-MM-DD, local day of the event).
#[derive(Clone, Debug, PartialEq)]
pub struct Override {
    pub pattern: String,
    pub input: f64,
    pub output: f64,
    pub from: String,
    pub until: String,
}

struct Overrides {
    mtime: Option<std::time::SystemTime>,
    list: Vec<Override>,
    version: u64,
}
static OVERRIDES: std::sync::Mutex<Overrides> = std::sync::Mutex::new(Overrides { mtime: None, list: Vec::new(), version: 0 });

pub fn overrides_path() -> std::path::PathBuf {
    crate::paths::tokenscope_dir().join("pricing.json")
}

pub fn parse_overrides(text: &str) -> Vec<Override> {
    let v: serde_json::Value = serde_json::from_str(text.trim_start_matches('\u{feff}')).unwrap_or_default();
    v["models"].as_array().into_iter().flatten().filter_map(|m| {
        let pattern = m["match"].as_str()?.trim().to_lowercase();
        let (input, output) = (m["input"].as_f64()?, m["output"].as_f64()?);
        if pattern.is_empty() || !(input >= 0.0 && output >= 0.0) {
            return None;
        }
        let s = |k: &str| m[k].as_str().unwrap_or("").to_string();
        Some(Override { pattern, input, output, from: s("from"), until: s("until") })
    }).collect()
}

/// Reloads the override file when it changed; returns a version that bumps on every change
/// (the event cache is keyed on it, since costs are computed at parse time).
pub fn refresh_overrides() -> u64 {
    let mtime = std::fs::metadata(overrides_path()).and_then(|m| m.modified()).ok();
    let mut o = OVERRIDES.lock().unwrap();
    if mtime != o.mtime {
        o.list = mtime.and_then(|_| std::fs::read_to_string(overrides_path()).ok()).map(|t| parse_overrides(&t)).unwrap_or_default();
        o.mtime = mtime;
        o.version += 1;
    }
    o.version
}

pub fn find_override(list: &[Override], model: &str, day: &str) -> Option<Override> {
    let m = model.to_lowercase();
    list.iter()
        .filter(|o| m.contains(&o.pattern))
        .filter(|o| (o.from.is_empty() || day >= o.from.as_str()) && (o.until.is_empty() || day < o.until.as_str()))
        .max_by_key(|o| o.pattern.len())
        .cloned()
}

/// Final (input, output) rate for one event + whether the model has a known price.
pub fn resolve(model: &str, day: &str, builtin: &Rate) -> (f64, f64, bool) {
    if let Some(o) = find_override(&OVERRIDES.lock().unwrap().list, model, day) {
        return (o.input, o.output, true);
    }
    (builtin.1, builtin.2, builtin.1 > 0.0 || builtin.2 > 0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overrides_and_dates() {
        let list = parse_overrides(r#"{"models":[
            {"match":"luna","input":1,"output":2},
            {"match":"GPT-6-Luna","input":0.2,"output":0.4,"from":"2026-12-01"},
            {"match":"bad","input":-1,"output":1}]}"#);
        assert_eq!(list.len(), 2);
        assert_eq!(find_override(&list, "gpt-6-luna", "2026-11-15").unwrap().input, 1.0); // dated rule not yet active
        assert_eq!(find_override(&list, "gpt-6-luna", "2026-12-02").unwrap().input, 0.2); // longest match wins
        assert!(find_override(&list, "gpt-6-sol", "2026-12-02").is_none());
        assert_eq!(rates(antigravity_pricing("Gemini 4 Argon", "2026-11-30")), (2.0, 10.0));
        assert_eq!(rates(antigravity_pricing("Gemini 4 Argon", "2026-12-01")), (4.0, 20.0));
        assert_eq!(rates(antigravity_pricing("Gemini 3.8 Flash", "2027-01-01")), (1.50, 7.50));
    }

    fn rates(x: Rate) -> (f64, f64) {
        (x.1, x.2)
    }

    #[test]
    fn claude() {
        assert_eq!(rates(claude_pricing("claude-opus-5-5", 0)), (4.0, 20.0));
        assert_eq!(rates(claude_pricing("claude-opus-5", 0)), (5.0, 25.0));
        assert_eq!(rates(claude_pricing("claude-opus-4-20250514", 0)), (15.0, 75.0));
        assert_eq!(rates(claude_pricing("claude-opus-4-8", 0)), (5.0, 25.0));
        assert_eq!(rates(claude_pricing("Claude Opus 4.6 (Thinking)", 0)), (5.0, 25.0));
        assert_eq!(rates(claude_pricing("claude-sonnet-5-5", 0)), (2.0, 10.0));
        assert_eq!(rates(claude_pricing("claude-sonnet-4-5-20250929", 0)), (3.0, 15.0));
        assert_eq!(rates(claude_pricing("claude-haiku-5-5", 0)), (0.10, 0.50));
        assert_eq!(rates(claude_pricing("claude-haiku-5-5", 150_000)), (0.50, 2.50));
        assert_eq!(rates(claude_pricing("claude-haiku-4-5-20251001", 0)), (1.0, 5.0));
        assert_eq!(rates(claude_pricing("claude-3-5-haiku-20241022", 0)), (0.8, 4.0));
        assert_eq!(rates(claude_pricing("claude-fable-5-1", 0)), (10.0, 50.0));
        assert_eq!(claude_cache_read_mult("claude-fable-5-1"), 0.025);
        assert_eq!(claude_cache_read_mult("claude-fable-5"), 0.1);
        assert_eq!(claude_cache_read_mult("claude-opus-5-5"), 0.05);
        assert_eq!(claude_cache_read_mult("claude-sonnet-4-6"), 0.1);
    }

    #[test]
    fn openai() {
        assert_eq!(rates(codex_pricing("gpt-6-luna")), (0.10, 0.50));
        assert_eq!(codex_pricing("gpt-6-sol").0, "GPT-6 Sol");
        assert_eq!(codex_pricing("gpt-6.1-sol").0, "GPT-6.1 Sol");
        assert_eq!(rates(codex_pricing("GPT 6.1 Terra")), (1.0, 5.0));
        assert_eq!(rates(codex_pricing("gpt-6-astra")), (10.0, 50.0));
        assert_eq!(rates(codex_pricing("gpt-5.6-sol")), (4.0, 20.0));
        assert_eq!(rates(codex_pricing("GPT 5.6 Terra")), (2.0, 12.0));
        assert_eq!(rates(codex_pricing("gpt-5-6-luna")), (0.20, 1.20));
        assert_eq!(rates(codex_pricing("gpt-5.2")), (1.75, 14.0));
        assert_eq!(rates(codex_pricing("gpt-5.1")), (1.25, 10.0));
        assert_eq!(rates(codex_pricing("gpt-5")), (1.25, 10.0));
        assert_eq!(rates(codex_pricing("gpt-5.7")), (0.0, 0.0));
        assert_eq!(rates(codex_pricing("gpt-5.3-codex")), (1.75, 14.0));
        assert_eq!(rates(codex_pricing("gpt-5.5")), (5.0, 30.0));
    }

    #[test]
    fn gemini() {
        let d = "2026-11-01";
        assert_eq!(rates(antigravity_pricing("Gemini 3.5 Flash (Medium)", d)), (1.50, 9.00));
        assert_eq!(rates(antigravity_pricing("Gemini 3.1 Flash Lite", d)), (0.25, 1.50));
        assert_eq!(rates(antigravity_pricing("Gemini 3.1 Pro (High)", d)), (2.0, 12.0));
        assert_eq!(rates(antigravity_pricing("Claude Sonnet 4.6 (Thinking)", d)), (3.0, 15.0));
        assert_eq!(rates(antigravity_pricing("GPT-OSS 120B", d)), (0.15, 0.60));
        assert_eq!(rates(antigravity_pricing("Gemini 3.8 Flash (High)", d)), (0.75, 3.75));
        assert_eq!(rates(antigravity_pricing("Gemini 3.5 Flash-Lite", d)), (0.30, 2.50));
        assert_eq!(rates(antigravity_pricing("Gemini 4 Argon", d)), (2.0, 10.0));
    }
}
