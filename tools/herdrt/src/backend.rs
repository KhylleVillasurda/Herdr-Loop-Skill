//! Markdown-side pieces of skills/herdr-engineering-loop/scripts/run_backend.py
//! that tools/role_bind.py, tools/activate_roles.py and tools/progress_view.py import.
//!
//! SQLite-active runs are refused plainly (the Python toolchain keeps operating
//! them until the SQLite cutover): no SQLite engine is embedded here yet.

use crate::util::ToolResult;
use regex::Regex;
use serde_json::{json, Value};
use std::io::Write;
use std::path::{Path, PathBuf};

pub const SELECTOR_FILE: &str = "backend.active.json";
pub const SCHEMA_VERSION: u16 = 3;
const VALID_BACKENDS: [&str; 2] = ["sqlite", "markdown"];
const PENDING_PHASES: [&str; 4] = ["PREPARING", "PREPARED", "SOURCE_RETIRED", "TARGET_ACTIVE"];
const PHASES: [&str; 6] = ["PREPARING", "PREPARED", "SOURCE_RETIRED", "TARGET_ACTIVE", "COMPLETE", "ROLLED_BACK"];

pub fn is_valid_backend(b: &str) -> bool {
    VALID_BACKENDS.contains(&b)
}

fn transfer_id_valid(s: &str) -> bool {
    Regex::new(r"^[0-9a-f]{16}$").unwrap().is_match(s)
}

pub fn read_selector(run_dir: &Path) -> ToolResult<Option<Value>> {
    let path = run_dir.join(SELECTOR_FILE);
    if path.is_symlink() {
        return Err("BACKEND_INTEGRITY_BLOCKED: selector is a symlink".into());
    }
    if !path.exists() {
        return Ok(None);
    }
    let text = crate::util::read_text(&path).map_err(|_| {
        "BACKEND_INTEGRITY_BLOCKED: selector is unreadable or invalid JSON".to_string()
    })?;
    let data: Value = serde_json::from_str(&text)
        .map_err(|_| "BACKEND_INTEGRITY_BLOCKED: selector is unreadable or invalid JSON".to_string())?;
    if !data.is_object() {
        return Err("BACKEND_INTEGRITY_BLOCKED: selector must be an object".into());
    }
    Ok(Some(data))
}

/// Returns non-terminal transfer receipts; malformed entries block the run.
pub fn pending_transfers(run_dir: &Path) -> ToolResult<Vec<Value>> {
    let base = run_dir.join("transfers");
    if base.is_symlink() {
        return Err("BACKEND_INTEGRITY_BLOCKED: transfer directory is a symlink".into());
    }
    if !base.exists() {
        return Ok(vec![]);
    }
    let mut pending = Vec::new();
    for entry in sorted_dirs(&base).map_err(|e| e.to_string())? {
        if !transfer_id_valid(entry.file_name().unwrap().to_string_lossy().as_ref())
            || entry.is_symlink() || !entry.is_dir() {
            return Err("BACKEND_INTEGRITY_BLOCKED: unmanaged entry in transfers/: "
                .to_string() + entry.file_name().unwrap().to_string_lossy().as_ref());
        }
        let receipt = read_receipt(&entry)?;
        if let Some(phase) = receipt.get("phase").and_then(Value::as_str) {
            if PENDING_PHASES.contains(&phase) {
                pending.push(receipt);
            }
        }
    }
    Ok(pending)
}

fn sorted_dirs(base: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut names: Vec<PathBuf> = std::fs::read_dir(base)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .collect();
    names.sort();
    Ok(names)
}

pub fn read_receipt(tdir: &Path) -> ToolResult<Value> {
    let path = tdir.join("receipt.json");
    let text = crate::util::read_text(&path).map_err(|_| {
        "BACKEND_INTEGRITY_BLOCKED: transfer receipt unreadable: ".to_string()
            + tdir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default().as_str()
    })?;
    let receipt: Value = serde_json::from_str(&text).map_err(|_| {
        format!("BACKEND_INTEGRITY_BLOCKED: transfer receipt unreadable: {}",
                tdir.file_name().unwrap().to_string_lossy())
    })?;
    let valid = receipt.is_object()
        && receipt.get("transfer_id").and_then(Value::as_str) == Some(
            tdir.file_name().unwrap().to_string_lossy().as_ref())
        && receipt.get("phase").and_then(Value::as_str)
            .map(|p| PHASES.contains(&p)).unwrap_or(false)
        && receipt.get("schema").and_then(Value::as_i64) == Some(1);
    if !valid {
        return Err(format!("BACKEND_INTEGRITY_BLOCKED: transfer receipt is invalid: {}",
                           tdir.file_name().unwrap().to_string_lossy()));
    }
    Ok(receipt)
}

