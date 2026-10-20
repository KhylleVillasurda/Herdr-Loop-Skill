//! Port of tools/update.py: updater and rollback utility for Herdr Role Pack.
//! Queries published GitHub releases, verifies asset checksums and archive
//! safety, backs up managed installations, and provides safe rollback.
//! Error strings mirror the Python tool verbatim (tests depend on them).

use crate::util::{self, ToolResult};
use regex::Regex;
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::File;
use std::io::{BufReader, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

pub const REPO_OWNER: &str = "Jacinth091";
pub const REPO_NAME: &str = "Herdr-Loop-Skill";
pub const GITHUB_API_URL: &str =
    "https://api.github.com/repos/Jacinth091/Herdr-Loop-Skill/releases";
pub const GITHUB_RELEASES_URL: &str =
    "https://github.com/Jacinth091/Herdr-Loop-Skill/releases";

pub const MANAGED_MARKER: &str = ".herdr-role-pack-managed";

pub const MANAGED_SKILLS: [&str; 7] = [
    "herdr-engineering-loop",
    "herdr-manager",
    "herdr-delegator",
    "herdr-lead",
    "herdr-worker",
    "herdr-verifier",
    "herdr-critic",
];

pub const AGENTS: [&str; 4] = ["claude", "codex", "agy", "opencode"];

const USER_AGENT: &str = "Herdr-Role-Pack-Updater";

/// Python Path.home() equivalent (HOME / USERPROFILE).
fn home_dir() -> ToolResult<PathBuf> {
    let key = if cfg!(target_os = "windows") { "USERPROFILE" } else { "HOME" };
    match std::env::var(key) {
        Ok(v) if !v.is_empty() => Ok(PathBuf::from(v)),
        _ => Err("Can't figure home directory".into()),
    }
}

fn agent_skill_dirs() -> ToolResult<Vec<(String, PathBuf)>> {
    let home = home_dir()?;
    Ok(AGENTS
        .iter()
        .map(|agent| {
            let base = match *agent {
                "claude" => home.join(".claude").join("skills"),
                "codex" => home.join(".agents").join("skills"),
                "agy" => home.join(".gemini").join("antigravity-cli").join("skills"),
                "opencode" => home.join(".config").join("opencode").join("skills"),
                _ => unreachable!(),
            };
            (agent.to_string(), base)
        })
        .collect())
}

fn backup_root() -> ToolResult<PathBuf> {
    Ok(home_dir()?.join(".herdr-backups"))
}

/// get_installed_versions(): skill dirs per agent as (dir, status) pairs.
/// Marker files are read with BOM-tolerant utf-8 (Python utf-8-sig).
pub fn get_installed_versions() -> ToolResult<Vec<(String, Vec<(String, Option<String>)>)>> {
    let dirs = agent_skill_dirs()?;
    let mut results = Vec::new();
    for (agent, base_dir) in dirs {
        let mut skills = Vec::new();
        for skill in MANAGED_SKILLS {
            let skill_dir = base_dir.join(skill);
            let ver = if skill_dir.exists() {
                let marker = skill_dir.join(MANAGED_MARKER);
                if marker.exists() {
                    Some(match util::read_text(&marker) {
                        Ok(content) => content.trim().to_string(),
                        Err(_) => "MANAGED".to_string(),
                    })
                } else {
                    Some("UNMANAGED".to_string())
                }
            } else {
                None
            };
            skills.push((skill.to_string(), ver));
        }
        results.push((agent, skills));
    }
    Ok(results)
}

/// 'Herdr Role Pack v2.8.0' / 'v2.8.0' -> (2, 8, 0).
/// The 2.8.0 installers wrote 'v2.8', read as 2.8.0.
pub fn parse_version(text: Option<&str>) -> Option<(u32, u32, u32)> {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"v(\d+)\.(\d+)(?:\.(\d+))?\b").unwrap());
    let caps = re.captures(text?)?;
    let third = caps
        .get(3)
        .and_then(|m| m.as_str().parse::<u32>().ok())
        .unwrap_or(0);
    Some((
        caps[1].parse::<u32>().ok()?,
        caps[2].parse::<u32>().ok()?,
        third,
    ))
}

/// Python: GITHUB_TOKEN or GH_TOKEN, then `gh auth token` (5s timeout, killed
/// via wait-timeout). Whitespace-only env values fall through to the gh CLI,
/// matching the Python truthiness rules.
pub fn get_github_token() -> Option<String> {
    let a = std::env::var("GITHUB_TOKEN").unwrap_or_default();
    let b = std::env::var("GH_TOKEN").unwrap_or_default();
    let env_token = if !a.is_empty() { a } else { b };
    if !env_token.trim().is_empty() {
        return Some(env_token.trim().to_string());
    }
    let gh = util::which("gh")?;
    let res = util::exec(&[&gh.to_string_lossy(), "auth", "token"], 5);
    if res.returncode() == 0 && !res.stdout_text().trim().is_empty() {
        return Some(res.stdout_text().trim().to_string());
    }
    None
}

fn http_error(code: u16, text: String) -> String {
    if code == 0 {
        text
    } else {
        format!("HTTP Error {code}: {text}")
    }
}

fn parse_body(resp: ureq::Response) -> ToolResult<Value> {
    let body = resp.into_string().map_err(|e| e.to_string())?;
    serde_json::from_str(&body).map_err(|e| e.to_string())
}

fn streamed_to_file(resp: ureq::Response, dest: &Path) -> ToolResult<()> {
    let mut reader = BufReader::new(resp.into_reader());
    let mut f = File::create(dest).map_err(|e| e.to_string())?;
    let mut chunk = [0u8; 65536];
    loop {
        let n = reader.read(&mut chunk).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        f.write_all(&chunk[..n]).map_err(|e| e.to_string())?;
    }
    f.flush().map_err(|e| e.to_string())?;
    Ok(())
}

