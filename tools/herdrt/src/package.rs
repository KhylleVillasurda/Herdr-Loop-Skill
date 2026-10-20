//! Port of tools/package.py: deterministic release packaging for Herdr Role Pack.
//!
//! Builds `herdr-role-pack-v<version>.zip`, `herdr-controller-v<version>.zip` and
//! `SHA256SUMS` with fixed timestamps (2026-01-01 00:00:00), sorted members,
//! LF-normalized text files and fixed permissions (0o755 scripts/bin, 0o644 rest).
//! The deflate byte stream may differ from the Python build's zlib output — the
//! published SHA256SUMS, not a rebuild, is the integrity authority.

use crate::util::{self, ToolResult};
use serde_json::Value;
use std::collections::HashSet;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, DateTime, ZipArchive, ZipWriter};

const MANDATORY_ROOT_FILES: [&str; 9] = [
    "LICENSE",
    "manifest.json",
    "package.json",
    "README.md",
    "INSTALL-FOR-AI.md",
    "CHANGELOG.md",
    "install.ps1",
    "install.sh",
    "bin/install.js",
];

const MANDATORY_DIRS: [&str; 4] = ["skills", "tools", "examples", "docs"];

const TEXT_EXTENSIONS: [&str; 9] = [".md", ".json", ".js", ".sh", ".ps1", ".py", ".yml", ".yaml", ".txt"];

/// Path components that must never appear inside a release archive.
const FORBIDDEN_PATH_PARTS: [&str; 6] = [".herdr-loop", ".sqlite-tracer", ".git", ".venv", "venv", ".env"];

const PROG: &str = "package.py";
const USAGE_ARGS: &str = "[-h] [--version VERSION] [--output-dir OUTPUT_DIR] [--dry-run]";
const USAGE_INDENT: usize = 18; // len("usage: package.py ")

/// argparse-style greedily wrapped usage line (width 80 when COLUMNS is unset;
/// the formatter reserves 2 columns, so lines break before exceeding width-2).
fn usage_string() -> String {
    let width = std::env::var("COLUMNS")
        .ok()
        .and_then(|c| c.trim().parse::<usize>().ok())
        .unwrap_or(80)
        .saturating_sub(2)
        .max(USAGE_INDENT + 11);
    let mut lines: Vec<String> = Vec::new();
    let mut cur = format!("usage: {PROG}");
    let mut cur_len = cur.chars().count();
    for word in USAGE_ARGS.split(' ') {
        let wlen = word.chars().count();
        if cur_len > USAGE_INDENT - 1 && cur_len + 1 + wlen > width {
            cur.truncate(cur_len);
            lines.push(std::mem::take(&mut cur));
            cur = format!("{}{}", " ".repeat(USAGE_INDENT), word);
            cur_len = USAGE_INDENT + wlen;
        } else {
            cur.push(' ');
            cur.push_str(word);
            cur_len += 1 + wlen;
        }
    }
    lines.push(cur);
    lines.join("\n")
}
const HELP_TEXT: &str = "\n\
\n\
Build and validate Herdr Role Pack release archives.\n\
\n\
options:\n\
 \x20 -h, --help            show this help message and exit\n\
 \x20 --version VERSION     Explicit version to validate against manifest.json\n\
 \x20 --output-dir OUTPUT_DIR\n\
 \x20                       Output directory for archives (default: dist/)\n\
 \x20 --dry-run             Validate source files without creating output archives\n";

/// argparse-style help text: wrapped usage line plus the fixed option block.
fn help_text() -> String {
    format!("{}{}", usage_string(), HELP_TEXT)
}

/// Repo root (…/tools/herdrt/../..), the package.py `ROOT` equivalent.
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("CARGO_MANIFEST_DIR must have two parent components")
        .to_path_buf()
}

struct Args {
    version: Option<String>,
    output_dir: Option<String>,
    dry_run: bool,
}

enum CliParse {
    Args(Args),
    Help,
    Error(String),
}

