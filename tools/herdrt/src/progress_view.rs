//! Port of tools/progress_view.py (Markdown path; SQLite-active runs fall back
//! to the Python tool in phase 1, because herdr_ledger.py owns that backend).

use crate::backend;
use crate::util::{self, ToolResult};
use regex::Regex;
use serde_json::Value;
use std::path::{Path, PathBuf};

const STATE_LABELS: [(&str, &str); 18] = [
    ("unstarted", "Not started"),
    ("planned", "Planned"),
    ("ready", "Queued"),
    ("assigned", "Assigned"),
    ("working", "In progress"),
    ("executing", "In progress"),
    ("reported", "Implementation finished; reviewing"),
    ("integrated", "Changes integrated; checks pending"),
    ("verifying", "Testing"),
    ("verified", "Verified; awaiting acceptance"),
    ("repair-needed", "Fixes needed"),
    ("blocked", "Blocked"),
    ("content-accepted-verification-pending", "Implementation approved; checks pending"),
    ("accepted", "Complete"),
    ("cleanup-blocked", "Cleanup needs attention"),
    ("cancelling", "Stopping work"),
    ("cancelled", "Cancelled"),
    ("cancellation-blocked", "Cancellation needs attention"),
];
const PLACEHOLDERS: [&str; 9] = ["", "-", "tbd", "unknown", "unset", "none", "pending", "not recorded", "needs approved title"];

fn cells(line: &str) -> Vec<String> {
    line.trim().trim_matches('|').split('|').map(|s| s.trim().to_string()).collect()
}

fn get_frontier(content: &str) -> ToolResult<(Vec<Vec<String>>, Option<Vec<String>>)> {
    let lines: Vec<&str> = content.lines().collect();
    for (idx, line) in lines.iter().enumerate() {
        if line.trim().to_lowercase() == "## task frontier" {
            let mut table: Vec<Vec<String>> = Vec::new();
            for current in &lines[idx + 1..] {
                if current.starts_with("## ") { break; }
                if current.trim().starts_with("|") {
                    table.push(cells(current));
                } else if !table.is_empty() && !current.trim().is_empty() {
                    break;
                }
            }
            if table.len() < 2 {
                return Ok((vec![], None));
            }
            let columns: Vec<String> = table[0].iter().map(|h| h.trim().to_lowercase()).collect();
            let sep_ok = table[1].iter().all(|x| Regex::new(r"^[: -]+$").unwrap().is_match(x));
            if !sep_ok {
                return Err("Task frontier table has no Markdown header separator".into());
            }
            return Ok((table[2..].to_vec(), Some(columns)));
        }
    }
    Err("SESSION.md is missing ## Task frontier".into())
}

fn header_index(headers: &[String], options: &[&str]) -> Option<usize> {
    options.iter().find_map(|c| headers.iter().position(|h| h == c))
}

fn human_state(state: &str) -> String {
    let state = state.trim().to_lowercase().replace('_', "-");
    let pending = Regex::new(r"\bci pending\b|\bverification pending\b").unwrap();
    if pending.is_match(&state) && state.contains("accepted") {
        return "Implementation approved; checks pending".into();
    }
    // Python sorts keys by length, longest first, before prefix matching.
    let mut keys: Vec<(&str, &str)> = STATE_LABELS.iter().copied().collect();
    keys.sort_by_key(|(k, _)| std::cmp::Reverse(k.len()));
    for (key, label) in keys {
        if state == key
            || state.starts_with(&format!("{key} ("))
            || state.starts_with(&format!("{key} /")) {
            return label.to_string();
        }
    }
    "Status needs reconciliation".into()
}

