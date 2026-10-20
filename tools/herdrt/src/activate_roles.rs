//! Port of tools/activate_roles.py: explicitly send existing Herdr agents their
//! assigned role prompts, recording durable receipts in the Markdown run ledger
//! (SESSION.md). No agent launch, no Git/worktree manipulation.
//!
//! SQLite-active runs are refused with a plain cutover message; the Python tool
//! remains authoritative for those until herdr_ledger is ported (phase 1).

use crate::probe::{classify_failure, sanitize_error, SpawnFailure};
use crate::util::{self, ExecOutcome, ToolResult};
use regex::Regex;
use serde_json::{json, Map, Value};
use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};

const RECEIPTS_START: &str = "<!-- HERDR_ACTIVATION_RECEIPTS_START -->";
const RECEIPTS_END: &str = "<!-- HERDR_ACTIVATION_RECEIPTS_END -->";
const ALLOWED: [&str; 6] = ["manager", "delegator", "lead", "worker", "verifier", "critic"];
const SQLITE_MSG: &str = "SQLITE_BACKEND_UNSUPPORTED_BY_HERDRT: use tools/activate_roles.py for SQLite-active runs until the ledger cutover";

struct Args {
    run_dir: String,
    roles: String,
    dry_run: bool,
}

fn arg_error(msg: &str) -> i32 {
    eprintln!("usage: activate_roles.py [-h] --run-dir RUN_DIR [--roles ROLES] [--dry-run]");
    eprintln!("activate_roles.py: error: {msg}");
    2
}

fn parse_args(argv: &[String]) -> Result<Args, String> {
    let mut run_dir = None;
    let mut roles = String::from("manager,delegator,lead");
    let mut dry_run = false;
    let mut i = 0;
    while i < argv.len() {
        let arg = argv[i].as_str();
        let (flag, inline) = match arg.split_once('=') {
            Some((f, v)) if f.starts_with("--") => (f, Some(v)),
            _ => (arg, None),
        };
        match flag {
            "--dry-run" => dry_run = true,
            "-h" | "--help" => {
                println!("Send existing Herdr agents their assigned role prompts (see skills/herdr-role-pack docs).");
                println!("usage: activate_roles.py [-h] --run-dir RUN_DIR [--roles ROLES] [--dry-run]");
                return Err(String::from("\0help")); // sentinel handled in cli()
            }
            "--run-dir" | "--roles" => {
                let value = match inline {
                    Some(v) => v.to_string(),
                    None => {
                        i += 1;
                        argv.get(i).cloned().ok_or_else(|| {
                            format!("argument {flag}: expected one value")
                        })?
                    }
                };
                if flag == "--run-dir" { run_dir = Some(value); } else { roles = value; }
            }
            _ => return Err(format!("unrecognized arguments: {}", arg.trim_start_matches('-'))),
        }
        i += 1;
    }
    let run_dir = run_dir.ok_or("the following arguments are required: --run-dir")?;
    Ok(Args { run_dir, roles, dry_run })
}

pub fn cli(argv: &[String]) -> i32 {
    let args = match parse_args(argv) {
        Ok(a) => a,
        Err(msg) if msg == "\0help" => return 0,
        Err(msg) => return arg_error(&msg),
    };
    match run(&args) {
        Ok(code) => code,
        Err(msg) => {
            eprintln!("ERROR {}", sanitize_error(&msg));
            2
        }
    }
}

// ---------------------------------------------------------------- tiny shims

/// Python bool() over JSON values (used for receipt/manifest truthiness).
fn py_truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// Python str() of a JSON scalar for printf-style interpolation.
fn py_str(v: &Value) -> String {
    match v {
        Value::Null => "None".into(),
        Value::Bool(b) => if *b { "True".into() } else { "False".into() },
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn req<'a>(v: &'a Value, key: &str) -> ToolResult<&'a Value> {
    v.get(key).ok_or_else(|| format!("'{key}'"))
}

fn io_py_err(e: &std::io::Error, path: &Path) -> String {
    // FileNotFoundError.__str__ renders "[Errno 2] No such file or directory: 'path'".
    match e.raw_os_error() {
        Some(code) => {
            let text = e.to_string();
            let msg = text.trim_end_matches(&format!("(os error {code})")).trim_end();
            format!("[Errno {code}] {msg}: '{}'", path.display())
        }
        None => e.to_string(),
    }
}

/// Same rendering for message-shaped errors (util::read_text returns String errors).
fn io_py_err_msg(msg: &str, path: &Path) -> String {
    let t = msg.trim();
    if let Some(i) = t.find("(os error") {
        let code: Option<i64> = t[i + 9..].trim()
            .trim_start_matches('=')
            .trim_end_matches(')')
            .trim().parse().ok();
        if let Some(code) = code {
            let text = t[..i].trim_end();
            return format!("[Errno {code}] {text}: '{}'", path.display());
        }
    }
    t.to_string()
}

/// io::Error Display appends " (os error N)"; Python's OSError.strerror omits it.
fn spawn_strerror(raw: &str) -> String {
    let t = raw.trim();
    match t.find("(os error") {
        Some(i) => t[..i].trim_end().to_string(),
        None => t.to_string(),
    }
}

