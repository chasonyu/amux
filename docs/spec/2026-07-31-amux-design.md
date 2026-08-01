# amux Design Spec

**Date:** 2026-07-31  
**Status:** Approved for MVP implementation (secondary confirm 1A/2B/3A/4A/5A/6B/7A)  
**Location:** `opensource/amux`

## 1. Goal

Build **amux**: a terminal control plane that **wraps** coding-agent CLIs (first: **omp**) without modifying them.

- Left: workspaces + sessions  
- Right: the agent’s **real** TUI, running in a PTY (same binary the user would run alone)  
- Switch sessions without killing background agent processes  
- Later: pluggable providers (Codex CLI, etc.) via config — same shell, different command lines  

amux is **not** bound to omp in naming or core types. Product stance: *No protocol adapters that re-paint the agent UI. Real CLIs in real PTYs.*  
(Aligned with [dux](https://github.com/patrickdappollonio/dux); amux is narrower: workspace/session focus, omp-first, no mandatory git-worktree/git staging pane.)

## 2. Locked decisions

| Topic | Decision |
|---|---|
| Product name | `amux` |
| Repo path | `opensource/amux` |
| Host language | **Rust** (`ratatui` + `crossterm` + `portable-pty` + **pinned** `alacritty_terminal`) |
| Platform v1 | **macOS + Linux only** (Windows out of scope) |
| Agent UI | **PTY-embed official `omp` CLI**. Do **not** fork/patch omp. Do **not** self-draw chat from RPC JSON. Do **not** in-process `pi-tui` assembly as primary path |
| omp upgrades | User upgrades `omp` normally; amux depends on CLI flags, on-disk sessions, plus a **version-gated** `PI_*` env pin table (overridable in config) |
| Main layout | Classic dual pane: always-visible sidebar + agent PTY surface |
| Workspace | One workspace = one local project directory (cwd) |
| Workspace discovery | **Manual add only** |
| Add-workspace UX | Centered modal with **directory browser** |
| Session list | List omp sessions for that cwd (`~/.omp/agent/sessions/...`, honor `--session-dir` / `--profile` if configured); attach via `omp --resume` |
| PTY lifecycle | **Lazy spawn**: first focus creates PTY; afterwards keep alive until explicit close or amux exit |
| Live session cap | **None in v1** (revisit background governance later) |
| Background | Switching away does not kill the session’s `omp` process |
| Exit amux | Kill **process group** of each child: SIGHUP → 300ms → SIGTERM → 500ms → SIGKILL; restore host terminal modes on all exit paths |
| Occupied session | Refuse attach: amux `flock` under `~/.amux/locks/<session_id>` + best-effort `pgrep` for external `omp -r`; no force-hijack |
| Escape key (AgentMode) | Default **`Ctrl+\` (`\x1c`)** toggle Nav; **double-tap within 500ms** forwards one literal `\x1c` (tmux-style). **Not** `Ctrl+B` (omp binds it to cursor-left) |
| Ctrl+C | AgentMode: forward `\x03` to PTY. Nav: do not kill children; prefer explicit quit |
| Copy v1 | No host copy-mode: Shift+drag yields to outer terminal selection; OSC 52 pass-through; `ClipboardLoad` must reply |
| Startup keystrokes | **Drop** (not flush) until ready; show drop count once; intercepts still work |
| Keyboard protocol | Probe outer terminal; enable kitty/modifyOtherKeys only if host can produce them; else status hint |
| Reference | **dux** (selective) + Opus embedding review (2026-07-31) |

### 2.1 Rejected approaches (and why)

| Approach | Why rejected |
|---|---|
| `omp --mode rpc` + custom chat render | Duplicates UI; breaks on omp UI changes; hard to tell “omp bug vs wrapper bug” |
| In-process `pi-tui` + SDK as right pane | Official `InteractiveMode` owns full `ProcessTerminal`; embedding needs fork/patch of omp — **blocked** (upgrade pain) |
| Fork/patch `@oh-my-pi/pi-coding-agent` for `tui`/`mount` inject | Same upgrade pain; user explicitly rejected |
| Runtime monkey-patch of omp packages | Not a product-grade path |

### 2.2 Spike summary (evidence)

1. **SDK multi-session:** feasible in-process; jsonl list/open works.  
2. **Official InteractiveMode in a rectangle:** not without inject/fork.  
3. **Wrap InteractiveMode:** ~50–100 LOC patch possible → rejected as product dependency.  
4. **PTY path:** matches dux; preserves stock `omp` upgrades.

## 3. Non-goals (v1)

- Replacing omp as a product  
- Re-implementing omp tool cards / ask UI  
- Forking or patching omp / `@oh-my-pi/*`  
- Full general-purpose tmux clone (arbitrary layouts, scripting API)  
- Detach-survive after amux exit  
- Hard cap / LRU eviction of live PTYs  
- Multi-user collab / remote relay  
- First-class Codex backend beyond a config-shaped provider hook  
- Cloud sync of `~/.amux`  
- Mandatory git worktree isolation / staging UI (dux has these; amux v1 does not)

## 4. Architecture

```text
┌────────────────────────────────────────────────────────────┐
│ amux (Rust TUI)                                           │
│                                                            │
│  ┌──────────────────┐   ┌──────────────────────────────┐   │
│  │ Sidebar          │   │ PtySurface (focused session) │   │
│  │ workspaces       │   │ VT grid ←→ PTY master        │   │
│  │ sessions         │   │ child: omp / omp -r …        │   │
│  │ status           │   │                              │   │
│  └────────┬─────────┘   └──────────────▲───────────────┘   │
│           │                            │                   │
│           ▼                            │                   │
│  WorkspaceStore              SessionSupervisor             │
│  (~/.amux/…)                 session_id → PtySession      │
│           │                            │                   │
│           └────────────┬───────────────┘                   │
│                        ▼                                   │
│               Provider registry (v1: OmpProvider)          │
│               spawn/resume/list via CLI + disk             │
└────────────────────────────────────────────────────────────┘
```

### 4.1 Components

**Nav**  
Dual-pane layout, modal host, focus (`Ctrl+\`), key routing (sidebar vs forward-to-PTY).

**WorkspaceStore**  
Persists manually added workspaces (`~/.amux/workspaces.json`).

**SessionSupervisor**  
- Discovers disk sessions for a workspace  
- Owns `HashMap<SessionId, PtySession>` for **live** (spawned) sessions  
- Lazy spawn on first attach/focus  
- Switch = rebind `PtySurface` to another live client; do not kill the previous  
- On amux exit: kill all children  

**PtySession**  
- `portable-pty` child + **reader thread** + **writer thread** (bounded queue; UI never blocks on PTY write)  
- VT: pinned `alacritty_terminal` + `Processor<StdSyncHandler>`; amux arms **2026 sync timeout** and calls `stop_sync`  
- `enqueue_write` / `resize` / snapshot for render; refuse writes after exit with status  

**OmpProvider** (v1)  
- `list_sessions(cwd)` — filesystem under session dir (default `~/.omp/agent/sessions/<encoded-cwd>/`; respect profile/session-dir)  
- `spawn_new` / `spawn_resume` with child env contract + `PI_*` pins (§4.2.0 / §4.2.8)  
- Occupied: `~/.amux/locks/<session_id>` flock + best-effort external `pgrep`  

**Future providers**  
Config-shaped like dux: `command`, `args`, `resume_args` — stub only in v1.

### 4.2 Terminal embedding interaction (normative)

MVP quality gate for “feels like bare omp”. Reference: [dux](https://github.com/patrickdappollonio/dux) (selective) + Opus review (2026-07-31). **Do not copy dux blindly.**

#### 4.2.0 Capability negotiation (host ↔ amux VT ↔ child)

**Invariant:** amux MUST NOT advertise to the child any terminal capability that amux cannot actually produce from the outer terminal / its VT layer.

Channels omp uses (all MUST be handled deliberately):

1. **Environment** — scrub identity vars; set honest `TERM`/`COLORTERM` (§4.2.8)  
2. **Keyboard probes** — kitty / modifyOtherKeys (§4.2.3a)  
3. **Mode DECSET/DECRQM** — mirror child modes onto host (§4.2.6)  
4. **OSC queries** — OSC 10/11 colors, OSC 52 clipboard (§4.2.6b)

Debug log + optional status SHOULD show: e.g. `alt · mouse:1003 · paste:2004 · kb:kitty|legacy`.

#### 4.2.1 Design tenets (amux)

1. **Agent mode = transparent pipe** (tiny intercept set only).  
2. **Nav mode = exclusive owner** — zero bytes to any child PTY.  
3. **Parity over cleverness.**  
4. **Fail loud** — drops, resize races, occupied sessions are visible.  
5. **Prove with tests.**  
6. **Critique dux where omp differs** (full TUI, permanent sidebar/footer).  
7. **Never lie about capabilities** (§4.2.0).

#### 4.2.2 Focus state machine

```text
   [Nav] ←── escape key ──► [AgentMode]
        │                              │
        └── modal ──► [ModalMode]      │
                         Esc back      │
                                       └── session switch stays in AgentMode
                                           (rebind focused PtySession)
```

Default escape: `Ctrl+\`. Enter/select session → AgentMode.

| State | stdin | PTY write | Host modes |
|---|---|---|---|
| Nav | amux | none | amux baseline (no child mouse/paste capture) |
| AgentMode | raw bytes (§4.2.3) | focused only | mirrored from child (§4.2.6) |
| ModalMode | amux | none | baseline |

Rules:

- Enter AgentMode: ensure PTY size == pane; apply mode mirror; if child FOCUS mode, send `CSI I`.  
- Leave AgentMode: stop forwarding; **do not clear** the global stdin sequence parser mid-sequence; completed sequences decide destination when finished; send mouse button-release if drag active; if FOCUS mode, send `CSI O`; reset host modes to baseline.  
- Session switch in AgentMode: same leave/enter hygiene for old→new; parser stays global.  
- Child exits: status `exited`; drop to Nav or keep pane with message; no silent PTY writes.

#### 4.2.3 Raw input path (AgentMode)

**Why raw bytes:** re-encoding `KeyEvent`→bytes is lossy for kitty CSI-u / modifyOtherKeys / unknown CSI.

MUST:

1. Read stdin raw bytes; poll set includes stdin, PTY wakeup, 2026 sync deadline (SHOULD: input→PTY ≤5ms, PTY→pixels ≤33ms).  
2. Streaming sequence splitter (CSI/OSC/SS3/UTF-8/bracket-paste/SGR mouse).  
3. Complete sequence → intercept table **or** `enqueue_write` verbatim.  
4. Inside bracketed paste: **no intercepts**; forward markers+payload.  
5. Bare Esc: **non-blocking** pending deadline (do **not** `thread::sleep` on UI thread).  
6. Never forward torn CSI.

#### 4.2.3a Keyboard protocol negotiation

Startup MUST:

1. Probe outer terminal keyboard enhancement support.  
2. If yes → enable disambiguate + report-alternate-keys on host; set VT `kitty_keyboard=true`; forward CSI-u; pop flags on every exit path.  
3. If no → keep `kitty_keyboard=false`; status that Shift+Enter/Ctrl+Enter unavailable.  
4. Child `modifyOtherKeys` MUST be mirrored to host or explicitly rejected — never swallowed silently.  
5. Log negotiated level.

#### 4.2.4 Intercept table (AgentMode)

Escape-hatch: `Ctrl+\` (`\x1c`), configurable as `escape_key`.

| Sequence | Action |
|---|---|
| `\x1c` | Toggle Nav |
| `\x1c\x1c` within 500ms | Forward one literal `\x1c`; stay AgentMode |

MUST: every intercept has a literal-send path; match only complete sequences; outside paste; help lists intercepts; startup logs table.

MUST forward everything else, including `\x02` (Ctrl+B → omp cursor-left), `\x03`, `\x04`, `\x1a`, Tab/Shift+Tab, arrows, Alt/Meta, F-keys, kitty/modifyOtherKeys forms, mouse (after translate), UTF-8.

**No host-scroll in AgentMode** (omp owns scrolling in alt-screen).

#### 4.2.5 Startup / readiness gate (**diverge** — drop, do not flush)

Readiness = first of:

- (a) child set ALT_SCREEN / BRACKETED_PASTE / any MOUSE mode, or  
- (b) visible non-whitespace in viewport  

While gated: intercepts active; keystrokes **dropped** (not flushed); show once `N keystrokes dropped during omp startup`.

Timeouts: 5s hint; 20s offer kill/retry; **no** hard auto-fail at 15s.

#### 4.2.6 Terminal mode mirroring (child → host)

Each frame, diff child `TermMode` vs last host state; apply with **raw escapes**:

| Child mode | Host | Forward |
|---|---|---|
| BRACKETED_PASTE (2004) | enable/disable host 2004 | If child off 2004, strip markers |
| MOUSE 1000/1002/**1003**/1006 | mirror (1003 for omp hover) | re-encode to child encoding; never SGR if child never enabled 1006 |
| FOCUS 1004 | mirror | `CSI I`/`O` on enter/leave AgentMode |
| ALTERNATE_SCROLL 1007 + alt-screen, no mouse | — | wheel → `CSI A/B` ×3 |
| Leave AgentMode / switch | reset host baseline | — |

Mouse: translate into pane; reject outside **all four** edges. dux only guards top/left (fullscreen overlay) — **amux must not copy that**.

Click outside pane: do not forward; click sidebar MAY enter Nav + hit-test. Pixel mouse 1016 unsupported v1.

#### 4.2.6b Copy / clipboard / selection

v1: **no** host copy-mode.

- Shift+drag: do not forward to child; yield to outer terminal native selection.  
- OSC 52 `ClipboardStore`: prefer pass-through to outer terminal.  
- `ClipboardLoad`: **MUST reply** (do not swallow like dux).  

#### 4.2.7 Resize policy

1. Focused size == agent pane cells.  
2. On SIGWINCH/layout: resize **all live** PTYs; track **per-session** `last_size`.  
3. Debounce SIGWINCH; `master.resize` + VT resize under same lock.  
4. ~500ms suppress activity heuristic after resize.  
5. Spawn with known pane size.  
6. Narrow terminal: SHOULD collapse sidebar below min cols with hint.

#### 4.2.8 PTY I/O, VT engine, child environment

**VT pinned:** `alacritty_terminal` **0.26.x** + `Processor<StdSyncHandler>`. **`vt100` NOT acceptable.**

**2026 sync:** after each `advance()`, arm sync timeout wakeup; on deadline without ESU call `stop_sync` + dirty; on EOF/`exited` MUST `stop_sync`.

**Writer:** per-session writer thread + bounded queue (~256KiB); non-blocking enqueue; backpressure stops stdin drain; >2s full → status; no writes after exit.

**Reader:** dedicated thread; handle PtyWrite/color/textarea; do not ignore ClipboardLoad.

**Child env MUST:** `TERM=xterm-256color`, `COLORTERM=truecolor`; **unset** `TERM_PROGRAM`, `TERM_PROGRAM_VERSION`, `TERM_FEATURES`, `KITTY_WINDOW_ID`, `GHOSTTY_RESOURCES_DIR`, `WEZTERM_*`, `ITERM_SESSION_ID`, `VSCODE_PID`, `ALACRITTY_WINDOW_ID`, `WT_SESSION`, `TMUX`, `TMUX_PANE`, `STY`, `LINES`, `COLUMNS`.

**PI_* pins (SHOULD, version-gated, overridable), validated vs omp v17.2.1:** e.g. `PI_FORCE_IMAGE_PROTOCOL=off`, `PI_NO_DECCARA=1`, `PI_NO_KITTY_PLACEHOLDERS=1`, `PI_TUI_SYNC_OUTPUT=1`.

**Lifecycle:** process group; close/exit = SIGHUP→300ms→SIGTERM→500ms→SIGKILL; panic/signal hooks restore host terminal; detect stopped child → SIGCONT or prompt.

**Render:** focused snapshot; SHOULD wrap host frame in 2026; hardware cursor follows child (IME); DECTCEM/DECSCUSR respected.

#### 4.2.9 Scrollback / pause

AgentMode: ingestion always on. If future pause buffer: drop on **sequence boundaries**.

#### 4.2.10 Signals & Ctrl+C

- AgentMode Ctrl+C → `\x03` to PTY.  
- Nav Ctrl+C → do not kill children; explicit quit.  
- SIGWINCH via signal-hook on raw path.  
- Full terminal restore on every exit path.

#### 4.2.11 Multi-session switching

1. Only focused session gets input.  
2. Background VT keeps advancing (SHOULD throttle floods).  
3. Global parser; completed sequences route to current focus; on switch: mouse release + focus-out to old.  
4. `live` chip + activity pulse (resize-suppressed).  
5. Locks: `~/.amux/locks/<id>` + pgrep hint; amux single-instance lock.

#### 4.2.12 Deliberately not copying from dux

| dux | amux |
|---|---|
| Host scroll-steal in interactive | Off |
| ExitInteractive | Escape `Ctrl+\` toggle |
| Fullscreen mouse math | Four-edge clip |
| Only resize visible PTY | All live + per-session last_size |
| Blocking PTY write on UI thread | Writer queue |
| Pass-through kitty/ghostty TERM identity | Scrub + PI_* pins |
| Silent startup drop | Drop + count warning |
| Swallow ClipboardLoad / OSC11 black | Reply clipboard; query real colors when possible |
| SIGKILL direct child only | Process-group graceful ladder |
| auto-reopen after quit | Rejected v1 |
| `thread::sleep` for Esc | Non-blocking deadline |

#### 4.2.13 Acceptance tests (embedding)

| ID | Scenario | Expected |
|---|---|---|
| E1 | CJK/emoji input + wide/ZWJ render | No column drift |
| E2 | Ctrl+C in AgentMode | omp semantics; amux alive |
| E3 | Escape `Ctrl+\` | Nav |
| E3b | Double `Ctrl+\` | One literal to omp; stay AgentMode |
| E4 | Nav keys | Never in omp |
| E5 | Multi-line paste | Host 2004 on; one paste not multi-submit |
| E6 | Shift+Tab / CSI-u when negotiated | Delivered |
| E7 | Resize with 2 live sessions | Both sized; switch sane |
| E8 | Mouse click/hover | Roughly matches bare omp |
| E9 | Smash keys during startup | Drop count; no flush storm |
| E10 | Happy path vs bare omp | Tools/ask equivalent |
| E11 | Shift+Enter when outer supports kb proto | Like bare omp |
| E12 | Missing ESU after 2026h | Unfreeze via `stop_sync` |
| E13 | Child `/exit` | Clean status; no ghost writes |
| E14 | Quit amux | No orphan omp; terminal restored |

#### 4.2.14 Debugging

- Triage: bare `omp` first  
- Log: intercepts, drops, resize, focus, mode-mirror diffs, kb negotiation  
- Nice: `--record-io`; external omp open if lock allows  

## 5. Data model

### 5.1 Workspace

```ts
interface Workspace {
  id: string;       // uuid
  path: string;     // absolute, normalized
  name: string;     // default basename(path)
  createdAt: string;
  order: number;
}
```

Persistence: `~/.amux/workspaces.json` with `{ version, workspaces }`.

### 5.2 Session summary (sidebar)

```ts
interface SessionSummary {
  id: string;              // omp session id or stable file key
  workspaceId: string;
  provider: "omp";
  title: string;
  cwd: string;
  mtime: string;
  live: boolean;           // has PtySession in supervisor
  status: "disk" | "starting" | "running" | "exited" | "error";
}
```

Structured `waiting_user` from omp internals is **best-effort / optional** on PTY path (may be heuristic or omitted in v1).

### 5.3 Live runtime

In-memory only: PTY handles, VT state, last size, desired running flag.

## 6. UX

### 6.1 Main layout

- **Left (~25–30%):** `amux` title; collapsible workspaces; sessions (title, relative time, `live`/`run` chip); footer: add workspace / new session  
- **Right:** focused session PTY surface  
- Focus chrome: clear indicator when sidebar owns focus  

### 6.2 Add workspace

Centered modal + directory browser (navigate dirs, confirm add, Esc cancel). Files grayed/not selectable. On confirm: validate → persist → select → refresh session list.

### 6.3 Sessions

- Select workspace → list disk sessions  
- Enter / click session → lazy spawn/attach → agent mode focus  
- New session → spawn `omp` in workspace cwd → agent mode  
- Explicit close session → kill that PTY only  
- Quit amux → kill all  

### 6.4 Focus keys

| Binding | Action |
|---|---|
| `Ctrl+\` | Toggle sidebar ↔ agent |
| `Ctrl+\` `Ctrl+\` (≤500ms) | Literal `\x1c` to omp; stay AgentMode |
| Agent mode + most keys | Forward to omp (incl. `Ctrl+B`, `Ctrl+C`) |
| Sidebar mode | amux navigation only |

## 7. omp CLI contract (v1)

Documented dependency surface (pin minimum omp version in README when known):

```bash
omp --cwd <workspace>                 # new interactive session
omp --cwd <workspace> --resume <id>   # resume by id/prefix
# listing: prefer scanning ~/.omp/agent/sessions/<cwd-encoding>/
```

amux must not require omp source changes. Flag drift → adapt provider layer only.

## 8. Error handling

| Case | Behavior |
|---|---|
| Workspace path missing | Modal/sidebar error; do not add |
| `omp` not on PATH | Clear startup/attach error |
| Session occupied | Refuse attach; message how to resolve |
| Child exits unexpectedly | Mark session `exited`/`error`; keep sidebar usable |
| Corrupt session file | Skip or mark error in list |
| VT/render glitch | Prefer recovery on refocus/resize; escape hatch: reopen bare omp |

## 9. Tech stack (v1)

- Rust; platform **macOS + Linux**  
- `ratatui` + `crossterm`  
- `portable-pty`  
- VT: **pinned** `alacritty_terminal` 0.26.x (`vt100` forbidden)  
- Config: JSON/TOML under `~/.amux/`  
- **no** omp UI crates as runtime UI  

Reference: `opensource/dux` (`src/pty.rs`, `src/raw_input.rs`, `src/app/input.rs`) — patterns only.

## 10. Repository layout (initial)

```text
opensource/amux/
  docs/spec/2026-07-31-amux-design.md
  Cargo.toml
  README.md
  src/
    main.rs
    shell/          # layout, focus, modals
    workspace/      # WorkspaceStore
    session/        # SessionSupervisor
    pty/            # PtySession, VT bridge
    provider/
      mod.rs
      omp.rs
    config/
```

## 11. MVP scope

**Must have**

1. Dual-pane TUI  
2. Add workspace via centered directory browser  
3. List omp sessions for workspace  
4. Lazy spawn new + resume sessions via PTY  
5. Switch sessions with background keep-alive  
6. **§4.2 embedding** including §4.2.0 capability negotiation, mode mirror, writer queue, 2026 `stop_sync`, env/`PI_*` pins  
7. Pass embedding tests **E1–E7, E9, E11–E14** at minimum (E8/E10/E12 may start manual)  
8. Persist workspaces; session flock + pgrep occupied check  
9. Graceful process-group teardown + terminal restore on exit  

**Nice to have**

- Status chips + negotiation status line  
- Help overlay (`?`) listing escape key  
- `--record-io`  
- “Open in external omp”  

**Later**

- Detach-survive / reattach after amux restart  
- Live session caps / LRU  
- Codex (or other) provider via config  
- Idle governance  

## 12. Risks

1. **Capability negotiation bugs** — lying about kitty/paste/mouse → silent UX gaps; §4.2.0 is the structural fix  
2. **2026 freeze** — missing `stop_sync` freezes pane; MUST implement timeout path  
3. **Raw stdin + mode mirror complexity** — highest eng risk; shell=crossterm, agent=raw  
4. **`PI_*` pin drift** — version-gated table may break on omp upgrades; keep overridable + logged  
5. **Occupied detection false negatives** — pgrep is best-effort  
6. **Resource use** — no live cap in v1  
7. **Outer terminal variance** — Shift+Enter depends on host keyboard protocol support  

## 13. Success criteria

- Add workspace `my-project` via modal browser  
- See existing omp sessions; resume one inside amux  
- Run two sessions; switch with `Ctrl+\`; background session keeps running  
- Agent-mode keyboard UX matches bare `omp` for normal coding turns (tools/ask)  
- Upgrading omp does not require amux patches to omp source  
- No amux code path that paints assistant transcripts from RPC events  

## 14. Approval / next steps

1. Review this revised spec  
2. On approval → implementation plan (`writing-plans`)  
3. Optionally checkout `opensource/dux` `origin/main` as reading reference  
4. Spike (recommended): minimal ratatui + one `omp` PTY pane + `Ctrl+\` escape + mode mirror / 2026 stop_sync before full sidebar  
5. Implement MVP  

## 15. Decision log

| ID | Decision |
|---|---|
| Grill Q7–Q10 | PTY wrap stock omp; Rust; exit kills children; lazy spawn; no live cap |
| UI | Classic dual pane; modal directory browser |
| Opus C1 / confirm 1A | Escape = `Ctrl+\` + double-tap literal (not Ctrl+B) |
| Opus C2 / confirm 2B | Negotiate keyboard protocol with outer terminal |
| Opus C7 / confirm 3A | Startup keys **drop** + count (not flush) |
| Opus I4 / confirm 4A | No host copy-mode; outer selection + OSC52 |
| Opus C8 / confirm 5A | Allow version-gated `PI_*` pins |
| Opus I6 / confirm 6B | flock + pgrep occupied detection |
| Opus I5 / confirm 7A | Process-group SIGHUP→TERM→KILL |
| Opus C3–C6 | Pin alacritty_terminal; 2026 stop_sync; mode mirror; writer queue; four-edge mouse |
