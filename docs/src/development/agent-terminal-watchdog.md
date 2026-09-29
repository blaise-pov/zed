# Agent Terminal Tool Watchdog

Status: Final (includes deltas: event-based waits, spinner convention, network IO as an open question).

## Problem

An agent terminal command can hang (waiting for interactive input, a pager, a block on network or another resource). If the model did not set `timeout_ms` — and for an unforeseen hang it won't — the tool call hangs forever, the agent stalls, and a human must intervene. A blanket hard timeout is unacceptable: legitimately long commands (builds that run for hours) and commands waiting for external events (`gh run watch`, `kubectl wait`, `docker wait`) are silent and/or CPU-passive but healthy.

## Solution

Two independent harness-level mechanisms (not model-level):

- **Idle detection**: kill when, simultaneously across a window — there is no PTY output AND the process tree's total CPU is below a threshold AND there is no disk IO. The conjunction distinguishes a quiet hang from silent healthy work (build/link — CPU active; wrapper buffering — CPU active; event-waiter — output active).
- **Hard cap**: protection against busy-runaway (a noisy infinite loop that idle detection cannot see). Applied only when the model did not set `timeout_ms`.

Any kill returns a diagnostic result to the model with partial output and remediation guidance, so the self-correction loop closes without a human.

## User Stories

1. The user is not interrupted by a hung command: the watchdog kills it and the agent fixes the command itself.
2. A long active command is never killed by idle detection, no matter how long it runs.
3. The watchdog is configurable: windows, thresholds, cap, on/off — globally and per project.
4. On a kill the model receives partial output, likely causes, and what to fix for a meaningful retry.
5. A model that set `timeout_ms` fully owns the call's lifetime — the watchdog does not intervene.
6. A command waiting for a finite external event, with `timeout_ms` set, completes without watchdog intervention; without one it survives at most one harmless kill and retry.
7. A wrapper (rtk) with a spinner does not break detection (see Spinner convention).

## Implementation Decisions

### Activity signals

| Signal | Source | "Active" means |
| --- | --- | --- |
| Output | Monotonic counter of output wakeups (ticks) from the PTY reader (added in the `terminal` crate; `get_content()` is unreliable due to scrollback trimming; a tick counter is also immune to spinner CR-rewrite — ticks keep coming) | counter grew since the last poll |
| CPU | `sysinfo` (already in workspace, 0.39), process tree rooted at the shell PID, summed over cumulative `cpu_time` | delta/interval ≥ threshold |
| Disk IO | `sysinfo` per-process `disk_usage` (read + write) over the tree | byte delta > ~1 KB (internal constant) |

- The process tree is the descendant closure from the shell PID (the PID is exported from the `terminal` crate — integration point). It must be the tree, not the direct child: the shell and rtk idle while `cargo` works.
- Signals are sampled every `poll_interval_ms` (default 5 s) by a watchdog future raced inline with the command future via `futures::select_biased!` in the terminal tool (no background task stored in a field); the race ends when the command completes.
- Watchdog lifecycle: from terminal creation to process exit.

### Trigger rules and priorities

```
kill_idle := (now - last_progress) > idle_window            // any of the 3 signals updates last_progress
kill_cap  := (now - started_at) > hard_timeout_ms           // settings-only; watchdog is fully disabled when timeout_ms is set
```

| `timeout_ms` in the call | Watchdog behavior |
| --- | --- |
| set | fully disabled — the model's timer governs |
| unset | idle from settings + cap from settings/defaults |

Rationale for the disable: an explicit `timeout_ms` is a declaration of expected duration. Legitimately quiet commands exist (`sleep`, event waits) that the conjunction would kill and that cannot be "fixed". Clean ownership model: didn't foresee → watchdog; foresaw → its timer.

Order in the inline race (`futures::select_biased!` in the terminal tool future): completion > user stop > cap > idle > model timer.

### Tool description delta

Extend the existing watchers bullet: "commands waiting for a finite external event (`gh run watch`, `kubectl wait`, `docker wait`, lock waits) must be accompanied by `timeout_ms` with margin over the expected duration".

### Degradation: WSL and remote

The host's sysinfo cannot see processes inside the WSL sandbox or on the remote machine. Degradation: CPU/IO signals unavailable → idle by output alone with a widened window (`idle_timeout_no_probe_ms`, default 10 min). Document that detection is weaker in these environments with a buffering wrapper; the rtk spinner partially compensates. A bridge via `wsl.exe` is out of scope.

