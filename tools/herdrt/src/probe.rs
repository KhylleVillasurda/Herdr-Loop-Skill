//! Shared Herdr-origin probe functions (ported from tools/access_probe.py).
//! classify_failure/sanitize_error are reused by activate-roles, so they live here.

use regex::Regex;
use serde_json::{json, Map, Value};

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SpawnFailure {
    NotFound,
    Timeout,
    Permission,
    Other,
}

pub const SECRET_ASSIGNMENT: &str =
    r"(?i)\b(token|password|secret|api[_-]?key|authorization)(\s*[:=]\s*)([^\s,;]+)";
pub const VERSION: &str = r"\b\d+\.\d+(?:\.\d+)?(?:[-+][0-9A-Za-z.-]+)?\b";

pub fn sanitize_error(value: &str) -> String {
    let text = value.split_whitespace().collect::<Vec<_>>().join(" ");
    let bearer = Regex::new(r"(?i)\bBearer\s+[^\s,;]+").unwrap();
    let text = bearer.replace_all(&text, "Bearer <redacted>").into_owned();
    let secrets = Regex::new(SECRET_ASSIGNMENT).unwrap();
    let text = secrets.replace_all(&text, "${1}${2}<redacted>").into_owned();
    let pats = Regex::new(r"\b(?:github_pat_[A-Za-z0-9_]{12,}|gh[pousr]_[A-Za-z0-9]{12,}|sk-[A-Za-z0-9_-]{12,})\b").unwrap();
    let text = pats.replace_all(&text, "<redacted>").into_owned();
    chars_prefix(&text, 200)
}