/// Fetch the releases list; Ok(Value::Null) means "no compatible release".
pub fn fetch_latest_release(token: Option<&str>, allow_prerelease: bool) -> ToolResult<Value> {
    let tok = token.filter(|t| !t.is_empty());
    let agent = ureq::AgentBuilder::new()
        // _DropAuthOnRedirect: keep Authorization only for same-host redirects
        // (never send the GitHub token to the asset CDN).
        .redirect_auth_headers(ureq::RedirectAuthHeaders::SameHost)
        .timeout_connect(Duration::from_secs(10))
        .timeout_read(Duration::from_secs(10))
        .build();
    let mut req = agent
        .get(GITHUB_API_URL)
        .set("User-Agent", USER_AGENT)
        .set("Accept", "application/vnd.github.v3+json");
    if let Some(t) = tok {
        req = req.set("Authorization", &format!("Bearer {t}"));
    }
    let resp = match req.call() {
        Ok(resp) => resp,
        Err(ureq::Error::Status(code, resp)) => {
            let text = resp.status_text().to_string();
            match code {
                401 => {
                    return Err(
                        "AUTH_REQUIRED: GitHub authentication required to access repository releases. \
Set GITHUB_TOKEN or authenticate via 'gh auth login'."
                            .into(),
                    )
                }
                403 => {
                    return Err("ACCESS_DENIED: Access denied to repository releases. Verify token permissions.".into())
                }
                404 => {
                    if tok.is_none() {
                        return Err(
                            "AUTH_REQUIRED: Received 404 from repository API. If this is a private repository, \
authentication is required. Set GITHUB_TOKEN or authenticate via 'gh auth login'."
                                .into(),
                        );
                    }
                    return Err(
                        "ACCESS_DENIED: Repository not visible with current credentials (404). \
Verify the token has access to this private repository."
                            .into(),
                    );
                }
                _ => return Err(http_error(code, text)),
            }
        }
        Err(e) => return Err(e.to_string()),
    };
    if resp.status() != 200 {
        return Ok(Value::Null);
    }
    let data = parse_body(resp)?;
    let releases = data
        .as_array()
        .ok_or_else(|| "releases payload is not a list".to_string())?;
    for release in releases {
        let prerelease = release
            .get("prerelease")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if !allow_prerelease && prerelease {
            continue;
        }
        // Python: skip when release.get("draft", False) is truthy; a missing
        // key defaults to False and the release is returned.
        let draft = release
            .get("draft")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if draft {
            continue;
        }
        return Ok(release.clone());
    }
    Ok(Value::Null)
}

/// Download a release asset, handling authenticated private repo downloads.
/// The GitHub token is never forwarded to a host other than the request host
/// (ureq RedirectAuthHeaders::SameHost mirrors update.py's _DropAuthOnRedirect).
pub fn download_asset(asset: &Value, dest_path: &Path, token: Option<&str>) -> ToolResult<()> {
    let tok = token
        .map(|t| t.to_string())
        .unwrap_or_else(|| get_github_token().unwrap_or_default());
    let name = asset.get("name").and_then(|v| v.as_str()).unwrap_or("unknown");

    // Try authenticated GitHub API asset download if token is present.
    if !tok.is_empty() {
        if let Some(url) = asset.get("url").and_then(|v| v.as_str()) {
            let agent = ureq::AgentBuilder::new()
                .redirect_auth_headers(ureq::RedirectAuthHeaders::SameHost)
                .timeout_connect(Duration::from_secs(30))
                .timeout_read(Duration::from_secs(30))
                .build();
            let req = agent
                .get(url)
                .set("User-Agent", USER_AGENT)
                .set("Authorization", &format!("Bearer {tok}"))
                .set("Accept", "application/octet-stream");
            match req.call() {
                Ok(resp) => return streamed_to_file(resp, dest_path),
                Err(ureq::Error::Status(code, _)) if code == 401 || code == 403 => {
                    return Err(
                        "ACCESS_DENIED: Failed to download release asset with current credentials.".into(),
                    );
                }
                // Other HTTP errors fall back to the browser URL, like Python.
                Err(_) => {}
            }
        }
    }

    // Fall back to browser download URL.
    let url = asset
        .get("browser_download_url")
        .and_then(|v| v.as_str())
        .ok_or_else(|| format!("No download URL available for asset {name}"))?;
    let agent = ureq::AgentBuilder::new()
        .redirect_auth_headers(ureq::RedirectAuthHeaders::SameHost)
        .timeout_connect(Duration::from_secs(30))
        .timeout_read(Duration::from_secs(30))
        .build();
    let mut req = agent.get(url).set("User-Agent", USER_AGENT);
    if !tok.is_empty() {
        req = req.set("Authorization", &format!("Bearer {tok}"));
    }
    match req.call() {
        Ok(resp) => streamed_to_file(resp, dest_path),
        Err(ureq::Error::Status(401, _)) => {
            Err("AUTH_REQUIRED: GitHub authentication required to download release asset.".into())
        }
        Err(ureq::Error::Status(403, _)) => {
            Err("ACCESS_DENIED: Access denied when downloading release asset.".into())
        }
        Err(ureq::Error::Status(404, _)) => {
            Err(format!("ASSET_NOT_FOUND: Release asset {name} not found (404)."))
        }
        Err(ureq::Error::Status(code, resp)) => Err(http_error(code, resp.status_text().to_string())),
        Err(e) => Err(e.to_string()),
    }
}

/// All files under root, keyed by POSIX-style relative path, sorted.
pub fn hash_tree(root: &Path) -> ToolResult<Value> {
    let mut files: BTreeMap<String, String> = BTreeMap::new();
    collect_tree(root, root, &mut files)?;
    let mut map = Map::new();
    for (k, v) in files {
        map.insert(k, Value::String(v));
    }
    Ok(Value::Object(map))
}

