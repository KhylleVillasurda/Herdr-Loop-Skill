//! Shared helpers for the herdrt tools. Output strings must match the Python
//! tools they replace (tests and operators depend on the exact wording).

use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub type ToolResult<T> = Result<T, String>;

/// Python `sys.platform`-compatible names; update.py/tests echo these values.
pub fn platform_name() -> &'static str {
    if cfg!(target_os = "windows") {
        "win32"
    } else if cfg!(target_os = "macos") {
        "darwin"
    } else {
        "linux"
    }
}

/// Python shutil.which equivalent (scans PATH; adds .exe on Windows).
pub fn which(name: &str) -> Option<PathBuf> {
    if name.contains('/') || name.contains('\\') {
        return if is_executable(Path::new(name)) { Some(PathBuf::from(name)) } else { None };
    }
    let path = std::env::var_os("PATH")?;
    let exts: Vec<String> = if cfg!(target_os = "windows") {
        std::env::var("PATHEXT")
            .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into())
            .split(';').map(|e| e.to_ascii_lowercase()).collect()
    } else {
        vec![String::new()]
    };
    for dir in std::env::split_paths(&path) {
        for ext in &exts {
            let candidate = dir.join(format!("{name}{ext}"));
            if is_executable(&candidate) {
                return Some(candidate);
            }
        }
    }
    None
}

#[cfg(unix)]
fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    let meta = match std::fs::metadata(p) {
        Ok(m) => m,
        Err(_) => return false,
    };
    meta.is_file() && meta.permissions().mode() & 0o111 != 0
}
#[cfg(not(unix))]
fn is_executable(p: &Path) -> bool {
    std::fs::metadata(p).map(|m| m.is_file()).unwrap_or(false)
}

pub struct ExecOutcome {
    pub exit: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    /// Some(expiry) when the child was reaped early because the deadline passed.
    pub timed_out: bool,
}

impl ExecOutcome {
    /// Python `subprocess.CompletedProcess.returncode` semantics: None means timeout/no exit.
    pub fn returncode(&self) -> i32 {
        self.exit.unwrap_or(1)
    }
    pub fn stdout_text(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }
    pub fn stderr_text(&self) -> String {
        String::from_utf8_lossy(&self.stderr).into_owned()
    }
}

/// subprocess.run(..., timeout=t) equivalent. On timeout the child is killed and
/// exit reports None; callers decide the failure code exactly as the Python does.
pub fn exec(argv: &[&str], timeout_secs: u64) -> ExecOutcome {
    let mut child = match Command::new(argv[0])
        .args(&argv[1..])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn() {
        Ok(c) => c,
        Err(e) => {
            return ExecOutcome {
                exit: None,
                stdout: Vec::new(),
                stderr: spawn_error_bytes(&e),
                timed_out: false,
            }
        }
    };
    let deadline = Duration::from_secs(timeout_secs);
    match wait_timeout::ChildExt::wait_timeout(&mut child, deadline) {
        Ok(Some(status)) => {
            let stdout = read_pipe(&mut child.stdout);
            let stderr = read_pipe(&mut child.stderr);
            ExecOutcome { exit: status.code(), stdout, stderr, timed_out: false }
        }
        Ok(None) => {
            let _ = child.kill();
            let _ = child.wait();
            let stdout = read_pipe(&mut child.stdout);
            let stderr = read_pipe(&mut child.stderr);
            ExecOutcome { exit: None, stdout, stderr, timed_out: true }
        }
        Err(_) => ExecOutcome {
            exit: None,
            stdout: Vec::new(),
            stderr: b"child wait failed".to_vec(),
            timed_out: false,
        },
    }
}

fn spawn_error_bytes(e: &std::io::Error) -> Vec<u8> {
    e.to_string().into_bytes()
}

fn read_pipe<R: Read>(pipe: &mut Option<R>) -> Vec<u8> {
    let mut buf = Vec::new();
    if let Some(p) = pipe {
        let _ = p.read_to_end(&mut buf);
    }
    buf
}

pub fn sha256_hex(data: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(data);
    hex(&h.finalize())
}

