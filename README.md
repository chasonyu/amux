# amux

Terminal control plane that **wraps** coding-agent CLIs (first: **omp**) in real PTYs — no RPC re-paint, no omp fork.

```
┌─────────────┬──────────────────────────┐
│ workspaces  │                          │
│ sessions    │   omp TUI (PTY + VT)     │
│             │                          │
└─────────────┴──────────────────────────┘
```

Spec: [`docs/spec/2026-07-31-amux-design.md`](docs/spec/2026-07-31-amux-design.md)

## Requirements

- Linux or macOS
- Rust 1.75+ (edition 2021)
- [`omp`](https://github.com/nicobailon/oh-my-pi) on `PATH` (tested with **omp/17.2.1**), e.g. `~/.bun/bin/omp`

## Build & run

```bash
cd opensource/amux
cargo build --release
./target/release/amux

# or
cargo run --release
```

Headless smoke (no TTY):

```bash
cargo test
cargo run --release -- --smoke
```

Optional debug log: `AMUX_LOG=1 cargo run --release` → `~/.amux/amux.log`

## Keybindings

| Binding | Mode | Action |
|--------|------|--------|
| `Ctrl+\` | Agent | Toggle → Nav |
| `Ctrl+\` `Ctrl+\` (≤500ms) | Agent | Forward one literal `\x1c` to omp; stay Agent |
| `Ctrl+\` | Nav | Toggle → AgentMode (if a session is focused) |
| most keys | Agent | Raw-forward to omp (incl. `Ctrl+C`, `Ctrl+B`) |
| `a` | Nav | Add workspace (centered directory browser) |
| `n` | Nav | New `omp --cwd <workspace>` session |
| `Enter` | Nav | Attach / `omp --resume <id>` (lazy PTY spawn) |
| `j` / `k` | Nav | Move session selection |
| `J` / `K` | Nav | Move workspace selection |
| `x` | Nav | Close focused live session (kill that PTY only) |
| `?` | Nav | Help |
| `q` then `y` | Nav | Quit amux (kills all live children) |

Nav keys never reach omp. AgentMode is a transparent pipe with a tiny intercept set (`Ctrl+\` only).

## Data

| Path | Purpose |
|------|---------|
| `~/.amux/workspaces.json` | Manually added workspaces |
| `~/.amux/locks/<session>.lock` | flock occupied detection |
| `~/.amux/config.json` | Optional: `omp_bin`, `pi_pins`, `escape_key`, `session_dir`, `profile` |
| `~/.omp/agent/sessions/<cwd-key>/` | omp on-disk sessions (listed by amux) |

## Manual acceptance

With a real TTY:

1. **Add workspace** — press `a`, browse to a project, `Enter` on the directory (use `.` for current). Confirm it appears in the sidebar and in `~/.amux/workspaces.json`.
2. **E1** — attach a session, type CJK/emoji in AgentMode; columns should not drift vs bare `omp`.
3. **E2** — AgentMode `Ctrl+C` reaches omp; amux stays alive.
4. **E3** — `Ctrl+\` → Nav (sidebar chrome active).
5. **E3b** — double `Ctrl+\` within 500ms → one literal to omp; stay AgentMode.
6. **Dual session** — `n` for session A, `Ctrl+\` back to shell, select another / `n` again for B, `Enter`; switch with `Ctrl+\` + select; background session stays live (`live` chip).
7. **Quit** — `q` `y`; no orphan `omp`; terminal modes restored.

## Known limitations (MVP)

- Kitty keyboard: enabled only when outer `TERM`/`TERM_PROGRAM` looks capable; otherwise status shows `Shift+Enter unavailable`.
- OSC 52: store pass-through to outer terminal; clipboard **load** replies empty string (must not swallow).
- No host copy-mode — use outer terminal Shift+drag selection.
- Pixel mouse (1016) unsupported.
- New sessions use synthetic ids (`new-N`) until omp persists a disk id; resume list refreshes from disk on next list.
- Writer backpressure surfaces as status text; UI does not block on PTY write.
- Advanced host 2026 frame wrap / DECTCEM hardware cursor polish is best-effort.

## Architecture (crate layout)

```
src/
  main.rs           CLI entry
  raw_input.rs      CSI/OSC/UTF-8 sequence splitter (+ tests)
  escape.rs         Ctrl+\ double-tap (+ tests)
  mouse.rs          SGR translate + four-edge clip (+ tests)
  config/           ~/.amux config + PI_* pins
  workspace/        WorkspaceStore
  provider/omp.rs   list/spawn/resume + cwd encoding
  lock.rs           flock + pgrep
  pty/              PtySession: portable-pty, alacritty_terminal, writer queue, stop_sync
  session/          SessionSupervisor
  shell/            ratatui dual-pane, modal browser, mode mirror
```

## License

MIT (this tree). Do not copy large portions of third-party projects with unclear licensing.