fn collect_tree(
    dir: &Path,
    root: &Path,
    files: &mut BTreeMap<String, String>,
) -> ToolResult<()> {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(|e| e.to_string())?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .collect();
    entries.sort();
    for p in entries {
        let meta = std::fs::metadata(&p).map_err(|e| e.to_string())?;
        if meta.is_file() {
            let rel = p
                .strip_prefix(root)
                .unwrap_or(&p)
                .to_string_lossy()
                .replace('\\', "/");
            let hash = util::sha256_file(&p)?;
            files.insert(rel, hash);
        } else if meta.is_dir() {
            collect_tree(&p, root, files)?;
        }
    }
    Ok(())
}

/// shutil.copytree equivalent (follows symlinks, copies empty dirs).
fn copy_tree(src: &Path, dst: &Path) -> ToolResult<()> {
    std::fs::create_dir_all(dst).map_err(|e| e.to_string())?;
    let mut entries: Vec<PathBuf> = std::fs::read_dir(src)
        .map_err(|e| e.to_string())?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .collect();
    entries.sort();
    for p in entries {
        let target = dst.join(p.file_name().unwrap_or_default());
        let meta = std::fs::metadata(&p).map_err(|e| e.to_string())?;
        if meta.is_dir() {
            copy_tree(&p, &target)?;
        } else {
            std::fs::copy(&p, &target).map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

fn remove_tree(path: &Path) -> ToolResult<()> {
    std::fs::remove_dir_all(path).map_err(|e| e.to_string())
}

/// 'agent/skill' -> path for every currently installed pack-managed skill.
pub fn managed_slots() -> ToolResult<Vec<(String, PathBuf)>> {
    Ok(slots_from_dirs(&agent_skill_dirs()?))
}

fn slots_from_dirs(dirs: &[(String, PathBuf)]) -> Vec<(String, PathBuf)> {
    let mut slots = Vec::new();
    for (agent, base_dir) in dirs {
        for skill in MANAGED_SKILLS {
            let target = base_dir.join(skill);
            if target.join(MANAGED_MARKER).exists() {
                slots.push((format!("{agent}/{skill}"), target));
            }
        }
    }
    slots
}

pub fn read_manifest(backup_dir: &Path) -> ToolResult<Value> {
    let path = backup_dir.join("backup_manifest.json");
    if !path.exists() {
        return Ok(json!({}));
    }
    // Python uses plain utf-8 here (no BOM tolerance).
    let bytes = std::fs::read(&path).map_err(|e| e.to_string())?;
    let text = String::from_utf8(bytes).map_err(|e| e.to_string())?;
    serde_json::from_str(&text).map_err(|e| e.to_string())
}

pub fn write_manifest(backup_dir: &Path, manifest: &Value) -> ToolResult<()> {
    let text = util::json_pretty(manifest);
    std::fs::write(backup_dir.join("backup_manifest.json"), text.as_bytes())
        .map_err(|e| e.to_string())
}

pub fn backup_managed_skills() -> ToolResult<PathBuf> {
    backup_slots(&backup_root()?, &managed_slots()?)
}

/// Copies the given slots into a new backup dir labelled with the current local
/// timestamp and a per-file hash manifest.
pub fn backup_slots(backup_root: &Path, slots: &[(String, PathBuf)]) -> ToolResult<PathBuf> {
    let ts_str = timestamp_now();
    let backup_dir = backup_root.join(format!("backup-{ts_str}"));
    std::fs::create_dir_all(&backup_dir).map_err(|e| e.to_string())?;

    let mut items = Map::new();
    for (key, target) in slots {
        copy_tree(target, &backup_dir.join(key))?;
        let resolved = target.canonicalize().unwrap_or_else(|_| target.clone());
        items.insert(
            key.clone(),
            json!({
                "target": resolved.to_string_lossy().to_string(),
                "files": hash_tree(target)?,
            }),
        );
    }

    let manifest = json!({"timestamp": ts_str, "items": Value::Object(items)});
    write_manifest(&backup_dir, &manifest)?;
    Ok(backup_dir)
}

/// Python datetime.now().strftime("%Y%m%d-%H%M%S-%f"): local time, microseconds.
fn timestamp_now() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let (secs, micros) = (now.as_secs() as i64, now.subsec_micros());
    #[cfg(target_os = "windows")]
    {
        // GetLocalTime = local wall time; kernel32 is linked by std, no extra crate.
        #[repr(C)]
        struct LpSystemTime {
            w_year: u16, w_month: u16, w_day_of_week: u16, w_day: u16,
            w_hour: u16, w_minute: u16, w_second: u16, w_milliseconds: u16,
        }
        extern "system" {
            fn GetLocalTime(lp_system_time: *mut LpSystemTime);
        }
        let mut st = LpSystemTime {
            w_year: 0, w_month: 0, w_day_of_week: 0, w_day: 0,
            w_hour: 0, w_minute: 0, w_second: 0, w_milliseconds: 0,
        };
        unsafe { GetLocalTime(&mut st); }
        if st.w_year != 0 {
            // Python %f is microseconds; GetLocalTime gives milliseconds.
            return format!(
                "{:04}{:02}{:02}-{:02}{:02}{:02}-{:06}",
                st.w_year, st.w_month, st.w_day, st.w_hour, st.w_minute, st.w_second,
                st.w_milliseconds as u32 * 1000
            );
        }
    }
    #[cfg(unix)]
    {
        // struct layout matches glibc/musl `struct tm`; libc is already in the link graph.
        #[repr(C)]
        struct Tm {
            tm_sec: i32,
            tm_min: i32,
            tm_hour: i32,
            tm_mday: i32,
            tm_mon: i32,
            tm_year: i32,
            tm_wday: i32,
            tm_yday: i32,
            tm_isdst: i32,
            tm_gmtoff: i64,
            tm_zone: *const i8,
        }
        extern "C" {
            fn localtime_r(timep: *const i64, result: *mut Tm) -> *mut Tm;
        }
        let mut tm = Tm {
            tm_sec: 0, tm_min: 0, tm_hour: 0, tm_mday: 0, tm_mon: 0,
            tm_year: 0, tm_wday: 0, tm_yday: 0, tm_isdst: 0, tm_gmtoff: 0,
            tm_zone: std::ptr::null(),
        };
        unsafe {
            if !localtime_r(&secs, &mut tm).is_null() {
                return format!(
                    "{:04}{:02}{:02}-{:02}{:02}{:02}-{:06}",
                    tm.tm_year + 1900, tm.tm_mon + 1, tm.tm_mday,
                    tm.tm_hour, tm.tm_min, tm.tm_sec, micros
                );
            }
        }
    }
    let civil = civil_from_unix(secs);
    format!(
        "{:04}{:02}{:02}-{:02}{:02}{:02}-{:06}",
        civil.0, civil.1, civil.2, civil.3, civil.4, civil.5, micros
    )
}

/// UTC (year, month, day, hour, min, sec) from unix seconds (Howard Hinnant algorithm).
fn civil_from_unix(secs: i64) -> (i64, u32, u32, u32, u32, u32) {
    let days = secs.div_euclid(86400);
    let rem = secs.rem_euclid(86400);
    let (h, m, s) = ((rem / 3600) as u32, ((rem % 3600) / 60) as u32, (rem % 60) as u32);
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = (z - era * 146097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // [0, 399]
    let y = (yoe as i64) + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if mo <= 2 { y + 1 } else { y };
    (y, mo, d, h, m, s)
}

/// Sorted list of (backup_dir, manifest) for dirs containing a valid
/// backup_manifest.json, newest first.
pub fn get_valid_backups() -> ToolResult<Vec<(PathBuf, Value)>> {
    get_valid_backups_in(&backup_root()?)
}

pub fn get_valid_backups_in(backup_root: &Path) -> ToolResult<Vec<(PathBuf, Value)>> {
    if !backup_root.exists() {
        return Ok(vec![]);
    }
    let mut valid: Vec<(PathBuf, Value, String)> = Vec::new();
    for entry in std::fs::read_dir(backup_root).map_err(|e| e.to_string())? {
        let p = entry.map_err(|e| e.to_string())?.path();
        if !p.is_dir() {
            continue;
        }
        let manifest = read_manifest(&p)?;
        let ok = match &manifest {
            Value::Null => false,
            Value::Object(map) => map.get("items").map(|i| i.is_object()).unwrap_or(false),
            _ => false,
        };
        if ok {
            // Python: manifest.get("timestamp") or str(st_mtime)
            let ts = match manifest.get("timestamp") {
                Some(Value::String(s)) if !s.is_empty() => s.clone(),
                _ => {
                    let mtime = std::fs::metadata(&p)
                        .and_then(|m| m.modified())
                        .ok()
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|d| d.as_secs_f64())
                        .unwrap_or(0.0);
                    format!("{mtime}")
                }
            };
            valid.push((p, manifest, ts));
        }
    }
    valid.sort_by(|a, b| b.2.cmp(&a.2));
    Ok(valid.into_iter().map(|(p, m, _)| (p, m)).collect())
}

/// Restores a backup, removing slots the following update created. Every check
/// runs before anything is touched.
pub fn restore_backup(backup_dir: &Path, snapshot_first: bool) -> ToolResult<()> {
    restore_backup_core(
        backup_dir,
        snapshot_first,
        &agent_skill_dirs()?,
        &backup_root()?,
    )
}

/// Refactored core with paths injected (identical behaviour to restore_backup);
/// the public form resolves them from the environment like the Python tool.
pub fn restore_backup_core(
    backup_dir: &Path,
    snapshot_first: bool,
    agent_dirs: &[(String, PathBuf)],
    backup_root: &Path,
) -> ToolResult<()> {
    let invalid_manifest = || {
        format!(
            "REFUSE: backup directory '{}' is missing or has an invalid 'backup_manifest.json'.",
            backup_dir.display()
        )
    };
    if !backup_dir.exists() {
        return Err(format!("Backup directory not found: {}", backup_dir.display()));
    }
    let manifest = read_manifest(backup_dir)?;
    if !manifest.is_object() || manifest.get("items").and_then(|i| i.as_object()).is_none() {
        return Err(invalid_manifest());
    }
    let items = manifest["items"].clone();
    let installed = manifest.get("installed").cloned().unwrap_or(json!({}));

    // Preflight: verify every original backup item exists on disk and matches hash.
    for (key, item_info) in items.as_object().unwrap() {
        if !item_info.is_object() || item_info.get("files").is_none() {
            return Err(format!(
                "REFUSE: backup manifest item '{key}' is missing required file hash data."
            ));
        }
        let parts: Vec<&str> = key.splitn(2, '/').collect();
        if parts.len() != 2 {
            return Err(format!("REFUSE: invalid item key in backup manifest: '{key}'."));
        }
        let (agent, skill) = (parts[0], parts[1]);
        let src = backup_dir.join(agent).join(skill);
        if !src.exists() {
            return Err(format!(
                "REFUSE: backup copy '{}' for originally managed skill '{key}' is missing. Rollback aborted to prevent deleting installed skills.",
                src.display()
            ));
        }
        let tree = hash_tree(&src)?;
        let want = item_info.get("files").cloned().unwrap_or(json!({}));
        if tree != want {
            return Err(format!(
                "REFUSE: backup copy '{}' does not match its manifest.",
                src.display()
            ));
        }
    }

    let mut restores: Vec<(PathBuf, PathBuf)> = Vec::new();
    let mut removals: Vec<PathBuf> = Vec::new();
    for (agent, base_dir) in agent_dirs {
        for skill in MANAGED_SKILLS {
            let key = format!("{agent}/{skill}");
            let src = backup_dir.join(agent).join(skill);
            let target = base_dir.join(skill);

            if items.get(&key).is_some() {
                if target.exists() {
                    if !target.join(MANAGED_MARKER).exists() {
                        return Err(format!(
                            "REFUSE: destination '{}' is unmanaged; rollback blocked to preserve user files.",
                            target.display()
                        ));
                    }
                    let tree = hash_tree(&target)?;
                    if installed.get(&key).is_some() && tree != installed[&key] {
                        return Err(format!(
                            "REFUSE: '{}' changed after the update; rollback blocked. Reconcile manually.",
                            target.display()
                        ));
                    }
                }
                restores.push((src, target));
            } else if installed.get(&key).is_some() && target.exists() {
                if !target.join(MANAGED_MARKER).exists() {
                    return Err(format!(
                        "REFUSE: destination '{}' is unmanaged; rollback blocked to preserve user files.",
                        target.display()
                    ));
                }
                if hash_tree(&target)? != installed[&key] {
                    return Err(format!(
                        "REFUSE: '{}' changed after the update; rollback blocked. Reconcile manually.",
                        target.display()
                    ));
                }
                removals.push(target);
            }
        }
    }

    if snapshot_first {
        let slots = slots_from_dirs(agent_dirs);
        let snapshot = backup_slots(backup_root, &slots)?;
        println!(
            "Saved current state first; undo this rollback with: --rollback {}",
            snapshot.display()
        );
    }
    for target in removals {
        remove_tree(&target)?;
        println!("Removed slot created by the update: {}", target.display());
    }
    for (src, target) in restores {
        if target.exists() {
            remove_tree(&target)?;
        }
        copy_tree(&src, &target)?;
        println!("Restored {}", target.display());
    }
    Ok(())
}

/// Strict SHA256SUMS parser: '<64 hex sha>  [*]<filename>'.
pub fn parse_checksums_file(sums_text: &str) -> ToolResult<HashMap<String, String>> {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"^([0-9a-fA-F]{64})\s+[*]?(.+)$").unwrap());
    let mut entries: HashMap<String, String> = HashMap::new();
    for line in sums_text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(caps) = re.captures(line) {
            let lower = caps[1].to_lowercase();
            // Name semantics mirror Path(fname.strip()).name.
            let fname = caps[2].trim().to_string();
            let name = match Path::new(&fname).file_name() {
                Some(n) => n.to_string_lossy().into_owned(),
                None => fname,
            };
            if let Some(existing) = entries.get(&name) {
                if *existing != lower {
                    return Err(format!(
                        "INTEGRITY_ERROR: conflicting SHA256SUMS entries for '{name}'"
                    ));
                }
            }
            entries.insert(name, lower);
        }
    }
    Ok(entries)
}

