# ARCHITECTURE.md — System Architecture & Codebase Map (Grit)

> **Purpose:** This document provides a structural map, architectural guidelines, and module breakdown for both human developers and AI assistants. Keep this file updated as key modules, traits, or data flows evolve.

---

## 1. Executive Overview

**Project Goal:** A fast, native, single-binary Git client written in Rust that bridges desktop UI performance and embedded local web convenience. It operates simultaneously as a native desktop application (`Iced` GUI) and an embedded web server daemon (`Axum` over WebSockets).

### Key Technology Stack
* **Language & Runtime:** Rust (latest stable), Tokio async runtime (`full` features)
* **Desktop UI:** `Iced` (v0.14) with native rendering (`wgpu` / `winit`)
* **Web Server Daemon:** `Axum` (with `ws`, `tokio`, `http1` features)
* **Static Asset Embedding:** `rust-embed` (embeds static frontend assets into single compiled binary)
* **FileSystem Watching:** `notify` (v8) monitoring `.git/` directory changes
* **CLI Engine:** `clap` (v4+ with `derive` macro support)
* **Serialization:** `serde` & `serde_json`
* **Logging/Tracing:** `tracing` & `tracing-subscriber`

### Core Design Principle: Single Writer

The **`TabRegistry`** (`src/server/registry.rs`) is the *only* mutation point for the tab list.
The desktop GUI and every web browser are **pure clients**: they render `WebState`
snapshots pushed through the registry's watch channel and request mutations through
the same shared operations (`open_repo_tab`, `close_tab_by_id`). Neither client ever
writes a full tab list back. This makes duplicate/ghost tabs structurally impossible:
ids come from one monotonic allocator and are never reused within a session.

---

## 2. Directory & Module Hierarchy

```text
.
├── Cargo.toml               # Project manifest and workspace settings
├── ARCHITECTURE.md          # System architecture, guidelines, and module breakdown
├── TASKS.md                 # Step-by-step roadmap for AI implementation
├── NOTES.md                 # Design notes and comparative stack evaluations
├── web/                     # Web UI source files (embedded at build time)
│   └── dist/                # Pre-built HTML/CSS/JS frontend assets
└── src/
    ├── main.rs              # Entrypoint parsing CLI args and routing execution mode
    ├── krust.rs             # Best-effort auto-launch of the krust terminal daemon
    ├── shared_config.rs     # Shared persistence: config.json load/save/restore/prune
    ├── git/                 # Git Engine subsystem
    │   ├── mod.rs           # Git CLI command execution logic and status queries
    │   ├── types.rs         # Core data models (RepoState, GitStatus, FileChange, GitAction)
    │   └── watcher.rs       # Debounced recursive repo-root FS monitoring
    ├── server/              # Embedded Axum Web Server subsystem
    │   ├── mod.rs           # Axum router setup, AppState, boot/sync loops, /browse /files /commit
    │   ├── registry.rs      # TabRegistry: single-writer tab list + watch channel + id allocator
    │   ├── websocket.rs     # WS protocol dispatch, shared open_repo_tab/close_tab_by_id ops
    │   └── static_files.rs  # Embedded asset server using rust-embed
    └── ui/                  # Native Desktop GUI subsystem (Iced)
        ├── mod.rs           # Module declarations only; run() lives in state.rs
        ├── remote.rs        # WS client for connect-mode: run_client + send_op
        ├── state.rs         # GritApp: pure-client state fed by WebTabsSync deliveries
        └── components/      # UI Layout components
            ├── header.rs    # Branch selector, Push, Pull, and Fetch controls
            ├── staging.rs   # Split-view un-staged/staged file tree list
            ├── diff.rs      # Diff text viewer panel
            ├── commit.rs    # Commit summary/description input form
            └── history.rs   # Scrollable commit log and revision list
```

---

## 3. Core Subsystems & Module Breakdown

### 3.1 CLI Entrypoint & Bootstrapping (`src/main.rs`)
* Parses `--headless`, `--port` (default **5000**), `--path` (optional).
* **Headless Mode**: boots Tokio runtime and runs only the Axum daemon.
* **GUI Mode (Default)**: probes `GET /health` on `127.0.0.1:<port>` first.
  * **No daemon found → Embedded**: spawns the Axum server on a background
    Tokio task, then launches the native `Iced` window; both share one cloned
    `TabRegistry`.
  * **Daemon found → Remote (`GuiMode::Remote`)**: the GUI attaches as a plain
    WebSocket client of the running daemon (`src/ui/remote.rs`). Tab state
    arrives via `WebTabsSync` deliveries, and open/close/git operations are
    sent as `{"tab":…,"action":…}` JSON built by `encode_client_message` —
    the exact wire format browsers use. No second server, no parallel writer.
  * In both modes the desktop remains a pure client of the single-writer
    registry; `config.json` is owned exclusively by whichever process runs
    the daemon. `--port` must match the daemon's port when connecting.