fn parse_cli(argv: &[String]) -> CliParse {
    let mut args = Args { version: None, output_dir: None, dry_run: false };
    let mut i = 0;
    while i < argv.len() {
        let a = argv[i].as_str();
        match a {
            "-h" | "--help" => return CliParse::Help,
            "--dry-run" => {
                args.dry_run = true;
                i += 1;
            }
            "--version" | "--output-dir" => {
                let name = a;
                i += 1;
                if i >= argv.len() || argv[i].starts_with('-') {
                    return CliParse::Error(format!(
                        "{}\n{PROG}: error: argument {name}: expected one argument\n", usage_string()
                    ));
                }
                if name == "--version" {
                    args.version = Some(argv[i].clone());
                } else {
                    args.output_dir = Some(argv[i].clone());
                }
                i += 1;
            }
            _ => {
                if let Some(v) = a.strip_prefix("--version=") {
                    args.version = Some(v.to_string());
                } else if let Some(v) = a.strip_prefix("--output-dir=") {
                    args.output_dir = Some(v.to_string());
                } else {
                    return CliParse::Error(format!(
                        "{}\n{PROG}: error: unrecognized arguments: {}\n", usage_string(),
                        argv[i..].join(" ")
                    ));
                }
                i += 1;
            }
        }
    }
    CliParse::Args(args)
}

fn get_canonical_version(root: &Path) -> ToolResult<String> {
    let manifest_path = root.join("manifest.json");
    if !manifest_path.exists() {
        return Err(format!(
            "FileNotFoundError: Missing manifest file: {}",
            manifest_path.display()
        ));
    }
    let text = fs::read_to_string(&manifest_path).map_err(|e| e.to_string())?;
    let data: Value = serde_json::from_str(&text).map_err(|e| format!("JSONDecodeError: {e}"))?;
    Ok(match data.get("version") {
        Some(Value::String(s)) => s.clone(),
        Some(v) => v.to_string(),
        None => String::new(),
    })
}

/// Python `data.replace(b"\r\n", b"\n")` for byte vectors.
fn normalize_lf(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut i = 0;
    while i < data.len() {
        if data[i] == b'\r' && data.get(i + 1) == Some(&b'\n') {
            out.push(b'\n');
            i += 2;
        } else {
            out.push(data[i]);
            i += 1;
        }
    }
    out
}

/// Adds a file to the zip with fixed timestamp, permissions and LF line endings.
fn add_file_deterministic(zf: &mut ZipWriter<File>, file_path: &Path, arcname: &str) -> ToolResult<()> {
    let mut data = fs::read(file_path).map_err(|e| e.to_string())?;
    let ext_raw = file_path
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    let ext = ext_raw.to_ascii_lowercase();
    let fname = file_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    if TEXT_EXTENSIONS.contains(&ext.as_str()) || fname == "LICENSE" || fname == "SHA256SUMS" {
        data = normalize_lf(&data);
    }
    // The exec check uses the raw suffix like Python (`suffix in (".sh", ".ps1")`).
    let is_exec = ext_raw == ".sh" || ext_raw == ".ps1" || arcname.starts_with("bin/");
    let perm = if is_exec { 0o755u32 } else { 0o644u32 };
    let options = SimpleFileOptions::default()
        .compression_method(CompressionMethod::Deflated)
        .last_modified_time(
            DateTime::from_date_and_time(2026, 1, 1, 0, 0, 0)
                .map_err(|e| format!("zip DateTime error: {e}"))?,
        )
        .unix_permissions(perm);
    zf.start_file(arcname, options).map_err(|e| e.to_string())?;
    zf.write_all(&data).map_err(|e| e.to_string())?;
    Ok(())
}

/// Inspects a built ZIP for path traversals, absolute paths, duplicates and
/// prohibited files. Returns Python-parity error strings.
fn validate_archive_members(zip_path: &Path) -> ToolResult<Vec<String>> {
    let file = File::open(zip_path).map_err(|e| e.to_string())?;
    let mut archive = ZipArchive::new(file).map_err(|e| format!("zip read error: {e}"))?;
    let mut errors = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for i in 0..archive.len() {
        let entry = archive.by_index(i).map_err(|e| e.to_string())?;
        let name = entry.name().to_string();
        if !seen.insert(name.clone()) {
            errors.push(format!("Duplicate archive entry: {name}"));
        }
        // Path(name).parts on posix only splits on '/'.
        let parts: Vec<&str> = name.split('/').collect();
        if name.starts_with('/') || name.starts_with('\\') || parts.iter().any(|p| *p == "..") {
            errors.push(format!("Unsafe path in archive: {name}"));
        }
        if parts.iter().any(|p| {
            FORBIDDEN_PATH_PARTS.contains(p) || p.ends_with(".pyc") || p.ends_with(".pyo")
        }) {
            errors.push(format!("Forbidden file pattern in archive: {name}"));
        }
        if parts.iter().any(|p| *p == "__pycache__") {
            errors.push(format!("Cache directory in archive: {name}"));
        }
    }
    Ok(errors)
}

