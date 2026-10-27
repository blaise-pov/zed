# TGS Change Specification

## 1. `task_graph`

### 1.1 Input

```json
{"goal_id": "string (optional)", "status": "string (optional)"}
```

- `goal_id`: current behavior.
- `status`: return only tasks with this status. Do not filter goals.
  Without the parameter, return all tasks and all goals.

### 1.2 Task objects

Each task object must contain:

```json
{"created_at": "int, epoch ms", "updated_at": "int, epoch ms"}
```

Update `updated_at` of a task when: its status changes; any field
changes; a `depends_on` edge is added or removed (update both tasks of
the edge); a turn is recorded.

### 1.3 Goal objects

Each goal object must contain `created_at` and `updated_at`
(int, epoch ms). Update `updated_at` of a goal when its fields or
status change.

### 1.4 Order

- Sort tasks by `created_at` ascending. Use `id` ascending as the
  tie-breaker.
- Sort goals by `created_at` ascending. Use `goal_id` ascending as the
  tie-breaker.

## 2. `task_update`

Behavior of `depends_on` changes as follows.

When the client replaces `depends_on`, check graph acyclicity in the
same transaction:

- If the check finds a cycle, roll back the full transaction. Return
  the error `cycle_detected: TASK-A -> TASK-B -> TASK-A`.
- If the new list equals the current list, do nothing. Return `Ok`.

## 3. `goal_update`

Behavior of `status: "completed"` changes as follows.

Permit the transition only when each task with this `goal_id` has a
status from `{completed, failed, cancelled, archived}`.

Otherwise, do not apply the change. Return the error:

```
goal_has_active_tasks: TASK-1, TASK-2
```

## 4. `artifact_list`

### 4.1 Input

```json
{"task_id": "string", "kind": "string (optional)"}
```

`kind`: return only artifacts of this kind.

### 4.2 Documentation

List these kinds in the tool description: `execution_report`,
`task_ref`, `graduation_summary`, `research_findings`,
`review_verdict`. Do not validate kind values.

## 5. `task_record_turn`

### 5.1 Input

Add the optional field:

```json
"diff": {
  "files_changed": "int",
  "insertions": "int",
  "deletions": "int"
}
```

### 5.2 Semantics

`diff` is a cumulative snapshot: task base commit against the current
worktree state.

- Store the snapshot in the turn row. Do not change it.
- Set the task-level metric `diff` to the value of the latest turn
  record. Do not sum snapshots across turns.
- If the field is absent, set the task-level metric `diff` to null.

### 5.3 Output

Add `diff` to the metrics object of `task_graph` task summaries.

## 6. `events_list`

Remove the tool. Keep the `events` table. Keep `events_tail` in
`task_get` unchanged.

## 7. Tests

1. `task_graph` returns tasks sorted by (`created_at`, `id`). Two tasks
   from one transaction keep a stable order by `id`. All tasks and
   goals contain `created_at` and `updated_at`.
2. `updated_at` increases after: a status change, a field change, a
   `depends_on` replacement, a recorded turn. A new entity has
   `updated_at` equal to `created_at`.
3. `task_update` with a cyclic `depends_on` returns `cycle_detected`.
   The `dependencies` table does not change. An identical replacement
   returns `Ok` and changes nothing.
4. `goal_update` to `completed` with a task in `ready` returns
   `goal_has_active_tasks` with the task id list. After all tasks reach
   terminal statuses, the same call returns `Ok`.
5. `task_graph` with `status=ready` returns only ready tasks and all
   goals.
6. `artifact_list` with `kind=task_ref` returns only artifacts of that
   kind.
7. `task_record_turn` with `diff` stores the snapshot in the turn row.
   The task metric equals the latest snapshot. Two turns with snapshots
   (10, 15) produce metric 15, not 25. A turn without `diff` sets the
   metric to null.