/// zipfile.infolist()-equivalent safety scan (runs before any extraction).
pub fn validate_archive_safety(zip_path: &Path) -> ToolResult<Vec<String>> {
    let file = File::open(zip_path).map_err(|e| e.to_string())?;
    let archive = zip::ZipArchive::new(file).map_err(|e| e.to_string())?;
    let mut seen: HashSet<String> = HashSet::new();
    let mut errors: Vec<String> = Vec::new();
    for name in archive.file_names() {
        let name = name.to_string();
        if !seen.insert(name.clone()) {
            errors.push(format!("Duplicate entry: {name}"));
        }
        if unsafe_path(&name) {
            errors.push(format!("Unsafe path traversal: {name}"));
        }
    }
    Ok(errors)
}

/// Python: name.startswith("/") or name.startswith("\\") or ".." in Path(name).parts
fn unsafe_path(name: &str) -> bool {
    if name.starts_with('/') || name.starts_with('\\') {
        return true;
    }
    #[cfg(target_os = "windows")]
    {
        name.split(['/', '\\']).any(|c| c == "..")
    }
    #[cfg(not(target_os = "windows"))]
    {
        Path::new(name)
            .components()
            .any(|c| matches!(c, Component::ParentDir))
    }
}

fn extract_archive(zip_path: &Path, dest: &Path) -> ToolResult<()> {
    // The zip crate sanitizes member paths during extract (traversal safety);
    // validate_archive_safety has already enforced the Python error list.
    let file = File::open(zip_path).map_err(|e| e.to_string())?;
    let mut archive = zip::ZipArchive::new(file).map_err(|e| e.to_string())?;
    archive.extract(dest).map_err(|e| e.to_string())
}

