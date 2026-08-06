//! Who is "me"? An agent is identified by `$JJ_EXTRACT_AGENT` (set per-process
//! for deliberately named agents), Claude's session id, or Codex's thread id.

pub const ENV_VAR: &str = "JJ_EXTRACT_AGENT";
/// Claude Code exports this for every session; it equals the hook payload's
/// `session_id` and survives into the `jj extract` alias.
pub const SESSION_ENV_VAR: &str = "CLAUDE_CODE_SESSION_ID";
/// Codex exports the current hook `session_id` to tool processes under this
/// name, so `jj extract` can resolve the same identity used for snapshots.
pub const CODEX_SESSION_ENV_VAR: &str = "CODEX_THREAD_ID";

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
        .or_else(|| env_nonempty(CODEX_SESSION_ENV_VAR))
        .unwrap_or_else(|| "unknown".to_string())
}

/// Resolve "me" for a CLI invocation: explicit `--agent`, else `$JJ_EXTRACT_AGENT`,
/// else the agent's exported session variable. These survive `jj util exec` and
/// equal the `session_id` that the recording hooks tagged onto the evolog.
pub fn from_cli(explicit: Option<&str>) -> Option<String> {
    if let Some(e) = explicit {
        if !e.is_empty() {
            return Some(e.to_string());
        }
    }
    env_nonempty(ENV_VAR)
        .or_else(|| env_nonempty(SESSION_ENV_VAR))
        .or_else(|| env_nonempty(CODEX_SESSION_ENV_VAR))
}

fn env_nonempty(var: &str) -> Option<String> {
    std::env::var(var).ok().filter(|s| !s.is_empty())
}
