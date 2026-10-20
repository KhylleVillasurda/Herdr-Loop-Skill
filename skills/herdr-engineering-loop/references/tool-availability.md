# Tool availability, integration awareness and compaction recovery

Apply this reference in every role, in governed and standalone mode. It answers three
questions that consume efficiency when left unanswered: **which tools actually exist in
this session**, **which to prefer**, and **how to resume after context compression**.

## 1. Inventory first, assume never

Model training data will happily describe integrations that are not attached to this
session. Before relying on any named integration (Serena, Playwright, headroom, Herdr,
project "graph" snapshots, etc.):

1. Enumerate what the active harness actually exposes: in OpenCode use the MCP resource
   list / tool search; on other CLIs use each tool's own catalog when offered. When in
   doubt, attempt the least-invasive call (a read-only lookup) and treat a failure as
   "not available", not as a bug to route around.
2. Record the inventory locally at role start (a short list in your packet notes or
   `.herdr-loop/` scratch is enough): which named services are attached, and which are
   absent. In a governed run, also append the inventory to the run's
   `.herdr-loop/<run>/ENVIRONMENT.md` if the Controller created one.
3. **Fail closed:** a tool not in the inventory does not exist. Never fabricate a
   wrapper, bridge, or "likely" MCP call for it, and never re-try the same call after
   the inventory shows it absent — report the gap instead.

## 2. Serena (or any LSP-backed code intelligence) before grep

When a symbol-level MCP/extension (e.g. Serena) is present, **highly prefer it over
text search for code navigation and code edits**:

- `get_symbols_overview` / `find_symbol` before reading a file just to find a definition.
- `find_referencing_symbols` / `find_implementations` before a rename or behavior change:
  callers and implementations you did not check are the blast radius.
- Prefer `replace_symbol_body` / `rename_symbol`-style edits over rewriting whole files,
  and check diagnostics for the edited file rather than assuming the edit is clean.

Keep grep/read for: documentation, configuration, fixtures, git history, and whole-repo
text hunts that symbol tools cannot answer. When the harness offers a project-graph
snapshot (e.g. Graphify), treat it as a **stale cache** useful for architecture or
impact questions, never as current file truth — re-read the file before editing.

## 3. Context compression tools (e.g. headroom)

Some harnesses compress saturated tool outputs behind a hash. Rules:

- Treat a compressed aggregate as a **pointer, not provenance**: before acting on its
  details, retrieve the original via the tool's retrieve-by-hash function.
- Never cite a compressed summary as evidence in a ledger, report, or critique; cite
  the retrieved original or re-observe.
- If the retrieval tool itself is not in your inventory, behave as if the content is
  absent and re-observe instead of trusting the summary.

## 4. Browser tools (e.g. Playwright) — Verifier only

A browser agent is a **test instrument**, not an ordinary tool:

- Only the `herdr-verifier` role may drive browsing (opt-in: your VERIFY.md must list
  the browser check, and the environment inventory must actually offer the tool).
- Observe **a running, authorized build** of the integrated checkout — the Verifier
  runs checks; treating the browser as a code editor, feature author, or Critic is a
  role violation. Never install, seed, or deploy anything from this role.
- Capture reproducible evidence: URL, interaction, screenshot/console excerpt, and the
  exact tested SHA in the same `verify_<...>.md` evidence file.
- Critic/Worker/Manager/Delegator roles do not use browsers (Worker: no tests, no runs;
  Critic: no execution; browser work is execution).

## 5. After context compaction: re-orient before acting

Compaction can silently drop role packet details, handoffs, signal recipients and
recent receipts. After any compaction or context refresh:

1. **Re-read your durable sources before the first action:**
   your role packet under `<run>/role-packets/<agent-name>.md` (or the standalone
   assignment), the live ledger `SESSION.md` (or the SQLite path your run uses), your
   current brief (`TASK.md` / `VERIFY.md` / `CRITIQUE-BRIEF.md`), and your
   `.herdr-loop/READY.md`/`REPORT.md` status if you have written them. Re-verify your Herdr agent/pane identity.
2. **Never replay a signal** (ROLE_READY / TASK_START / REPORT_READY / VERIFY_READY /
   CRITIQUE_READY / CANCEL_ACK) that may already be in flight or receipted. If the last
   state is uncertain, say so explicitly and ask the counterpart in one message to
   confirm the observed state before resuming.
3. **Never fabricate elapsed history** ("I remember doing X") that is not in your
   durable sources; verify claimed actions against Git, files, and ledger evidence.
4. If key durable files are unreadable or missing, report `BLOCKED` with the exact
   missing paths rather than recreating coordination state from memory.