* Without an explicit `--path` and with an empty config, startup lands on the
  Add Repository form instead of seeding a fallback `"."` tab — a cleared
  workspace stays cleared across restarts.
* **Headless display fallback**: when GUI mode is requested but no display
  server is detected (`WAYLAND_DISPLAY`/`WAYLAND_SOCKET`/`DISPLAY` all unset
  or empty), `display_available()` (`main.rs`) downgrades the request to
  headless daemon mode — safe under systemd units and SSH sessions.

### 3.2 Core Git Engine & Data Types (`src/git/`)
* **`types.rs`**: strictly typed models — `GitStatus`, `FileChange`, `FilePair`,
  `CommitInfo`, `CommitSummary`, `RepoState`, and the `GitAction` enum
  (`Stage`, `Unstage`, `Commit`, `Push`, `Pull`, `CheckoutBranch`, `Revert`,
  `Reclone`, ...).
* **`mod.rs`**: invokes the local `git` CLI via `std::process::Command`
  (`get_repository_status`, `get_file_diff`, `get_file_pair`, `get_commit_summary`,
  `execute_action`), wrapping stderr into structured `GitError`s.
* **`watcher.rs`**: one recursive watch per repository covering the working tree
  and `.git` (the root watch subsumes `.git`; a separate `.git` watch would
  duplicate every event) with a 200 ms debouncer. Events are filtered before
  debouncing: pure reads (`Access`) never arm a refresh, nor do events living
  exclusively inside churn directories (`target`, `node_modules`,
  `__pycache__`, `.venv`, `venv`) — so builds and package installs cannot spin
  the refresh loop. Watching itself is server-owned only: a single persistent
  `watch_reconciler` task (`src/server/mod.rs`) keeps exactly one recursive
  root watcher alive per unique canonical repository path among the open
   tabs, spawning and retiring them as tabs are opened or closed through any
   client (each new watch is followed by a refresh kick). A successful Reclone
   sends its repo path over a reset channel so the reconciler drops the stale
   watch (the delete/re-clone cycle invalidated it) and respawns one on the
   fresh directory. The desktop GUI subscribes to sync broadcasts instead of
   the filesystem.

### 3.3 Tab Registry & Shared Persistence (`src/server/registry.rs`, `src/shared_config.rs`)
* **`TabRegistry`** holds `WebState { active, tabs: Vec<WebTab> }` behind a
  `tokio::sync::watch` channel plus an `AtomicUsize` id allocator
  (`alloc_id()` never repeats; `raise_next_id_floor()` protects ids adopted from
  disk). Cloning copies the counters but shares the channel. Mutations run
  through `modify()` behind a short-held `write_lock: std::sync::Mutex<()>`
  (poison-tolerant: a panicked writer cannot wedge later mutations).
  Two `AtomicU64` counters complete the picture:
  * `revision` — bumped on every mutation; `sync_loop` compares it before and
    after each refresh and skips the broadcast when nothing changed.
  * `next_log_seq` — monotonic sequencer giving every log entry a stable
    order across clients (`append_log`/`finish_log_entry`).
  * Live streaming: actions run with piped output; `update_log_output`
    revises the in-flight `running` entry in place (throttled snapshots,
    150 ms floor) so clients watch slow commands execute, and the final
    transcript still replaces the placeholder via `finish_log_entry`.
    Delivery stays live because each WebSocket connection dispatches
    inbound actions on a dedicated sequential worker — the connection
    loop never blocks on a running git command and keeps forwarding
    broadcast frames (including to the client that issued the action).
* **`WebTab`** = `{ id, name, repo_path, state: RepoState, log: Vec<LogEntry> }`
  — the wire format for both WS broadcasts and desktop sync messages.
* **`WebState.active` is daemon-side truth**: the web client reconciles its
  selection from it (deep-link `?t=N`), while the desktop `apply_sync`
  deliberately ignores it and keeps selection local.
* **`shared_config.rs`** owns `$XDG_CONFIG_HOME/bitshift/grit/config.json`
  (`SavedTab { id, name, path }`): `save_tabs`, `load_tabs(_from)`,
  `persist_web_state`, `restore_web_state` (with `prune_dead_tabs` filtering
  paths whose `.git` no longer exists). The server is the sole writer of this file.

