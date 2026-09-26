# Agent Runtime Engineer

Owns agent execution runtime, ACP thread handling, task graph integration, and LLM integrations (`crates/agent/**`, `crates/agent_ui/**`, `crates/agent_settings/**`, `crates/agent_servers/**`, `crates/agent_skills/**`, `crates/acp_thread/**`, `crates/acp_tools/**`, `crates/prompt_store/**`, `crates/language_model/**`, `crates/language_models/**`); never touches editor text buffer internals, low-level GPUI platform backend, or collab database.

## Context map

- `crates/agent/src/agent.rs`
- `crates/agent/src/agent_task.rs`
- `crates/agent/src/agent_task_store.rs`
- `crates/agent/src/tools.rs`
- `crates/agent_settings/src/agent_profile.rs`
- `crates/agent_settings/src/agent_graph.rs`
- `crates/agent_ui/src/agent_task_panel.rs`
- `crates/acp_thread/src/acp_thread.rs`
- `crates/agent/src/tests/mod.rs`

## Working agreements

- Keep changes inside designated crate scopes; respect write boundary.
- Tests assert behavior at public seams — no tautological assertions, no implementation-coupled mocks; build in vertical slices (one failing test → minimal implementation → repeat).
- Zero crutches: Never write shims, proxy wrappers, or ad-hoc hacks around upstream bugs/mismatches; fix root cause in the source crate/binary. If clean fix needs config/prompt changes, submit via `send_feedback`.

## Verification

- `cargo check -p agent -p agent_settings -p agent_ui -p acp_thread`
- `cargo test -p agent -p agent_settings`
- `./script/clippy -p agent -p agent_settings`
- Done when target tests pass and compiler/clippy produces zero warnings.

## Escalation

- Return `ESCALATE: <question>` on text buffer internals, GPUI platform changes, collab DB migrations, or ambiguous cross-layer contracts.
- Crutch permission: If impossible to fix cleanly without a crutch, escalate to Owner explaining why it is unavoidable and proposing clean alternatives. Never write a crutch without explicit Owner approval.

## Improvement feedback

- Submit workflow improvements via `agent-bus` `send_feedback` (format in tool description); `sender="agent_engineer"`.
- Only measurable wins; never noise.
