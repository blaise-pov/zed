> [!IMPORTANT]
> Remove this line to confirm you've reviewed this PR before submitting.

# Zed

## Возможности этого форка

Форк превращает Zed в IDE и Execution Runtime для **иерархии специализированных автономных агентов**. Все настройки определяются декларативно в `settings.json` (пользовательском или `.zed/settings.json` проекта; настройки проекта дополняют и переопределяют глобальные).

В качестве Control Plane для управления целями, графом задач, ревью и координацией используется [**Task Graph Runtime (TGR)**](https://github.com/blaise-pov/tgr) — автономный MCP-сервер.

---

### 1. Профили агентов (Agent Profiles)

Каждый агент — это профиль с границами ответственности:

- `custom_prompt` — слой специализированных системных инструкций;
- `description` — описание для каталога делегирования;
- `skills` — белый список доступных скиллов;
- `delegation` — правила делегирования (`allowed` профили, `max_depth`);
- `tool_permissions` — гранулярные политики инструментов (`always_allow`, `always_deny`, `write_scopes`);
- `default_model` / `tools` / `context_servers` — стандартные переопределения модели, тулов и MCP-серверов.

Настройки профилей редактируются как в JSON, так и в UI: **Agent Panel → Manage Profiles** (редакторы Delegation, Skills и Tool Permissions).

---

### 2. Иерархия агентов и делегирование (`spawn_agent`)

- Профиль с блоком `delegation` может порождать субагентов через `spawn_agent(profile, task_id?, ...)`. Профиль без блока считается соло-агентом (спавн запрещен).
- `delegation.allowed` задает разрешенные дочерние профили, `max_depth` (1–5) — допустимую глубину рекурсии.
- Глобальный лимит `agent.nested_sub_agents` (`enabled`, `max_depth`, `max_concurrent`) ограничивает параллелизм во всем дереве сессий.
- Модуль `AgentGraph` статически валидирует граф делегирования при загрузке настроек (проверка циклов и недостижимых узлов).
- Субагент сохраняет профиль при смене родительского профиля; результат возвращается в тред родителя, продолжение диалога — через `session_id`.

---

### 3. Безопасность автономных агентов (Tool Permissions & Write Scopes)

Для защиты кодовой базы при автономной работе агентов:

- **Приоритет глобального Deny**: глобальные запреты нельзя переопределить профилем.
- **Fail-Closed для автономных профилей**: если профилю заданы `tool_permissions`, любые действия, требующие подтверждения пользователя (`Confirm`), **автоматически отклоняются** (`PolicyDenied`), предотвращая зависание агента. Невалидные glob-шаблоны в `write_scopes` и невалидные regex-правила тоже блокируют инструмент (fail-closed), а не молча игнорируются.
- **Write Scopes (`write_scopes`)**: файловые операции (`edit_file`, `write_file`, `copy_path`, `move_path`, `delete_path`, `create_directory`) ограничены списком glob-шаблонов (например, `["backend/**", "proto/**"]`). Попытка изменить файлы вне скоупа блокируется.
  - Scopes задаются **per-tool**: инструмент без собственной записи `write_scopes` ограничен только `default` профиля. Рекомендуется `default: "deny"` с явными `allow`-правилами, чтобы забытый инструмент не остался неограниченным.
- **Anti-Escape & Sandbox Protection**: попытки выхода за пределы рабочей директории через симлинки, изменение чувствительных файлов (`.zed/settings.json`, `.cargo/config.toml`) или глобальных скиллов (`~/.agents/skills`) блокируются с кодом `PolicyDenied`.
- **Ограничения автономных профилей** (побочный эффект fail-closed, важно учитывать при настройке):
  - Любая sandbox-эскалация запрещена: терминал с повышенными правами, создание директорий вне проекта и `fetch` к ещё не выданным хостам возвращают `PolicyDenied` (хосты нельзя выдать без пользовательского промпта).
  - `rename_symbol` запрещён: LSP-переименование правит произвольный набор файлов проекта, поэтому не может быть ограничено `write_scopes`. Используйте `edit_file`.

---

### 4. Панель задач (Agent Task Panel) и изоляция в Git Worktree

Интегрированный UI для работы с задачами TGR:

- **Дерево задач**: визуализация иерархии Goal → Task → Subtasks со статусами (`Ready`, `Claimed`, `Running`, `Review`, `Waiting for Approval`, `Completed`, `Failed`, `Stale`, `Blocked`) и номером текущей попытки (`#attempt`).
- **Действия**: запуск (`Run Task`), утверждение (`Force Approve`), запрос доработок (`Request Changes`), отклонение (`Reject`), повтор (`Retry`).
- **Diff & Timeline**: просмотр изменений задачи относительно базовой ветки (`View Task Diff`) и живая хронологическая лента событий (`AgentTaskTimeline`).
- **Изоляция в Git Worktree**: автоматическое создание ветки `agent-task/{task_id}` и отдельной рабочей директории. Параллельные воркеры работают в изолированных файловых деревьях, не мешая друг другу.

---

### 5. Интеграция с Task Graph Runtime (TGR)

[**TGR (Task Graph Runtime)**](https://github.com/blaise-pov/tgr) — легковесный, независимый Control Plane демон на Go (`cmd/taskgraph`), предоставляющий MCP-сервер по протоколу JSON-RPC 2.0 (stdio) на базе SQLite WAL:

- **Задачи и DAG**: 12-состояний жизненного цикла, проверка ацикличности, pull-based планировщик с учетом приоритетов.
- **Лизинг и Recovery**: атомарный захват задач (`task_claim`), аренда с heartbeat и автоматический сборщик зависших воркеров при сбоях (`recovery.Worker`).
- **Двухфазное ревью**: встроенный Review & Approval Workflow с защитой от саморевью (**Dual-Actor Safety**).
- **База знаний и обучение**: фиксация выводов (`lessons`) и предложений новых навыков (`skill_candidates`).
- **Артефакты**: версионируемые неизменяемые результаты работы.

Zed взаимодействует с TGR через встроенный `McpAgentTaskProvider` и реактивный `AgentTaskStore`.

---

### 6. Скиллы (Skills) и 3-уровневая фильтрация

Скиллы загружаются из `~/.agents/skills/` (глобальные), `.agents/skills/` проекта (перекрывают глобальные по имени, требуют доверия) и встроенных наборов. Фильтр профиля `skills` работает на 3 уровнях:
1. Каталог скиллов в системном промпте.
2. Список доступных скиллов (`available_skills`) в сессии.
3. Инструмент `skill` (блокирует неразрешенные вызовы).

---

### 7. Rate-Limit парковка (Adaptive Backoff)

При ошибках `429 Too Many Requests` от LLM-провайдеров ход не падает, а паркуется:

- Адаптивный экспоненциальный поллинг: 60с → 120с → 240с → потолок 300с, в пределах общего бюджета (по умолчанию 5 часов).
- Визуальный таймер обратного отсчета в UI с возможностью отмены (Esc / Стоп) или ручного перезапуска.
- Настраивается индивидуально для каждого провайдера:

```jsonc
// .zed/settings.json
{
  "language_models": {
    "anthropic_compatible": {
      "providers": {
        "my-proxy": {
          "api_url": "...",
          "rate_limit": {
            "initial_wait_seconds": 60,
            "max_wait_seconds": 300,
            "max_total_wait_seconds": 18000 // 0 = ждать до отмены
          }
        }
      }
    }
  }
}
```

---

### 8. MCP-серверы и переменные окружения

В `command.env` и `command.args` context-серверов поддерживается подстановка переменных окружения `${VAR}` и `${VAR:-default}`. Секреты и пути не попадают в репозиторий:

```jsonc
// .zed/settings.json
{
  "agent": {
    "default_profile": "orchestrator",
    "nested_sub_agents": {
      "enabled": true,
      "max_depth": 3,
      "max_concurrent": 6
    },
    "profiles": {
      "orchestrator": {
        "name": "Orchestrator",
        "description": "Coordinates task execution",
        "delegation": {
          "allowed": ["backend", "reviewer"],
          "max_depth": 2
        }
      },
      "backend": {
        "name": "Backend Engineer",
        "custom_prompt": "You implement backend services...",
        "skills": ["go", "postgres"],
        "tool_permissions": {
          "default": "deny",
          "tools": {
            "terminal": {
              "default": "deny",
              "always_allow": [
                { "pattern": "^go\\s+(test|build)" },
                { "pattern": "^task\\s+test" }
              ]
            },
            "edit_file": {
              "default": "allow",
              "write_scopes": ["backend/**", "proto/**"]
            },
            "write_file": {
              "default": "allow",
              "write_scopes": ["backend/**", "proto/**"]
            }
          }
        }
      },
      "reviewer": {
        "name": "Code Reviewer",
        "tools": {
          "edit_file": false,
          "write_file": false,
          "terminal": false
        }
      }
    },
    "context_servers": {
      "task-graph": {
        "command": "taskgraph",
        "args": ["serve", "--project", "${ZED_PROJECT_PATH}"],
        "env": {
          "TGR_PROJECT_ID": "${ZED_PROJECT_ID}",
          "TGR_AUTH_TOKEN": "${TGR_TOKEN:-default_token}"
        }
      }
    }
  }
}
```

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
