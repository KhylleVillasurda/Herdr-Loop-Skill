//! Port of tools/access_probe.py: read-only Herdr origin probe.

use crate::probe::{self, RunOutcome, SpawnFailure};
use crate::util::{self, ToolResult};
use regex::Regex;
use serde_json::{json, Map, Value};

pub struct InspectArgs {
    pub agent: Option<String>,
    pub pane: Option<String>,
    pub write_probe: bool,
    pub skip_herdr: bool,
    pub harness_kind: Option<String>,
}

const PROBE_TIMEOUT: u64 = 8;

fn push_blocker(blockers: &mut Vec<String>, details: &mut Vec<Value>,
                code: &str, boundary: &str, target_category: &str, error: &str) {
    if !blockers.contains(&code.to_string()) {
        blockers.push(code.to_string());
    }
    let mut detail = json!({"code": code, "boundary": boundary, "target_category": target_category});
    if !error.is_empty() {
        detail["error"] = json!(probe::sanitize_error(error));
    }
    if !details.contains(&detail) {
        details.push(detail);
    }
}

fn run_outcome_json(o: &RunOutcome) -> Value {
    let mut v = json!({
        "ok": o.ok, "exit": o.exit, "error": o.error, "boundary": o.boundary,
    });
    if !o.ok {
        v["failure_code"] = o.failure_code.clone().into();
    }
    v
}

/// Mirrors access_probe.inspect() including JSON key order.
pub fn inspect(workspace: &std::path::Path, agent: Option<&str>, pane: Option<&str>,
               write_probe: bool, skip_herdr: bool, harness_kind: Option<&str>) -> ToolResult<Value> {
    if let Some(kind) = harness_kind {
        if !Regex::new(r"^[a-z][a-z0-9_-]{0,31}$").unwrap().is_match(kind) {
            return Err("--harness-kind must be a short CLI kind name".into());
        }
    }
    if workspace.is_symlink() {
        return Err("--workspace must not be a symlink".into());
    }
    let root = workspace.canonicalize().map_err(|e| e.to_string())?;
    if !root.is_dir() {
        return Err("--workspace must be a directory".into());
    }

    let env_confirmed = std::env::var("HERDR_ENV").ok().as_deref() == Some("1");
    let mut checks = Map::new();
    let mut blockers: Vec<String> = Vec::new();
    let mut details: Vec<Value> = Vec::new();
    let mut result = json!({
        "workspace": root.to_string_lossy(),
        "platform": util::platform_name(),
        "herdr_version": null,
        "checks": {},
        "blockers": [],
        "blocker_details": [],
        "herdr_origin": {
            "env_confirmed": env_confirmed, "agent_confirmed": null, "pane_confirmed": null,
        },
        "server_reachable": null,
        "named_agent_confirmed": null,
        "workspace_write_test": {
            "attempted": write_probe, "ok": null, "target_category": "assigned workspace .herdr-loop",
        },
    });

    // Python calls _probe_harness unconditionally; it stamps nulls when kind is absent.
    probe::probe_harness(&mut result, &mut checks,
                         harness_kind.unwrap_or(""));

    if skip_herdr {
        push_blocker(&mut blockers, &mut details, "HERDR_ORIGIN_UNCONFIRMED", "herdr_origin", "active Herdr agent", "");
    } else if !env_confirmed {
        push_blocker(&mut blockers, &mut details, "HERDR_ORIGIN_UNCONFIRMED", "herdr_origin", "HERDR_ENV", "");
    } else if let Some(herdr) = util::which("herdr") {
        let exe = herdr.to_string_lossy().into_owned();
        let version = probe::run(&[&exe, "--version"], PROBE_TIMEOUT, "herdr_version", true);
        checks.insert("herdr_version_probe".into(), json!({"ok": version.ok, "version": version.version}));
        result["herdr_version"] = version.version.into();
        let server = probe::run(&[&exe, "status", "server"], PROBE_TIMEOUT, "herdr_server", false);
        checks.insert("herdr_server".into(), run_outcome_json(&server));
        result["server_reachable"] = Value::Bool(server.ok);
        if !server.ok {
            let code = server.failure_code.clone().unwrap_or_else(|| "HERDR_TRANSPORT_DENIED".into());
            push_blocker(&mut blockers, &mut details, &code, &server.boundary, "Herdr server or IPC", &server.error);
        } else if let Some(agent) = agent {
            let named = probe::run(&[&exe, "agent", "get", agent], PROBE_TIMEOUT, "herdr_agent", false);
            checks.insert("named_agent".into(), run_outcome_json(&named));
            result["named_agent_confirmed"] = Value::Bool(named.ok);
            result["herdr_origin"]["agent_confirmed"] = Value::Bool(named.ok);
            if !named.ok {
                push_blocker(&mut blockers, &mut details, "HERDR_ORIGIN_UNCONFIRMED",
                             &named.boundary, "assigned Herdr agent", &named.error);
            }
        }
    } else {
        push_blocker(&mut blockers, &mut details, "HERDR_CLI_UNAVAILABLE", "herdr_executable", "Herdr CLI", "");
    }

    let observed = std::env::var("HERDR_PANE_ID").unwrap_or_default();
    if let Some(pane) = pane {
        let matches: Option<bool> = if observed.is_empty() { None } else { Some(observed == pane) };
        checks.insert("pane_matches_env".into(), matches.into());
        result["herdr_origin"]["pane_confirmed"] = matches.into();
        if matches.is_none() || matches == Some(false) {
            push_blocker(&mut blockers, &mut details, "HERDR_ORIGIN_UNCONFIRMED", "herdr_pane", "assigned Herdr pane", "");
        }
    }
    if agent.is_none() && pane.is_none() {
        push_blocker(&mut blockers, &mut details, "HERDR_ORIGIN_UNCONFIRMED", "herdr_identity", "assigned Herdr agent or pane", "");
    }
    let confirmed = env_confirmed
        && result["server_reachable"] == Value::Bool(true)
        && (result["named_agent_confirmed"] == Value::Bool(true)
            || result["herdr_origin"]["pane_confirmed"] == Value::Bool(true));
    result["herdr_origin"]["confirmed"] = Value::Bool(confirmed);

    let git = probe::run(&["git", "-C", &root.to_string_lossy(), "rev-parse", "--show-toplevel"],
                         PROBE_TIMEOUT, "workspace_git", false);
    let mut git_check = json!({"ok": git.ok, "exit": git.exit});
    if !git.ok {
        git_check["failure_code"] = git.failure_code.into();
    }
    checks.insert("git_workspace".into(), git_check);

    if write_probe {
        let scratch = root.join(".herdr-loop");
        let is_usable_dir = std::fs::symlink_metadata(&scratch)
            .map(|m| !m.file_type().is_symlink() && m.is_dir())
            .unwrap_or(false);
        if !is_usable_dir {
            let error = "missing, symlinked, or non-directory workspace-local .herdr-loop";
            checks.insert("scratch_write".into(), json!({"ok": false, "error": error}));
            result["workspace_write_test"]["ok"] = Value::Bool(false);
            push_blocker(&mut blockers, &mut details, "WORKTREE_WRITE_DENIED",
                         "workspace_write", "assigned workspace .herdr-loop", error);
        } else {
            match write_probe_file(&scratch) {
                Ok(()) => {
                    checks.insert("scratch_write".into(), json!({"ok": true}));
                    result["workspace_write_test"]["ok"] = Value::Bool(true);
                }
                Err((error_str, failure)) => {
                    let code = probe::classify_failure(&error_str, "workspace_write", failure);
                    checks.insert("scratch_write".into(), json!({
                        "ok": false, "error": probe::sanitize_error(&error_str), "failure_code": code,
                    }));
                    result["workspace_write_test"]["ok"] = Value::Bool(false);
                    push_blocker(&mut blockers, &mut details, "WORKTREE_WRITE_DENIED",
                                 "workspace_write", "assigned workspace .herdr-loop", &error_str);
                }
            }
        }
    }

    blockers.sort();
    blockers.dedup();
    let ready = blockers.is_empty() && !skip_herdr;
    result["blockers"] = blockers.into();
    result["blocker_details"] = details.into();
    result["checks"] = Value::Object(checks);
    result["ready_for_role_handshake"] = Value::Bool(ready);
    finish(result)
}