/// os.walk equivalent: sorted entries, filtered dirs (excludes + dot-prefixed),
/// filtered files (skip list + .pyc/.pyo). Symlinked dirs are listed but not
/// followed, matching os.walk(followlinks=False).
fn walk_collect(
    dir: &Path,
    base: &Path,
    skip_dirs: &[&str],
    skip_files: &[&str],
    collected: &mut Vec<(PathBuf, String)>,
) -> ToolResult<()> {
    let mut entries: Vec<std::fs::DirEntry> = fs::read_dir(dir)
        .map_err(|e| e.to_string())?
        .collect::<Result<_, _>>()
        .map_err(|e| e.to_string())?;
    entries.sort_by(|a, b| a.file_name().cmp(&b.file_name()));
    for entry in entries {
        let name = entry.file_name().to_string_lossy().into_owned();
        let ft = entry.file_type().map_err(|e| e.to_string())?;
        let path = entry.path();
        let is_dir = ft.is_dir()
            || (ft.is_symlink() && fs::metadata(&path).map(|m| m.is_dir()).unwrap_or(false));
        if is_dir {
            if skip_dirs.contains(&name.as_str()) || name.starts_with('.') {
                continue;
            }
            if ft.is_dir() {
                walk_collect(&path, base, skip_dirs, skip_files, collected)?;
            }
        } else if skip_files.contains(&name.as_str())
            || name.ends_with(".pyc")
            || name.ends_with(".pyo")
        {
            continue;
        } else {
            let rel = path.strip_prefix(base).map_err(|e| e.to_string())?;
            let arcname = rel
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/");
            collected.push((path, arcname));
        }
    }
    Ok(())
}

/// Builds the complete herdr-role-pack ZIP archive deterministically.
pub fn build_role_pack_archive(root: &Path, output_path: &Path) -> ToolResult<()> {
    if let Some(parent) = output_path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let file = File::create(output_path).map_err(|e| e.to_string())?;
    let mut zf = ZipWriter::new(file);

    // 1. Root files sorted
    let mut root_files = MANDATORY_ROOT_FILES.to_vec();
    root_files.sort_unstable();
    for rel in root_files {
        let src = root.join(rel);
        if !src.exists() {
            return Err(format!(
                "FileNotFoundError: Missing required file for role pack: {}",
                src.display()
            ));
        }
        add_file_deterministic(&mut zf, &src, rel)?;
    }

    // 2. Collect all directory files, then write them sorted by arcname
    let skip_dirs = ["__pycache__", ".git", ".herdr-loop", ".sqlite-tracer", "tests", "target"]; // target = cargo build output (tools/herdrt)
    let skip_files = [".DS_Store", "Thumbs.db"];
    let mut collected: Vec<(PathBuf, String)> = Vec::new();
    let mut dirs = MANDATORY_DIRS.to_vec();
    dirs.sort_unstable();
    for rel_dir in dirs {
        let dir = root.join(rel_dir);
        if !dir.is_dir() {
            continue;
        }
        walk_collect(&dir, root, &skip_dirs, &skip_files, &mut collected)?;
    }
    collected.sort_by(|a, b| a.1.cmp(&b.1));
    for (path, arcname) in collected {
        add_file_deterministic(&mut zf, &path, &arcname)?;
    }

    zf.finish().map_err(|e| e.to_string())?;
    Ok(())
}