### 3.4 Axum Web Server (`src/server/mod.rs`, `websocket.rs`, `static_files.rs`)
* **Routes**: `/health`, `/ws` (WebSocket), `/files?tab=&path=` (file diff/pair),
  `/commit?tab=&hash=` (commit summary), `/browse` (server-side folder listing for
  the add-repo form), `/filetree?tab=&path=` (file browser listing), `/filecontent?tab=&path=&raw=`
  (lazy preview content; `raw` serves literal file bytes), `/filesearch?tab=&q=` (case-insensitive
  file name search), `/apps?path=` (external editor apps for a file), `/*` embedded static assets.
* **`boot(registry)`**: restores tabs from config **only if the registry is empty**
  (then re-persists the healed state), spawns the `watch_reconciler`, and starts
  the persist task (writes config on every registry change). It then kicks off a
  **background initial refresh**: clients may connect while git scans are still
  running; each finished tab's `update_state` publish flows through the sync
  loop, so tabs appear one by one. `boot` returns `(AppState, refresh_rx)` and
  does NOT run the sync loop itself — `run_server(listener, app, refresh_rx)`
  spawns `sync_loop` (broadcasting snapshots to every WS client on
  registry/watcher events), and `run()` wires boot → `create_listener` →
  run_server. `refresh_tab` re-validates that the path still contains `.git`
  before shelling out.
* **`create_listener(port)`** binds with `SO_REUSEADDR` so an immediate
  close-and-restart can rebind the port even with lingering TIME_WAIT sockets.
* **`websocket.rs`**: parses `ClientMessage { tab: Option<usize>, action }`.
  Git actions execute against the target tab's repo then trigger a refresh;
  tab mutations go through the extracted shared ops:
  * `open_repo_tab(&registry, name, path) -> Result<usize, String>` — validates
    tilde expansion, directory existence, and `.git` presence; allocates a fresh id;
    appends; sets `active` to it.
  * `close_tab_by_id(&registry, id) -> bool` — removes any tab (repo files on disk
    are untouched); closing the last tab yields an empty registry, which renders as
    the new-tab page everywhere.
* **`static_files.rs`**: serves the embedded `web/dist` bundle from memory.

### 3.5 Native Desktop GUI (`src/ui/state.rs`, `components/`)
* **`GritApp`** is a pure registry client. Its tab list is derived exclusively from
  `Message::WebTabsSync(Vec<WebTab>)` deliveries (fed by a subscription watching the
  registry): unknown non-empty-path ids are adopted as local `RepoTab`s (and
  auto-selected, hiding any open add-form); missing ids are dropped; known ids merge
  server identity while preserving local UI fields (`commit_message`, `diff`,
  `reclone_armed`, `error`).
* Opening/closing repos calls the same shared ops as the web via `iced::Task`;
  errors surface in the form. Git actions stay local (disk operations).
* Zero tabs ⇒ the Add Repository form is the active view ("+" button toggles it
  locally). No config is written by the GUI itself.
* **`components/`**: `header.rs` (branch switcher, push/pull/fetch, reclone),
  `staging.rs`, `diff.rs`, `commit.rs`, `history.rs`, `actions.rs`.

### 3.6 Project Actions (`src/actions.rs`)
* **Self-contained subsystem** for discovering and launching repository
  executables; removable by deleting the file plus its few `actions::` call
  sites. Security comes from the containment checks in `launch`, not a
  kill-switch.
* **Discovery** (`discover`): non-recursive scan of only the repo root,
  `scripts/`, and `tools/`; unix exec-bit detection (`#[cfg(windows)]`
  extension fallback: `.bat/.cmd/.ps1/.exe`); hidden files skipped; results
  sorted and capped at 32. Runs inside `get_repository_status`, so discovered
  scripts ride `RepoState.scripts` through the existing watcher → refresh →
  broadcast plumbing — new/deleted executables appear automatically.
* **Launch** (`launch`): runs scripts inside a **terminal window** so
  interactive menu/TUI scripts get a real TTY. Selection order (unix):
  `$TERMINAL`, then well-known emulators (`x-terminal-emulator`, `gnome-terminal`,
  `konsole`, `xfce4-terminal`, `alacritty`, `kitty`, `tilix`, `xterm`, each with
  its exec-flag convention); macOS drives Terminal.app via osascript; Windows
  opens a console with `cmd /K`. The shell payload reports the exit status and
  keeps the window open until Enter. If no terminal can be spawned — or
  `GRIT_NO_TERMINAL=1` (test hook) — it falls back to a direct detached spawn
  with inherited stdio and a `/bin/sh` fallback for shebang-less scripts.
  Fire-and-forget: detached process group, stdin nulled, a detached thread
  reaps the spawner child; Grit never tracks the launched script. Guards:
  reject absolute/`..` paths, canonicalize containment (symlink escapes fail),
  require an executable file.
