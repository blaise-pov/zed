# Архитектура: Zed Agent Runtime + Task Graph Service (TGS)

Форк превращает Zed в IDE и runtime для иерархии автономных coding-агентов. Ответственность жёстко разделена между тремя слоями:

| Слой | Отвечает за |
|---|---|
| **Zed** (Execution Layer) | Профили агентов, жизненный цикл сессий (`Thread` / `AcpThread` / `NativeAgent`), иерархическое делегирование (`spawn_agent`), безопасность (`tool_permissions`, `write_scopes`, fail-closed), изоляция задач в Git Worktree, песочница терминала, UI панелей |
| **TGS** (Control Plane) | Цели (Goals), DAG задач с валидацией циклов, стейт-машина задач, контракты, лизинг с heartbeat, двухфазное ревью, неизменяемые артефакты, append-only аудит |
| **LLM** (Reasoning Engine) | Понимание контекста, декомпозиция задач, выбор целевого профиля агента, архитектура и реализация кода |

Zed и TGS общаются по протоколу MCP (JSON-RPC 2.0, stdio). [TGS](https://github.com/blaise-pov/tgr) — независимый демон на Go с хранилищем SQLite WAL; ключ сервера в `context_servers` задаётся настройкой `agent.task_graph_server_id` (по умолчанию `tgs`) и применяется без перезапуска.

```text
USER ──► Zed UI (Agent Panel / Agent Task Panel)
              │
              ├── Agent Runtime: профили, сессии, spawn_agent,
              │   tool_permissions, write_scopes, worktrees, песочница
              │
              └── McpAgentTaskProvider ──► TGS MCP Server:
                  задачи, DAG, лизинг, ревью, артефакты, события
```

---

## 1. Агент — это профиль Zed

Агент определяется декларативно в `settings.json` — глобальном или в `.zed/settings.json` проекта (настройки проекта дополняют и переопределяют пользовательские; такие профили помечаются бейджем **Project** в UI). Редактирование — в JSON или через **Agent Panel → Manage Profiles**.

```jsonc
{
  "agent": {
    "profiles": {
      "backend": {
        "name": "Backend Agent",
        "description": "Implements backend business logic, storage and APIs",
        "custom_prompt": "You are responsible for backend implementation in Go.",
        "default_model": { "provider": "anthropic", "model": "claude-sonnet-4-latest" },
        "skills": ["go", "postgres", "rest-api"],
        "delegation": {
          "allowed": ["repository-engineer", "transport-engineer"],
          "max_depth": 2
        },
        "tool_permissions": {
          "default": "deny",
          "tools": {
            "terminal": {
              "default": "deny",
              "always_allow": [{ "pattern": "^go\\s+(test|build|vet)" }],
              "always_deny": [{ "pattern": "^git\\s+push\\s+--force" }]
            },
            "edit_file": { "default": "allow", "write_scopes": ["backend/**", "proto/**"] },
            "write_file": { "default": "allow", "write_scopes": ["backend/**", "proto/**"] }
          }
        }
      }
    }
  }
}
```

Параметры профиля:

| Параметр | Назначение |
|---|---|
| `name` / `description` | Имя в UI; описание роли для каталога делегирования родительского агента |
| `custom_prompt` / `custom_prompt_path` | Слой специализированных системных инструкций — текстом или ссылкой на файл (абсолютный путь, `~/`, относительный от проекта/`.zed`/каталога промптов); при указании обоих файл в приоритете |
| `default_model` | LLM-модель профиля (поддерживает подстановку `${VAR}`) |
| `skills` | Белый список скиллов; для кастомных профилей без списка действует **default-deny** (нет скиллов), встроенные видят все |
| `delegation` | Правила делегирования: `allowed` профили и `max_depth` (1–5); профиль без блока — соло-агент |
| `tool_permissions` | Политики инструментов: `default`, `always_allow` / `always_deny` (regex), `write_scopes` (glob) |

TGS не хранит содержимое профилей — задача в графе ссылается на целевой профиль полем `assigned_profile = "backend"`.

**Ключи раскладки окон (`dock`, `task_dock`, `flexible`) — всегда пользовательские:** проектные настройки их игнорируют, чтобы файл `.zed/settings.json` в репозитории не двигал окна у всех участников.

---

## 2. Делегирование: две независимые операции

1. **Регистрация подзадачи в TGS** (MCP): `task_create` с контрактом, `assigned_profile`, `write_scope` и `depends_on`. TGS валидирует DAG и переводит задачу в `READY`.
2. **Запуск агента в Zed** (встроенный инструмент): `spawn_agent(profile, task_id, label, message)`.

Runtime Flow при `spawn_agent`:

- **Статическая валидация графа** — `AgentGraph` при загрузке настроек проверяет граф делегирования DFS-ом на циклы и ссылки на несуществующие профили.
- **Проверка прав и глубины** — наличие блока `delegation`, вхождение в `delegation.allowed`, лимиты `delegation.max_depth` и глобального `agent.nested_sub_agents` (`enabled`, `max_depth` 1–5, `max_concurrent` 1–32, по умолчанию 16).
- **Семафор конкурентности** — атомарный `SubagentSlotPool`, общий на всё дерево сессий от корня, освобождает слот даже при отмене вызова.
- **Изолированный тред** — `Thread::new_subagent` со своей моделью, скиллами, `write_scopes` и связанным action log; дочерний агент сохраняет профиль даже при смене профиля родителем, продолжение диалога — по `session_id`.

```text
Root (orchestrator)
  ├── TGS: task_create(assigned_profile="backend")  ──► TASK-1
  └── Zed: spawn_agent(profile="backend", task_id="TASK-1")
        ├── TGS: task_create(parent=TASK-1, profile="repository-engineer") ──► TASK-2
        └── TGS: task_create(parent=TASK-1, profile="transport-engineer",
                             depends_on=["TASK-2"])                      ──► TASK-3
```

---

## 3. Сборка системного промпта

Шаблон `system_prompt.hbs` (Handlebars) компилирует промпт в фиксированном порядке: базовые инструкции Zed → руководство по инструментам → каталог делегирования → системная информация (ОС, shell, worktrees) → описание песочницы терминала → информация о модели → каталог скиллов `<available_skills>` → пользовательские инструкции (`AGENTS.md`, `.rules` каждого ворктри) → `## Profile Instructions` из `custom_prompt` → контекст задачи (Task ID, критерии приёмки, write scopes).

---

## 4. Task Graph Service (Control Plane)

TGS — легковесный демон на Go (`cmd/taskgraph`), MCP-сервер по JSON-RPC 2.0 (stdio) на SQLite WAL с ACID-транзакциями.

Доменная модель: `goals` (цели), `tasks` (задачи с `contract`, `assigned_profile`, `write_scopes`), `task_dependencies` (рёбра DAG с проверкой ацикличности), `artifacts` (неизменяемые версионированные результаты с `supersedes`), `events` (append-only аудит), `leases` (аренда с TTL и heartbeat).

Стейт-машина задачи (статусы, видимые в Zed):

```mermaid
stateDiagram-v2
    [*] --> Blocked
    Blocked --> Ready: deps resolved
    Ready --> Running: task_claim
    Running --> Review: task_complete
    Review --> Completed: approve
    Review --> Running: request changes
    Running --> Failed: task_fail
    Running --> Stale: lease timeout
    Stale --> Ready: recovery
    Failed --> Ready: task_retry
```

- **Dual-Actor Review**: завершённая исполнителем задача переходит в `REVIEW` и требует подтверждения ревьюером или пользователем — саморевью исключено.
- **Лизинг и Recovery**: задачи захватываются атомарным `task_claim`; при сбое воркера и истечении аренды задача помечается `STALE` и перезапускается сборщиком.

Группы MCP-инструментов: управление задачами (`task_create` / `get` / `update` / `claim` / `complete` / `fail` / `cancel` / `retry` / `ready` / `blocked`), граф (`task_graph`, `task_children`, `task_dependencies` / `dependents`, `task_lock` / `unlock` / `conflicts`), артефакты и события (`artifact_publish` / `get` / `list`, `events_list`), агенты и координация (`agent_register`, `agent_heartbeat`, `agent_complete` / `fail`, `scheduler_intents`).

---

## 5. Безопасность: Tool Permissions & Write Scopes

Решение о вызове инструмента принимается по цепочке приоритетов:

1. **Hardcoded-правила** — запреты, которые нельзя переопределить ничем: рекурсивное удаление `/`, `~`, `$HOME`, `.` и `..` (включая нормализацию путей, обходные комбинации флагов и разбор под-команд shell против инъекций вроде `ls && rm -rf /`).
2. **Глобальный deny** — `always_deny` из пользовательских настроек; профиль не может его обойти.
3. **Правила профиля** — `always_deny` → `always_confirm` → `always_allow`; совпавший `deny` сильнее `allow`.
4. **Write scopes** — для файловых инструментов (`edit_file`, `write_file`, `copy_path`, `move_path`, `delete_path`, `create_directory`): запись только внутрь glob-шаблонов. Scopes задаются per-tool: инструмент без собственной записи ограничен только `default` профиля.
5. **Anti-Escape и чувствительные файлы** — блокировка выхода за пределы проекта через симлинки; защита `.zed/settings.json`, `.cargo/config.toml` и глобальных скиллов `~/.agents/skills`.

Принципы:

- **Fail-Closed**: если у профиля заданы `tool_permissions`, операции с исходом `Confirm` автоматически отклоняются (`PolicyDenied`) — автономный агент не зависает в ожидании пользователя. Невалидные glob/regex-правила тоже блокируют инструмент, а не игнорируются.
- **Ограничения автономных профилей**: эскалация песочницы (`unsandboxed`, терминал с повышенными правами, запись вне проекта, `fetch` к невыданным хостам) и `rename_symbol` (правит произвольный набор файлов, не ограничивается scopes) возвращают `PolicyDenied`.

---

## 6. Панель задач и изоляция в Git Worktree

- **`McpAgentTaskProvider` + `AgentTaskStore`** — реактивное получение графа задач и ленты событий из TGS.
- **Дерево задач**: Goal → Task → Subtasks со статусами (`Ready`, `Blocked`, `Running`, `Stale`, `Review`, `Completed`, `Failed`) и номером попытки (`#attempt`).
- **Действия**: `Run Task`, `Force Approve`, `Request Changes`, `Reject`, `Retry`, `View Task Diff` (изменения относительно базовой ветки), лента `AgentTaskTimeline`.
- **Изоляция**: при запуске задачи создаётся ветка `agent-task/{task_id}` и отдельный worktree `agent-task-{task_id}` — параллельные воркеры работают в независимых файловых деревьях.
- **Док панели** задаётся отдельно от панели агента: `agent.task_dock` (по умолчанию `left`).

---

## 7. Парковка сессии: rate-limit и сбои соединения

При ошибке `429 Too Many Requests` **или** сбое соединения с провайдером (обрыв потока, сетевая ошибка, битый ответ) ход не падает, а паркуется:

- адаптивный экспоненциальный опрос: 60с → 120с → 240с → потолок 300с в рамках общего бюджета (по умолчанию 5 часов, `0` — до отмены);
- таймер обратного отсчёта в UI с отменой или немедленным перезапуском;
- полный контекст диалога сохраняется; параметры настраиваются per-provider (`rate_limit`).

---

## 8. Скиллы и трёхуровневая фильтрация

Источники: глобальные `~/.agents/skills/`, проектные `.agents/skills/` (перекрывают глобальные по имени, требуют доверия) и встроенные наборы. Фильтр профиля `skills` действует на трёх уровнях:

1. каталог `<available_skills>` в системном промпте;
2. автокомплит slash-команд в сессии редактора;
3. инструмент `skill` — вызов неразрешённого скилла блокируется.

Для кастомных профилей без списка `skills` действует default-deny; встроенные профили без списка видят все скиллы.

---

## 9. Переменные окружения и `.env`

- Файл `.env` в корне проекта автоматически загружается при открытии (и при изменении) — критические переменные (`PATH`, `HOME`, `USER`, `USERNAME`, `SHELL`, `SYSTEMROOT`) не перезаписываются; настройки пересчитываются, чтобы подстановка применилась без перезапуска.
- Подстановка `${VAR}` и `${VAR:-default}` поддерживается в `command.env` / `command.args` MCP-серверов и в идентификаторах моделей (`default_model`, `subagent_model`, `commit_message_model` и др.) — секреты не попадают в репозиторий.