/// Builds a standalone controller skill archive deterministically.
pub fn build_controller_archive(root: &Path, output_path: &Path) -> ToolResult<()> {
    let controller_dir = root.join("skills").join("herdr-engineering-loop");
    if !controller_dir.exists() {
        return Err(format!(
            "FileNotFoundError: Controller skill directory not found: {}",
            controller_dir.display()
        ));
    }
    if let Some(parent) = output_path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let file = File::create(output_path).map_err(|e| e.to_string())?;
    let mut zf = ZipWriter::new(file);
    let skip_dirs = ["__pycache__"];
    let skip_files: [&str; 0] = [];
    let mut collected: Vec<(PathBuf, String)> = Vec::new();
    walk_collect(&controller_dir, &controller_dir, &skip_dirs, &skip_files, &mut collected)?;
    collected.sort_by(|a, b| a.1.cmp(&b.1));
    for (path, arcname) in collected {
        add_file_deterministic(&mut zf, &path, &arcname)?;
    }
    zf.finish().map_err(|e| e.to_string())?;
    Ok(())
}

/// Verifies that all required root files and directories exist.
fn check_input_completeness(root: &Path) -> Vec<String> {
    let mut missing = Vec::new();
    for rel_file in MANDATORY_ROOT_FILES {
        if !root.join(rel_file).exists() {
            missing.push(format!("Missing file: {rel_file}"));
        }
    }
    for rel_dir in MANDATORY_DIRS {
        if !root.join(rel_dir).is_dir() {
            missing.push(format!("Missing directory: {rel_dir}"));
        }
    }
    missing
}

/// pathlib.resolve equivalent for existent/never-created paths: absolutize,
/// then lexically drop "." and resolve "..". Unlike pathlib it does not follow
/// symlinks (unresolved paths here never contain them in practice).
fn resolve_path(p: &Path) -> PathBuf {
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(p)
    };
    let mut out = PathBuf::new();
    for comp in abs.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            c => out.push(c.as_os_str()),
        }
    }
    out
}

fn run(root: &Path, args: Args, out: &mut impl Write, err_out: &mut impl Write) -> i32 {
    let canonical = match get_canonical_version(root) {
        Ok(v) => v,
        Err(e) => {
            let _ = writeln!(err_out, "{e}");
            return 1;
        }
    };

    if let Some(v) = &args.version {
        if v.as_str() != canonical.as_str() {
            let _ = writeln!(
                err_out,
                "ERROR: Specified version '{v}' does not match manifest.json ('{canonical}')"
            );
            return 1;
        }
    }

    let version = canonical;
    let output_dir = resolve_path(
        &args
            .output_dir
            .map(PathBuf::from)
            .unwrap_or_else(|| root.join("dist")),
    );

    // Preflight input files check
    let missing = check_input_completeness(root);
    if !missing.is_empty() {
        let _ = writeln!(err_out, "ERROR: Incomplete release inputs:");
        for m in missing {
            let _ = writeln!(err_out, "  - {m}");
        }
        return 1;
    }

    if args.dry_run {
        let _ = writeln!(out, "[DRY-RUN] Validated manifest version: {version}");
        let _ = writeln!(
            out,
            "[DRY-RUN] Verified presence of all mandatory root files and directories (including LICENSE)."
        );
        let _ = writeln!(out, "[DRY-RUN] Archive targets would be placed in: {}", output_dir.display());
        return 0;
    }

    if let Err(e) = fs::create_dir_all(&output_dir) {
        let _ = writeln!(err_out, "{e}");
        return 1;
    }

    let role_pack_name = format!("herdr-role-pack-v{version}.zip");
    let controller_name = format!("herdr-controller-v{version}.zip");
    let role_pack_zip = output_dir.join(&role_pack_name);
    let controller_zip = output_dir.join(&controller_name);
    let sums_file = output_dir.join("SHA256SUMS");

    let _ = writeln!(out, "Building {role_pack_name}...");
    let errors = match build_role_pack_archive(root, &role_pack_zip) {
        Ok(()) => validate_archive_members(&role_pack_zip),
        Err(e) => Err(e),
    };
    let errors = match errors {
        Ok(errors) => errors,
        Err(e) => {
            let _ = writeln!(err_out, "{e}");
            return 1;
        }
    };
    if !errors.is_empty() {
        let _ = writeln!(err_out, "ERROR: Archive validation failed for {role_pack_name}:");
        for e in errors {
            let _ = writeln!(err_out, "  - {e}");
        }
        return 1;
    }

    let _ = writeln!(out, "Building {controller_name}...");
    let errors = match build_controller_archive(root, &controller_zip) {
        Ok(()) => validate_archive_members(&controller_zip),
        Err(e) => Err(e),
    };
    let errors = match errors {
        Ok(errors) => errors,
        Err(e) => {
            let _ = writeln!(err_out, "{e}");
            return 1;
        }
    };
    if !errors.is_empty() {
        let _ = writeln!(err_out, "ERROR: Archive validation failed for {controller_name}:");
        for e in errors {
            let _ = writeln!(err_out, "  - {e}");
        }
        return 1;
    }

    let _ = writeln!(out, "Generating SHA256SUMS...");
    let role_hash = match util::sha256_file(&role_pack_zip) {
        Ok(h) => h,
        Err(e) => {
            let _ = writeln!(err_out, "{e}");
            return 1;
        }
    };
    let ctrl_hash = match util::sha256_file(&controller_zip) {
        Ok(h) => h,
        Err(e) => {
            let _ = writeln!(err_out, "{e}");
            return 1;
        }
    };
    let sums = format!("{role_hash}  {role_pack_name}\n{ctrl_hash}  {controller_name}\n");
    if let Err(e) = fs::write(&sums_file, sums.as_bytes()) {
        let _ = writeln!(err_out, "{e}");
        return 1;
    }

    let _ = writeln!(out, "Successfully packaged version {version}:");
    for line in [format!("{role_hash}  {role_pack_name}"), format!("{ctrl_hash}  {controller_name}")] {
        let _ = writeln!(out, "  {line}");
    }
    0
}