/// (backend, selector) like run_backend.get_active_backend, markdown-returning only.
/// A run selected for sqlite raises SQLITE_BACKEND_UNSUPPORTED_BY_HERDRT.
pub fn get_active_backend(run_dir: &Path, ignore_transfers: bool) -> ToolResult<(Option<String>, Option<Value>)> {
    if run_dir.is_symlink() {
        return Err("BACKEND_INTEGRITY_BLOCKED: run directory is a symlink".into());
    }
    let selector = read_selector(run_dir)?;
    let session = run_dir.join("SESSION.md");
    let db = run_dir.join("ledger.sqlite");
    if session.is_symlink() || db.is_symlink() {
        return Err("BACKEND_INTEGRITY_BLOCKED: canonical ledger is a symlink".into());
    }
    if !ignore_transfers && !pending_transfers(run_dir)?.is_empty() {
        return Err("BACKEND_TRANSFER_IN_PROGRESS: a backend transfer is unfinished; run run_backend.py recover".into());
    }
    if let Some(sel) = selector {
        let backend = sel.get("selected_backend").and_then(Value::as_str).unwrap_or("");
        // JSON null present under a key behaves like Python's absent/None value here.
        let null_of = |k: &str| match sel.get(k) {
            None | Some(Value::Null) => None,
            Some(v) => Some(v.clone()),
        };
        let receipt_id = null_of("transfer_receipt_id");
        let profile = null_of("planning_profile").unwrap_or(Value::Null);
        let run_id_ok = sel.get("run_id").and_then(Value::as_str)
            == run_dir.file_name().map(|n| n.to_string_lossy().into_owned()).as_deref();
        let epoch_val = sel.get("epoch").cloned().unwrap_or(Value::Null);
        let is_int = epoch_val.is_i64() || epoch_val.is_u64();
        let epoch_ok = is_int && epoch_val.as_i64().map(|e| e >= 1).unwrap_or(false);
        let profile_ok = profile.is_null() || profile.is_object();
        let receipt_ok = receipt_id.is_none()
            || matches!(receipt_id, Some(Value::String(_)))
                && transfer_id_valid(receipt_id.clone().unwrap().as_str().unwrap_or(""));
        if !is_valid_backend(backend) || !run_id_ok || !epoch_ok
            || sel.get("schema_version").and_then(Value::as_u64) != Some(SCHEMA_VERSION as u64)
            || !receipt_ok || !profile_ok {
            return Err("BACKEND_INTEGRITY_BLOCKED: invalid selector identity, epoch, or format".into());
        }
        let selected = if backend == "sqlite" { &db } else { &session };
        let other = if backend == "sqlite" { &session } else { &db };
        if !selected.is_file() || other.exists() {
            return Err("BACKEND_INTEGRITY_BLOCKED: selected ledger missing or competing ledger exists".into());
        }
        if backend == "sqlite" {
            // No embedded SQLite yet; the Python toolchain owns sqlite-active runs.
            return Err("SQLITE_BACKEND_UNSUPPORTED_BY_HERDRT: the run is bound to SQLite; \
                        use the Python tools (tools/progress_view.py, tools/role_bind.py) until the cutover".into());
        }
        return Ok((Some(backend.into()), Some(sel)));
    }
    if session.exists() && db.exists() {
        return Err("BACKEND_CONFLICT: both canonical ledgers exist without a selector".into());
    }
    if session.exists() {
        return Ok((Some("markdown".into()), None));
    }
    if db.exists() {
        // Legacy offline pilot; never promoted by Rust binding.
        return Ok((Some("sqlite".into()), None));
    }
    Ok((None, None))
}