pub fn render(content: &str, include_internal_ids: bool) -> ToolResult<String> {    let (rows, headers) = get_frontier(content)?;
    if headers.is_none() || rows.is_empty() {
        return Ok("# Work progress\n\nNo work slices have been recorded yet.\n".into());
    }
    let headers = headers.unwrap();
    let task_idx = header_index(&headers, &["task", "task id"]);
    let title_idx = header_index(&headers, &["user-facing title", "slice title"]);
    let outcome_idx = header_index(&headers, &["expected outcome", "user-visible outcome"]);
    let state_idx = header_index(&headers, &["state", "status"]);
    let (Some(t), Some(ti), Some(oi), Some(si)) = (task_idx, title_idx, outcome_idx, state_idx) else {
        return Err("Task frontier requires Task, User-facing title, Expected outcome, State columns".into());
    };
    let mut entries: Vec<String> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let task_re = Regex::new(r"^T[0-9]{2,4}$").unwrap();
    for row in &rows {
        if row.len() != headers.len() {
            return Err("Task frontier has inconsistent Markdown table columns".into());
        }
        let task = row[t].as_str();
        if !task_re.is_match(task) {
            return Err("Task frontier has invalid internal task keys".into());
        }
        if seen.contains(task) {
            return Err("Task frontier contains duplicate task keys".into());
        }
        seen.insert(task.to_string());
        let title = row[ti].trim().to_string();
        let outcome = row[oi].trim().to_string();
        let bad = PLACEHOLDERS.contains(&title.to_lowercase().as_str())
            || PLACEHOLDERS.contains(&outcome.to_lowercase().as_str())
            || title.contains('<') || title.contains('>')
            || outcome.contains('<') || outcome.contains('>');
        if bad {
            return Err("Task frontier has missing descriptive title/outcome; update it from the approved plan before publishing status".into());
        }
        let suffix = if include_internal_ids { format!(" ({task})") } else { String::new() };
        entries.push(format!("- **{title}{suffix}**: {}\n  {}", human_state(row[si].as_str()), outcome));
    }
    Ok(format!("# Work progress\n\n{}\n\n_This reflects the recorded session ledger, not a live Git/Herdr verification._\n",
               entries.join("\n")))
}

pub fn cli(argv: &[String]) -> i32 {
    let mut session: Option<PathBuf> = None;
    let mut run_dir: Option<PathBuf> = None;
    let mut include_internal_ids = false;
    let mut it = argv.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--session" => session = it.next().map(PathBuf::from),
            "--run-dir" => run_dir = it.next().map(PathBuf::from),
            "--include-internal-ids" => include_internal_ids = true,
            _ => {
                eprintln!("usage: herdrt progress-view --session PATH | --run-dir DIR [--include-internal-ids]");
                return 2;
            }
        }
    }
    if session.is_some() == run_dir.is_some() {
        eprintln!("ERROR: one of the arguments --session --run-dir is required");
        return 2;
    }
    match run_cli(session.as_deref(), run_dir.as_deref(), include_internal_ids) {
        Ok(out) => {
            print!("{out}");
            0
        }
        Err(message) => {
            eprintln!("ERROR: {message}");
            2
        }
    }
}

fn run_cli(session: Option<&Path>, run_dir: Option<&Path>, include_internal_ids: bool) -> ToolResult<String> {
    if let Some(run_dir_arg) = run_dir {
        let resolved = absolute(run_dir_arg);
        let (backend, _) = backend::get_active_backend(&resolved, false)?;
        if backend.as_deref() == Some("sqlite") {
            return Err("SQLITE_BACKEND_UNSUPPORTED_BY_HERDRT: use the Python tool for SQLite-active runs (tools/progress_view.py)".into());
        }
        if backend.is_none() {
            return Err(format!("Run in {} has no active backend or is uninitialized", resolved.display()));
        }
        let session_path = resolved.join("SESSION.md");
        if !session_path.exists() {
            return Err(format!("SESSION.md missing in Markdown run {}", resolved.display()));
        }
        return render(&util::read_text(&session_path)?, include_internal_ids);
    }
    let session_path = session.unwrap();
    let resolved = absolute(session_path);
    if resolved.parent().is_some_and(|p| p.is_dir()) {
        let (backend, _) = backend::get_active_backend(resolved.parent().unwrap(), false)?;
        if backend.as_deref() == Some("sqlite") {
            return Err(format!("Cannot read archived/inactive SESSION.md: run '{}' active backend is sqlite",
                               resolved.parent().unwrap().file_name().unwrap().to_string_lossy()));
        }
    }
    if !resolved.is_file() {
        return Err(format!("[Errno 2] No such file or directory: '{}'", resolved.display()));
    }
    render(&util::read_text(&resolved)?, include_internal_ids)
}