fn int_field(v: &Value, key: &str, default: i64) -> ToolResult<i64> {
    match v.get(key) {
        None | Some(Value::Null) => Ok(default), // Python .get(k, default); int(None) would differ but never occurs
        Some(Value::Number(n)) => Ok(n.as_i64().unwrap_or(n.as_f64().map(|f| f as i64).unwrap_or(default))),
        Some(Value::String(s)) => s.trim()
            .parse::<i64>()
            .map_err(|_| format!("invalid literal for int() with base 10: '{s}'")),
        Some(_) => Err("int() argument must be a string, a bytes-like object or a real number, not 'dict'".into()),
    }
}

// ------------------------------------------------------- Python-style JSON

fn py_json_str(s: &str, out: &mut String) {
    // json.dumps(ensure_ascii=True): control chars plus every non-ASCII char escaped.
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                out.push_str(&format!("\\u{:04x}", c as u32))
            }
            c if (c as u32) > 0xFFFF => {
                let v = c as u32 - 0x1_0000;
                out.push_str(&format!("\\u{:04x}\\u{:04x}", 0xD800 + (v >> 10), 0xDC00 + (v & 0x3FF)));
            }
            c if (c as u32) > 0x7F => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

fn py_dumps(v: &Value, out: &mut String, depth: usize, indent2: bool) {
    fn indent(out: &mut String, depth: usize) {
        for _ in 0..depth { out.push_str("  "); }
    }
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => out.push_str(&n.to_string()),
        Value::String(s) => py_json_str(s, out),
        Value::Array(items) => {
            if items.is_empty() { out.push_str("[]"); return; }
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 { out.push_str(","); }
                if indent2 { out.push('\n'); indent(out, depth + 1); }
                py_dumps(item, out, depth + 1, indent2);
            }
            if indent2 { out.push('\n'); indent(out, depth); }
            out.push(']');
        }
        Value::Object(map) => {
            if map.is_empty() { out.push_str("{}"); return; }
            out.push('{');
            for (i, (k, item)) in map.iter().enumerate() {
                if i > 0 { out.push_str(","); }
                if indent2 { out.push('\n'); indent(out, depth + 1); }
                py_json_str(k, out);
                out.push_str(if indent2 { ": " } else { ":" });
                py_dumps(item, out, depth + 1, indent2);
            }
            if indent2 { out.push('\n'); indent(out, depth); }
            out.push('}');
        }
    }
}

/// json.dumps(v, indent=2, sort_keys=True): nested dicts sorted too.
fn sorted_json(v: &Value) -> Value {
    match v {
        Value::Object(m) => {
            let mut keys: Vec<&String> = m.keys().collect();
            keys.sort();
            let mut out = Map::new();
            for k in keys { out.insert(k.clone(), sorted_json(&m[k])); }
            Value::Object(out)
        }
        Value::Array(a) => Value::Array(a.iter().map(sorted_json).collect()),
        other => other.clone(),
    }
}

fn dumps_sorted_indent2(v: &Value) -> String {
    let mut out = String::new();
    py_dumps(&sorted_json(v), &mut out, 0, true);
    out
}

fn dumps_sorted_compact(v: &Value) -> String {
    let mut out = String::new();
    py_dumps(&sorted_json(v), &mut out, 0, false);
    out
}

// ------------------------------------------------------------ receipt store

type ReceiptsRead = (String, Vec<Value>, Option<(usize, usize)>);

fn read_receipts(session_path: &Path) -> ToolResult<ReceiptsRead> {
    let meta = std::fs::symlink_metadata(session_path)
        .map_err(|e| io_py_err(&e, session_path))?;
    if meta.file_type().is_symlink() || !meta.is_file() {
        return Err("Markdown run ledger is missing or symlinked; activation refused".into());
    }
    let bytes = std::fs::read(session_path).map_err(|e| io_py_err(&e, session_path))?;
    // Python read_text uses universal newlines: CRLF/CR all become LF in memory.
    let decoded = String::from_utf8(bytes)
        .map_err(|e| utf8_py_err(&e, session_path))?
        .replace("\r\n", "\n")
        .replace('\r', "\n");
    let text = decoded;
    let start = text.find(RECEIPTS_START);
    let end = text.find(RECEIPTS_END);
    if start.is_none() && end.is_none() {
        return Ok((text, vec![], None));
    }
    let start = start.ok_or("Activation receipt section is malformed; inspect SESSION.md before continuing")?;
    let end = end.ok_or("Activation receipt section is malformed; inspect SESSION.md before continuing")?;
    if end < start {
        return Err("Activation receipt section is malformed; inspect SESSION.md before continuing".into());
    }
    let body = text[start + RECEIPTS_START.len()..end].trim();
    let fence = Regex::new(r"(?s)\A```json\s*(.*?)\s*```\z").unwrap();
    let caps = fence.captures(body)
        .ok_or("Activation receipt JSON is malformed; inspect SESSION.md before continuing")?;
    let receipts: Value = serde_json::from_str(caps.get(1).map(|m| m.as_str()).unwrap_or(""))
        .map_err(|_| "Activation receipt JSON is malformed; inspect SESSION.md before continuing".to_string())?;
    let items = match receipts {
        Value::Array(items) => items,
        _ => return Err("Activation receipt JSON must be a list of operation records".into()),
    };
    if !items.is_empty() {
        let bad = items.iter().any(|item| !item.is_object()
            || !item.get("operation_id").map(py_truthy).unwrap_or(false));
        if bad {
            return Err("Activation receipt JSON must be a list of operation records".into());
        }
    }
    let ids: Vec<&Value> = items.iter()
        .map(|item| item.get("operation_id").unwrap())
        .collect();
    if ids.iter().enumerate().any(|(i, id)| ids[..i].contains(id)) {
        return Err("Duplicate activation operation IDs in SESSION.md; reconcile before continuing".into());
    }
    Ok((text, items, Some((start, end))))
}