fn finish(result: Value) -> ToolResult<Value> {
    Ok(result)
}

fn write_probe_file(scratch: &std::path::Path) -> Result<(), (String, SpawnFailure)> {
    use tempfile::Builder;
    use std::io::Write;
    let t = Builder::new().prefix(".herdr-probe-").rand_bytes(8).tempfile_in(scratch)
        .map_err(|e| (io_message(&e), spawn_kind(&e)))?;
    t.as_file().write_all(b"ready\n").map_err(|e| (io_message(&e), SpawnFailure::Other))
}

fn io_message(e: &std::io::Error) -> String {
    e.kind().to_string()
}

fn spawn_kind(e: &std::io::Error) -> SpawnFailure {
    match e.kind() {
        std::io::ErrorKind::NotFound => SpawnFailure::NotFound,
        std::io::ErrorKind::PermissionDenied => SpawnFailure::Permission,
        std::io::ErrorKind::TimedOut => SpawnFailure::Timeout,
        _ => SpawnFailure::Other,
    }
}

/// CLI mirroring access_probe.main() including exit codes and error output shape.
pub fn cli(argv: &[String]) -> i32 {
    let mut workspace: Option<String> = None;
    let mut agent = None;
    let mut pane = None;
    let mut harness_kind = None;
    let mut write_probe = false;
    let mut skip_herdr = false;
    let mut it = argv.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--workspace" => { workspace = it.next().cloned(); }
            "--agent" => { agent = it.next().cloned(); }
            "--pane" => { pane = it.next().cloned(); }
            "--harness-kind" => { harness_kind = it.next().cloned(); }
            "--probe-write" => write_probe = true,
            "--skip-herdr" => skip_herdr = true,
            _ => {
                eprintln!("usage: herdrt access-probe --workspace PATH [--agent NAME] [--pane ID] \
                           [--harness-kind KIND] [--probe-write] [--skip-herdr]");
                return 2;
            }
        }
    }
    let Some(ws) = workspace else {
        eprintln!("ERROR: the following arguments are required: --workspace");
        return 2;
    };
    let path = std::path::PathBuf::from(ws);
    match inspect(&path, agent.as_deref(), pane.as_deref(), write_probe, skip_herdr, harness_kind.as_deref()) {
        Ok(result) => {
            println!("{}", util::json_pretty(&result));
            if result["ready_for_role_handshake"] == Value::Bool(true) { 0 } else { 2 }
        }
        Err(message) => {
            let code = if message.starts_with("--") {
                "WORKSPACE_INVALID".to_string()
            } else {
                probe::classify_failure(&message, "workspace_input", SpawnFailure::Other)
            };
            let out = json!({
                "ready_for_role_handshake": false,
                "platform": util::platform_name(),
                "approval_capability": {"status": "unverified"},
                "blockers": [code],
                "blocker_details": [{"code": code.clone(), "boundary": "workspace_input",
                    "target_category": "assigned workspace", "error": probe::sanitize_error(&message)}],
            });
            println!("{}", util::json_pretty(&out));
            2
        }
    }
}