pub fn sha256_file(path: &Path) -> ToolResult<String> {
    let mut f = File::open(path).map_err(|e| e.to_string())?;
    let mut h = Sha256::new();
    let mut buf = [0u8; 65536];
    loop {
        let n = f.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(hex(&h.finalize()))
}

pub fn hex(data: &[u8]) -> String {
    data.iter().map(|b| format!("{b:02x}")).collect()
}

/// Constant-time compare of two digest strings (hmac.compare_digest equivalent).
pub fn digest_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

pub fn read_text(path: &Path) -> ToolResult<String> {
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    if bytes.len() >= 3 && bytes[0] == 0xEF && bytes[1] == 0xBB && bytes[2] == 0xBF {
        return String::from_utf8(bytes[3..].to_vec()).map_err(|e| e.to_string());
    }
    String::from_utf8(bytes).map_err(|e| e.to_string())
}

pub fn write_text(path: &Path, content: &str) -> ToolResult<()> {
    std::fs::write(path, content).map_err(|e| e.to_string())
}

/// Serialize a JSON value the way Python `json.dumps(v, indent=2)` renders it
/// (2-space indent, ", " separators implied, insertion order preserved because
/// serde_json Map keeps it with the preserve_order feature).
pub fn json_pretty(v: &serde_json::Value) -> String {
    let mut out = String::new();
    write_json(v, &mut out, 0);
    out.shrink_to_fit();
    out
}

fn write_json(v: &serde_json::Value, out: &mut String, depth: usize) {
    const INDENT: &str = "  ";
    match v {
        serde_json::Value::Null => out.push_str("null"),
        serde_json::Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        serde_json::Value::Number(n) => out.push_str(&n.to_string()),
        serde_json::Value::String(s) => {
            let mut quoted = String::new();
            append_json_string(s, &mut quoted);
            out.push_str(&quoted);
        }
        serde_json::Value::Array(items) => {
            if items.is_empty() {
                out.push_str("[]");
                return;
            }
            out.push_str("[");
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push_str(",");
                }
                out.push('\n');
                out.push_str(&INDENT.repeat(depth + 1));
                write_json(item, out, depth + 1);
            }
            out.push('\n');
            out.push_str(&INDENT.repeat(depth));
            out.push_str("]");
        }
        serde_json::Value::Object(map) => {
            if map.is_empty() {
                out.push_str("{}");
                return;
            }
            out.push_str("{");
            for (i, (k, item)) in map.iter().enumerate() {
                if i > 0 {
                    out.push_str(",");
                }
                out.push('\n');
                out.push_str(&INDENT.repeat(depth + 1));
                let mut quoted = String::new();
                append_json_string(k, &mut quoted);
                out.push_str(&quoted);
                out.push_str(": ");
                write_json(item, out, depth + 1);
            }
            out.push('\n');
            out.push_str(&INDENT.repeat(depth));
            out.push_str("}");
        }
    }
}

fn append_json_string(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            // Python json.dumps default ensure_ascii=True: escape every non-ASCII rune.
            c if (c as u32) > 0x7E => {
                let cp = c as u32;
                if cp > 0xFFFF {
                    let v = cp - 0x10000;
                    out.push_str(&format!("\\u{:04x}\\u{:04x}", 0xD800 + (v >> 10), 0xDC00 + (v & 0x3FF)));
                } else {
                    out.push_str(&format!("\\u{:04x}", cp));
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

pub fn elapsed(start: Instant) -> Duration {
    start.elapsed()
}

/// Python str(v) for JSON scalars as used in f-string/config coercion
/// (None/True/False, decimal ints, floats with e±NN exponents).
/// shortcut: composite values (arrays/objects) are rendered via py_repr-like
/// recursion below only approximated; real configs carry scalars here,
/// upgrade to role_bind.rs's full py_str port if a fixture ever needs it.
pub fn py_str(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::Null => "None".into(),
        serde_json::Value::Bool(b) => if *b { "True" } else { "False" }.into(),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                i.to_string()
            } else if let Some(u) = n.as_u64() {
                u.to_string()
            } else {
                match n.as_f64() {
                    Some(f) => format!("{:?}", f).replace("e0", "e+0").replace("e+e", "e+"),
                    None => n.to_string(),
                }
            }
        }
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod py_json_tests {
    use super::*;

    #[test]
    fn json_pretty_matches_python_ensure_ascii() {
        assert_eq!(
            json_pretty(&serde_json::json!({"k": "aé𝄞\u{0007}"})),
            "{\n  \"k\": \"a\\u00e9\\ud834\\udd1e\\u0007\"\n}"
        );
        // idempotent for callers that pass again
        assert_eq!(json_pretty(&serde_json::json!("\\u00e9")), "\"\\\\u00e9\"");
    }

    #[test]
    fn py_str_scalar_table() {
        let v = |s: &str| serde_json::from_str::<serde_json::Value>(s).unwrap();
        assert_eq!(py_str(&v("null")), "None");
        assert_eq!(py_str(&v("true")), "True");
        assert_eq!(py_str(&v("12")), "12");
        assert_eq!(py_str(&v("\"x\"")), "x");
    }
}
