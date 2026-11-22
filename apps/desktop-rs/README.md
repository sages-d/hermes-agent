# Hermes Native Companion (`apps/desktop-rs`) — GPUI Companion

A native, memory-safe Rust desktop companion for Hermes Agent built on **GPUI** (Zed's GPU-accelerated UI framework) that attaches directly to the canonical `hermes-gateway-v1` gateway without bundling Chromium or Node.

> **Status:** Milestones 0, 1, and 2 complete (macOS-first; compiles and passes all tests on macOS and Linux). Completely isolated from `apps/desktop/` (Electron) and core Python.

---

## Architecture

```text
apps/desktop-rs/
├── scripts/
│   └── gen_protocol.py          # OpenRPC 1.3.2 -> Rust serde codegen (`--check` supported)
└── crates/
    ├── hermes-protocol/         # Generated wire types (311 schemas, 64 RPC methods,
    │                            #   16 server->client requests, 79 event notifications)
    ├── hermes-gateway/          # `hermes gateway attach --json`, async WebSocket
    │                            #   (`tokio-tungstenite`), ticket redaction, heartbeat,
    │                            #   exponential reconnect backoff + bounded `--recover`,
    │                            #   `session.events.since` replay / `session.resume` fallback,
    │                            #   canonical Bot Chat resolver, profiles & subagents
    ├── hermes-terminal/         # Native PTY + VT100 screen emulator (`portable-pty` + `vt100`)
    ├── hermes-git/              # Repo status, worktrees, unified diffs, stage/unstage
    └── hermes-app/              # GPUI native UI (`hermes-companion` binary):
                                 #   macOS transparent titlebar, Nous dark/light + skin accent,
                                 #   full TextBuffer composer, syntect code highlighting + Copy,
                                 #   session search, profile switcher, Bot Chat door, subagent
                                 #   monitor bar, Cmd+K Command Palette, Cmd+Shift+K Quick Entry,
                                 #   Cmd+J Workspace Inspector (Terminal / Git / Files),
                                 #   Cmd+, Control Center (Config / Skills / Cron / Usage)
```

### Wire Contract Single Source of Truth

Python (`tui_gateway/contracts/`) remains the sole schema authority. To refresh Rust wire types after a gateway contract update:

```bash
python3 apps/desktop-rs/scripts/gen_protocol.py
python3 apps/desktop-rs/scripts/gen_protocol.py --check
```

---

## Running on macOS (Primary Target)

Prerequisites: Rust toolchain (`rustup`, stable 1.85+) and Xcode Command Line Tools (`xcode-select --install`).

By default `hermes-app` enables `gpui/runtime_shaders` so Metal shaders compile at runtime without requiring a full Xcode installation (`xcrun metal`). For AOT `.metallib` shaders with full Xcode + Metal Toolchain installed, pass `--no-default-features`.

```bash
cd apps/desktop-rs

# 1. Offline interactive demo (no running Hermes gateway required; Terminal/Git/Files work live!)
cargo run -p hermes-app -- --demo --cwd ../..

# 2. Attach to your local Hermes gateway (invokes `hermes gateway attach --json --start`)
cargo run -p hermes-app -- --cwd ../..

# 3. Attach to a named profile or explicit remote WebSocket URL
cargo run -p hermes-app -- --profile work
cargo run -p hermes-app -- --url ws://127.0.0.1:9119/api/ws

# Release build (recommended for benchmarking RAM & startup latency)
cargo build --release -p hermes-app
./target/release/hermes-companion
```

> **Linux note:** `Cargo.lock` intentionally pins `libc` to `0.2.186` because `gpui 0.2.2` transitively pulls `xattr 0.2.3` (via `zed-async-tar`), which relies on `libc::ENOATTR` removed on Linux in `libc 0.2.190+`. Keep `--locked` on Linux or upgrade to a newer `gpui` release in Milestone 3.

### Keyboard Shortcuts

| Shortcut | Action |
|---|---|
| `Enter` / `Shift+Enter` | Send prompt / insert newline |
| `Left` / `Right` / `Alt+Left` / `Alt+Right` | Move cursor by character or word (add `Shift` to select) |
| `Cmd+Left` / `Cmd+Right` | Jump to start / end of line |
| `Cmd+A` / `Cmd+C` / `Cmd+X` / `Cmd+V` | Select all / Copy / Cut / Paste |
| `↑` / `↓` (in single-line composer) | Cycle prompt history |
| `/` (at start of input) | Open live slash command palette (`↑`/`↓` navigate, `Tab` complete) |
| `Cmd+K` (`Ctrl+K`) | Open Command Palette (sessions, profiles, workspace panes, actions) |
| `Cmd+Shift+K` (`Ctrl+Shift+K`) | Open floating Quick Entry mini-composer window |
| `Cmd+J` (`Ctrl+J`) | Toggle right-side Workspace Inspector (Terminal / Git / Files) |
| `Cmd+,` (`Ctrl+,`) | Open Control Center overlay (Config / Skills / Cron / Usage) |
| `Cmd+F` (`Ctrl+F`) | Focus sidebar session search |
| `Cmd+N` (`Ctrl+N`) | Start a new session |
| `Cmd+B` (`Ctrl+B`) | Toggle session sidebar |
| `Esc` | Exit terminal focus / close overlay / cancel active streaming turn |

---

## Tests

```bash
cd apps/desktop-rs
cargo test --workspace
```

---

## Roadmap

- **Milestone 0 (Done):** OpenRPC → Rust codegen, ticketed `hermes-gateway-v1` WS client, minimal GPUI shell.
- **Milestone 1 (Done):** Full `TextBuffer` composer, `syntect` syntax highlighting + Copy, auto-reconnect + `session.events.since` replay, multi-profile switcher, canonical `"Bot Chat"` resolver, subagent monitor bar, `Cmd+K` Command Palette, floating `QuickEntryView`, and skin accent adoption.
- **Milestone 2 (Done):** `hermes-terminal` (`portable-pty` + `vt100`) embedded native shell, `hermes-git` (branch/worktrees/unified diffs/stage/unstage), workspace file explorer with `syntect` preview, and Control Center overlay (Config / Skills / Cron / Usage).
- **Milestone 3 (Next — Native Polish & Optional Web Boundary):**
  - macOS native notifications, menu bar tray (`tray-icon`), and OS keychain (`keyring`)
  - Lazy-loaded `wry` webview pane for interactive HTML artifacts / live web preview only when opened (0 MB overhead when unused)
  - Upgrade to latest GPUI + Linux (Wayland/X11) & Windows packaging polish
