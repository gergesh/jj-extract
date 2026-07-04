//! Who is "me"? An agent is identified by `$JJ_EXTRACT_AGENT` (set per-process
//! for deliberately-named agents) or, failing that, the Claude `session_id`
//! (distinct per top-level `claude` process).

pub const ENV_VAR: &str = "JJ_EXTRACT_AGENT";
/// Claude Code exports this for every session; it equals the hook payload's
/// `session_id`. Unlike [`ENV_VAR`] (stamped into `$CLAUDE_ENV_FILE`, which is
/// sourced *unexported* and so never reaches `jj util exec`'d subprocesses), it
/// survives into the `jj extract` alias — so it's the reliable default identity.
pub const SESSION_ENV_VAR: &str = "CLAUDE_CODE_SESSION_ID";

/// Resolve the acting agent from a hook payload's `session_id`, letting an
/// explicit `$JJ_EXTRACT_AGENT` win.
pub fn from_payload(session_id: Option<&str>) -> String {
    if let Ok(v) = std::env::var(ENV_VAR) {
        if !v.is_empty() {
            return v;
        }
    }
    session_id
        .map(str::to_string)
        .filter(|s| !s.is_empty())
        .or_else(|| env_nonempty(SESSION_ENV_VAR))
        .unwrap_or_else(|| "unknown".to_string())
}

/// Resolve "me" for a CLI invocation: explicit `--agent`, else `$JJ_EXTRACT_AGENT`,
/// else Claude's `$CLAUDE_CODE_SESSION_ID`. The last is the one that reliably
/// reaches us: the `jj extract` alias runs us via `jj util exec`, and the
/// SessionStart hook stamps `$JJ_EXTRACT_AGENT` *unexported* into the shell, so it
/// usually won't propagate here — but `$CLAUDE_CODE_SESSION_ID` does, and it's the
/// same id the recording hooks tagged the evolog with.
pub fn from_cli(explicit: Option<&str>) -> Option<String> {
    if let Some(e) = explicit {
        if !e.is_empty() {
            return Some(e.to_string());
        }
    }
    env_nonempty(ENV_VAR).or_else(|| env_nonempty(SESSION_ENV_VAR))
}

fn env_nonempty(var: &str) -> Option<String> {
    std::env::var(var).ok().filter(|s| !s.is_empty())
}