### Model diagnostics

Two new `process_content` branches modeled on the existing `timed_out` one. Separate from `user_stopped`: a watchdog kill is not a user stop — the model retries meaningfully instead of asking.

Idle:

```
Command "<cmd>" was stopped by the watchdog: no terminal output, no CPU usage, and no disk
activity from the process tree for <N>s. This usually means the command is blocked waiting
for interactive input (confirmation prompt, pager, editor), stalled on a network/resource,
or legitimately waiting for an external event (CI run, container, lock). Output captured
before stopping:

<partial output>

Before retrying: check for -y/--yes/--no-pager/non-interactive flags and verify the command
does not expect input; if it legitimately waits for an external event, rerun with timeout_ms
set to the expected duration plus margin, or switch to periodic polling of the event source.
Do not retry the exact same command unchanged.
```

Cap:

```
Command "<cmd>" exceeded the default maximum runtime of <T> (timeout_ms was not set).
Output captured before stopping:

<partial output>

If this command legitimately needs longer, rerun it with timeout_ms set to the expected
duration plus margin.
```

### Settings

Section under agent settings; Zed's layering (default → user → project) provides per-project overrides for free:

```json
{
  "agent": {
    "terminal_watchdog": {
      "enabled": true,                    // master switch
      "idle_timeout_ms": 300000,          // idle window; null disables idle detection
      "idle_cpu_threshold_percent": 5.0,  // tree CPU below this counts as "idle"
      "hard_timeout_ms": 3600000,         // cap when timeout_ms is absent; null disables
      "poll_interval_ms": 5000,           // signal sampling period
      "idle_timeout_no_probe_ms": 600000  // idle window where CPU/IO sampling is unavailable
    }
  }
}
```

Defaults follow the cost asymmetry: a false kill is expensive (lost work), a missed hang is cheap (visible in the UI, stopped by hand).

### Spinner convention of the wrapper (Zed side: zero changes)

A wrapper (rtk) may spin a one-line spinner during output-suppression periods. To Zed this is just a byte stream — the output counter ticks and idle stays off. Conscious trade-off: Zed cannot verify spinner semantics and trusts the convention "spinner = real output suppressed, ≠ the process is alive". A wrapper violating the convention masks hangs (active output keeps idle off even at zero CPU) — the wrapper's responsibility, to be recorded in documentation. Stripping the last spinner frame from model output is optional cosmetics (rtk cleans its own line).

### Edge cases

- The kill reuses `terminal.kill()` — sandbox/proxy teardown is already guaranteed by the current timeout path.
- A test with an infinite loop (output stalls, a thread burns CPU): idle does not trigger (conjunction) — the cap catches it. Role separation is correct.
- Multiple agent terminals: one watchdog instance per terminal.
- UI badge for watchdog stops: optional, meta key modeled on `SANDBOX_NOT_APPLIED_META_KEY`.

## Testing Decisions

- Diagnostics: unit tests for both branches (texts, partial/empty output) at the `process_content` seam, following the existing `test_process_content_*` suite.
- Conjunction logic: signals behind an injectable `ActivityProbe` trait (output/CPU/IO); tests on fakes.
- Integration (second-scale windows): quiet command → kill + diagnostic; command with output → survives; busy-loop without output → CPU keeps it alive; `timeout_ms` set → watchdog does not intervene.
- Timers only through the GPUI executor (`cx.background_executor().timer(...)`), never `smol::Timer` — repo rules.
- Settings: defaults, null semantics, layering overrides.

## Out of Scope

- Background launch/polling (`exec_start`/`exec_check`) — separate spec.
- ACP protocol changes.
- A CPU/IO bridge for WSL and remote.
- Per-process network IO (→ open question 3).
- Verifying spinner semantics on the Zed side.

## Open Questions

1. Calibration of the defaults (5 min idle / 1 h cap / 10 min no-probe) pending feedback.
2. Whether to require user confirmation for retries with `timeout_ms` above a threshold — not in the MVP.
3. Per-process network IO as a fourth signal: cleanly separates "waiting on an event with traffic" from "hung on a dead socket"; sysinfo has no per-process network — Linux `/proc/net` + inode mapping, Windows ETW. Add if false positives on event-waiters become frequent.
