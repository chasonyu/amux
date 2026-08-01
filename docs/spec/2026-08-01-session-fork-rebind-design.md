# omp fork/branch live rebind

Date: 2026-08-01  
Status: implemented

## Problem

omp `/fork` and file-creating `/branch` rebind the **same process** to a new JSONL
(`sessionId` / `sessionFile` change). amux kept `live[oldId]` + `focused_session_id`,
so the sidebar selection, live dot, Agent title, and status name pointed at the
**parent** while I/O already went to the **child**.

## Approach

1. Read `header.parentSession` (uuid for fork, source path for branch).
2. Per workspace, track `known_disk_ids`. First scan **seeds only** (no adopt of historical forks).
3. When a **new** jsonl appears whose `parentSession` refers to a non-exited live session → move `LiveEntry` to the child id, reacquire lock, push `(old, new)` rebind.
4. UI `refresh_sessions` applies rebinds to `focused_session_id` / selection; Agent header shows session **name**.

## Non-goals

- Parsing PTY text / OSC for session id
- Default `/branch`→`/tree` (same file; no rebind needed)
