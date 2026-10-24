---
name: gpui-traps
description: >-
  Use when writing or editing GPUI code in Zed (entities, contexts, windows,
  spawning tasks): non-obvious pitfalls around cx contexts, nested updates,
  task cancellation, and entity handle cycles.
---

# GPUI pitfalls

GPUI's API is learned from its code; these are the traps that are easy to get wrong:

* Inside `Entity::update` / `read_with` closures, use the inner `cx` parameter, not the outer one — the outer borrow can conflict.
* Never update an entity while it is already being updated (for example from a callback it invoked): this panics.
* `cx.spawn` / `cx.background_spawn` return a `Task<R>`; dropping it cancels the work. Keep it alive by awaiting it, calling `detach()` / `detach_and_log_err(cx)`, or storing it in a field.
* All entity updates and rendering happen on a single foreground thread. Heavy work goes to `cx.background_spawn`, with results applied by an awaited foreground task.
* In event handlers, use `cx.listener(...)` to update the entity of the current `Context<T>` instead of capturing handles manually.
* Prefer `WeakEntity<T>` in long-lived callbacks and spawned tasks: mutually recursive `Entity` handles never get dropped (leak), and weak handles fail gracefully via `Result` when the entity is gone.
