//! Data locations, overridable with the same TOKENSCOPE_* variables as the Python app.
use std::env;
use std::path::PathBuf;

pub fn home() -> PathBuf {
    env::var_os("USERPROFILE").or_else(|| env::var_os("HOME")).map(PathBuf::from).unwrap_or_else(|| ".".into())
}

fn var_or(var: &str, default: impl FnOnce() -> PathBuf) -> PathBuf {
    env::var_os(var).filter(|v| !v.is_empty()).map(PathBuf::from).unwrap_or_else(default)
}

pub fn claude_dir() -> PathBuf {
    var_or("TOKENSCOPE_CLAUDE_DIR", || home().join(".claude"))
}

pub fn codex_dir() -> PathBuf {
    var_or("TOKENSCOPE_CODEX_DIR", || home().join(".codex"))
}

pub fn claude_rpc_dir() -> PathBuf {
    var_or("TOKENSCOPE_CLAUDE_RPC_DIR", || home().join(".claude-rpc"))
}

pub fn tokenscope_dir() -> PathBuf {
    var_or("TOKENSCOPE_DIR", || home().join(".tokenscope"))
}

pub fn antigravity_conv_dir() -> PathBuf {
    var_or("TOKENSCOPE_GEMINI_DIR", || home().join(".gemini")).join("antigravity").join("conversations")
}

fn appdata_dir() -> PathBuf {
    if cfg!(target_os = "macos") {
        return home().join("Library").join("Application Support");
    }
    if cfg!(not(windows)) {
        return var_or("XDG_CONFIG_HOME", || home().join(".config"));
    }
    var_or("APPDATA", || home().join("AppData").join("Roaming"))
}

pub fn antigravity_state_db() -> PathBuf {
    var_or("TOKENSCOPE_ANTIGRAVITY_STATE", || {
        appdata_dir().join("Antigravity").join("User").join("globalStorage").join("state.vscdb")
    })
}
