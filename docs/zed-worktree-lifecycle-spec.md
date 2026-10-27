# Zed Change Specification

Scope: worktree lifecycle, task store, task panel, turn telemetry.
Do not change the TGS contract. See `tgs-v3-hardening-spec.md`.

## 1. `crates/agent/src/tools/spawn_agent_tool.rs`

### 1.1 Terminal cleanup at session end

Current code gates cleanup on `task_worktree::is_task_terminal`.

- Remove the `is_task_terminal` gate.
- After the subagent session ends, run cleanup when: the worktree
  exists; no commit error is recorded; no merge conflict is recorded.
- Call `mark_task_terminal` after `set_task_status_registered`
  completes with `Completed`.

### 1.2 Diff snapshot at turn end

Where the code collects `branch` and `commit_sha` for a turn:

- Read the task base sha with `task_worktree_base_sha`.
- If a base sha and a worktree exist, run in the worktree:

```
git diff --shortstat <base_sha>
```

- Parse `files_changed`, `insertions`, `deletions`.
- Pass the `diff` object to `record_turn`. If no base sha or no
  worktree exists, pass no `diff` field. Do not return an error when
  the command fails; omit the field.

## 2. `crates/agent/src/thread.rs`

### 2.1 Terminal tool detection

Extend `extract_terminal_task_id`:

- Match tool names that end with `task_update` when the input has
  `status` from `{completed, failed, cancelled}`.
- Extract the task id from input `task_id` or `id`. Fall back to the
  thread task id.
- Keep the current matches for names that end with `task_complete`,
  `task_fail`, `task_cancel`.

### 2.2 Tests

Mirror the tests of `extract_completed_goal_id`. Add cases:
`task_update` with status `completed` returns the task id; with status
`running` returns none; with another tool name returns none.

## 3. `crates/agent/src/task_worktree.rs`

### 3.1 Removal order

Apply to `release_stale_task_worktree` and
`remove_task_worktree_internal`:

1. Find the worktree entity in the project by path. Call
   `project.remove_worktree(id, cx)`.
2. Call `repository.remove_worktree(path, force)`.
3. If the directory still exists, call `fs.remove_dir` with recursive
   options.

### 3.2 Removal retry

- When step 2 or 3 fails on Windows with a sharing violation, retry
  the failed step. Use a maximum of 5 attempts with backoff
  (50 ms to 800 ms).
- After the last failed attempt, record the error with
  `record_task_worktree_cleanup_error`. Return the error.

### 3.3 Bounded slot wait

In `acquire_task_worktree_slot`:

- Replace `let _ = receiver.await` with a select of the receiver and a
  timer of 45 seconds.
- On timeout, return an error. The error message must list: the pool
  limit, each task id that holds a slot, and the hold reason (active
  session, commit error, merge conflict). Add the setting name
  `task_worktree_limit` to the message.

## 4. `crates/agent/src/agent_task_store.rs`

### 4.1 Refresh

- Fetch only `task_graph` in `refresh`. Remove the `list_events` call.
- Remove the `events` field, `events()` and
  `policy_denied_event_for_task`.

### 4.2 Terminal status reaction

After a graph refresh:

- For each task that changed to a status from `{completed, failed,
  cancelled, archived}` and has a registered worktree, schedule
  `auto_cleanup_task_worktree`.
- Do not clean a worktree that has an active session, a recorded
  commit error, or a recorded merge conflict.

## 5. `crates/agent/src/agent_task_panel.rs`

- Sort tasks by `created_at` from the task summary. Use task id as the
  tie-breaker.
- Delete `compute_task_creation_timestamps`.
- Remove the `events` parameter from `build_goal_groups` and
  `build_task_rows`.

## 6. `crates/agent/src/agent_task.rs`

- Add to `AgentTaskMetrics` the optional field `diff` with
  `files_changed`, `insertions`, `deletions` (int). Default to none.

## 7. `crates/agent/src/mcp_agent_task_provider.rs`

- Extend `record_turn` with the `diff` argument. Send it as the `diff`
  object.
- Remove `list_events`.

## 8. Tests

1. `spawn_agent`: a subagent that returns without a terminal tool call
   gets its worktree cleaned after the session ends.
2. `spawn_agent`: a session with a commit error keeps the worktree.
3. Turn end with a worktree: `record_turn` receives a `diff` object
   with three non-negative integers.
4. Worktree removal: `project.remove_worktree` runs before
   `repository.remove_worktree` (order assertion with a test project).
5. Removal retry: a fake fs that fails twice with a sharing violation
   then succeeds; removal completes.
6. Slot wait: a full pool with no eviction candidates returns a timeout
   error within the test timer; the error lists holder ids.
7. Store: a graph refresh with a task that changed to `completed`
   triggers cleanup; a task with an active session does not.
8. Panel: tasks sort by summary `created_at`; no code path reads
   events.
