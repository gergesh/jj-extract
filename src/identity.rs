//! Who is "me"? An agent is identified by `$JJ_EXTRACT_AGENT` (set per-process
//! for deliberately-named agents) or, failing that, the Claude `session_id`
//! (distinct per top-level `claude` process).

pub const ENV_VAR: &str = "JJ_EXTRACT_AGENT";

/// Resolve the acting agent from a hook payload's `session_id`, letting an
/// explicit `$JJ_EXTRACT_AGENT` win.
pub fn from_payload(session_id: Option<&str>) -> String {
    if let Ok(v) = std::env::var(ENV_VAR) {
        if !v.is_empty() {
            return v;
        }
    }
    session_id.filter(|s| !s.is_empty()).unwrap_or("unknown").to_string()
}

/// Resolve "me" for a CLI invocation: explicit `--agent`, else the env var
/// (normally populated by the SessionStart hook into `$CLAUDE_ENV_FILE`).
pub fn from_cli(explicit: Option<&str>) -> Option<String> {
    if let Some(e) = explicit {
        if !e.is_empty() {
            return Some(e.to_string());
        }
    }
    std::env::var(ENV_VAR).ok().filter(|s| !s.is_empty())
}