fn utf8_py_err(e: &std::string::FromUtf8Error, path: &Path) -> String {
    format!("'utf-8' codec can't decode byte in position {}: {}",
            e.utf8_error().valid_up_to(), path.display())
}

fn receipts_block(receipts: &[Value]) -> String {
    let dumped = dumps_sorted_indent2(&Value::Array(receipts.to_vec()));
    [RECEIPTS_START, "```json", dumped.as_str(), "```", RECEIPTS_END].join("\n")
}

fn write_receipts(session_path: &Path, receipts: &[Value]) -> ToolResult<()> {
    let (text, _, bounds) = read_receipts(session_path)?;
    let raw = std::fs::read(session_path).map_err(|e| io_py_err(&e, session_path))?;
    let decoded = String::from_utf8_lossy(&raw);
    let crlf = decoded.contains("\r\n");
    let newline = if crlf { "\r\n" } else { "\n" };
    let block = receipts_block(receipts);
    let updated = match bounds {
        Some((start, end)) => {
            let mut out = String::new();
            out.push_str(&text[..start]);
            out.push_str(&block);
            out.push_str(&text[end + RECEIPTS_END.len()..]);
            out
        }
        None => {
            let mut out = text.trim_end().to_string();
            out.push_str(newline);
            out.push_str(newline);
            out.push_str(&block);
            out.push_str(newline);
            out
        }
    };
    let updated = if crlf { updated.replace('\n', "\r\n") } else { updated };

    // Atomic replace: temp file in the session dir, same file mode, os.replace.
    let dir: &Path = session_path.parent().unwrap_or(Path::new("."));
    #[cfg(unix)]
    let mode = {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(session_path).map_err(|e| io_py_err(&e, session_path))?
            .permissions().mode() & 0o777
    };
    let mut temp = tempfile::Builder::new()
        .prefix(".SESSION.activation.")
        .suffix(".tmp")
        .tempfile_in(dir)
        .map_err(|e| e.to_string())?;
    temp.write_all(updated.as_bytes()).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        temp.as_file().set_permissions(std::fs::Permissions::from_mode(mode))
            .map_err(|e| e.to_string())?;
    }
    temp.persist(session_path).map_err(|e| e.error.to_string())?;
    Ok(())
}

fn store_receipt(session_path: &Path, receipt: &Value) -> ToolResult<()> {
    let (_, mut receipts, _) = read_receipts(session_path)?;
    let op = receipt.get("operation_id").cloned().unwrap_or(Value::Null);
    let position = receipts.iter().position(|prev| prev.get("operation_id") == Some(&op));
    match position {
        Some(index) => {
            let previous = &receipts[index];
            let mut history = previous.get("history").and_then(Value::as_array)
                .cloned().unwrap_or_default();
            history.push(json!({
                "status": previous.get("status").cloned().unwrap_or(json!("UNKNOWN")),
                "evidence": previous.get("evidence").cloned().unwrap_or(json!("")),
            }));
            let mut updated = receipt.clone();
            if let Some(obj) = updated.as_object_mut() {
                obj.insert("history".into(), Value::Array(history));
            }
            receipts[index] = updated;
        }
        None => receipts.push(receipt.clone()),
    }
    write_receipts(session_path, &receipts)
}

/// Map key for a receipt's operation_id (json values are not Ord/Hashable here).
fn op_key(v: &Value) -> String {
    match v { Value::String(s) => s.clone(), other => other.to_string() }
}

// --------------------------------------------------------- operation hashes

fn operation(entry: &Value, run_id: &str) -> ToolResult<(String, String)> {
    let prompt = req(entry, "prompt")?.as_str().unwrap_or("");
    let prompt_hash = util::sha256_hex(prompt.as_bytes());
    let identity = json!({
        "run_id": run_id,
        "task_id": entry.get("task_id").cloned().unwrap_or(json!("")),
        "attempt_id": entry.get("attempt_id").cloned().unwrap_or(json!("")),
        "role": req(entry, "role")?.clone(),
        "agent": req(entry, "name")?.clone(),
        "prompt_sha256": prompt_hash,
    });
    let operation_id = util::sha256_hex(dumps_sorted_compact(&identity).as_bytes())[..24].to_string();
    Ok((operation_id, prompt_hash))
}

fn make_receipt(entry: &Value, run_id: &str, operation_id: &str, prompt_hash: &str,
                status: &str, attempts: i64, retry_count: i64, evidence: &str) -> Value {
    let name = req(entry, "name").map(|v| v.clone()).unwrap_or(Value::Null);
    let mut m = Map::new();
    m.insert("operation_id".into(), json!(operation_id));
    m.insert("run_id".into(), json!(run_id));
    m.insert("backend_epoch".into(), entry.get("backend_epoch").cloned().unwrap_or(json!(1)));
    m.insert("task_id".into(), entry.get("task_id").cloned().unwrap_or(json!("")));
    m.insert("attempt_id".into(), entry.get("attempt_id").cloned().unwrap_or(json!("")));
    m.insert("role".into(), entry.get("role").cloned().unwrap_or(Value::Null));
    m.insert("agent".into(), name.clone());
    m.insert("action".into(), json!("agent_prompt"));
    m.insert("target".into(), name);
    m.insert("prompt_sha256".into(), json!(prompt_hash));
    m.insert("status".into(), json!(status));
    m.insert("attempts".into(), json!(attempts));
    m.insert("retry_count".into(), json!(retry_count));
    m.insert("evidence".into(), json!(evidence));
    Value::Object(m)
}

