# Руководство по настройке окружения: `.zed`, `.env`, агенты и MCP

Данный документ описывает структуру директории `.zed`, использование файла `.env`, конфигурирование профилей агентов, матрицу прав инструментов (`tool_permissions`), периметры записи (`write_scopes`) и практическую работу с добавленными серверами Model Context Protocol (MCP).

---

## Содержание

1. [Обзор архитектуры конфигурации](#1-обзор-архитектуры-конфигурации)
2. [Файл переменных окружения `.env`](#2-файл-переменных-окружения-env)
   - [Механизм автозагрузки и Hot-Reload](#механизм-автозагрузки-и-hot-reload)
   - [Синтаксис и правила](#синтаксис-и-правила)
   - [Подстановка в конфигурации `${VAR}`](#подстановка-в-конфигурации-var)
   - [Безопасность `.env`](#безопасность-env)
   - [Пример файла `.env`](#пример-файла-env)
3. [Структура директории `.zed`](#3-структура-директории-zed)
   - [`.zed/settings.json`](#zedsettingsjson)
   - [`.zed/prompts/`](#zedprompts)
   - [`.zed/mcp/`](#zedmcp)
   - [`.zed/tasks.json` и `.zed/debug.json`](#zedtasksjson-и-zeddebugjson)
4. [Настройка профилей агентов (`agent.profiles`)](#4-настройка-профилей-агентов-agentprofiles)
   - [Анатомия профиля](#анатомия-профиля)
   - [Файловые промпты профилей](#файловые-промпты-профилей)
   - [Рекурсивное делегирование (`delegation`)](#рекурсивное-делегирование-delegation)
   - [Скиллы (`skills`)](#скиллы-skills)
5. [Права доступа и безопасность (`tool_permissions` и `write_scopes`)](#5-права-доступа-и-безопасность-tool_permissions-и-write_scopes)
   - [Режимы и значения по умолчанию](#режимы-и-значения-по-умолчанию)
   - [Политика Fail-Closed для автономных агентов](#политика-fail-closed-для-автономных-агентов)
   - [Периметры записи (`write_scopes`)](#периметры-записи-write_scopes)
   - [Защита скрытых файлов (dotfiles) и конфигурации](#защита-скрытых-файлов-dotfiles-и-конфигурации)
   - [Фильтрация команд терминала](#фильтрация-команд-терминала)
6. [Конфигурация MCP-серверов (`context_servers`)](#6-конфигурация-mcp-серверов-context_servers)
   - [Формат объявления серверов](#формат-объявления-серверов)
   - [Кроссплатформенные бинарники (`platforms`)](#кроссплатформенные-бинарники-platforms)
   - [Подключение MCP к агентам](#подключение-mcp-к-агентам)
7. [Работа с добавленными MCP-серверами](#7-работа-с-добавленными-mcp-серверами)
   - [Task Graph Service (`taskgraph`)](#task-graph-service-taskgraph)
   - [Межагентная шина (`agent-bus`)](#межагентная-шина-agent-bus)
   - [Каталог скиллов (`skills-hub`)](#каталог-скиллов-skills-hub)
   - [Поиск и установка серверов (`mcpfinder`)](#поиск-и-установка-серверов-mcpfinder)
8. [Пошаговый быстрый старт](#8-пошаговый-быстрый-старт)

---

## 1. Обзор архитектуры конфигурации

В данном форке Zed конфигурация многоагентной среды вынесена в проектную область:

```text
<project-root>/
├── .env                       # Локальные секреты, API-ключи и переменные моделей (git-ignored)
├── docs/
│   └── .env.example           # Шаблон конфигурации переменных окружения
├── .zed/
│   ├── settings.json          # Проектные настройки: MCP, профили агентов, tool_permissions
│   ├── tasks.json             # Задачи сборки и запуска проекта
│   ├── debug.json             # Профили DAP-отладчика
│   ├── prompts/               # Системные инструкции и промпты профилей (*.md)
│   │   ├── orchestrator.md
│   │   ├── editor_engineer.md
│   │   ├── reviewer.md
│   │   └── ...
│   └── mcp/                   # Локальные встроенные MCP-серверы
│       ├── agent-bus/         # Шина обратной связи и IPC сообщений
│       └── taskgraph/         # Control Plane сервис графа задач (TGS)
```

Каждый компонент изолирован: секреты хранятся в `.env`, параметры оркестрации и права — в `.zed/settings.json`, инструкции агентов — в `.zed/prompts/`, а встроенные бинарники координации — в `.zed/mcp/`.

---

## 2. Файл переменных окружения `.env`

### Механизм автозагрузки и Hot-Reload

1. **Автоматическая загрузка при старте**:
   При открытии проекта или рабочей директории (worktree) Zed проверяет наличие файла `.env` в корне проекта (`WorktreeStore::create_local_worktree`). Если файл существует, переменные считываются и загружаются в окружение текущего процесса редактора.
2. **Реактивное обновление (Hot-Reload)**:
   Файловый наблюдатель (`project_settings.rs`) отслеживает изменения пути `.env`. При сохранении файла значения мгновенно обновляются в окружении без перезапуска редактора, и инициируется перезагрузка `SettingsStore`, благодаря чему обновленные переменные сразу вступают в силу.
3. **Защита критических системных переменных**:
   Рантайм блокирует перезапись ключевых переменных операционной системы (`PATH`, `HOME`, `USER`, `USERNAME`, `SHELL`, `SYSTEMROOT`), если они уже заданы в системе.

### Синтаксис и правила

Файл `.env` поддерживает стандартный синтаксис:

- Формат пар: `KEY=value` или `export KEY=value`
- Пробелы вокруг знака `=` игнорируются: `KEY = value`
- Кавычки: допускаются одинарные (`'...'`) и двойные (`"..."`) кавычки для сохранения пробелов и спецсимволов.
- Встроенные комментарии: `# комментарий` как на отдельной строке, так и после значения (например: `KEY=value # комментарий`).
- Пустые значения: `KEY=` задает пустую строку.

### Подстановка в конфигурации `${VAR}`

В файле `.zed/settings.json` поддерживается подстановка переменных окружения:

- `${VAR}` — подставляет значение переменной `VAR`. Если переменная не задана, выводится предупреждение, а токен `${VAR}` сохраняется без изменений.
- `${VAR:-default}` — подставляет значение `VAR`, а если переменная отсутствует или не задана, подставляет резервное значение `default`.

Подстановка работает:
- В выборе моделей (`agent.default_model`, `agent.subagent_model`, `profiles.<id>.default_model`).
- В параметрах MCP-серверов (`context_servers.<id>.command`, `context_servers.<id>.args`, `context_servers.<id>.env`).

### Безопасность `.env`

- Файл `.env` включен в глобальный список `private_files` Zed: содержимое файла закрыто от непреднамеренного чтения LLM.
- Файл классифицируется как защищенный скрытый путь (`SensitiveSettingsKind::Hidden`). Автономные агенты не могут перезаписать или повредить `.env` (`PolicyDenied`). В интерактивном режиме изменение файла требует явного подтверждения пользователя.
- В песочнице терминала существующий `.env` файл монтируется в режиме read-only.

### Пример файла `.env`

Файл-шаблон доступен в репозитории по пути `docs/.env.example`. Для настройки скопируйте его в корень проекта:

```sh
cp docs/.env.example .env
```

Содержимое файла:

```bash
# ==============================================================================
# Zed Multi-Agent Environment Configuration (.env)
# ==============================================================================

# 1. Модели LLM для агентов (используются в .zed/settings.json)
FRONTIER_MODEL_PROVIDER=anthropic
FRONTIER_MODEL=claude-3-7-sonnet-latest

MID_TIER_MODEL_PROVIDER=anthropic
MID_TIER_MODEL=claude-3-5-haiku-latest

# 2. API-ключи провайдеров LLM
ANTHROPIC_API_KEY=sk-ant-api03-...
OPENAI_API_KEY=sk-proj-...
GOOGLE_AI_API_KEY=AIzaSy...
DEEPSEEK_API_KEY=sk-...
GROQ_API_KEY=gsk_...
OLLAMA_API_URL=http://localhost:11434

# 3. Control Plane: Task Graph Service (TGS / .zed/mcp/taskgraph)
TGS_PROJECT_ID=zed
TGS_DB_PATH=.zed/mcp/taskgraph/taskgraph.db
TGS_LOG_LEVEL=warn
TGS_LOG_FILE=.zed/mcp/taskgraph/taskgraph.log

# 4. Межагентная шина: Agent Bus (.zed/mcp/agent-bus)
AGENT_BUS_PROJECT_ID=zed
```

---

## 3. Структура директории `.zed`

Директория `.zed/` в корне проекта содержит настройки, промпты и сервисы, специфичные для данного репозитория:

### `.zed/settings.json`

Главный конфигурационный файл уровня проекта. Переопределяет глобальные настройки пользователя Zed для текущего проекта. Включает:
- Секцию `context_servers` — регистрация серверов MCP.
- Секцию `agent` — параметры рантайма агентов:
  - `task_graph_server_id` — ID MCP-сервера управления задачами (по умолчанию `"taskgraph"`).
  - `default_profile` — профиль по умолчанию для новых сессий (например, `"orchestrator"`).
  - `default_model` и `subagent_model` — модели для корневых и дочерних агентов.
  - `tool_permissions` — базовые разрешения на запуск инструментов.
  - `profiles` — словарь профилей агентов («Лестница абстракций»).

### `.zed/prompts/`

Каталог Markdown-файлов с системными инструкциями для каждого профиля.
- **Соглашение об именовании**: если профиль имеет идентификатор `editor_engineer`, рантайм автоматически ищет файл `.zed/prompts/editor_engineer.md`.
- **Явное указание**: в настройках профиля можно переопределить путь параметром `"custom_prompt_path": ".zed/prompts/custom_name.md"`.
- **Шаблонизация**: промпты компилируются через Handlebars (`system_prompt.hbs`), автоматически добавляя контекст задачи, список доступных субагентов и разрешенные инструменты.

### `.zed/mcp/`

Локальные MCP-серверы, скомпилированные под разные архитектуры:

1. **`agent-bus/`**:
   - `bin/` — исполняемые файлы (`agent-bus-windows-amd64.exe`, `agent-bus-darwin-arm64`, `agent-bus-linux-amd64`).
   - `messages.db` — SQLite база данных для межагентного обмена сообщениями.
   - `topics.jsonc` — конфигурация топиков шины сообщений.
   - `agent-bus.log` — файл журнала работы шины.
2. **`taskgraph/`**:
   - `bin/` — исполняемые файлы TGS демона (`taskgraph-windows-amd64.exe`, `taskgraph-darwin-arm64`, `taskgraph-linux-amd64`).
   - `taskgraph.db` — SQLite база данных DAG задач, целей и артефактов.
   - `taskgraph.log` — файл журнала работы Task Graph Service.

### `.zed/tasks.json` и `.zed/debug.json`

- `tasks.json`: команды сборки, тестирования и форматирования проекта, вызываемые через палитру команд Zed.
- `debug.json`: конфигурации DAP (Debug Adapter Protocol) для пошаговой отладки.

---

## 4. Настройка профилей агентов (`agent.profiles`)

Каждый профиль задает специализированную роль на лестнице абстракций.

### Анатомия профиля

Пример настройки профиля в `.zed/settings.json`:

```jsonc
"agent": {
  "profiles": {
    "editor_engineer": {
      "name": "Editor & Language Engineer",
      "description": "Implements core editor buffers, display maps, project worktrees, and LSP integrations",
      "default_model": {
        "provider": "${MID_TIER_MODEL_PROVIDER}",
        "model": "${MID_TIER_MODEL}"
      },
      "custom_prompt_path": ".zed/prompts/editor_engineer.md",
      "tools": [
        "read_file",
        "grep",
        "find_path",
        "list_directory",
        "edit_file",
        "write_file",
        "terminal"
      ],
      "context_servers": {
        "agent-bus": {
          "tools": ["send_feedback"]
        },
        "taskgraph": {
          "tools": [
            "task_get",
            "task_start",
            "task_complete",
            "task_fail",
            "artifact_publish",
            "artifact_get",
            "artifact_list"
          ]
        }
      },
      "delegation": {
        "allowed": ["reviewer"],
        "max_depth": 2
      },
      "tool_permissions": {
        "default": "allow",
        "tools": {
          "edit_file": {
            "default": "allow",
            "write_scopes": [
              "crates/editor/**",
              "crates/project/**",
              "crates/workspace/**"
            ]
          },
          "write_file": {
            "default": "allow",
            "write_scopes": [
              "crates/editor/**",
              "crates/project/**",
              "crates/workspace/**"
            ]
          }
        }
      }
    }
  }
}
```

Параметры профиля:
- `name` *(string)*: отображаемое имя профиля в интерфейсе.
- `description` *(string)*: краткое описание роли. Видно оркестраторам в каталоге субагентов.
- `default_model` *(object)*: `provider`, `model`, и опционально `enable_thinking: true`.
- `custom_prompt_path` *(string)*: путь к файлу инструкций.
- `tools` *(array)*: доступные встроенные инструменты (`read_file`, `edit_file`, `write_file`, `terminal`, `grep`, `find_path`, `list_directory`, `spawn_agent`, `fetch`, `search_web`).
- `context_servers` *(object)*: список подключенных MCP-серверов и перечень разрешенных инструментов.
- `delegation` *(object)*: правила порождения субагентов.
- `tool_permissions` *(object)*: правила авторизации инструментов для данного профиля.

> **Известное ограничение периметра**: `write_scopes` ограничивает только файловые инструменты (`edit_file`, `write_file` и т.п.). Инструмент `terminal` не подчиняется `write_scopes` — профиль с разрешенным `terminal` технически может изменить любой файл через shell-команды. Выдавайте `terminal` только профилям с минимальным списком команд в `always_allow` и осознанным уровнем доверия.

### Файловые промпты профилей

Инструкции для агента хранятся в формате Markdown в `.zed/prompts/<profile_id>.md`.
При редактировании профиля в UI (**Manage Profiles**) Zed открывает соответствующий файл во вкладке редактора.

В системном промпте агент видит:
1. Базовые системные директивы редактора.
2. Каталог доступных для вызова профилей субагентов (строго из `delegation.allowed`).
3. Каталог разрешенных скиллов.
4. Документацию доступных MCP-инструментов.
5. Индивидуальные инструкции из файла промпта.
6. Контекст текущей задачи из TGS (Task ID, критерии приёмки, скоуп записи).

### Рекурсивное делегирование (`delegation`)

Инструмент `spawn_agent` позволяет агентам запускать специализированных помощников:
- `allowed` *(array)*: белый список профилей, которые данный агент имеет право вызывать. Если агент пытается вызвать профиль, не входящий в список, вызов отклоняется.
- `max_depth` *(integer)*: предельная глубина дерева вызовов (от 1 до 5).
- **Семафор слотов**: общий пул параллелизма (`agent.nested_sub_agents.max_concurrent`, по умолчанию 16) предотвращает лавинообразные затраты ресурсов.
- **Статический контроль графа**: модуль `AgentGraph` при старте проверяет отсутствие циклов в графе делегирования.

### Скиллы (`skills`)

- Поле `skills: ["skill-1", "skill-2"]` задает перечень навыков, доступных профилю.
- Для кастомных профилей без явного указания поля `skills` действует политика **default-deny** (доступ к сторонним скиллам заблокирован).

---

## 5. Права доступа и безопасность (`tool_permissions` и `write_scopes`)

Система безопасности Zed построена по принципу эшелонированной защиты (Defense-in-Depth).

### Режимы и значения по умолчанию

Поле `default` в `tool_permissions` определяет базовое действие при вызове инструмента:
- `"allow"` — действие выполняется автоматически без запросов.
- `"confirm"` — действие требует подтверждения пользователя через UI-диалог.
- `"deny"` — действие немедленно блокируется.

### Политика Fail-Closed для автономных агентов

Если агент выполняет задачу автономно (фоновый воркер, задача из Task Graph Service):
- Любой запрос с исходом `"confirm"` **автоматически отклоняется** с ошибкой `PolicyDenied`.
- Автономный агент никогда не блокируется в бесконечном ожидании клика пользователя.

### Периметры записи (`write_scopes`)

Для инструментов модификации файлов (`edit_file`, `write_file`, `create_directory`, `move_path`, `copy_path`, `delete_path`) настраивается массив glob-шаблонов `write_scopes`:

```jsonc
"edit_file": {
  "default": "allow",
  "write_scopes": [
    "crates/ui/**",
    "crates/theme/**"
  ]
}
```

- Если агент пытается изменить файл, не попадающий ни под один из шаблонов `write_scopes`, операция блокируется:
  `PolicyDenied: 'crates/editor/src/buffer.rs' outside write_scopes [...]`
- Это гарантирует строгую изоляцию слоев: агент интерфейса не может случайно повредить сетевой слой или базу данных.

### Защита скрытых файлов (dotfiles) и конфигурации

Все файлы и каталоги, компоненты которых начинаются с точки (`.zed/**`, `.env*`, `.github/**`, `.cargo/**`, `.husky/**`, `~/.agents/skills/**`), защищены:
- В автономном режиме модификация dotfiles отклоняется, если целевой путь **не указан явно** в `write_scopes` профиля.
- В интерактивном режиме изменение dot-файла всегда запрашивает подтверждение через UI.
- В терминале скрытые файлы и директории воркспейса монтируются в режиме read-only на уровне песочницы ОС.

### Фильтрация команд терминала

Для инструмента `terminal` настраиваются списки регулярных выражений:

```jsonc
"terminal": {
  "default": "deny",
  "always_allow": [
    { "pattern": "^cargo\\s+(test|check|build)\\b" },
    { "pattern": "^git\\s+(status|diff|log)\\b" }
  ],
  "always_deny": [
    { "pattern": "rm\\s+-rf" },
    { "pattern": "git\\s+push" }
  ]
}
```

**Неудаляемые системные правила безопасности**:
Следующие команды блокируются на уровне ядра рантайма и не могут быть переопределены никакими настройками:
- `rm -rf /` и `rm -rf /*`
- `rm -rf ~` и `rm -rf ~/*`
- `rm -rf $HOME` и `rm -rf ${HOME}`
- `rm -rf .` и `rm -rf ./*`
- `rm -rf ..` и `rm -rf ../*`

---

## 6. Конфигурация MCP-серверов (`context_servers`)

MCP-серверы расширяют возможности агентов внешними инструментами и источниками данных.

### Формат объявления серверов

Серверы описываются в объекте `context_servers` в `.zed/settings.json`:

```jsonc
{
  "context_servers": {
    // 1. Запуск через Node.js / npx
    "skills-hub": {
      "command": "npx",
      "args": ["-y", "@skills-hub-ai/mcp"]
    },

    // 2. Локальный бинарник с автовыбором платформы
    "agent-bus": {
      "command": "agent-bus",
      "args": [
        "--db", ".zed/mcp/agent-bus/messages.db",
        "--topics-file", ".zed/mcp/agent-bus/topics.jsonc",
        "--log-level", "warn",
        "--log-file", ".zed/mcp/agent-bus/agent-bus.log"
      ],
      "env": {
        "AGENT_BUS_PROJECT_ID": "zed"
      },
      "platforms": {
        "windows": { "command": ".zed/mcp/agent-bus/bin/agent-bus-windows-amd64.exe" },
        "macos": { "command": ".zed/mcp/agent-bus/bin/agent-bus-darwin-arm64" },
        "linux": { "command": ".zed/mcp/agent-bus/bin/agent-bus-linux-amd64" }
      }
    }
  }
}
```

### Кроссплатформенные бинарники (`platforms`)

Блок `platforms` задает пути к исполняемым файлам под конкретные ОС и архитектуры:
- Приоритет совпадения: `<os>-<arch>` (например `darwin-arm64`, `linux-amd64`, `windows-amd64`) → имя ОС (`windows`, `macos`, `linux`) → корневой `command`.
- Автоматическая нормализация алиасов: `macos` ↔ `darwin`, `win32` ↔ `windows`, `amd64` ↔ `x64` ↔ `x86_64`, `arm64` ↔ `aarch64`.

### Подключение MCP к агентам

В профиле агента перечисляются доступные серверы и конкретные инструменты:

```jsonc
"context_servers": {
  "taskgraph": {
    "tools": [
      "task_get",
      "task_start",
      "task_complete",
      "task_fail"
    ]
  }
}
```

В секции `tool_permissions` права на MCP-инструменты задаются в формате:
`mcp:<server_id>:<tool_name>`:

```jsonc
"tool_permissions": {
  "tools": {
    "mcp:taskgraph:task_complete": {
      "default": "allow"
    },
    "mcp:agent-bus:send_feedback": {
      "default": "allow"
    }
  }
}
```

---

## 7. Работа с добавленными MCP-серверами

В проект включены 4 основных MCP-сервера:

### Task Graph Service (`taskgraph`)

Task Graph Service — это локальный Go-демон с базой данных SQLite WAL (`.zed/mcp/taskgraph/taskgraph.db`), выступающий в роли Control Plane для управления задачами.

#### Жизненный цикл задачи
`Ready` → `Claimed` → `Running` → `Review` → `WaitingApproval` → `Completed` (или `Failed` / `Stale` / `Cancelled`).

#### Ключевые инструменты `taskgraph`

| Инструмент | Назначение | Параметры |
|---|---|---|
| `goal_create` | Создать высокоуровневую цель | `title`, `description`, `priority`, `acceptance_criteria` |
| `goal_list` | Список целей проекта | `project_id` |
| `goal_get` | Получить цель по ID | `goal_id` |
| `goal_update` | Обновить поля и статус цели | `goal_id`, `status`, `title`, `acceptance_criteria` |
| `goal_tasks` | Список задач, привязанных к цели | `goal_id` |
| `task_create` | Создать задачу в DAG | `title`, `goal_id`, `description`, `assigned_profile`, `contract`, `depends_on`, `write_scopes` |
| `task_get` | Получить детали задачи | `task_id` |
| `task_list` | Список задач с фильтрацией | `status`, `goal_id`, `assigned_profile` |
| `task_start` | Взять задачу в работу (`READY` → `RUNNING`) | `task_id` |
| `task_complete` | Завершить выполнение (`RUNNING` → `COMPLETED`) | `task_id`, `result`, `artifact_id` |
| `task_fail` | Зафиксировать ошибку (`RUNNING` → `FAILED`) | `task_id`, `reason` |
| `task_retry` | Перезапустить упавшую задачу (`FAILED` → `READY`) | `task_id` |
| `task_cancel` | Отменить задачу | `task_id`, `reason`, `recursive` |
| `task_add_dependency` | Добавить зависимость между задачами | `task_id`, `depends_on_task_id` |
| `task_remove_dependency` | Удалить зависимость | `task_id`, `depends_on_task_id` |
| `task_dependencies` | Список задач, от которых зависит данная | `task_id` |
| `task_dependents` | Список задач, зависящих от данной | `task_id` |
| `task_ready` | Проверить готовность задачи к старту | `task_id` или `project_id` |
| `task_graph` | Получить полный граф задач проекта | `project_id` |
| `artifact_publish` | Опубликовать неизменяемый артефакт | `task_id`, `content`, `kind`, `summary`, `supersedes` |
| `artifact_get` | Получить артефакт по ID | `artifact_id` |
| `artifact_list` | Список артефактов задачи | `task_id` |
| `events_list` | Лента аудита событий | `task_id`, `limit` |

#### Пример использования в диалоге с агентом
Оркестратор может декомпозировать фичу:
1. Создает задачу: `task_create(title="Реализовать API роутер", assigned_profile="editor_engineer", write_scopes=["crates/editor/**"])`.
2. Назначает зависимость: `task_add_dependency(task_id="TASK-2", depends_on_task_id="TASK-1")`.
3. Исполнитель берет задачу: `task_start(task_id="TASK-1")`.
4. Публикует отчет: `artifact_publish(task_id="TASK-1", kind="code_review", content="...")`.
5. Завершает задачу: `task_complete(task_id="TASK-1", result="Готово")`.

### Межагентная шина (`agent-bus`)

Шина `agent-bus` предназначена для асинхронного обмена сообщениями, предложениями по архитектуре и репортами между агентами без засорения контекста основного треда.

#### Инструменты `agent-bus`

| Инструмент | Назначение | Параметры |
|---|---|---|
| `send_feedback` | Отправить предложение по улучшению | `sender` (профиль), `content` (канонический формат) |
| `read_feedback` | Прочитать сообщения из шины | `status` (`new`, `read`, `resolved`, `archived`, `all`), `limit`, `mark_read` |
| `resolve_feedback` | Пометить сообщение как решенное | `message_id`, `resolution` |

#### Канонический формат предложений (`content` в `send_feedback`)
```text
impact: quality|resources|speed|bugs|agents | problem: <описание проблемы> | proposal: <конкретное решение>
```
Пример:
```text
impact: bugs | problem: race condition in buffer save | proposal: use atomic rename on save
```

### Каталог скиллов (`skills-hub`)

Сервер `@skills-hub-ai/mcp` предоставляет поиск и установку готовых скиллов для агентов:
- `search_skills`: поиск скиллов по ключевым словам.
- `get_skill_detail`: просмотр содержимого `SKILL.md` и зависимостей скилла.

### Поиск и установка серверов (`mcpfinder`)

Сервер `@mcpfinder/server` позволяет искать общедоступные MCP-серверы:
- `search_mcp_servers`: поиск серверов в глобальном каталоге.
- `get_server_details`: детальная информация об инструментах и провайдере.
- `get_install_config`: получение готового JSON-фрагмента для вставки в `context_servers` файла `.zed/settings.json`.
- `browse_categories`: навигация по категориям (базы данных, DevOps, веб-поиск и т.д.).

---

## 8. Пошаговый быстрый старт

### Шаг 1: Создание файла `.env`
Скопируйте шаблон и укажите ваши ключи и модели:

```sh
cp docs/.env.example .env
```

Отредактируйте `.env`:
```bash
FRONTIER_MODEL_PROVIDER=anthropic
FRONTIER_MODEL=claude-3-7-sonnet-latest

MID_TIER_MODEL_PROVIDER=anthropic
MID_TIER_MODEL=claude-3-5-haiku-latest

ANTHROPIC_API_KEY=sk-ant-api03-ваш_ключ
```

### Шаг 2: Проверка статуса MCP-серверов
1. Откройте **Settings → AI → MCP Servers** в Zed (или выполните команду `agent: open settings`).
2. Убедитесь, что индикаторы рядом с `taskgraph` и `agent-bus` горят зеленым цветом («Server is active»).
3. При необходимости отредактируйте пути к бинарникам в `.zed/settings.json` под вашу операционную систему.

### Шаг 3: Выбор профиля и старт сессии
1. Откройте панель агента (**Agent Panel**).
2. Выберите профиль **Orchestrator** (или нажмите `Configure` для просмотра настроек).
3. Поставьте задачу:
   > "Декомпозируй задачу создания нового компонента и зарегистрируй цели в taskgraph".
4. Оркестратор создаст цели через `mcp:taskgraph:goal_create`, породит задачи через `mcp:taskgraph:task_create` и делегирует их соответствующим инженерам (`spawn_agent`).

### Шаг 4: Отслеживание выполнения в Task Panel
1. Откройте панель задач (**Agent Task Panel**, иконка `ListTodo` на боковой панели).
2. Наблюдайте за переходами статусов задач (`Ready` → `Running` → `Completed`).
3. При необходимости используйте **View Task Diff** для проверки изменений перед аппрувом.
