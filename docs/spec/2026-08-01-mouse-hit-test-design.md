# Mouse hit-test: pane focus + sidebar select

Date: 2026-08-01  
Status: implemented

## Problem

Mouse today only works meaningfully in **Agent** focus: SGR events inside the
PTY inner area are translated and forwarded to omp; a coarse
`col < sidebar_width` click drops to Nav. **Nav disables host mouse**
(`baseline_host_modes` → `1000l`…`1006l`), so workspace/session rows cannot be
clicked. Users want tmux-like click-to-focus and click-to-select without always
using `j/k` / `Ctrl-\`.

## Product rules (locked)

| Click target | Behavior |
|--------------|----------|
| Agent content area (PTY **inner** / preview inner) | `attach` **currently selected** session + `enter_agent`. If that id is already a **live non-Exited** focused PTY, only `enter_agent` (no re-spawn). Agent border/title (outside inner) → ignore. |
| Workspace list row `i` | `enter_nav` → `selected_ws = i` → reset session index as today (`selected_session = 0` or refresh equivalent) → refresh session list |
| Session list row `i` | `enter_nav` → `selected_session = i` only (**no attach**). Agent pane continues to follow selection (JSONL preview / live snapshot rules unchanged). |
| Sidebar chrome (border, section titles including Sessions `Borders::TOP`, empty rows past last item in ws/sess lists) | `enter_nav` only |
| Status bar / outside layout | Ignore |
| Shift+SGR | Yield to outer terminal selection (unchanged) |
| Modal open | Ignore hit-test clicks (keys still handle modal) |

**Agent → sidebar:** any **button press** that hits the sidebar (row or chrome)
**first** `enter_nav`, then apply select rules above. Keys return to Nav; do not
leave focus on Agent while selection changes. **Motion/drag/release must not**
trigger Nav/select (see Input routing).

**Rationale for Agent click = attach:** while in Nav, switching selection already
updates the Agent pane content (preview). Clicking that pane means “go into what
I’m looking at,” same as Enter on the selected row.

## Approach (chosen)

**Global hit-test on host SGR**, not a crossterm Event rewrite.

1. **Nav baseline matches dux / crossterm `EnableMouseCapture`:**
   `1000h+1002h+1003h+1015h+1006h`. Do **not** strip `1002`/`1003` — WebSSH
   clients in tmux may only deliver clicks with the full stack (A/B: dux works,
   amux with only 1000+1006 was silent while injected SGR still worked).
2. **Record geometry at draw time:** `sidebar_rect`, `ws_list_rect`,
   `sess_list_rect`, `pty_area` (agent **inner**, already stored).
3. **Shell hit-test dispatcher（仅 button press）:** gate with
   `sgr_is_button_press`; release/motion/drag **do not** trigger
   `enter_nav` / select / attach.
4. **Agent → omp forward（与上独立）:** when `Focus::Agent` and
   `translate_sgr_mouse_clipped` succeeds for `pty_area`, forward
   press/**drag**/**release** (existing path). Shell hit-test must not swallow
   those events. Sidebar hits never forward.

### Rejected alternatives

- Keep Nav mouse-off → cannot click lists.
- Full migrate to crossterm `Event::Mouse` → large rewrite of raw_input / mode_mirror.

## Geometry / hit-test details

**Coordinates:** after SGR parse, convert once to the same **0-based** screen
space as ratatui `Rect` (same as existing `parse_sgr_xy`).
`translate_sgr_mouse_clipped` still consumes **1-based wire** sequences — do not
mix spaces in hit-test math.

**draw_sidebar records:**

- `sidebar_rect` = left pane outer `area` (includes `Borders::ALL`).
- `ws_list_rect` = outer `inner`’s `Constraint::Length(7)` `chunks[0]` (no nested
  border; row `i` → `y + i`).
- `sess_list_rect` = Sessions block **`Borders::TOP` 之后的 inner**
  (`sess_block.inner(chunks[1])`), **not** `chunks[1]` itself.
- TOP title row ∈ sidebar chrome (inside `sidebar_rect` but outside both list
  rects).

Row mapping (MVP, no list scroll):

- Workspace row `i` at `ws_list_rect.y + i`. If `i >= workspaces.len()` or below
  visible bottom → treat as chrome (`enter_nav` only) / ignore past visible —
  empty lines in the Length(7) area are chrome.
- Session row `i` at `sess_list_rect.y + i`. If `i >= session_list.len()` or past
  visible bottom → chrome / ignore.

Collapsed sidebar (`sidebar_width == 0`): no sidebar hits; Agent-inner rule still
applies.

## Mode mirror changes

| Transition | Host mouse |
|------------|------------|
| Enter Nav / app start in Nav | `apply_nav_host_modes`: `1000h` + `1006h`; disable motion/drag variants |
| Enter Agent | Existing `apply_host_modes` from child `MirroredModes` (may upgrade to 1002/1003) |
| Leave Agent → Nav | Re-apply **`apply_nav_host_modes`** (not full mouse-off) |
| Process exit / panic restore | Full disable via `baseline_host_modes` / main restore — **exit path stays mouse-off** |

Split helpers: `apply_nav_host_modes` vs `baseline_host_modes` (exit only).
`run_inner` startup must call `apply_nav_host_modes`, not exit baseline.
Panic/teardown comments should say “exit restore,” not “Nav baseline.”

**Host mouse floor:** while amux runs, host always keeps at least `1000h`+`1006h`
so sidebar/pane hit-test works even when the child has not enabled mouse.
`apply_host_modes` may upgrade to 1002/1003 for omp; it must not fully disable
click capture. In-pane events are forwarded only when the child wants mouse.

## Input routing changes

### Agent path (`handle_agent_raw` today)

On SGR:

1. Shift → continue (yield).
2. If `translate_sgr_mouse_clipped` into `pty_area` succeeds:
   - Update `mouse_button_down` / last coords for leave-Agent release hygiene.
   - Forward press/drag/release via `route_agent_bytes`.
   - **Do not** run shell hit-test.
3. Else (outside PTY inner):
   - **Do not** set `mouse_button_down = true` for this event.
   - If **and only if** `sgr_is_button_press` and hit is inside `sidebar_rect`
     → `enter_nav` then select/chrome (ws/sess/chrome rules).
   - Because this press never entered `pty_area`, `enter_nav` **must not** emit
     a synthetic button-release to the child.
   - Otherwise ignore (Agent border, status, sidebar motion/release, etc.).

Do **not** use only `col < sidebar_width`; use stored rects.

### Nav / Modal path (`handle_shell_bytes`)

After Nav enables mouse:

1. Modal → ignore SGR (or only shift-yield).
2. Nav → shell hit-test on **button press** only (Agent inner →
   attach+enter_agent; sidebar → select / Nav). Ignore release/motion.

## Attach helper

Reuse `attach_selected` semantics with a clear short-circuit:

- No workspace / no session → status message, stay Nav.
- If `sessions` reports selected id **live and not Exited** and
  `focused_session_id == selected_id` → **only** `enter_agent` (do not call
  `attach_resume`; avoid leave/re-enter focus flicker when already in Agent).
- **Exited** or not live → existing resume/attach path, then `enter_agent`
  (Exited entries may still sit in `focused_session_id`; must respawn).
- Other attach errors: status only, no process abort (unchanged).

## Non-goals (MVP)

- Double-click to attach
- Wheel scroll for long workspace/session lists
- Drag to resize sidebar
- Click status bar chips
- Host selection without Shift while mouse reporting is on (inherent terminal
  tradeoff; document in `?` help if useful)

## Files (expected)

- `docs/spec/2026-08-01-mouse-hit-test-design.md` (this file)
- `src/shell/mode_mirror.rs` — `apply_nav_host_modes`; exit `baseline_host_modes` unchanged (mouse off)
- `src/shell/mod.rs` — geometry fields, hit-test, wire Nav + Agent mouse
- `src/mouse.rs` — optional pure helpers (point-in-rect, row index); keep unit
  tests for clipping/press
- Help text (`?` / status hints) — one line that clicks select / Agent attaches

## Test plan

- Unit: point-in-rect; ws/sess row index from `(x,y)` given fixture rects; press
  vs release/motion ignored by dispatcher; sidebar press does not require
  synthetic release path.
- Manual: Nav click ws → session list switches; click sess → preview switches,
  still Nav; click Agent → attach + Agent focus; from Agent click other sess →
  Nav + select, keys not to omp; drag inside omp PTY still works (1002/1003);
  dragging from PTY into sidebar does **not** steal focus mid-drag; Shift-drag
  still selects in outer terminal; quit restores no mouse capture.
- `cargo test --lib` + `cargo build --release`

## Review notes (2026-08-01)

Reviewer verdict was REQUEST_CHANGES; the following are incorporated above:

- B1: shell hit-test **press-only**; PTY drag/motion/release still forward.
- B2: `mouse_button_down` only when forwarding in-pane; no fake release on
  sidebar press → `enter_nav`.
- B3: same-session short-circuit requires **live non-Exited**.
- B4: 0-based hit-test; `sess_list_rect` = TOP inner, not `chunks[1]`.

## Open points (resolved)

- Agent click when selected ≠ live: **N/A for UX** — pane already follows
  selection; always attach selected.
- Agent + sidebar click: **always enter_nav first**, then select (press only).