fn absolute(p: &Path) -> PathBuf {
    if p.is_absolute() { p.to_path_buf() } else {
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")).join(p)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session_with_state(state: &str) -> String {
        format!("# Herdr Run: demo\n\n## Task frontier\n\
        | Task | User-facing title | Expected outcome | Depends on | State | Worker / Herdr pane | Workspace or worktree | Branch | Source SHA(s) | Integrated SHA(s) | Report / Manager verdict | Local verification (SHA) | Latest-head CI |\n\
        | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |\n\
        | T01 | Sign-in recovery message | Show a clear recovery message when a session expires | - | {state} | pane-1 | wt | br | sha | sha | - | - | - |\n")
    }

    #[test]
    fn empty_frontier_renders_placeholder() {
        let out = render("# Herdr Run: demo\n\n## Task frontier\n| header |\n", false).unwrap();
        assert_eq!(out, "# Work progress\n\nNo work slices have been recorded yet.\n");
    }

    #[test]
    fn missing_frontier_is_an_error() {
        assert_eq!(render("# Herdr Run: demo\n", false).unwrap_err(),
                   "SESSION.md is missing ## Task frontier");
    }

    #[test]
    fn populated_table_renders_human_state_and_outcome() {
        let out = render(&session_with_state("reported"), false).unwrap();
        assert!(out.starts_with("# Work progress\n\n- **Sign-in recovery message**: Implementation finished; reviewing\n  Show a clear recovery message when a session expires\n"));
        assert!(out.ends_with("_This reflects the recorded session ledger, not a live Git/Herdr verification._\n"));
    }

    #[test]
    fn state_aliases_map_to_plain_language() {
        assert_eq!(human_state("executing"), "In progress");
        assert_eq!(human_state("ACCEPTED"), "Complete");
        assert_eq!(human_state("content_accepted_verification_pending"), "Implementation approved; checks pending");
        assert_eq!(human_state("accepted CI pending"), "Implementation approved; checks pending");
        assert_eq!(human_state("verification_pending accepted"), "Status needs reconciliation"); // python: 'verification-pending accepted' is not keyed
        assert_eq!(human_state("cleanup-blocked"), "Cleanup needs attention");
        assert_eq!(human_state("weird state"), "Status needs reconciliation");
        // longest-prefix wins over shorter keys:
        assert_eq!(human_state("cancellation-blocked"), "Cancellation needs attention");
    }

    #[test]
    fn placeholders_and_angle_brackets_are_refused() {
        let base = session_with_state("working");
        let angle = base.replace("Sign-in recovery message", "<required title>");
        assert_eq!(render(&angle, false), Err(
            "Task frontier has missing descriptive title/outcome; update it from the approved plan before publishing status".into()
        ));
        let placeholder = base.replace("Sign-in recovery message", "needs approved title");
        assert_eq!(render(&placeholder, false), Err(
            "Task frontier has missing descriptive title/outcome; update it from the approved plan before publishing status".into()
        ));
    }

    #[test]
    fn missing_columns_are_refused() {
        let bad = "# x\n## task frontier\n| Task | State |\n| --- | --- |\n| T01 | working |\n";
        assert_eq!(render(bad, false).unwrap_err(),
                   "Task frontier requires Task, User-facing title, Expected outcome, State columns");
    }

    #[test]
    fn duplicate_or_invalid_task_ids_are_refused() {
        let dup = "# x\n## task frontier\n| Task | User-facing title | Expected outcome | State |\n| --- | --- | --- | --- |\n| T01 | a | o | working |\n| T01 | a | o | working |\n";
        assert_eq!(render(dup, false).unwrap_err(), "Task frontier contains duplicate task keys");
        let bad_id = dup.replace("T01 | a", "TXYZ | a");
        assert_eq!(render(&bad_id, false).unwrap_err(), "Task frontier has invalid internal task keys");
    }
}