/// Values render like the Python f-string of a dict entry (strings unquoted,
/// nulls as the JSON literal since Python prints None).
fn as_str(v: Option<&Value>, default: Option<&str>) -> String {
    match v {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Null) | None => default.unwrap_or("").to_string(),
        Some(other) => other.to_string(),
    }
}

/// subprocess.run(check=True) equivalent; output inherits the terminal like the
/// Python caller. Error mirrors CalledProcessError str.
fn run_installer(node_cmd: &str, install_js: &Path, agent: &str, roles: &str) -> ToolResult<()> {
    let argv: Vec<String> = vec![
        node_cmd.to_string(),
        install_js.to_string_lossy().into_owned(),
        "--force".into(),
        "--agents".into(),
        agent.to_string(),
        "--roles".into(),
        roles.to_string(),
    ];
    let status = std::process::Command::new(node_cmd)
        .args(&argv[1..])
        .status()
        .map_err(|e| e.to_string())?;
    if !status.success() {
        let code = status.code().unwrap_or(-1);
        let quoted: Vec<String> = argv.iter().map(|a| format!("'{a}'")).collect();
        return Err(format!(
            "Command '[{}]' returned non-zero exit status {code}.",
            quoted.join(", ")
        ));
    }
    Ok(())
}

/// Entry point for `herdrt update ...`. Returns the process exit code.
pub fn cli(argv: &[String]) -> i32 {
    let mut check = false;
    let mut dry_run = false;
    let mut rollback: Option<String> = None;
    let mut list_backups = false;
    let mut prerelease = false;
    let mut force = false;
    let mut it = argv.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--check" => check = true,
            "--dry-run" => dry_run = true,
            "--rollback" => match it.next() {
                Some(v) => rollback = Some(v.clone()),
                None => {
                    eprintln!("usage: herdrt update [--check] [--dry-run] [--rollback DIR|latest] [--list-backups] [--prerelease] [--force]");
                    return 2;
                }
            },
            "--list-backups" => list_backups = true,
            "--prerelease" => prerelease = true,
            "--force" => force = true,
            "-h" | "--help" => {
                println!("usage: herdrt update [--check] [--dry-run] [--rollback DIR|latest] [--list-backups] [--prerelease] [--force]");
                return 0;
            }
            _ => {
                eprintln!("usage: herdrt update [--check] [--dry-run] [--rollback DIR|latest] [--list-backups] [--prerelease] [--force]");
                return 2;
            }
        }
    }
    run(check, dry_run, rollback.as_deref(), list_backups, prerelease, force)
}

