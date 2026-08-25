> [!IMPORTANT]
> Remove this line to confirm you've reviewed this PR before submitting.

# Zed

## Возможности этого форка

Форк расширяет агентов Zed до иерархии специализированных агентов с декларативными профилями. Все настройки — обычные профили в `settings.json` (user или `.zed/settings.json` проекта; проект переопределяет пользовательские по ключу).

### Иерархия агентов (делегирование)

- Профиль с блоком `delegation` может порождать субагентов через встроенный инструмент `spawn_agent(profile, task_id?, ...)`; без блока — соло-агент, спавн для него запрещён.
- `delegation.allowed` — список профилей, которым можно делегировать; `max_depth` (1–5, по умолчанию 1) — глубина вложенности.
- Дополнительно действует глобальный гейт `agent.nested_sub_agents` (`enabled`, `max_depth`, `max_concurrent` — лимит одновременных субагентов на всё дерево сессий).
- Циклы и висячие ссылки в графе делегирования ловятся валидатором при загрузке настроек (ошибки — в лог).
- Результат работы субагента возвращается в тред родителя; follow-up — через `session_id`.

### Профили

Помимо стандартных полей (`tools`, `context_servers`, `default_model`) профиль поддерживает:

- `custom_prompt` — слой инструкций в системном промпте агента;
- `description` — описание для каталога делегирования (видит родитель);
- `skills` — фильтр видимых скиллов;
- `delegation` — правила делегирования.

Все новые поля редактируются в UI: Agent Panel → Manage Profiles → Configure Delegation (allowed-чеклист, max depth, description) и Configure Skills (ограничение + чеклист скиллов).

### Скиллы

Скиллы — из `~/.agents/skills/` (глобальные), `.agents/skills/` проекта (перекрывают глобальные по имени, требуют trust) и встроенных. Профильный фильтр `skills` применяется в трёх точках: каталог в системном промпте, `available_skills` и инструмент `skill`.

### MCP-серверы и секреты

В `command.env` и `command.args` контекст-серверов поддерживается интерполяция `${VAR}` из окружения — секреты не попадают в коммитируемые настройки. Незаданная переменная — явная ошибка с её именем.

### Пример

```jsonc
// .zed/settings.json
{
  "agent": {
    "default_profile": "orchestrator",
    "nested_sub_agents": { "enabled": true, "max_depth": 3 },
    "profiles": {
      "orchestrator": {
        "name": "Orchestrator",
        "description": "Coordinates implementation",
        "delegation": { "allowed": ["backend"], "max_depth": 1 }
      },
      "backend": {
        "name": "Backend Agent",
        "custom_prompt": "You implement backend tasks...",
        "skills": ["deploy"]
      }
    },
    "context_servers": {
      "task-graph": {
        "command": { "path": "taskgraph.exe", "args": [], "env": { "TGR_TOKEN": "${TGR_TOKEN}" } }
      }
    }
  }
}
```

`spawn_agent(profile="backend", task_id="TASK-42")` запустит сессию профиля `backend`; ссылка на задачу попадёт в промпт субагента, детали он получит сам через свои MCP-инструменты (Task Graph Runtime подключается как обычный context-сервер).

---

[![Zed](https://img.shields.io/endpoint?url=https://raw.githubusercontent.com/zed-industries/zed/main/assets/badge/v0.json)](https://zed.dev)
[![CI](https://github.com/zed-industries/zed/actions/workflows/run_tests.yml/badge.svg)](https://github.com/zed-industries/zed/actions/workflows/run_tests.yml)

Welcome to Zed, a high-performance, multiplayer code editor from the creators of [Atom](https://github.com/atom/atom) and [Tree-sitter](https://github.com/tree-sitter/tree-sitter).

---

### Installation

On macOS, Linux, and Windows you can [download Zed directly](https://zed.dev/download) or install Zed via your local package manager ([macOS](https://zed.dev/docs/installation#macos)/[Linux](https://zed.dev/docs/linux#installing-via-a-package-manager)/[Windows](https://zed.dev/docs/windows#package-managers)).

Other platforms are not yet available:

- Web ([tracking discussion](https://github.com/zed-industries/zed/discussions/26195))

### Developing Zed

- [Building Zed for macOS](./docs/src/development/macos.md)
- [Building Zed for Linux](./docs/src/development/linux.md)
- [Building Zed for Windows](./docs/src/development/windows.md)

### Contributing

See [CONTRIBUTING.md](./CONTRIBUTING.md) for ways you can contribute to Zed.

Also... we're hiring! Check out our [jobs](https://zed.dev/jobs) page for open roles.

### Licensing

Zed source code is licensed primarily under GPL-3.0-or-later, with Apache-2.0 components where marked.

License information for third party dependencies must be correctly provided for CI to pass.

We use [`cargo-about`](https://github.com/EmbarkStudios/cargo-about) to automatically comply with open source licenses. If CI is failing, check the following:

- Is it showing a `no license specified` error for a crate you've created? If so, add `publish = false` under `[package]` in your crate's Cargo.toml.
- Is the error `failed to satisfy license requirements` for a dependency? If so, first determine what license the project has and whether this system is sufficient to comply with this license's requirements. If you're unsure, ask a lawyer. Once you've verified that this system is acceptable add the license's SPDX identifier to the `accepted` array in `script/licenses/zed-licenses.toml`.
- Is `cargo-about` unable to find the license for a dependency? If so, add a clarification field at the end of `script/licenses/zed-licenses.toml`, as specified in the [cargo-about book](https://embarkstudios.github.io/cargo-about/cli/generate/config.html#crate-configuration).

## Sponsorship

Zed is developed by **Zed Industries, Inc.**, a for-profit company.

If you’d like to financially support the project, you can do so via GitHub Sponsors.
Sponsorships go directly to Zed Industries and are used as general company revenue.
There are no perks or entitlements associated with sponsorship.
