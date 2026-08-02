# Pane-clipped text selection (amux)

## Problem

With host mouse reporting on, Shift+drag often falls through to the **outer
terminal** native selection, which is screen-wide and includes the workspace
sidebar. tmux avoids this by owning selection inside the pane.

## Behavior (v1)

1. **Host-owned stream selection** clipped to the Agent content rect (`pty_area`,
   or body below the exited banner).
2. **When it activates**
   - Nav focus, drag in Agent content → always host selection.
   - Agent focus, child has no mouse → plain left-drag → host selection.
   - Agent focus, child wants mouse → **Shift or Alt/Meta** + left-drag → host
     selection; plain drag still forwards to the child.
3. **Visual**: `REVERSED` (or selection theme) overlay on selected cells each frame.
4. **Release**: extract visible text → OSC 52 `ClipboardStore` to the host; clear
   selection; status `copied N chars`.
5. **Click without drag**: do not copy; in Agent + child-mouse, forward as today
   (press is delayed until we know it is not a drag — see implementation: if
   child-mouse and no modifier, forward immediately without host selection).

## Non-goals (v1)

- Full keyboard copy-mode
- Selecting outside the current viewport / across scrollback without scrolling first
- Changing child OSC 52 pass-through

## Spec delta

Supersedes §4.2.6b “Shift+drag yield to outer” while mouse reporting is on:
Shift/Alt drag is handled by amux when the terminal still delivers those SGR
events. Prefer Alt+drag if the terminal uses Shift-bypass for native selection.