fn run(
    check: bool,
    dry_run: bool,
    rollback: Option<&str>,
    list_backups: bool,
    prerelease: bool,
    force: bool,
) -> i32 {
    if list_backups {
        let valid_backups = match get_valid_backups() {
            Ok(v) => v,
            Err(e) => {
                eprintln!("ERROR: {e}");
                return 1;
            }
        };
        if valid_backups.is_empty() {
            println!("No local backups found.");
            return 0;
        }
        println!("Available local backups:");
        for (b, m) in &valid_backups {
            let ts = match m.get("timestamp") {
                Some(Value::String(s)) => s.clone(),
                Some(Value::Null) | None => "unknown".to_string(),
                Some(other) => other.to_string(),
            };
            let item_count = m
                .get("items")
                .and_then(|i| i.as_object())
                .map(|o| o.len())
                .unwrap_or(0);
            println!(
                "  {} -> {} ({} items, ts: {})",
                b.file_name().unwrap_or_default().to_string_lossy(),
                b.display(),
                item_count,
                ts
            );
        }
        return 0;
    }

    if let Some(rb) = rollback {
        let target_backup = if rb == "latest" {
            let valid_backups = match get_valid_backups() {
                Ok(v) => v,
                Err(e) => {
                    eprintln!("ERROR during rollback: {e}");
                    return 1;
                }
            };
            if valid_backups.is_empty() {
                eprintln!("ERROR: No valid backups available to restore.");
                return 1;
            }
            valid_backups[0].0.clone()
        } else {
            PathBuf::from(rb)
        };
        println!("Restoring from: {}...", target_backup.display());
        if let Err(e) = restore_backup(&target_backup, true) {
            eprintln!("ERROR during rollback: {e}");
            return 1;
        }
        println!("Rollback completed successfully.");
        return 0;
    }

    // Version inspection
    let installed = match get_installed_versions() {
        Ok(v) => v,
        Err(e) => {
            eprintln!("ERROR: {e}");
            return 1;
        }
    };
    println!("Installed skills inspection:");
    let mut any_installed = false;
    for (agent, skills) in &installed {
        for (skill, ver) in skills {
            if let Some(v) = ver {
                if !v.is_empty() {
                    any_installed = true;
                    println!("  [{agent}] {skill}: {v}");
                }
            }
        }
    }
    if !any_installed {
        println!("  No pack-managed skills currently detected.");
    }

    println!("\nChecking for published GitHub releases...");
    let latest = match fetch_latest_release(None, prerelease) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("ERROR: {e}");
            return 1;
        }
    };

    if latest.is_null() {
        println!(
            "Status: Development version. No published releases found on GitHub ({GITHUB_RELEASES_URL})."
        );
        println!("To install or update from current repository source, run:");
        println!("  npx . --force");
        println!("  or: .\\install.ps1 -Force");
        return 0;
    }

    let tag_name = as_str(latest.get("tag_name"), Some("unknown"));
    println!("Latest published release on GitHub: {tag_name}");
    let empty = Vec::new();
    let assets = latest.get("assets").and_then(|a| a.as_array()).unwrap_or(&empty);
    let pack_asset = assets.iter().find(|a| {
        let name = as_str(a.get("name"), Some(""));
        name.starts_with("herdr-role-pack-") && name.ends_with(".zip")
    });
    let sums_asset = assets
        .iter()
        .find(|a| a.get("name").and_then(|n| n.as_str()) == Some("SHA256SUMS"));

    if pack_asset.is_none() {
        println!("Warning: Release {tag_name} has no role-pack archive asset attached.");
        return 0;
    }

    let latest_version = parse_version(Some(&tag_name));
    let managed_versions: Vec<Option<(u32, u32, u32)>> = installed
        .iter()
        .flat_map(|(_, skills)| skills.iter())
        .filter(|(_, v)| v.as_deref().is_some_and(|s| s != "UNMANAGED"))
        .map(|(_, v)| parse_version(v.as_deref()))
        .collect();
    // Unreadable markers count as outdated, so an unknown install is always offered the update.
    let up_to_date = !managed_versions.is_empty()
        && latest_version.is_some()
        && managed_versions
            .iter()
            .all(|v| v.is_some() && v.unwrap() >= latest_version.unwrap());
    if up_to_date && !force {
        println!("\nUp to date: every managed skill is at {tag_name} or newer. Use --force to reinstall.");
        return 0;
    }

    if check {
        let html_url = as_str(latest.get("html_url"), Some("None"));
        println!("\nUpdate available: {tag_name}");
        println!("Release URL: {html_url}");
        return 0;
    }

    if dry_run {
        let pack_name = as_str(pack_asset.unwrap().get("name"), Some(""));
        let sums_name = match sums_asset.and_then(|a| a.get("name")).and_then(|n| n.as_str()) {
            Some(n) => n,
            None => "MISSING",
        };
        println!("\n[DRY-RUN] Would download: {pack_name}");
        println!("[DRY-RUN] Would verify checksum from: {sums_name}");
        let backups = backup_root().unwrap_or_else(|_| PathBuf::from("~/.herdr-backups"));
        println!("[DRY-RUN] Would back up current skills to: {}", backups.display());
        println!("[DRY-RUN] Would extract and invoke installer with --force for agents that already have the pack");
        return 0;
    }

    // Fail fast, before downloading or backing up anything
    let pack_asset = pack_asset.unwrap();
    let pack_name = as_str(pack_asset.get("name"), Some(""));
    if sums_asset.is_none() {
        eprintln!("ERROR: INTEGRITY_ERROR: Release is missing required SHA256SUMS asset.");
        return 1;
    }
    let node_cmd = match util::which("node") {
        Some(p) => p.to_string_lossy().into_owned(),
        None => {
            eprintln!("ERROR: Node.js runtime is required for updating. Please install Node.js.");
            return 1;
        }
    };
    // Update only the agents that already have the pack.
    let mut agents: Vec<String> = match managed_slots() {
        Ok(slots) => slots,
        Err(e) => {
            eprintln!("ERROR: {e}");
            return 1;
        }
    }
    .into_iter()
    .map(|s| s.0)
    .collect();
    agents = agents
        .into_iter()
        .map(|key| key.split('/').next().unwrap_or_default().to_string())
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    agents.sort();
    if agents.is_empty() {
        eprintln!("ERROR: No pack-managed installation found. Install first (see README).");
        return 1;
    }

    let temp_dir = match tempfile::Builder::new()
        .prefix("herdr-update-")
        .tempdir()
    {
        Ok(t) => t,
        Err(e) => {
            eprintln!("ERROR: Update failed: {e}");
            return 1;
        }
    };
    let temp_path = temp_dir.path().to_path_buf();
    let mut backup_dir: Option<PathBuf> = None;
    let result = (|| -> ToolResult<()> {
        let archive_path = temp_path.join(&pack_name);
        println!("Downloading {pack_name}...");
        download_asset(pack_asset, &archive_path, None)?;

        let sums_path = temp_path.join("SHA256SUMS");
        let sums_asset = sums_asset.unwrap();
        download_asset(sums_asset, &sums_path, None)?;
        let sums_text = std::fs::read_to_string(&sums_path).map_err(|e| e.to_string())?;
        let parsed_sums = parse_checksums_file(&sums_text)?;

        let expected_hash = parsed_sums.get(&pack_name).cloned().ok_or_else(|| {
            format!("INTEGRITY_ERROR: No checksum entry found for '{pack_name}' in SHA256SUMS.")
        })?;

        let actual_hash = util::sha256_file(&archive_path)?;
        if !util::digest_eq(&actual_hash.to_lowercase(), &expected_hash.to_lowercase()) {
            return Err(format!(
                "INTEGRITY_ERROR: Checksum mismatch for {pack_name}! Expected {expected_hash}, got {actual_hash}"
            ));
        }
        println!("Checksum verified successfully.");

        // Validate archive layout safety
        let safety_errors = validate_archive_safety(&archive_path)?;
        if !safety_errors.is_empty() {
            let quoted: Vec<String> = safety_errors.iter().map(|e| format!("'{e}'")).collect();
            return Err(format!("Archive safety check failed: [{}]", quoted.join(", ")));
        }

        let extract_dir = temp_path.join("extracted");
        extract_archive(&archive_path, &extract_dir)?;

        // Pre-update backup created right before execution
        println!("\nInitiating safe update to {tag_name}...");
        let bd = backup_managed_skills()?;
        backup_dir = Some(bd.clone());
        println!("Saved pre-update backup: {}", bd.display());

        // Run extracted node installer preserving installed agents and roles
        let install_js = extract_dir.join("bin").join("install.js");
        if !install_js.exists() {
            let message = format!("Missing installer inside archive: {}", install_js.display());
            return Err(std::io::Error::new(std::io::ErrorKind::NotFound, message).to_string());
        }

        let manifest = read_manifest(&bd)?;
        let mut agent_roles: BTreeMap<String, Vec<String>> = BTreeMap::new();
        if let Some(items) = manifest.get("items").and_then(|i| i.as_object()) {
            for key in items.keys() {
                let parts: Vec<&str> = key.splitn(2, '/').collect();
                if parts.len() == 2 {
                    let list = agent_roles.entry(parts[0].to_string()).or_default();
                    if !list.iter().any(|r| r == parts[1]) {
                        list.push(parts[1].to_string());
                    }
                }
            }
        }

        for (agent, roles) in agent_roles {
            run_installer(&node_cmd, &install_js, &agent, &roles.join(","))?;
        }
        // Record what the update installed so a later rollback can detect user edits.
        let mut manifest = manifest;
        if let Value::Object(ref mut map) = manifest {
            let mut installed_map = Map::new();
            for (key, path) in managed_slots()? {
                installed_map.insert(key, hash_tree(&path)?);
            }
            map.insert("installed".to_string(), Value::Object(installed_map));
        }
        write_manifest(&bd, &manifest)?;
        println!("Update to {tag_name} completed successfully.");
        Ok(())
    })();

    if let Err(e) = result {
        eprintln!("ERROR: Update failed: {e}");
        if let Some(bd) = &backup_dir {
            if bd.exists() {
                eprintln!("Restoring from backup...");
                match restore_backup(bd, false) {
                    Ok(()) => {
                        eprintln!("Original installation restored cleanly.");
                    }
                    Err(re) => {
                        eprintln!("CRITICAL: Rollback encountered an error: {re}");
                    }
                }
            }
        }
        return 1;
    }
    // temp_dir drops: shutil.rmtree(temp_dir, ignore_errors=True)
    drop(temp_dir);
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "herdrt-update-test-{tag}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn parse_version_table() {
        assert_eq!(parse_version(Some("Herdr Role Pack v2.8.0")), Some((2, 8, 0)));
        assert_eq!(parse_version(Some("Herdr Role Pack v2.8")), Some((2, 8, 0)));
        assert_eq!(parse_version(Some("v2.8")), Some((2, 8, 0)));
        assert_eq!(parse_version(Some("v2.8.0")), Some((2, 8, 0)));
        assert_eq!(parse_version(Some("v10.2.3")), Some((10, 2, 3)));
        assert_eq!(parse_version(Some("")), None);
        assert_eq!(parse_version(Some("/")), None);
        assert_eq!(parse_version(Some("unknown")), None);
        assert_eq!(parse_version(Some("2.9.1-rc1")), None);
        assert_eq!(parse_version(None), None);
    }

    #[test]
    fn sha256sums_parse_valid_conflict_and_comments() {
        let e = "e".repeat(64);
        let a = "A".repeat(64);
        let sums = format!(
            "# comment\n{e}  herdr-role-pack-v2.8.0.zip\n{a} *herdr-controller-v2.8.0.zip\n"
        );
        let parsed = parse_checksums_file(&sums).unwrap();
        assert_eq!(parsed.get("herdr-role-pack-v2.8.0.zip").unwrap(), &e.to_lowercase());
        assert_eq!(parsed.get("herdr-controller-v2.8.0.zip").unwrap(), &"a".repeat(64));

        // Conflicting entries for the same name raise the verbatim INTEGRITY_ERROR.
        let conflict = format!("{}  x.zip\n{}  x.zip\n", "a".repeat(64), "b".repeat(64));
        let err = parse_checksums_file(&conflict).unwrap_err();
        assert!(err.contains("conflicting SHA256SUMS entries for 'x.zip'"), "got: {err}");

        // Non-checksum lines silently ignored; binary marker handled above.
        assert!(parse_checksums_file("not a checksum line\n").unwrap().is_empty());
    }

    #[test]
    fn backup_manifest_round_trip_and_restore() {
        let root = temp_root("roundtrip");
        let home = root.join("home");
        let backups = root.join("backups");
        let skills_dir = home.join(".claude").join("skills");
        fs::create_dir_all(&skills_dir).unwrap();

        let skill = skills_dir.join(MANAGED_SKILLS[1]); // herdr-manager
        fs::create_dir_all(&skill).unwrap();
        fs::write(skill.join("SKILL.md"), "v1 body").unwrap();
        fs::write(skill.join(MANAGED_MARKER), "MANAGED").unwrap();

        let dirs = vec![("claude".to_string(), skills_dir.clone())];
        let slots = slots_from_dirs(&dirs);
        assert_eq!(slots.len(), 1);
        assert_eq!(slots[0].0, "claude/herdr-manager");

        let backup_dir = backup_slots(&backups, &slots).unwrap();
        let manifest = read_manifest(&backup_dir).unwrap();
        let item = &manifest["items"]["claude/herdr-manager"];
        assert!(item.is_object());
        assert!(item["files"].is_object());
        assert_eq!(
            item["files"]["SKILL.md"].as_str().unwrap(),
            util::sha256_file(&skill.join("SKILL.md")).unwrap()
        );

        // Restore: edited target restored to the original backup content.
        fs::write(skill.join("SKILL.md"), "v9 body").unwrap();
        restore_backup_core(&backup_dir, false, &dirs, &backups).unwrap();
        assert_eq!(fs::read_to_string(skill.join("SKILL.md")).unwrap(), "v1 body");

        // Tampered backup copy -> REFUSE with the verbatim message.
        fs::write(
            backup_dir.join("claude").join(MANAGED_SKILLS[1]).join("SKILL.md"),
            "tampered",
        )
        .unwrap();
        let err = restore_backup_core(&backup_dir, false, &dirs, &backups).unwrap_err();
        assert!(
            err.starts_with("REFUSE:") && err.contains("does not match its manifest"),
            "got: {err}"
        );

        // Missing backup copy -> REFUSE, and the installed slot was NOT touched.
        fs::write(skill.join("SKILL.md"), "v1 body").unwrap();
        fs::remove_dir_all(backup_dir.join("claude").join(MANAGED_SKILLS[1])).unwrap();
        let err = restore_backup_core(&backup_dir, false, &dirs, &backups).unwrap_err();
        assert!(
            err.starts_with("REFUSE: backup copy") && err.contains("is missing"),
            "got: {err}"
        );
        assert_eq!(fs::read_to_string(skill.join("SKILL.md")).unwrap(), "v1 body");

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn archive_safety_scan() {
        let root = temp_root("safety");
        let zip_path = root.join("evil.zip");
        {
            let file = fs::File::create(&zip_path).unwrap();
            let mut w = zip::ZipWriter::new(file);
            w.start_file("../escaped.txt", zip::write::SimpleFileOptions::default()).unwrap();
            w.write_all(b"evil").unwrap();
            w.finish().unwrap();
        }
        let errors = validate_archive_safety(&zip_path).unwrap();
        assert!(
            errors.iter().any(|e| e.to_lowercase().contains("traversal")),
            "got: {errors:?}"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn get_valid_backups_ordering_and_invalid_dirs() {
        let root = temp_root("validity");
        let backups = root.join("backups");
        // Valid manifest.
        let b1 = backups.join("backup-20260101-000000-000001");
        fs::create_dir_all(&b1).unwrap();
        write_manifest(&b1, &json!({"timestamp": "20260101-000000-000001", "items": {}})).unwrap();
        // Newer dir without a manifest must be ignored (mirrors the node-install-backup case).
        let b2 = backups.join("install-backup-9999999999999");
        fs::create_dir_all(&b2).unwrap();
        fs::write(b2.join("some_file.txt"), "node backup data").unwrap();
        // Stale one.
        let b3 = backups.join("backup-20250101-000000-000001");
        fs::create_dir_all(&b3).unwrap();
        write_manifest(&b3, &json!({"timestamp": "20250101-000000-000001", "items": {}})).unwrap();

        let valid = get_valid_backups_in(&backups).unwrap();
        assert_eq!(valid.len(), 2);
        assert_eq!(valid[0].0.file_name().unwrap(), "backup-20260101-000000-000001");
        let _ = fs::remove_dir_all(&root);
    }
}
