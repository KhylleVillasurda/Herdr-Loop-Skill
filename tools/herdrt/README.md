# herdrt — Rust port of the Herdr Role Pack tools

One binary mirrors the Python tools in `tools/*.py`:

```sh
cd tools/herdrt
cargo build --release
target/release/herdrt access-probe --workspace . --agent worker --pane pane-1
```

## Status (phase 1: `tools/` only; SQLite ledger stays Python)

| Tool | Python source | Rust module | Status |
|---|---|---|---|
| access_probe | tools/access_probe.py | src/access_probe.rs + src/probe.rs | **Ported; byte-identical output vs Python** |
| progress_view | tools/progress_view.py | src/progress_view.rs | **Ported (Markdown path); sqlite-active runs → use Python** |
| role_bind | tools/role_bind.py | src/role_bind.rs | **Ported; byte-identical artifacts on all 4 example fixtures** |
| activate_roles | tools/activate_roles.py | src/activate_roles.rs | **Ported; CLI-identical incl. sqlite gate** |
| package | tools/package.py | src/package.rs | **Ported; member list/perm/timestamp-identical artifacts** |
| update | tools/update.py | src/update_tool.rs | **Ported** |
| render_reference | tools/render_reference.py | — | **Stays Python** (it renders the Python ledger's parser; moves with the ledger cutover) |

Shared helpers:

- `src/util.rs` — exec-with-timeout, which(), sha256, constant-time digest compare, Python-style `json.dumps(indent=2)` serializer (insertion-ordered maps via serde_json `preserve_order`).
- `src/probe.rs` — `sanitize_error` / `classify_failure` / subprocess probe semantics from access_probe.py (reused by activate_roles).
- `src/backend.rs` — run_backend.py's markdown-side pieces: selector read/write, pending-transfer journal checks, exclusive-create run lock, `init_selector`, canonical `SESSION.md` template, `profile_from_config`.

## Deliberate boundaries in phase 1

- **SQLite-active runs are refused, not approximated.** Any run whose selector selects sqlite (or legacy pilot dirs) returns
  `SQLITE_BACKEND_UNSUPPORTED_BY_HERDRT` with a pointer to the Python tool. No SQLite engine is embedded; no validation is skipped silently.
  Per SQLITE_ADOPTION_DECISION.md, Markdown is the promoted backend; the SQLite toolchain remains Python until its Windows E2E gates pass.
- **render_reference stays Python.** It imports the Python ledger module to render its command reference; the Rust crate must not
  drift from the Python parser it documents.
- **Deflate determinism:** archive members (names, order, timestamps, permissions, LF endings) are deterministic like package.py, but
  the DEFLATE byte stream comes from a different (Rust) zlib implementation; published SHA256SUMS remain the integrity authority.
- **Archive member modes:** the zip crate always records Unix modes with the regular-file type bit (`0o100644`) where Python's
  `zipfile` writes bare modes (`0o644`). Permission bits and extraction behavior are identical; the central-directory attribute
  bytes differ and there is no crate option to suppress the type bit.
- **Windows/Linux:** every platform split (`PATHEXT`, spawn-error classification, local timestamps via `GetLocalTime` on Windows /
  `localtime_r` on unix, chmod preservation no-op on Windows) has explicit or no-op branches on both sides.

## Parity testing

The rule: **byte-identical output vs Python for identical inputs; identical exit codes; identical stderr rows.**

Run a parity check any time (Python must be available):

```sh
# 1. Bind a fixture run with Python, render progress with both, diff:
python3 tools/role_bind.py --config examples/roles.example.json --repo /tmp/repo
python3 tools/progress_view.py --session /tmp/repo/.herdr-loop/feature-example/SESSION.md > /tmp/py.out
./target/release/herdrt progress-view --session /tmp/repo/.herdr-loop/feature-example/SESSION.md > /tmp/rs.out
diff /tmp/py.out /tmp/rs.out

# 2. access-probe (same directory, no Herdr available):
python3 tools/access_probe.py --workspace /tmp/repo > /tmp/py.json
./target/release/herdrt access-probe --workspace /tmp/repo > /tmp/rs.json
diff /tmp/py.json /tmp/rs.json
```

Unit tests live inside each module: `cargo test`.

## Follow-up phases (not started)

1. Docs/install scripts currently call `py -3 tools/<tool>.py` — switch to shipped `herdrt` binaries once all tools land (skill-editing phase).
2. SQLite ledger + `render_reference` migration (gated by SQLITE_ADOPTION_DECISION.md cutover gates).
3. Environment inventory for role packets (the original workflow-optimization item this migration was chosen alongside).