/// Exclusive-create run lock (run_backend.run_lock): token file removed iff unchanged.
pub struct RunLock {
    path: PathBuf,
    token: String,
    active: bool,
}

impl RunLock {
    pub fn acquire(run_dir: &Path) -> ToolResult<RunLock> {
        let path = run_dir.join("backend.lock");
        let token = format!("{}:{}", std::process::id(),
                            crate::util::hex(&random_bytes(8)));
        match std::fs::OpenOptions::new().create_new(true).write(true).open(&path) {
            Ok(mut f) => {
                f.write_all(token.as_bytes()).map_err(|e| e.to_string())?;
                f.sync_all().map_err(|e| e.to_string())?;
                Ok(RunLock { path, token, active: true })
            }
            Err(_) => Err("BACKEND_LOCK_HELD: inspect the run before retrying".into()),
        }
    }

    pub fn release(&mut self) {
        if !self.active {
            return;
        }
        self.active = false;
        if let Ok(content) = std::fs::read_to_string(&self.path) {
            if content == self.token {
                let _ = std::fs::remove_file(&self.path);
            }
        }
    }
}

impl Drop for RunLock {
    fn drop(&mut self) {
        self.release();
    }
}

pub fn random_bytes(n: usize) -> Vec<u8> {
    use std::cell::Cell;
    thread_local! {
        static STATE: Cell<u64> = const { Cell::new(0) };
    }
    STATE.with(|s| {
        if s.get() == 0 {
            let seed = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64).unwrap_or(0x9E3779B97F4A7C15)
                ^ (std::process::id() as u64) << 32;
            s.set(seed.max(1));
        }
        let mut out = Vec::with_capacity(n);
        for _ in 0..n {
            let mut x = s.get();
            x ^= x << 13; x ^= x >> 7; x ^= x << 17;
            s.set(x);
            out.push((x & 0xFF) as u8);
        }
        out
    })
}

pub fn write_selector(run_dir: &Path, data: &Value) -> ToolResult<()> {
    let path = run_dir.join(SELECTOR_FILE);
    let staged = run_dir.join(format!("{}.tmp.{}", SELECTOR_FILE, std::process::id()));
    let mut f = std::fs::OpenOptions::new().create_new(true).write(true).open(&staged)
        .map_err(|e| e.to_string())?;
    f.write_all(crate::util::json_pretty(data).as_bytes()).map_err(|e| e.to_string())?;
    f.sync_all().map_err(|e| e.to_string())?;
    drop(f);
    std::fs::rename(&staged, &path).map_err(|e| e.to_string())
}

pub fn init_selector(run_dir: &Path, run_id: &str, backend: &str, epoch: i64, profile: &Value) -> ToolResult<()> {
    let run_ok = Regex::new(r"^[A-Za-z0-9][A-Za-z0-9_-]{0,63}$").unwrap().is_match(run_id);
    if !run_ok || !is_valid_backend(backend) || epoch != 1 {
        return Err("invalid new-run backend selection".into());
    }
    let path = run_dir.join(SELECTOR_FILE);
    if path.exists() || path.is_symlink() {
        return Err("BACKEND_INTEGRITY_BLOCKED: selector already exists".into());
    }
    write_selector(run_dir, &json!({
        "run_id": run_id,
        "selected_backend": backend,
        "epoch": epoch,
        "schema_version": SCHEMA_VERSION as i64,
        "transfer_receipt_id": null,
        "planning_profile": profile,
    }))
}