* **Wire format**: `GitAction::RunScript(rel_path)` executes via the normal
  action dispatch in both Embedded and Remote modes; picking a script in the
  dropdown launches it immediately (no confirmation step). The section is
hidden when no scripts exist. The web UI exposes the same launcher inside
    its Actions section action row (`#script-select` + `#run-script-btn`,
    inline with the other buttons), fed by
    the identical `RepoState.scripts` payload.

### 3.7 Krust Terminal Dock (`src/krust.rs`, `web/dist`)
* **What it is**: the web UI renders an embedded `krust` web-terminal daemon as
  two dock views (`term-1` / `term-2`), each a full-height `<iframe>` at
  `http://localhost:3000/?s=<session>&dir=<repo>` backed by an xterm.js client.
  The krust server lives OUTSIDE this repo (`~/Projects/krust`); Grit only
  auto-launches it and shells cross-origin against it.
* **Auto-launch** (`src/krust.rs`, `ensure_krust()`): spawned from
  `server::run` on a background Tokio task, best-effort and never fatal.
  `krust_is_up()` TCP-probes `127.0.0.1:3000` (400 ms timeout); only when the
  daemon is down does it resolve a binary — `$KRUST_BIN` wins over a `$PATH`
  scan for `krust`/`krust.exe` — and `Command::spawn()` it detached. Tests
  exercise the pure `finding_krust_binary(krust_bin, path_var)` helper (env
  reads are injected) so they never touch the environment. Because tests call
  `run_server` (never `server::run`), no test ever triggers a spawn.
* **Per-repo sessions**: the frame src is built lazily on first view with a
  session id computed from the ACTIVE repo path —
  `grit-{repoScope(repoPath)}-term-{n}` where `repoScope` is
  `<basename>-<hash36>` (or `root` when no repo). Repo switches rebind the
  iframe to the new repo's session and fire-and-forget
  `GET /reset?session_id=<old>` to kill the abandoned PTY. The `dir=` query
  param makes each session start-shell inside that repo.
* **Reset button**: each terminal view has a hover `Reset` overlay that calls
  krust's `GET /reset?session_id=<sid>` (kills the PTY child + removes the
  session) and reloads the iframe with a `&r=<ts>` cache-buster.
* **Probing & availability**: `probeKrust()` (`KRUST_PROBE_MS=5000` cadence)
  fetches krust's `/` with `mode:"cors"`; when down, the `T` dock buttons hide
  and an active terminal view is force-switched back to the Dashboard.
* **krust requirements**: built from `~/Projects/krust` HEAD — `/reset` and
  `?dir=` are recent additions absent from older installs — and served with
  `CorsLayer::permissive()` so all fetch probes, `/reset` calls, and WebSocket
  upgrades work cross-origin from `localhost:5000`. `KRUST_PORT` (default
  3000) overrides krust's bind port.


---

## 4. Primary Data & Event Flow

### Startup
```
main.rs
  ├── Parse CLI (clap: --headless, --port=5000 default, --path optional)
  └── IF --headless (or GUI requested but no display server detected):
        └── Spawn Tokio runtime → server::run(registry)
              ├── boot(): restore-from-config-if-empty → watch_reconciler +
              │     persist task + background initial refresh (→ refresh_rx)
              ├── run_server(): spawns sync_loop, then Axum routes
              │     (/health /ws /files /commit /browse /filetree
              │      /filecontent /filesearch /apps /*)
              └── background task: krust::ensure_krust() (best-effort)
      ELSE (GUI):
        ├── Probe GET /health on 127.0.0.1:<port>
        ├── Daemon found (Remote): Iced GUI as WS client of that daemon
        │     ├── remote.rs run_client(port) → WebTabsSync deliveries
        │     └── ops sent via send_op(encode_client_message(...))
        └── No daemon (Embedded): spawn server::run(new registry), then
              └── Iced GUI seeded by apply_sync(snapshot)
```

### Tab List Mutation (single writer)
```
User opens/closes a repo (desktop button OR web WS message)
  └─> shared op: open_repo_tab() / close_tab_by_id()
        └─> registry.set(new WebState)            [the ONLY list mutation]
              ├─> watch channel fires
              │     ├─> persist task writes config.json
              │     └─> sync loop broadcasts snapshot to every WS client
              ├─> web clients reconcile selection + URL (?t=N) and render
              └─> desktop registry_subscription delivers WebTabsSync
                    └─> apply_sync: adopt/drop/merge → render
```

