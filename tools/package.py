#!/usr/bin/env python3
"""
Deterministic packaging and archive validation utility for Herdr Role Pack.
Creates release ZIP archives, validates archive safety, and calculates SHA256 checksums.
Archives are deterministic for a given source tree (fixed timestamps, sorted members,
LF line endings, fixed permissions). Cross-OS byte identity also depends on the zlib
build, so the published SHA256SUMS, not a rebuild, is the integrity authority.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import sys
import zipfile
from pathlib import Path


ROOT = Path(__file__).resolve().parent.parent

MANDATORY_ROOT_FILES = [
    "LICENSE",
    "manifest.json",
    "package.json",
    "README.md",
    "INSTALL-FOR-AI.md",
    "CHANGELOG.md",
    "install.ps1",
    "install.sh",
    "bin/install.js",
]

MANDATORY_DIRS = [
    "skills",
    "tools",
    "examples",
    "docs",
]


def get_canonical_version() -> str:
    manifest_path = ROOT / "manifest.json"
    if not manifest_path.exists():
        raise FileNotFoundError(f"Missing manifest file: {manifest_path}")
    data = json.loads(manifest_path.read_text(encoding="utf-8"))
    return str(data.get("version", ""))


def calculate_sha256(file_path: Path) -> str:
    hasher = hashlib.sha256()
    with open(file_path, "rb") as f:
        while chunk := f.read(65536):
            hasher.update(chunk)
    return hasher.hexdigest()


def add_file_deterministic(zf: zipfile.ZipFile, file_path: Path, arcname: str) -> None:
    """Adds a file to zip with normalized timestamps, permissions, and line endings for reproducibility."""
    data = file_path.read_bytes()
    # Normalize line endings for text files so git autocrlf settings across OS produce identical archives
    text_extensions = {".md", ".json", ".js", ".sh", ".ps1", ".py", ".yml", ".yaml", ".txt"}
    if file_path.suffix.lower() in text_extensions or file_path.name in ("LICENSE", "SHA256SUMS"):
        data = data.replace(b"\r\n", b"\n")

    # Canonical fixed timestamp (2026-01-01 00:00:00)
    zinfo = zipfile.ZipInfo(filename=arcname, date_time=(2026, 1, 1, 0, 0, 0))
    # Normalized permissions (0o755 for scripts/executables, 0o644 for regular files)
    is_exec = file_path.suffix in (".sh", ".ps1") or arcname.startswith("bin/")
    perm = 0o755 if is_exec else 0o644
    zinfo.external_attr = (perm & 0xFFFF) << 16
    zinfo.compress_type = zipfile.ZIP_DEFLATED
    zf.writestr(zinfo, data)


def validate_archive_members(zip_path: Path) -> list[str]:
    """Inspects ZIP archive to verify there are no path traversals, absolute paths, or prohibited files."""
    errors: list[str] = []
    forbidden_prefixes = (".herdr-loop", ".sqlite-tracer", ".git", ".venv", "venv", ".env")

    with zipfile.ZipFile(zip_path, "r") as zf:
        seen_names = set()
        for info in zf.infolist():
            name = info.filename
            # Prohibit duplicates
            if name in seen_names:
                errors.append(f"Duplicate archive entry: {name}")
            seen_names.add(name)

            # Prohibit absolute paths and traversal
            if name.startswith("/") or name.startswith("\\") or ".." in Path(name).parts:
                errors.append(f"Unsafe path in archive: {name}")

            # Prohibit temporary or sensitive paths
            parts = Path(name).parts
            if any(part in forbidden_prefixes or part.endswith((".pyc", ".pyo")) for part in parts):
                errors.append(f"Forbidden file pattern in archive: {name}")
            if "__pycache__" in parts:
                errors.append(f"Cache directory in archive: {name}")

    return errors


def build_role_pack_archive(output_path: Path) -> None:
    """Builds the complete herdr-role-pack ZIP archive deterministically."""
    exclude_dirs = {"__pycache__", ".git", ".herdr-loop", ".sqlite-tracer", "tests", "target"}
    exclude_files = {".DS_Store", "Thumbs.db"}

    output_path.parent.mkdir(parents=True, exist_ok=True)
    with zipfile.ZipFile(output_path, "w", zipfile.ZIP_DEFLATED) as zf:
        # 1. Add root files sorted
        for rel_file in sorted(MANDATORY_ROOT_FILES):
            src = ROOT / rel_file
            if src.exists():
                add_file_deterministic(zf, src, rel_file)
            else:
                raise FileNotFoundError(f"Missing required file for role pack: {src}")

        # 2. Collect and sort all directory files
        collected_files: list[tuple[Path, str]] = []
        for rel_dir in sorted(MANDATORY_DIRS):
            dir_src = ROOT / rel_dir
            if not dir_src.exists():
                continue
            for root, dirs, files in os.walk(dir_src):
                dirs[:] = sorted([d for d in dirs if d not in exclude_dirs and not d.startswith(".")])
                for file in sorted(files):
                    if file in exclude_files or file.endswith((".pyc", ".pyo")):
                        continue
                    file_path = Path(root) / file
                    arcname = str(file_path.relative_to(ROOT)).replace("\\", "/")
                    collected_files.append((file_path, arcname))

        # Write collected files in sorted order
        for file_path, arcname in sorted(collected_files, key=lambda x: x[1]):
            add_file_deterministic(zf, file_path, arcname)


def build_controller_archive(output_path: Path) -> None:
    """Builds a standalone controller skill archive deterministically."""
    controller_dir = ROOT / "skills" / "herdr-engineering-loop"
    if not controller_dir.exists():
        raise FileNotFoundError(f"Controller skill directory not found: {controller_dir}")

    exclude_dirs = {"__pycache__"}
    output_path.parent.mkdir(parents=True, exist_ok=True)
    collected_files: list[tuple[Path, str]] = []
    with zipfile.ZipFile(output_path, "w", zipfile.ZIP_DEFLATED) as zf:
        for root, dirs, files in os.walk(controller_dir):
            dirs[:] = sorted([d for d in dirs if d not in exclude_dirs and not d.startswith(".")])
            for file in sorted(files):
                if file.endswith((".pyc", ".pyo")):
                    continue
                file_path = Path(root) / file
                arcname = str(file_path.relative_to(controller_dir)).replace("\\", "/")
                collected_files.append((file_path, arcname))

        for file_path, arcname in sorted(collected_files, key=lambda x: x[1]):
            add_file_deterministic(zf, file_path, arcname)


def check_input_completeness() -> list[str]:
    """Verifies that all required files and directories exist."""
    missing = []
    for rel_file in MANDATORY_ROOT_FILES:
        if not (ROOT / rel_file).exists():
            missing.append(f"Missing file: {rel_file}")
    for rel_dir in MANDATORY_DIRS:
        if not (ROOT / rel_dir).exists():
            missing.append(f"Missing directory: {rel_dir}")
    return missing


def main() -> int:
    parser = argparse.ArgumentParser(description="Build and validate Herdr Role Pack release archives.")
    parser.add_argument("--version", help="Explicit version to validate against manifest.json")
    parser.add_argument("--output-dir", default=str(ROOT / "dist"), help="Output directory for archives (default: dist/)")
    parser.add_argument("--dry-run", action="store_true", help="Validate source files without creating output archives")

    args = parser.parse_args()
    canonical_version = get_canonical_version()

    if args.version and args.version != canonical_version:
        print(f"ERROR: Specified version '{args.version}' does not match manifest.json ('{canonical_version}')", file=sys.stderr)
        return 1

    version = canonical_version
    output_dir = Path(args.output_dir).resolve()

    # Preflight input files check
    missing = check_input_completeness()
    if missing:
        print("ERROR: Incomplete release inputs:", file=sys.stderr)
        for m in missing:
            print(f"  - {m}", file=sys.stderr)
        return 1

    if args.dry_run:
        print(f"[DRY-RUN] Validated manifest version: {version}")
        print(f"[DRY-RUN] Verified presence of all mandatory root files and directories (including LICENSE).")
        print(f"[DRY-RUN] Archive targets would be placed in: {output_dir}")
        return 0

    output_dir.mkdir(parents=True, exist_ok=True)
    role_pack_zip = output_dir / f"herdr-role-pack-v{version}.zip"
    controller_zip = output_dir / f"herdr-controller-v{version}.zip"
    sums_file = output_dir / "SHA256SUMS"

    print(f"Building {role_pack_zip.name}...")
    build_role_pack_archive(role_pack_zip)
    errors = validate_archive_members(role_pack_zip)
    if errors:
        print(f"ERROR: Archive validation failed for {role_pack_zip.name}:", file=sys.stderr)
        for err in errors:
            print(f"  - {err}", file=sys.stderr)
        return 1

    print(f"Building {controller_zip.name}...")
    build_controller_archive(controller_zip)
    ctrl_errors = validate_archive_members(controller_zip)
    if ctrl_errors:
        print(f"ERROR: Archive validation failed for {controller_zip.name}:", file=sys.stderr)
        for err in ctrl_errors:
            print(f"  - {err}", file=sys.stderr)
        return 1

    print("Generating SHA256SUMS...")
    hashes = [
        f"{calculate_sha256(role_pack_zip)}  {role_pack_zip.name}",
        f"{calculate_sha256(controller_zip)}  {controller_zip.name}",
    ]
    sums_file.write_text("\n".join(hashes) + "\n", encoding="utf-8")

    print(f"Successfully packaged version {version}:")
    for line in hashes:
        print(f"  {line}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
