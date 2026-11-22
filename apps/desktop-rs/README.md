# Hermes Native Companion (`apps/desktop-rs`) — GPUI Milestone 0

A native, memory-safe Rust desktop companion for Hermes Agent built on **GPUI** (Zed's GPU-accelerated UI framework) that attaches directly to the canonical `hermes-gateway-v1` gateway without bundling Chromium or Node.

> **Status:** Milestone 0 prototype (macOS-first; compiles on macOS and Linux). Completely isolated from `apps/desktop/` (Electron) and core Python.

---

## Architecture

```text
apps/desktop-rs/
├── scripts/
│   └── gen_protocol.py          # OpenRPC 1.3.2 -> Rust serde codegen
└── crates/
    ├── hermes-protocol/         # Generated wire types (311 schemas, 64 RPC methods,
    │                            #   16 server->client requests, 79 event notifications)
    ├── hermes-gateway/          # `hermes gateway attach --json` + async WebSocket
    │                            #   (`tokio-tungstenite`), ticket redaction, heartbeat,
    │                            #   `runtime.describe`, `client.capabilities`, replay
    └── hermes-app/              # GPUI native UI (`hermes-companion` binary):
                                 #   macOS transparent titlebar, Nous dark/light themes,
                                 #   session sidebar, streaming Markdown, tool cards,
                                 #   inline approval/clarify prompts, `/` slash palette
```

### Wire Contract Single Source of Truth

Python (`tui_gateway/contracts/`) remains the sole schema authority. To refresh Rust wire types after a gateway contract update:

```bash
python3 apps/desktop-rs/scripts/gen_protocol.py
```

---

## Running on macOS (Primary Target)

Prerequisites: Rust toolchain (`rustup`, stable 1.85+) and Xcode Command Line Tools (`xcode-select --install`).

By default `hermes-app` enables `gpui/runtime_shaders` so Metal shaders compile at runtime without requiring a full Xcode installation (`xcrun metal`). For AOT `.metallib` shaders with full Xcode + Metal Toolchain installed, pass `--no-default-features`.

```bash
cd apps/desktop-rs

# 1. Offline interactive demo (no running Hermes gateway required)
cargo run -p hermes-app -- --demo

# 2. Attach to your local Hermes gateway (invokes `hermes gateway attach --json --start`)
cargo run -p hermes-app

# 3. Attach to a named profile or explicit remote WebSocket URL
cargo run -p hermes-app -- --profile work
cargo run -p hermes-app -- --url ws://127.0.0.1:9119/api/ws

# Release build (recommended for benchmarking RAM & startup latency)
cargo build --release -p hermes-app
./target/release/hermes-companion
```

> **Linux note:** `Cargo.lock` intentionally pins `libc` to `0.2.186` because `gpui 0.2.2` transitively pulls `xattr 0.2.3` (via `zed-async-tar`), which relies on `libc::ENOATTR` removed on Linux in `libc 0.2.190+`. Keep `--locked` on Linux or upgrade to a newer `gpui` release when moving past Milestone 0.

### Keyboard Shortcuts (Milestone 0)

| Shortcut | Action |
|---|---|
| `Enter` / `Shift+Enter` | Send prompt / insert newline |
| `/` (at start of input) | Open live slash command palette (`↑`/`↓` navigate, `Tab` complete) |
| `Cmd+N` (`Ctrl+N`) | Start a new session |
| `Cmd+B` (`Ctrl+B`) | Toggle session sidebar |
| `Cmd+V` (`Ctrl+V`) | Paste from clipboard |
| `Esc` | Cancel active streaming turn or dismiss slash palette |

---

## Tests

```bash
cd apps/desktop-rs
cargo test --workspace
```

Covers:
- `hermes-protocol`: JSON-RPC frame classification, typed `GatewayNotification` & `ServerRequestPayload` decoding, multipart transcript text extraction.
- `hermes-gateway`: single-use ticket redaction in `Debug` output, full mock WebSocket `hermes-gateway-v1` handshake (`Sec-WebSocket-Protocol` ticket redemption, `client.capabilities`, `runtime.describe`, `session.list`, streaming `message.delta`, and `approval` server→client request round-trip).
- `hermes-app`: headless `CompanionState` streaming delta reducer, slash palette filtering, and `pulldown-cmark` streaming Markdown parser (including unclosed code fences).

---

## Roadmap Beyond Milestone 0

- **Milestone 1 (Daily-driver Companion):** Full `EntityInputHandler` composer (IME, cursor selection, multi-line editing), auto-reconnect with `session.events.since` gap recovery, multi-profile switcher, Bot Mode canonical `"Bot Chat"` view, Quick Entry global hotkey window, and `syntect`/`tree-sitter` code highlighting.
- **Milestone 2 (Power-user Parity):** `hermes-terminal` (`portable-pty` + `alacritty_terminal`), `hermes-git` (worktrees, diff review, stage/commit), file explorer, Settings/Cron/Capabilities views, macOS notifications + menu bar tray.
- **Milestone 3 (Optional Web Boundary):** Lazy-loaded `wry` webview pane for interactive HTML artifacts and live preview only when opened (0 MB overhead when unused), plus Linux & Windows polish.