/// Byte-for-byte the canonical planning SESSION.md (run_backend.render_session).
pub fn render_session(profile: &Value) -> String {
    let p = profile;
    let lead = p["lead"].as_bool().unwrap_or(false);
    (String::from("# Herdr Run: ") + p["run_id"].as_str().unwrap_or("") + "\n"
        + "Status: unstarted (configuration only; reconcile live Herdr/Git)\n"
        + "Repo: `" + p["repo"].as_str().unwrap_or("") + "`\n"
        + "Base: `" + p["base_ref"].as_str().unwrap_or("") + "` @ UNRESOLVED\n"
        + "Integration: `" + p["integration_branch"].as_str().unwrap_or("") + "` @ UNRESOLVED (worktree not observed)\n"
        + "Plan revision: NONE ACCEPTED\n"
        + "Approved intent: goal/requirements/constraints/non-goals/acceptance NOT RECORDED\n"
        + "Execution grant: NOT RECORDED (an explicit human request to execute is sufficient; planning-only is not)\n"
        + "Resource budget: active implementation workers " + &p["max_active_workers"].to_string()
        + " (parallel authorized: " + (if p["parallel_workers_authorized"].as_bool() == Some(true) { "True" } else { "False" })
        + "); concurrent heavy local verification jobs 1; remote CI separately observed\n"
        + (if lead {
            "Lead: inspect Herdr name/kind/pane (Manager + Delegator duties; human approves plan, scope changes and final acceptance)\n"
        } else {
            "Manager: inspect Herdr name/kind/pane\nDelegator: inspect Herdr name/kind/pane\n"
        })
        + "Critic: optional named Claude Sonnet 5.5, ROLE_READY then CRITIQUE_START/CRITIQUE_READY\n"
        + "Verifier: optional dedicated Claude Haiku 5.5 tester or other explicitly assigned capable verifier; exact integrated SHA, ROLE_READY then VERIFY_START + exclusive lock, and VERIFY_READY report\n"
        + "Model overrides: inspect requested worker model/effort, authorized non-Codex spawn owner, SPAWN_REQUEST/READY receipt and runtime confirmation\n"
        + "Verification lock: NONE (record task/owner/pane/tested SHA when held)\n"
        + "Candidate: UNFROZEN (record integration checkout + full SHA + expected diff)\n"
        + "Candidate writer state: OPEN (freeze checkout before verification)\n"
        + "Communication: ROLE_READY -> TASK_START (worker), ROLE_READY -> VERIFY_START + lock (verifier), REPORT_READY / VERIFY_READY / decisions / blockers; avoid polling loops\n\n"
        + "## Task frontier\n"
        + "| Task | User-facing title | Expected outcome | Depends on | State | Worker / Herdr pane | Workspace or worktree | Branch | Source SHA(s) | Integrated SHA(s) | Report / Manager verdict | Local verification (SHA) | Latest-head CI |\n"
        + "| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |\n"
        + "\nHuman summaries: show User-facing title + Expected outcome + verified State; never expose Task or Attempt IDs unless explicitly requested. These descriptions must come from approved PLAN slices, not guesses.\n"
        + "\n## Active attempts and write ownership\n"
        + "| Task/attempt | Agent / pane | Worktree | State | Previous attempt / evidence |\n"
        + "| --- | --- | --- | --- | --- |\n"
        + "Write ownership: one active writer per checkout; no concurrent writers\n"
        + "Retry policy: preserve prior attempt evidence; escalate repeated failures\n\n"
        + "## Signal readiness and receipts\n"
        + "- No ROLE_READY receipts observed; log recipient, event, task, pane, artifact, and observed vs pending state.\n"
        + "- Do not dispatch Worker edits before observed READY.md + ROLE_READY and separate TASK_START.\n"
        + "- Transport blocked: NONE OBSERVED (explicit human approval required for file-only mode)\n\n"
        + "## Decisions and exceptions\n"
        + "- Record human-approved budgets, narrowly scoped deviations and Manager plan revisions here; link evidence rather than duplicating logs.\n\n"
        + "## Cancellation state\n"
        + "- None; when authorized record actor, reason, CANCELLING, affected processes, final CANCELLED/CANCELLATION_BLOCKED; preserve artifacts.\n\n"
        + "## Recoverable ownership\n"
        + "- Workspaces/panes created by run: NONE OBSERVED\n"
        + "- Human-owned workspace/panes NEVER auto-close: inspect and record\n"
        + "- Pending report signals, decisions and blocked states: NONE OBSERVED\n\n"
        + "## Agent prompt submission receipts\n"
        + "Herdr prompt success records submission only. Reconcile the same target and task before replaying.\n\n"
        + "<!-- HERDR_ACTIVATION_RECEIPTS_START -->\n```json\n[]\n```\n"
        + "<!-- HERDR_ACTIVATION_RECEIPTS_END -->\n")
}

