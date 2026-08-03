# amux

![License](https://img.shields.io/badge/license-MIT-blue)
![Rust](https://img.shields.io/badge/rust-1.75%2B-orange)
![Platform](https://img.shields.io/badge/platform-Linux%20%7C%20macOS-lightgrey)
![omp](https://img.shields.io/badge/omp-17.2.5%2B-purple)

> Terminal control plane for coding-agent CLIs — wrap them in real PTYs, not RPC re-paint.

amux runs your coding agent's **real TUI** inside a PTY. No fork, no RPC, no re-implementation of the agent's UI. Switch between sessions without killing background agents.

```
┌─────────────┬──────────────────────────┐
│             │                          │
│  Sidebar    │   Agent TUI (PTY + VT)   │
│             │   omp / omp --resume     │
│  workspaces │                          │
│  sessions   │                          │
│  status     │                          │
│             │                          │
└─────────────┴──────────────────────────┘
```

## Why

In 2024, mainstream coding agents — Codex, Claude Code, Cursor — converged on a desktop GUI layout: sidebar on the left, agent conversation on the right. But if you develop remotely over SSH, that GUI doesn't follow you.

amux brings that same dual-pane experience into the terminal. It wraps the [`omp`](https://github.com/nicobailon/oh-my-pi) CLI in a real PTY — no fork, no RPC, no re-rendering — so you get the full agent TUI on any machine you can SSH into.

## Features

- **Real PTY embedding** — runs the stock `omp` CLI in a PTY; no fork/patch, so agent upgrades are painless
- **Multi-session** — run several agent sessions side by side; switch without killing background processes
- **Workspace management** — add project directories via a modal file browser
- **Session discovery** — lists and resumes omp's on-disk sessions automatically
- **Transcript preview** — omp-faithful markdown rendering for disk sessions
- **Theme sync** — light/dark appearance probed from host terminal via OSC 11
- **Transparent pipe** — AgentMode forwards raw bytes (kitty/modifyOtherKeys/UTF-8/mouse) verbatim to the agent
- **Mouse + clipboard** — SGR mouse translate with four-edge clipping; OSC 52 clipboard pass-through

## Prerequisites

| Requirement | Detail |
|---|---|
| OS | Linux or macOS (Windows not supported) |
| Rust | 1.75+ (edition 2021) |
| omp CLI | [`omp`](https://github.com/nicobailon/oh-my-pi) on `PATH` (tested with **omp/17.2.5+**) |

## Installation

**From source:**

```bash
git clone https://github.com/chasonyu/amux.git
cd amux
cargo build --release
```

The binary is at `./target/release/amux`. Add it to your `PATH` or copy it somewhere convenient:

```bash
cp ./target/release/amux /usr/local/bin/   # or ~/.local/bin/
```

## Quick start

```bash
amux
```

1. Press **`a`** — add a workspace (browse to a project directory, `Enter` to confirm)
2. Press **`n`** — start a new omp session in that workspace
3. Press **`Enter`** — attach; you're now in **AgentMode** (omp's real TUI)
4. Press **`Ctrl+\`** — toggle between Nav (sidebar) and AgentMode
5. Press **`n`** again for a second session — the first stays alive in the background
6. Press **`q`** then **`y`** — quit (kills all child processes, restores terminal)

## Keybindings

| Binding | Mode | Action |
|--------|------|--------|
| `Ctrl+\` | Agent | Toggle → Nav |
| `Ctrl+\` `Ctrl+\` (≤500ms) | Agent | Forward one literal `\x1c`; stay Agent |
| `Ctrl+\` | Nav | Toggle → AgentMode (if a session is focused) |
| most keys | Agent | Raw-forward to omp (incl. `Ctrl+C`, `Ctrl+B`) |
| `a` | Nav | Add workspace (directory browser) |
| `n` | Nav | New `omp --cwd <workspace>` session |
| `Enter` | Nav | Attach / `omp --resume <id>` (lazy PTY spawn) |
| `j` / `k` | Nav | Move session selection |
| `J` / `K` | Nav | Move workspace selection |
| `r` | Nav | Rename focused session |
| `x` | Nav | Close focused live session |
| `?` | Nav | Help |
| `q` then `y` | Nav | Quit amux (kills all live children) |

Nav keys never reach omp. AgentMode is a transparent pipe with a tiny intercept set (`Ctrl+\` only).

## Configuration

### Data paths

| Path | Purpose |
|------|---------|
| `~/.amux/workspaces.json` | Manually added workspaces |
| `~/.amux/locks/<session>.lock` | flock occupied detection |
| `~/.amux/config.json` | Optional config (see below) |
| `~/.omp/agent/sessions/<cwd-key>/` | omp on-disk sessions (listed by amux) |

### Optional config (`~/.amux/config.json`)

```json
{
  "omp_bin": "~/.bun/bin/omp",
  "escape_key": "\\x1c",
  "session_dir": "~/.omp/agent/sessions",
  "profile": null
}
```

### Debug log

```bash
AMUX_LOG=1 amux   # writes to ~/.amux/amux.log
```

## How it works

amux is a dual-pane TUI built with [ratatui](https://ratatui.rs/) and [crossterm](https://github.com/crossterm-rs/crossterm):

```
┌────────────────────────────────────────────────────────────┐
│ amux (Rust TUI)                                           │
│                                                            │
│  ┌──────────────┐   ┌──────────────────────────────────┐   │
│  │ Sidebar      │   │ PtySurface (focused session)    │   │
│  │ workspaces   │   │ VT grid ←→ PTY master            │   │
│  │ sessions     │   │ child: omp / omp --resume …     │   │
│  └──────┬───────┘   └──────────────▲───────────────────┘   │
│         │                          │                       │
│         ▼                          │                       │
│  SessionSupervisor ──── Provider registry (v1: OmpProvider) │
│  lazy spawn · multi-session       spawn / resume / list    │
└────────────────────────────────────────────────────────────┘
```

**Two modes:**

- **Nav** — amux owns input; sidebar navigation, workspace/session management. No bytes reach the agent PTY.
- **AgentMode** — transparent pipe; raw bytes forward verbatim to omp. Only `Ctrl+\` is intercepted.

**PTY lifecycle:** lazy spawn on first attach; keep alive until explicit close or amux exit. Switching sessions rebinds the surface — background sessions continue running.

For the full design spec, see [`docs/spec/2026-07-31-amux-design.md`](docs/spec/2026-07-31-amux-design.md).

## Known limitations

- Kitty keyboard: enabled only when the outer terminal looks capable; otherwise Shift+Enter / Ctrl+Enter unavailable (shown in status).
- No host copy-mode — use the outer terminal's Shift+drag selection; OSC 52 pass-through supported.
- Pixel mouse (mode 1016) unsupported.
- New sessions use synthetic IDs (`new-N`) until omp persists a disk ID.

## Development

```bash
cargo build            # debug build
cargo test             # unit tests
cargo run -- --smoke   # headless smoke test (no TTY required)
AMUX_LOG=1 cargo run   # debug log → ~/.amux/amux.log
```

## Contributing

Pull requests are welcome. For major changes, please open an issue first to discuss what you'd like to change.

```bash
git checkout -b feat/your-feature
# make changes
cargo build && cargo test
git push origin feat/your-feature
# open a PR on GitHub
```

## License

MIT