### Refresh Loop (state content, not list membership)
```
.git change → debounced notify event → refresh_tab(id)
  → get_repository_status → registry.update_state(id, state) [re-snapshots]
  → broadcast (same pipeline as above)
The desktop does NOT watch the filesystem; it mirrors these updates purely
through sync broadcasts (`WebTabsSync` / registry subscription).
```

### Git Action Dispatch
```
UI interaction (desktop OR web) → GitAction → std::process::Command("git" ...)
  → success/failure (GitError carries stderr) → refresh → broadcast
```

---

## 5. Architectural Invariants & Key Rules

1. **Single-Binary Portability**: no external static files, node runtimes, or sidecar daemons in release mode; all web assets embed via `rust-embed`. The JS stays tooling-free (no bundler).
2. **Unified Data Structures**: desktop UI, WS payloads, and persisted config all derive from the models in `src/git/types.rs` + `registry::{WebState, WebTab}`.
3. **Single Writer**: only registry operations mutate the tab list; clients are renderers + operation requesters. Ids are allocated once and never reused.
4. **Non-Blocking UI Thread**: all `git` execution and filesystem work runs on Tokio tasks / background threads; the Iced thread only renders.
5. **Git CLI Delegation**: shell out to local `git`; no `git2-rs`/libgit2 — preserves SSH keys, GPG signing, and `.gitconfig`.
6. **Async Mutexes**: lock state across Axum/Tokio tasks with `tokio::sync::Mutex`, never `std::sync::Mutex`.

---

## 6. How to Extend

### Adding a New Git Operation
1. Add a variant to `GitAction` in `src/git/types.rs`.
2. Implement it in `execute_action` (`src/git/mod.rs`).
3. Handle the JSON action string in `src/server/websocket.rs` dispatch.
4. Add triggers in `web/dist/app.js` and/or `src/ui/components/` + `Message` in `src/ui/state.rs`.

### Adding a New Web / Desktop UI Component
1. Widget layout in `src/ui/components/<name>.rs`; hook into `GritApp::view`.
2. Mirror the view in `web/dist/app.js` (rendered inside `render()`'s branches).
3. Any new cross-client data must flow through `RepoState`/`WebTab` so both sides receive it via the existing sync pipelines.

---

## 7. Validation & Testing

```bash
cargo check                              # quick compiler check (zero warnings expected)
cargo test                               # unit + integration suites
cargo run -- --headless --port 8080 --path /repo   # headless daemon
cargo run -- --path .                    # dual-mode GUI + web (http://localhost:5000)
cargo build --release                    # single-binary packaging
```

Integration tests boot real daemons on ephemeral ports with isolated
`XDG_CONFIG_HOME` tempdirs, drive them over real WebSocket connections
(`tokio-tungstenite`), and assert on broadcast sequences.

---

## 8. Key Files Quick Reference

| File | Purpose |
|------|---------|
| `src/main.rs` | CLI parsing, mode routing, registry construction |
| `src/shared_config.rs` | Shared `config.json` persistence (load/save/restore/prune) |
| `src/git/types.rs` | Core data models (`RepoState`, `FileChange`, `GitAction`, ...) |
| `src/git/mod.rs` | Git CLI invocation + structured `GitError` |
| `src/git/watcher.rs` | Debounced recursive repo-root watcher |
| `src/server/registry.rs` | `TabRegistry`: watch channel, monotonic ids, `WebState`/`WebTab` |
| `src/server/mod.rs` | Router, `boot()`, sync loop, persist task, `/browse` `/files` `/commit` handlers |
| `src/server/websocket.rs` | WS protocol, shared `open_repo_tab`/`close_tab_by_id` ops |
| `src/server/static_files.rs` | rust-embed asset serving |
| `src/krust.rs` | Best-effort krust terminal-daemon auto-launch (`ensure_krust`, `find_krust_binary`) |
| `src/ui/state.rs` | `GritApp` pure-client state, `run()`, subscriptions, tests |
| `src/ui/remote.rs` | Connect-mode WebSocket client (`run_client`/`send_op`) |
| `src/ui/components/` | Desktop widget panels (header/staging/diff/commit/history/actions) |
| `web/dist/app.js` | Web client: selection invariant, "+" form mode, deep-links (`?t=`, `?view=`), view dock routing, krust terminal integration |
| `TASKS.md` | Original build roadmap (historical) |
| `NOTES.md` | Design rationale |