pub fn profile_from_config(config: &Value, repo: &str) -> Value {
    json!({
        "run_id": config["run_id"].as_str().unwrap_or(""),
        "repo": repo,
        // Python coerces via str(config.get(...)); non-string config values must survive.
        "base_ref": config.get("base_ref").map(crate::util::py_str).unwrap_or_else(|| "<unresolved>".into()),
        "integration_branch": config.get("integration_branch").map(crate::util::py_str).unwrap_or_else(|| "<not-created>".into()),
        "max_active_workers": config["max_active_workers"].as_i64().unwrap_or(1),
        "parallel_workers_authorized": config.get("parallel_workers_authorized").and_then(Value::as_bool).unwrap_or(false),
        "lead": config.get("agents").map(|a| a.get("lead").is_some()).unwrap_or(false),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tempdir() -> PathBuf {
        let d = std::env::temp_dir().join(format!("herdrt-test-{}-{:?}",
            std::process::id(), std::time::SystemTime::now()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn run_lock_is_exclusive_and_removes_own_token() {
        let dir = tempdir();
        let mut l1 = RunLock::acquire(&dir).unwrap();
        assert!(RunLock::acquire(&dir).is_err(), "second lock must fail: BACKEND_LOCK_HELD");
        l1.release();
        let mut l2 = RunLock::acquire(&dir).expect("lock must be free after release");
        l2.release();
        assert!(!dir.join("backend.lock").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn init_selector_roundtrip_and_missing_file() {
        let parent = tempdir();
        let dir = parent.join("r1");
        std::fs::create_dir_all(&dir).unwrap();
        let profile = serde_json::json!({"run_id": "r1", "repo": "/r", "base_ref": "main",
            "integration_branch": "i", "max_active_workers": 1,
            "parallel_workers_authorized": false, "lead": false});
        init_selector(&dir, "r1", "markdown", 1, &profile).unwrap();
        assert!(init_selector(&dir, "r1", "markdown", 1, &profile).is_err(), "double init refused");
        std::fs::write(dir.join("SESSION.md"), "").unwrap();
        let (backend, selector) = get_active_backend(&dir, false).unwrap();
        assert_eq!(backend.as_deref(), Some("markdown"));
        assert_eq!(selector.unwrap()["epoch"], 1);
        std::fs::remove_dir_all(&parent).unwrap();
    }

    #[test]
    fn backend_conflict_is_refused() {
        let dir = tempdir();
        std::fs::write(dir.join("SESSION.md"), "x").unwrap();
        std::fs::write(dir.join("ledger.sqlite"), "y").unwrap();
        let err = get_active_backend(&dir, false).unwrap_err();
        assert!(err.starts_with("BACKEND_CONFLICT"), "got {err}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn render_session_is_the_canonical_template() {
        let profile = serde_json::json!({"run_id": "demo", "repo": "/repo", "base_ref": "main",
            "integration_branch": "herdr/demo/integration", "max_active_workers": 2,
            "parallel_workers_authorized": true, "lead": true});
        let text = render_session(&profile);
        assert!(text.starts_with("# Herdr Run: demo\nStatus: unstarted"));
        assert!(text.contains("Resource budget: active implementation workers 2 (parallel authorized: True);"));
        assert!(text.contains("Lead: inspect Herdr name/kind/pane"));
        assert!(!text.contains("Manager: inspect Herdr name/kind/pane"));
        assert!(text.ends_with("<!-- HERDR_ACTIVATION_RECEIPTS_END -->\n"));
    }

    #[test]
    fn sqlite_selected_run_is_refused_not_silently_read() {
        let parent = tempdir();
        let dir = parent.join("r1");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("ledger.sqlite"), "x").unwrap();
        std::fs::write(dir.join(SELECTOR_FILE), r#"{"run_id":"r1","selected_backend":"sqlite",
            "epoch":1,"schema_version":3,"transfer_receipt_id":null,"planning_profile":null}"#).unwrap();
        assert!(get_active_backend(&dir, false).unwrap_err().starts_with("SQLITE_BACKEND_UNSUPPORTED_BY_HERDRT"));
        std::fs::remove_dir_all(&parent).unwrap();
    }
}