fn dispatch_state(entry: &Value, run_id: &str,
                  by_id: &std::collections::HashMap<String, Value>) -> ToolResult<(String, Value, String)> {
    let (operation_id, prompt_hash) = operation(entry, run_id)?;
    let Some(previous) = by_id.get(&operation_id) else {
        let receipt = make_receipt(entry, run_id, &operation_id, &prompt_hash,
                                   "SUBMITTING", 1, 0, "Intent recorded before one prompt submission.");
        return Ok(("send".into(), receipt, String::new()));
    };
    let status = previous.get("status").and_then(Value::as_str).unwrap_or("");
    match status {
        "CONFIRMED_APPLIED" => Ok(("skip".into(), previous.clone(), String::new())),
        "CONFIRMED_NOT_APPLIED" => {
            let attempts = int_field(previous, "attempts", 1)?;
            let retry_count = int_field(previous, "retry_count", 0)?;
            if attempts >= 2 || retry_count >= 1 {
                return Ok(("blocked".into(), previous.clone(),
                           "one authorized retry has already been used".into()));
            }
            let receipt = make_receipt(entry, run_id, &operation_id, &prompt_hash,
                                       "SUBMITTING", attempts + 1, retry_count + 1,
                                       "One retry authorized after recorded read-only reconciliation.");
            Ok(("send".into(), receipt, String::new()))
        }
        "SUBMITTING" | "SUBMITTED" => {
            let mut uncertain = previous.clone();
            if let Some(obj) = uncertain.as_object_mut() {
                obj.insert("status".into(), json!("OUTCOME_UNCERTAIN"));
                obj.insert("evidence".into(),
                           json!("Submission may have reached the named agent; correlate its task receipt before replay."));
            }
            Ok(("blocked".into(), uncertain,
                "prompt submission is not proof of receipt; reconcile before retry".into()))
        }
        "OUTCOME_UNCERTAIN" => Ok(("blocked".into(), previous.clone(),
            "reconcile the same agent, task and artifact before retry".into())),
        "DENIED" => Ok(("blocked".into(), previous.clone(),
            "this operation was denied; change authorization or environment before any new operation".into())),
        _ => Ok(("blocked".into(), previous.clone(),
                "unknown receipt state; inspect SESSION.md before continuing".into())),
    }
}

fn preflight_probe(outcome: &ExecOutcome) -> (bool, String) {
    if outcome.timed_out {
        let msg = if !outcome.stderr_text().is_empty() { outcome.stderr_text() }
            else if !outcome.stdout_text().is_empty() { outcome.stdout_text() }
            else { "TimeoutExpired".to_string() };
        (false, msg)
    } else if outcome.exit.is_none() {
        let raw = outcome.stderr_text();
        let msg = if raw.is_empty() { "OSError".to_string() } else { spawn_strerror(&raw) };
        (false, msg)
    } else {
        let msg = if !outcome.stderr_text().is_empty() { outcome.stderr_text() }
            else { outcome.stdout_text() };
        (outcome.exit == Some(0), msg)
    }
}

// -------------------------------------------------------------------- main

fn expanduser(path: &str) -> String {
    match path.strip_prefix('~') {
        Some(rest) if rest.is_empty() || rest.starts_with('/') => {
            match std::env::var("HOME") {
                Ok(home) => format!("{home}{rest}"),
                Err(_) => path.to_string(),
            }
        }
        _ => path.to_string(),
    }
}

fn read_manifest(run_dir: &Path) -> ToolResult<Value> {
    let manifest_path = run_dir.join("ACTIVATION.json");
    let text = util::read_text(&manifest_path)
        .map_err(|e| io_py_err_msg(&e, &manifest_path))?;
    serde_json::from_str(&text).map_err(|e| e.to_string())
}