pub fn cli(argv: &[String]) -> i32 {
    let root = repo_root();
    match parse_cli(argv) {
        CliParse::Help => {
            print!("{}", help_text());
            0
        }
        CliParse::Error(msg) => {
            eprint!("{msg}");
            2
        }
        CliParse::Args(args) => run(&root, args, &mut std::io::stdout(), &mut std::io::stderr()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    struct Fixture {
        _tmp: tempfile::TempDir,
        root: PathBuf,
    }

    fn fixture() -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("repo");
        fs::create_dir_all(&root).unwrap();
        for f in MANDATORY_ROOT_FILES {
            let p = root.join(f);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            // manifest.json gets valid JSON; other root files get generic CRLF text.
            let bytes: &[u8] = if f == "manifest.json" {
                br#"{"name": "herdr", "version": "1.2.3"}"#
            } else {
                b"content\r\nline2\r\n"
            };
            fs::write(&p, bytes).unwrap();
        }
        for d in MANDATORY_DIRS {
            fs::create_dir_all(root.join(d)).unwrap();
        }
        Fixture { _tmp: tmp, root }
    }

    /// (a) Missing mandatory file: preflight message and builder error message.
    #[test]
    fn test_mandatory_file_missing_message() {
        let fx = fixture();
        fs::remove_file(fx.root.join("LICENSE")).unwrap();
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run(&fx.root, Args { version: None, output_dir: None, dry_run: true }, &mut out, &mut err);
        assert_eq!(code, 1);
        assert_eq!(
            String::from_utf8(err).unwrap(),
            "ERROR: Incomplete release inputs:\n  - Missing file: LICENSE\n"
        );
        assert!(out.is_empty());

        // Restore LICENSE so the builder surfaces the README.md gap instead.
        fs::write(fx.root.join("LICENSE"), b"content\r\n").unwrap();
        fs::remove_file(fx.root.join("README.md")).unwrap();
        let e = build_role_pack_archive(&fx.root, &fx.root.join("never.zip")).unwrap_err();
        assert_eq!(
            e,
            format!(
                "FileNotFoundError: Missing required file for role pack: {}",
                fx.root.join("README.md").display()
            )
        );
    }

    /// (b) Archive round trip: member order, perms, LF normalization, timestamp.
    #[test]
    fn test_archive_round_trip_metadata() {
        let fx = fixture();
        fs::create_dir_all(fx.root.join("skills/herdr-engineering-loop")).unwrap();
        fs::write(fx.root.join("skills/herdr-engineering-loop/SKILL.md"), b"# skill\r\nbody\r\n").unwrap();
        fs::write(fx.root.join("examples/notes.bin"), b"keep\r\nme\r\n").unwrap();
        let out_zip = fx._tmp.path().join("pack.zip");
        build_role_pack_archive(&fx.root, &out_zip).unwrap();

        let file = File::open(&out_zip).unwrap();
        let mut ar = ZipArchive::new(file).unwrap();

        // Member ordering: sorted root files, then collected files sorted by arcname.
        let mut expected_root = MANDATORY_ROOT_FILES.to_vec();
        expected_root.sort_unstable();
        let mut expected = expected_root;
        expected.extend(["examples/notes.bin", "skills/herdr-engineering-loop/SKILL.md"]);
        assert_eq!(ar.len(), expected.len());
        for (i, name) in expected.iter().enumerate() {
            assert_eq!(ar.by_index(i).unwrap().name(), *name, "member {i}");
        }

        let mut mode = |n: &str| ar.by_name(n).unwrap().unix_mode().unwrap() & 0o7777;
        assert_eq!(mode("install.sh"), 0o755);
        assert_eq!(mode("install.ps1"), 0o755);
        assert_eq!(mode("bin/install.js"), 0o755);
        assert_eq!(mode("manifest.json"), 0o644);
        assert_eq!(mode("examples/notes.bin"), 0o644);

        // LF normalization for text extensions only.
        let mut buf = Vec::new();
        ar.by_name("install.sh").unwrap().read_to_end(&mut buf).unwrap();
        assert_eq!(buf, b"content\nline2\n");
        buf.clear();
        ar.by_name("examples/notes.bin").unwrap().read_to_end(&mut buf).unwrap();
        assert_eq!(buf, b"keep\r\nme\r\n");

        // Fixed canonical timestamp 2026-01-01 00:00:00.
        let dt = ar.by_name("manifest.json").unwrap().last_modified().unwrap();
        assert_eq!(
            (dt.year(), dt.month(), dt.day(), dt.hour(), dt.minute(), dt.second()),
            (2026, 1, 1, 0, 0, 0)
        );

        // Deterministic across rebuilds.
        let out_zip2 = fx._tmp.path().join("pack2.zip");
        build_role_pack_archive(&fx.root, &out_zip2).unwrap();
        assert_eq!(
            util::sha256_file(&out_zip).unwrap(),
            util::sha256_file(&out_zip2).unwrap()
        );
    }

    /// (c) Version mismatch: exact message, exit 1.
    #[test]
    fn test_version_mismatch_message() {
        let fx = fixture();
        let mut out = Vec::new();
        let mut err = Vec::new();
        let args = Args { version: Some("9.9.9".into()), output_dir: None, dry_run: false };
        let code = run(&fx.root, args, &mut out, &mut err);
        assert_eq!(code, 1);
        assert_eq!(
            String::from_utf8(err).unwrap(),
            "ERROR: Specified version '9.9.9' does not match manifest.json ('1.2.3')\n"
        );
        assert!(out.is_empty());
    }

    /// (d) Dry run: exit 0, exact output, nothing written.
    #[test]
    fn test_dry_run_writes_nothing() {
        let fx = fixture();
        let target = fx._tmp.path().join("dist");
        let mut out = Vec::new();
        let mut err = Vec::new();
        let args = Args {
            version: None,
            output_dir: Some(target.to_string_lossy().into_owned()),
            dry_run: true,
        };
        let code = run(&fx.root, args, &mut out, &mut err);
        assert_eq!(code, 0, "stderr: {}", String::from_utf8_lossy(&err));
        assert_eq!(
            String::from_utf8(out).unwrap(),
            format!(
                "[DRY-RUN] Validated manifest version: 1.2.3\n\
                 [DRY-RUN] Verified presence of all mandatory root files and directories (including LICENSE).\n\
                 [DRY-RUN] Archive targets would be placed in: {}\n",
                target.display()
            )
        );
        assert!(!target.exists());
    }
}