fn chars_prefix(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

pub fn classify_failure(message: &str, boundary: &str, failure: SpawnFailure) -> String {
    let early = match failure {
        SpawnFailure::NotFound => Some(String::from(if boundary.starts_with("herdr") {
            "HERDR_CLI_UNAVAILABLE"
        } else {
            "EXECUTABLE_UNAVAILABLE"
        })),
        SpawnFailure::Timeout => Some("COMMAND_TIMEOUT".to_string()),
        SpawnFailure::Permission => Some("OS_ACCESS_DENIED".to_string()),
        SpawnFailure::Other => None,
    };
    if let Some(c) = early {
        return c;
    }
    let lower = message.to_lowercase();
    let denied = ["denied", "permission", "eacces", "winerror 5", "not permitted"]
        .iter().any(|t| lower.contains(t));
    if ["approval denied", "user denied", "request rejected", "declined by user", "permission request denied"]
        .iter().any(|t| lower.contains(t)) { return "DENIED".into(); }
    if denied && ["sandbox", "policy", "approval"].iter().any(|t| lower.contains(t)) {
        return "AGENT_SANDBOX_DENIED".into();
    }
    if boundary.starts_with("herdr") {
        if regex::Regex::new(r"\bcode\s*:\s*5\b").unwrap().is_match(&lower)
            && lower.contains("permissiondenied") {
            return "HERDR_TRANSPORT_DENIED".into();
        }
        if ["connection refused", "failed to connect", "no server", "socket", "named pipe", "ipc"]
            .iter().any(|t| lower.contains(t)) {
            return "HERDR_TRANSPORT_DENIED".into();
        }
    }
    if denied {
        return "ACCESS_DENIED_UNCLASSIFIED".into();
    }
    if boundary.starts_with("herdr") {
        return "HERDR_TRANSPORT_DENIED".into();
    }
    "COMMAND_FAILED".into()
}

pub struct RunOutcome {
    pub ok: bool,
    pub exit: Option<i32>,
    pub error: String,
    pub boundary: String,
    pub failure_code: Option<String>,
    pub version: Option<String>,
}

/// Python access_probe.run(): execute, sanitize stderr/stdout, classify failures.
pub fn run(argv: &[&str], timeout: u64, boundary: &str, capture_version: bool) -> RunOutcome {
    let outcome = crate::util::exec(argv, timeout);
    if !outcome.timed_out && outcome.exit.is_none() {
        // Spawn failure (executable missing / OS refusal), mirrors the except branch.
        // Spawn failure (executable missing / OS refusal), mirrors the except branch.
        let raw = outcome.stderr_text();
        let message = if raw.is_empty() { "SpawnFailed".to_string() } else { raw.clone() };
        let kind = if spawn_not_found(&raw) {
            SpawnFailure::NotFound
        } else if spawn_denied(&raw) {
            SpawnFailure::Permission
        } else {
            SpawnFailure::Other
        };
        return RunOutcome {
            ok: false,
            exit: None,
            error: sanitize_error(&message),
            failure_code: Some(classify_failure(&message, boundary, kind)),
            boundary: boundary.into(),
            version: None,
        };
    }
    let raw = if !outcome.stderr_text().is_empty() {
        outcome.stderr_text()
    } else {
        outcome.stdout_text()
    };
    let ok = outcome.exit == Some(0);
    let mut error = sanitize_error(&raw);
    let mut failure_code = None;
    let mut version = None;
    if !ok {
        let kind = if outcome.timed_out {
            SpawnFailure::Timeout
        } else {
            SpawnFailure::Other
        };
        failure_code = Some(classify_failure(&raw, boundary, kind));
        if outcome.timed_out && error.is_empty() {
            // Python sets the message from the exception type name for timeouts.
            error = "TimeoutExpired".into();
        }
        if error.is_empty() {
            error = format!("command exited with status {}", outcome.returncode());
        }
    } else if capture_version {
        let stdout_text = outcome.stdout_text();
        let m = Regex::new(VERSION).unwrap().find(&stdout_text);
        version = m.map(|m| m.as_str().to_string());
    }
    RunOutcome { ok, exit: outcome.exit, error, failure_code, boundary: boundary.into(), version }
}

fn spawn_not_found(msg: &str) -> bool {
    #[cfg(target_os = "windows")]
    { msg.contains("1060") || msg.to_lowercase().contains("not found") }
    #[cfg(not(target_os = "windows"))]
    { msg.to_lowercase().contains("no such file") }
}

fn spawn_denied(msg: &str) -> bool {
    msg.to_lowercase().contains("denied") || msg.to_lowercase().contains("permission")
}

/// Which-probe of a known harness with version capture, mirroring _probe_harness.
pub fn probe_harness(result: &mut Value, checks: &mut Map<String, Value>, harness_kind: &str) {
    if harness_kind.is_empty() {
        result["harness_kind"] = Value::Null;
        result["harness_version"] = Value::Null;
        result["approval_capability"] = json!({
            "status": "unverified",
            "evidence": "The active harness tool and its current approval channel are not observable by this probe.",
        });
        return;
    }
    result["harness_kind"] = json!(harness_kind);
    result["harness_version"] = Value::Null;
    result["approval_capability"] = json!({
        "status": "unverified",
        "evidence": "The active harness tool and its current approval channel are not observable by this probe.",
    });
    let executable_name = match harness_kind {
        "codex" => "codex",
        "claude" => "claude",
        "agy" => "agy",
        "opencode" => "opencode",
        _ => return,
    };
    let Some(executable) = crate::util::which(executable_name) else { return };
    let exe = executable.to_string_lossy().into_owned();
    let version = run(&[&exe, "--version"], 8, "harness_version", true);
    checks.insert("harness_version_probe".into(), json!({
        "ok": version.ok, "version": version.version,
    }));
    result["harness_version"] = version.version.into();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::exec;

    #[test]
    fn sanitize_redacts_bearer_and_secrets() {
        assert_eq!(sanitize_error("Bearer abc.def.ghi failed"), "Bearer <redacted> failed");
        assert_eq!(sanitize_error("api_key: hunter2 wrong"), "api_key: <redacted> wrong");
        assert_eq!(sanitize_error("password=hunter2"), "password=<redacted>");
        assert_eq!(sanitize_error("github_pat_ABCDEFGHIJKLMNOP"), "<redacted>");
        assert_eq!(sanitize_error("sk-AAAAAAAAAAAAAAAAAAAA"), "<redacted>");
        assert_eq!(sanitize_error("gh[pousr]_literal"), "gh[pousr]_literal");
    }

    #[test]
    fn sanitize_truncates_to_200_chars() {
        let long = "a".repeat(500);
        assert_eq!(sanitize_error(&long).chars().count(), 200);
    }

    #[test]
    fn classify_distinguishes_boundaries() {
        use SpawnFailure::*;
        assert_eq!(classify_failure("No such file", "herdr_executable", NotFound), "HERDR_CLI_UNAVAILABLE");
        assert_eq!(classify_failure("No such file", "workspace_git", NotFound), "EXECUTABLE_UNAVAILABLE");
        assert_eq!(classify_failure("tm out", "herdr", Timeout), "COMMAND_TIMEOUT");
        assert_eq!(classify_failure("user denied", "herdr_agent", Other), "DENIED");
        assert_eq!(classify_failure("sandbox denied", "herdr", Other), "AGENT_SANDBOX_DENIED");
        assert_eq!(classify_failure("permission denied", "herdr", Other), "ACCESS_DENIED_UNCLASSIFIED");
        assert_eq!(classify_failure("boom", "herdr", Other), "HERDR_TRANSPORT_DENIED");
        assert_eq!(classify_failure("boom", "git", Other), "COMMAND_FAILED");
        assert_eq!(classify_failure("code : 5 PermissionDenied", "herdr", Other), "HERDR_TRANSPORT_DENIED");
    }

    #[test]
    fn exec_splits_streams() {
        let o = exec(&["echo", "hello"], 5);
        assert_eq!(o.exit, Some(0));
        assert_eq!(o.stdout_text().trim(), "hello");
        let timeout_probe = exec(&["sleep", "3"], 1);
        assert!(timeout_probe.timed_out);
        assert_eq!(timeout_probe.exit, None);
    }
}
