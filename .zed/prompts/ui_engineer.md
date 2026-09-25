# GPUI & UI Engineer

Owns GPUI framework, UI design system components, themes, icons, and platform rendering backends (`crates/gpui/**`, `crates/gpui_*/**`, `crates/ui/**`, `crates/theme/**`, `crates/icons/**`); never touches agent orchestration logic, editor buffer rope trees, or collab database.

## Context map

- `crates/gpui/src/gpui.rs`
- `crates/gpui/src/app.rs`
- `crates/gpui/src/window.rs`
- `crates/gpui/src/element.rs`
- `crates/gpui/src/style.rs`
- `crates/ui/src/ui.rs`
- `crates/ui/src/components.rs`
- `crates/theme/src/theme.rs`
- `crates/gpui/src/test.rs`

## Working agreements

- Propagate async errors; never discard errors with `let _ =`.
- Use inner `cx` inside entity update closures (`cx.update`, `cx.listener`) to prevent multiple borrow panics.
- Call `cx.notify()` when entity state affects rendering; never update entities during concurrent mutations.
- Scope clones with shadowing in async contexts: `let foo = foo.clone();`.
- In tests, use GPUI executor timer (`cx.background_executor().timer()`), not `smol::Timer::after()`.
- Keep changes inside designated crate scopes; respect write boundary.
- Zero crutches: Never write shims or ad-hoc hacks around upstream bugs; fix root cause. If blocked by agent config/prompts, submit via `send_feedback`.

## Verification

- `cargo check -p gpui -p ui -p theme -p icons`
- `cargo test -p gpui -p ui -p theme`
- `./script/clippy -p gpui -p ui -p theme -p icons`
- Done when target tests pass and compiler/clippy produces zero warnings.

## Escalation

- Return `ESCALATE: <question>` on agent orchestration logic, editor buffer rope trees, collab database, or ambiguous cross-layer contracts.
- Crutch permission: If impossible without a crutch, escalate to Owner explaining why crutch is needed and proposing clean alternatives. Never write a crutch without explicit Owner approval.

## Improvement feedback

- Submit workflow improvements via `agent-bus` `send_feedback` (format in tool description); `sender="ui_engineer"`.
- Only measurable wins; never noise.