fn run(args: &Args) -> ToolResult<i32> {
    let expanded = expanduser(&args.run_dir);
    let run_dir: PathBuf = Path::new(&expanded).canonicalize()
        .map(|c| util::strip_verbatim(Path::new(&expanded), c))
        .map_err(|e| io_py_err(&e, Path::new(&expanded)))?;

    let check_generated_by = |manifest: &Value| -> ToolResult<()> {
        if manifest.get("generated_by").and_then(Value::as_str) != Some("herdr-role-pack") {
            return Err("Activation manifest was not created by Herdr Role Pack".into());
        }
        Ok(())
    };

    let mut manifest = read_manifest(&run_dir)?;
    check_generated_by(&manifest)?;
    if manifest.get("execution_mode").and_then(Value::as_str).unwrap_or("governed") == "standalone" {
        return Err("Standalone assignment is delivered directly from ACTIVATE.md; managed activation requires a governed run and ledger".into());
    }

    let _held_lock;
    if !args.dry_run {
        let lock = crate::backend::RunLock::acquire(&run_dir)?;
        _held_lock = Some(lock);
        manifest = read_manifest(&run_dir)?;
        check_generated_by(&manifest)?;
    }

    let (backend, selector) = match crate::backend::get_active_backend(&run_dir, false) {
        Ok(pair) => pair,
        Err(e) if e.contains("SQLITE_BACKEND_UNSUPPORTED_BY_HERDRT") => return Err(SQLITE_MSG.into()),
        Err(e) => return Err(e),
    };
    if backend.as_deref() == Some("sqlite") {
        return Err(SQLITE_MSG.into());
    }
    let state_backend = manifest.get("state_backend").cloned().unwrap_or_else(|| json!("markdown"));
    let expected_epoch = selector.as_ref()
        .and_then(|s| s.get("epoch")).cloned().unwrap_or_else(|| json!(1));
    let manifest_epoch = manifest.get("backend_epoch").cloned().unwrap_or_else(|| json!(1));
    if state_backend != backend.as_deref().map(|b| json!(b)).unwrap_or(Value::Null)
        || manifest_epoch != expected_epoch {
        return Err("BACKEND_INTEGRITY_BLOCKED: activation packet does not match selected backend and epoch".into());
    }

    let requested: HashSet<String> = if args.roles == "all" {
        ALLOWED.iter().map(|s| s.to_string()).collect()
    } else {
        args.roles.split(',').map(|s| s.to_string()).collect()
    };
    if requested.is_empty() || !requested.iter().all(|r| ALLOWED.contains(&r.as_str())) {
        return Err("--roles must contain only manager,delegator,lead,worker,verifier,critic or all".into());
    }

    let agents = match manifest.get("agents") {
        Some(Value::Array(a)) => a.clone(),
        _ => return Err("'agents'".into()),
    };
    let mut targets: Vec<Value> = Vec::new();
    for entry in &agents {
        match entry.get("role") {
            None => return Err("'role'".into()),
            Some(Value::String(role)) if requested.contains(role) => targets.push(entry.clone()),
            Some(_) => {}
        }
    }
    if targets.is_empty() {
        return Err("No agents match the requested roles".into());
    }
    let names: Vec<&Value> = targets.iter().map(|t| req(t, "name").unwrap_or(&Value::Null))
        .collect();
    if names.iter().enumerate().any(|(i, n)| names[..i].contains(n)) {
        return Err("Duplicate agent name in activation manifest".into());
    }
    for entry in targets.iter_mut() {
        if let Some(obj) = entry.as_object_mut() {
            obj.insert("backend_epoch".into(), manifest.get("backend_epoch").cloned().unwrap_or_else(|| json!(1)));
            obj.insert("state_backend".into(), backend.as_deref().map(|b| json!(b)).unwrap_or(Value::Null));
        }
        if entry.get("role").and_then(Value::as_str) == Some("worker") {
            let name = py_str(req(entry, "name")?);
            let worktree_value = entry.get("worktree").cloned().unwrap_or(json!(""));
            if !py_truthy(&worktree_value) {
                return Err(format!("Worker {name} has no assigned workspace; Delegator must allocate one"));
            }
            let wt_text = req(entry, "worktree")?.as_str().unwrap_or("");
            let expanded_wt = expanduser(wt_text);
            let worktree = Path::new(&expanded_wt).canonicalize()
                .map(|c| util::strip_verbatim(Path::new(&expanded_wt), c))
                .map_err(|e| io_py_err(&e, Path::new(&expanded_wt)))?;
            for file in [worktree.join(".herdr-loop").join("ROLE.md"),
                         worktree.join(".herdr-loop").join("TASK.md")] {
                let is_file = std::fs::metadata(&file).map(|m| m.is_file()).unwrap_or(false);
                if !is_file {
                    return Err(format!("Worker packet/brief not copied: {}", file.display()));
                }
            }
        }
        match entry.get("role").and_then(Value::as_str) {
            Some("critic") => println!("NOTICE critic activation does not grant CRITIQUE_START or authority to edit/test"),
            Some("verifier") => println!("NOTICE verifier activation does not grant a test lock or approve any commands"),
            _ => {}
        }
    }

    if args.dry_run {
        for entry in &targets {
            let (op_id, _) = operation(entry, req(&manifest, "run_id")?.as_str().unwrap_or(""))?;
            println!("DRY RUN {} ({}, {}) operation_id={op_id}: {}",
                     py_str(req(entry, "name")?), py_str(req(entry, "role")?),
                     py_str(req(entry, "kind")?), py_str(req(entry, "prompt")?));
        }
        return Ok(0);
    }

    if std::env::var("HERDR_ENV").ok().as_deref() != Some("1") {
        return Err("HERDR_ENV=1 required; run from an existing Herdr-managed pane".into());
    }

    let session_path = run_dir.join("SESSION.md");
    let (session_text, receipts, _) = read_receipts(&session_path)?;
    let receipts_empty = receipts.is_empty();
    if receipts_empty && !session_text.contains(RECEIPTS_START) {
        // Older Markdown runs get the receipt section on their first attempted submission.
        write_receipts(&session_path, &[])?;
    }
    let herdr = util::which("herdr")
        .ok_or_else(|| "HERDR_CLI_UNAVAILABLE: herdr command not found".to_string())?;
    let herdr = herdr.to_string_lossy().into_owned();

    let mut preflight: Vec<(String, bool, String)> = Vec::new();
    for entry in &targets {
        let name = req(entry, "name")?.as_str().unwrap_or("").to_string();
        let outcome = util::exec(&[herdr.as_str(), "agent", "get", name.as_str()], 15);
        let (ok, message) = preflight_probe(&outcome);
        preflight.push((name, ok, sanitize_error(&message)));
    }
    if preflight.iter().any(|(_, ok, _)| !ok) {
        for (name, ok, error) in &preflight {
            println!("PREFLIGHT {} {} {}", name, if *ok { "CONFIRMED" } else { "BLOCKED" }, error);
        }
        eprintln!("ERROR no role prompts were sent; all selected agents must pass read-only preflight");
        return Ok(2);
    }

    let mut by_id: std::collections::HashMap<String, Value> = std::collections::HashMap::new();
    for receipt in &receipts {
        if let Some(op) = receipt.get("operation_id") {
            by_id.insert(op_key(op), receipt.clone());
        }
    }
    let mut dispatch: Vec<(Value, Value)> = Vec::new();
    let mut blocked = false;
    for entry in &targets {
        let (state, receipt, reason) = dispatch_state(entry, req(&manifest, "run_id")?.as_str().unwrap_or(""), &by_id)?;
        if state == "blocked" {
            let uncertain = receipt.get("status").and_then(Value::as_str) == Some("OUTCOME_UNCERTAIN")
                && by_id.get(req(&receipt, "operation_id")?.as_str().unwrap_or(""))
                    .and_then(|p| p.get("status")).and_then(Value::as_str) != Some("OUTCOME_UNCERTAIN");
            if uncertain {
                store_receipt(&session_path, &receipt)?;
                if let Some(op) = receipt.get("operation_id") {
                    by_id.insert(op_key(op), receipt.clone());
                }
            }
            println!("BLOCKED {} {} operation_id={} {}",
                     py_str(req(entry, "name")?),
                     receipt.get("status").map(py_str).unwrap_or_else(|| "None".into()),
                     py_str(req(&receipt, "operation_id")?), reason);
            blocked = true;
        } else if state == "skip" {
            println!("CONFIRMED_APPLIED {} operation_id={}",
                     py_str(req(entry, "name")?), py_str(req(&receipt, "operation_id")?));
        } else {
            dispatch.push((entry.clone(), receipt));
        }
    }
    if blocked {
        eprintln!("ERROR no new prompts sent; reconcile blocked operation(s) in the selected ledger");
        return Ok(2);
    }

    for (entry, receipt) in &dispatch {
        store_receipt(&session_path, receipt)?;
        let name = req(entry, "name")?.as_str().unwrap_or("").to_string();
        let op = req(receipt, "operation_id")?.as_str().unwrap_or("").to_string();
        let prompt = req(entry, "prompt")?.as_str().unwrap_or("").to_string();
        let outcome = util::exec(&[herdr.as_str(), "agent", "prompt", name.as_str(), prompt.as_str()], 30);
        if outcome.timed_out {
            let raw = if !outcome.stderr_text().is_empty() { outcome.stderr_text() }
                else { outcome.stdout_text() };
            let mut updated = receipt.clone();
            let evidence = format!("Herdr prompt call timed out; delivery may have occurred. {}",
                                   sanitize_error(&raw));
            if let Some(obj) = updated.as_object_mut() {
                obj.insert("status".into(), json!("OUTCOME_UNCERTAIN"));
                obj.insert("evidence".into(), json!(evidence));
            }
            store_receipt(&session_path, &updated)?;
            println!("OUTCOME_UNCERTAIN {name} operation_id={op} {evidence}");
            break;
        }
        if outcome.exit.is_none() {
            let raw = outcome.stderr_text();
            let msg = if raw.is_empty() { "OSError".to_string() } else { spawn_strerror(&raw) };
            let mut updated = receipt.clone();
            let evidence = format!("Herdr process could not be started: {}", sanitize_error(&msg));
            if let Some(obj) = updated.as_object_mut() {
                obj.insert("status".into(), json!("CONFIRMED_NOT_APPLIED"));
                obj.insert("evidence".into(), json!(evidence));
            }
            store_receipt(&session_path, &updated)?;
            println!("CONFIRMED_NOT_APPLIED {name} operation_id={op} {evidence}");
            break;
        }
        if outcome.exit == Some(0) {
            let mut updated = receipt.clone();
            if let Some(obj) = updated.as_object_mut() {
                obj.insert("status".into(), json!("SUBMITTED"));
                obj.insert("evidence".into(),
                           json!("Herdr returned 0; submission is not proof of recipient receipt."));
            }
            store_receipt(&session_path, &updated)?;
            println!("SUBMITTED {name} (prompt submission is not proof of receipt) operation_id={op}");
            continue;
        }
        let raw_snd = if !outcome.stderr_text().is_empty() { outcome.stderr_text() }
            else { outcome.stdout_text() };
        let error = sanitize_error(&raw_snd);
        let failure = classify_failure(&error, "agent_prompt", SpawnFailure::Other);
        let status = if failure == "DENIED" { "DENIED" } else { "OUTCOME_UNCERTAIN" };
        let evidence = if error.is_empty() {
            format!("{failure}; Herdr returned {}", outcome.exit.unwrap_or(1))
        } else {
            format!("{failure}: {error}")
        };
        let mut updated = receipt.clone();
        if let Some(obj) = updated.as_object_mut() {
            obj.insert("status".into(), json!(status));
            obj.insert("evidence".into(), json!(evidence));
        }
        store_receipt(&session_path, &updated)?;
        println!("{status} {name} operation_id={op} {evidence}");
        break;
    }

    let dispatch_ids: Vec<Value> = dispatch.iter()
        .map(|(_, r)| r.get("operation_id").cloned().unwrap_or(Value::Null))
        .collect();
    let (_, final_receipts, _) = read_receipts(&session_path)?;
    let all_ok = final_receipts.iter()
        .filter(|item| item.get("operation_id")
            .map(|op| dispatch_ids.iter().any(|d| d == op)).unwrap_or(false))
        .all(|item| {
            matches!(item.get("status").and_then(Value::as_str),
                     Some("SUBMITTED") | Some("CONFIRMED_APPLIED"))
        });
    Ok(if all_ok { 0 } else { 2 })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tempdir() -> PathBuf {
        let d = std::env::temp_dir().join(format!("herdrt-ar-{}-{:?}",
            std::process::id(), std::time::SystemTime::now()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn session(dir: &Path) -> PathBuf { dir.join("SESSION.md") }

    fn entry() -> Value {
        serde_json::json!({"name": "architect", "role": "manager", "kind": "codex",
                           "task_id": "", "attempt_id": "", "prompt": "hello prompt"})
    }

    fn by_id_of(receipts: &[Value]) -> std::collections::HashMap<String, Value> {
        receipts.iter()
            .map(|r| (op_key(&r["operation_id"]), r.clone()))
            .collect()
    }

    #[test]
    fn read_receipts_no_markers_malformed_and_bad_shapes() {
        let dir = tempdir();
        let path = session(&dir);
        std::fs::write(&path, "prose\n").unwrap();
        let (text, receipts, bounds) = read_receipts(&path).unwrap();
        assert_eq!(text, "prose\n");
        assert!(receipts.is_empty() && bounds.is_none());

        std::fs::write(&path, "x\n<!-- HERDR_ACTIVATION_RECEIPTS_START -->\n").unwrap();
        assert_eq!(read_receipts(&path).unwrap_err(),
                   "Activation receipt section is malformed; inspect SESSION.md before continuing");

        std::fs::write(&path,
            format!("<!-- HERDR_ACTIVATION_RECEIPTS_START -->\n```json\n{{bad}}\n```\n<!-- HERDR_ACTIVATION_RECEIPTS_END -->\n")).unwrap();
        assert_eq!(read_receipts(&path).unwrap_err(),
                   "Activation receipt JSON is malformed; inspect SESSION.md before continuing");

        std::fs::write(&path,
            format!("<!-- HERDR_ACTIVATION_RECEIPTS_START -->\n```json\n[1]\n```\n<!-- HERDR_ACTIVATION_RECEIPTS_END -->\n")).unwrap();
        assert_eq!(read_receipts(&path).unwrap_err(),
                   "Activation receipt JSON must be a list of operation records");

        let good = |a: &str, b: &str| format!(
            "<!-- HERDR_ACTIVATION_RECEIPTS_START -->\n```json\n[{{\"operation_id\":\"{a}\"}},{{\"operation_id\":\"{b}\"}}]\n```\n<!-- HERDR_ACTIVATION_RECEIPTS_END -->\n");
        std::fs::write(&path, format!(
            "<!-- HERDR_ACTIVATION_RECEIPTS_START -->\n```json\n[{{\"operation_id\":\"dup\"}},{{\"operation_id\":\"dup\"}}]\n```\n<!-- HERDR_ACTIVATION_RECEIPTS_END -->\n")).unwrap();
        assert_eq!(read_receipts(&path).unwrap_err(),
                   "Duplicate activation operation IDs in SESSION.md; reconcile before continuing");
        let _ = good("", "");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn operation_id_is_stable_against_sha256_of_sorted_identity() {
        let (op, prompt_hash) = operation(&entry(), "r1").unwrap();
        assert_eq!(prompt_hash,
                   "a94eb709fb27abb1097000cbd3a43d5ba95444dcc70a5c670f3a2a8c4808e58c");
        assert_eq!(op, "bf02b8479ea9dae6220f6c86");
        assert_eq!(op.len(), 24);
    }

    #[test]
    fn dispatch_state_transitions_match_python() {
        let (op, _) = operation(&entry(), "r1").unwrap();
        let run = "r1";
        let by = &std::collections::HashMap::new();

        // fresh -> send, SUBMITTING with intent evidence
        let (state, receipt, reason) = dispatch_state(&entry(), run, by).unwrap();
        assert_eq!((state.as_str(), reason.as_str()), ("send", ""));
        assert_eq!(receipt["status"], "SUBMITTING");
        assert_eq!(receipt["evidence"], "Intent recorded before one prompt submission.");
        assert_eq!(receipt["action"], "agent_prompt");
        assert_eq!(receipt["attempts"], 1);

        // CONFIRMED_APPLIED -> skip with the previous receipt verbatim
        let applied = serde_json::json!({"operation_id": op, "status": "CONFIRMED_APPLIED", "attempts": 1});
        let (state, receipt, reason) = dispatch_state(&entry(), run, &by_id_of(&[applied])).unwrap();
        assert_eq!(state, "skip");
        assert_eq!(receipt["status"], "CONFIRMED_APPLIED");
        assert!(reason.is_empty());

        // CONFIRMED_NOT_APPLIED once -> authorized retry
        let not_applied = serde_json::json!({"operation_id": op, "status": "CONFIRMED_NOT_APPLIED",
                                             "attempts": 1, "retry_count": 0});
        let (state, receipt, reason) = dispatch_state(&entry(), run, &by_id_of(&[not_applied])).unwrap();
        assert_eq!(state, "send");
        assert!(reason.is_empty());
        assert_eq!(receipt["status"], "SUBMITTING");
        assert_eq!(receipt["attempts"], 2);
        assert_eq!(receipt["retry_count"], 1);
        assert_eq!(receipt["evidence"], "One retry authorized after recorded read-only reconciliation.");

        // ...but the second CONFIRMED_NOT_APPLIED is blocked
        let exhausted = serde_json::json!({"operation_id": op, "status": "CONFIRMED_NOT_APPLIED",
                                           "attempts": 2, "retry_count": 1});
        let (state, _, reason) = dispatch_state(&entry(), run, &by_id_of(&[exhausted])).unwrap();
        assert_eq!(state, "blocked");
        assert_eq!(reason, "one authorized retry has already been used");

        // SUBMITTING/SUBMITTED turn OUTCOME_UNCERTAIN (evidence swap), then any later OBS blocks
        let submitted = serde_json::json!({"operation_id": op, "status": "SUBMITTED", "evidence": "x"});
        let (state, receipt, reason) = dispatch_state(&entry(), run, &by_id_of(&[submitted])).unwrap();
        assert_eq!(state, "blocked");
        assert_eq!(receipt["status"], "OUTCOME_UNCERTAIN");
        assert_eq!(reason, "prompt submission is not proof of receipt; reconcile before retry");
        assert_eq!(receipt["evidence"],
                   "Submission may have reached the named agent; correlate its task receipt before replay.");

        let uncertain = serde_json::json!({"operation_id": op, "status": "OUTCOME_UNCERTAIN"});
        let (state, receipt, reason) = dispatch_state(&entry(), run, &by_id_of(&[uncertain])).unwrap();
        assert_eq!((state.as_str(), receipt["status"].as_str().unwrap()), ("blocked", "OUTCOME_UNCERTAIN"));
        assert_eq!(reason, "reconcile the same agent, task and artifact before retry");

        let denied = serde_json::json!({"operation_id": op, "status": "DENIED"});
        let (_state, _, reason) = dispatch_state(&entry(), run, &by_id_of(&[denied])).unwrap();
        assert_eq!(reason, "this operation was denied; change authorization or environment before any new operation");

        let weird = serde_json::json!({"operation_id": op, "status": "MYSTERY"});
        let (_state, _, reason) = dispatch_state(&entry(), run, &by_id_of(&[weird])).unwrap();
        assert_eq!(reason, "unknown receipt state; inspect SESSION.md before continuing");
    }

    #[test]
    fn receipts_write_roundtrip_preserves_crlf_choice_and_prose() {
        let dir = tempdir();
        let path = session(&dir);
        std::fs::write(&path, "header\r\nkeep\r\n<!-- HERDR_ACTIVATION_RECEIPTS_START -->\n```json\n[]\n```\n<!-- HERDR_ACTIVATION_RECEIPTS_END -->\ntail\n").unwrap();
        let receipt = serde_json::json!({"operation_id": "abc", "role": "manager", "evidence": "caf\u{e9}"});
        write_receipts(&path, &[receipt.clone()]).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.starts_with("header\r\nkeep\r\n"), "prose before section preserved: {text:?}");
        assert!(text.replace("\r\n", "").find('\n').is_none(),
                "CRLF choice must be applied everywhere: {text:?}");
        assert!(text.contains("<!-- HERDR_ACTIVATION_RECEIPTS_END -->\r\n"));
        assert!(text.ends_with("tail\r\n"), "prose after section preserved: {text:?}");
        let (_, receipts, _) = read_receipts(&path).unwrap();
        assert_eq!(receipts.len(), 1);
        assert_eq!(receipts[0]["operation_id"], Value::String("abc".into()));
        assert_eq!(receipts[0]["evidence"], Value::String("caf\u{e9}".into()));

        // LF file stays LF, and seeding appends the empty section directly.
        std::fs::write(&path, "plain prose\n").unwrap();
        write_receipts(&path, &[]).unwrap();
        let text = String::from_utf8(std::fs::read(&path).unwrap()).unwrap();
        assert!(text.starts_with("plain prose\n\n"));
        assert!(text.ends_with("```json\n[]\n```\n<!-- HERDR_ACTIVATION_RECEIPTS_END -->\n"));
        let (_, receipts, _) = read_receipts(&path).unwrap();
        assert!(receipts.is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn store_receipt_appends_then_records_history_on_update() {
        let dir = tempdir();
        let path = session(&dir);
        std::fs::write(&path, "run notes\n").unwrap();
        let first = make_receipt(&entry(), "r1", "op", "hash", "SUBMITTING", 1, 0, "intent");
        store_receipt(&path, &first).unwrap();
        let (_, receipts, _) = read_receipts(&path).unwrap();
        assert_eq!(receipts.len(), 1);
        assert!(receipts[0].get("history").is_none());

        let resolved = serde_json::json!({"operation_id": "op", "status": "SUBMITTED",
                                          "evidence": "ok"});
        store_receipt(&path, &resolved).unwrap();
        let (_, receipts, _) = read_receipts(&path).unwrap();
        assert_eq!(receipts.len(), 1);
        assert_eq!(receipts[0]["status"], "SUBMITTED");
        assert_eq!(receipts[0]["history"][0]["status"], "SUBMITTING");
        assert_eq!(receipts[0]["history"][0]["evidence"], Value::String("intent".into()));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
