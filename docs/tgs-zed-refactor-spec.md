# ТЗ v2 — Рефакторинг Task Graph Service (TGS) и интеграции с Zed

Итоговая редакция. Сводит исходное ТЗ v1 с правками по итогам ревью кода
(`spawn_agent_tool.rs`, `mcp_agent_task_provider.rs`, `agent_task_store.rs`,
`agent_task.rs`, `.zed/settings.json`). Ссылки на строки кода приведены для
текущего состояния репозитория и служат ориентиром, а не строгим контрактом.

## Журнал решений относительно v1

| # | Решение |
|---|---------|
| 1 | Жизненный цикл worktree НЕ меняется. Требование v1 «не уничтожать worktree между ходами» снято: continuity обеспечивают ACP-тред + покоммитный merge в goal-ветку; защита от зависания уже есть (`WorktreeReleaseSafety`, LRU-пул, startup sweep — `task_worktree.rs`) |
| 2 | Привязка task→session восстанавливается в новом `refresh` через `register_task_session` из данных `task_graph` (сегодня это делает `merge_items_into_graph`, который удаляется) |
| 3 | Артефакты остаются MCP-инструментами TGS; включаются точечно в профилях (researcher/github_researcher — publish+get+list; reviewer — get+list). Нативные инструментыpublish/read (вариант C) отклонены |
| 4 | Генерация `execution_report` переносится на сторону TGS в обработчик `task_record_turn` |
| 5 | `task_create` получает опциональный `task_id` с ensure-семантикой; `idempotency_key` не вводится |
| 6 | `goal_update` поддерживает полный enum статусов; действия панели не вырезаются |
| 7 | Оркестратору возвращён `task_get` (итого 6 инструментов); `parent_id` возвращён в schema |
| 8 | FSM дополнена переходом `failed -> ready`; `blocked` — производный статус |
| 9 | Совместимость со старыми сборками не требуется (альфа): cutover в один шаг, схема БД пересоздаётся |

---

# Часть 1. Task Graph Service (Go-демон)

## 1. Цели

1. Сократить API до **13 MCP-инструментов + 1 сервисного эндпоинта телеметрии**:
   `task_create`, `task_update`, `task_get`, `task_delete`, `goal_create`,
   `goal_update`, `goal_get`, `goal_delete`, `task_graph`, `events_list`,
   `artifact_publish`, `artifact_get`, `artifact_list` + `task_record_turn`.
2. Устранить FSM-блокировки повторного открытия (`completed -> running`,
   `failed -> running`) для переиспользования сессий сабагентов по `session_id`.
3. Пакетное создание задач с зависимостями в одной SQLite-транзакции.
4. Компактные ответы мутирующих методов (Compact Responses).
5. Единый вызов телеметрии хода `task_record_turn`, включающий генерацию
   `execution_report`.

## 2. Спецификация API

### 2.1. `task_create`

Создание одной задачи или пакета со встроенными зависимостями.

Вход (одиночная):

```json
{
  "task_id": "string (optional, ensure-семантика, см. ниже)",
  "title": "string (required)",
  "goal_id": "string (optional)",
  "parent_id": "string (optional)",
  "description": "string (optional)",
  "depends_on": ["string"] (optional),
  "acceptance_criteria": ["string"] (optional),
  "write_scopes": ["string"] (optional),
  "priority": "integer (optional)"
}
```

Вход (батч) — дополнительно массив `tasks`, каждый элемент повторяет поля
одиночной схемы плюс обязательный `temp_id`:

```json
{
  "goal_id": "string (optional, наследуется элементами без своего goal_id)",
  "tasks": [
    {
      "temp_id": "string (required)",
      "task_id": "string (optional)",
      "title": "string (required)",
      "goal_id": "string (optional)",
      "parent_id": "string (optional, temp_id или task_id)",
      "description": "string (optional)",
      "depends_on": ["string (temp_id или существующий task_id)"] (optional),
      "acceptance_criteria": ["string"] (optional),
      "write_scopes": ["string"] (optional),
      "priority": "integer (optional)"
    }
  ]
}
```

Логика:

- **Ensure-семантика `task_id`**: если задача с таким `task_id` существует —
  вернуть её ID без изменения данных (ответ как при создании). Уникальность
  `task_id` — единственный механизм идемпотентности; отдельный
  `idempotency_key` не вводится. Ошибка возвращается только если существующая
  задача принадлежит другому `goal_id`.
- Батч выполняется в единой транзакции SQLite: либо все задачи созданы, либо
  ни одной.
- Ссылки `depends_on`/`parent_id` принимают `temp_id` из текущего пакета или
  существующий `task_id`. `temp_id` не должен совпадать по формату с реальными
  ID (запрещён префикс `TASK-`/`GOAL-`) — иначе ошибка валидации.
- TGS строит DAG пакета, отвергает циклы и ссылки на несуществующие ID.
- Лимит батча — 50 задач (защита от раздувания транзакции).

Компактные ответы:

- Одиночная: `{"task_id": "TASK-101", "status": "ready"}`
- Батч: `{"created": {"<temp_id>": "TASK-101", ...}}`

### 2.2. `task_update`

Единый инструмент мутации (заменяет `task_start`, `task_complete`,
`task_fail`, `task_cancel`, `task_retry`, `task_archive`, `task_unarchive`,
`task_add_dependency`, `task_remove_dependency`).

```json
{
  "task_id": "string (required)",
  "status": "ready | running | completed | failed | cancelled | archived (optional)",
  "error": "string (optional, причина при failed/cancelled)",
  "depends_on": ["string"] (optional, полная замена списка),
  "title": "string (optional)",
  "description": "string (optional)",
  "acceptance_criteria": ["string"] (optional),
  "write_scopes": ["string"] (optional),
  "priority": "integer (optional)",
  "model": "string (optional)",
  "assignee": "string (optional)",
  "session_id": "string (optional)",
  "parent_id": "string | null (optional)"
}
```

Ответ: `{"task_id": "TASK-101", "status": "running"}`.

Примечание: `depends_on` — замена всего списка; при конкурентных
оркестраторах возможен lost update (принятая цена альфы, Single-writer
предположение). `dependency_type` у рёбер не поддерживается (осознанный
отказ, см. §7).

### 2.3. `task_get`

Вход: `{"task_id": "..."}`. Ответ — полная карточка (соответствует
`AgentTaskDetail` на стороне Zed):

```json
{
  "summary": { "id", "parent_id", "goal_id", "title", "status", "attempt",
               "assignee", "write_scopes", "created_at", "assigned_profile",
               "model", "session_id", "metrics" },
  "description": "string",
  "acceptance_criteria": ["string"],
  "events_tail": [ ... последние N событий задачи ... ]
}
```

`task_get` — обязательный инструмент оркестратора: `task_graph` отдаёт только
summary-поля и не содержит `description`/`acceptance_criteria`.

### 2.4. `task_delete`

- Вход: `{"task_id": "..."}`. Ответ: `{"deleted": true}`.
- **Предусловие:** задача в статусе `archived`; иначе ошибка.
- Каскад: рёбра зависимостей и записи `turns` удаляются; артефакты задачи
  удаляются; журнал `events` сохраняется (append-only аудит) и продолжает
  ссылаться на удалённый ID.

### 2.5. `goal_create` / `goal_update` / `goal_get` / `goal_delete`

`goal_create`: `{"title" (required), "description" (optional), "priority" (optional)}`
-> `{"goal_id": "GOAL-1"}`.

`goal_update` — универсальная мутация, **полный enum статусов** (действия
панели Zed не вырезаются):

```json
{
  "goal_id": "string (required)",
  "title": "string (optional)",
  "description": "string (optional)",
  "priority": "integer (optional)",
  "status": "running | blocked | failed | completed | cancelled | archived (optional)"
}
```

Значение `active` принимается как alias `running` (Zed-парсер уже толерантен,
`agent_task.rs:159-164`). Ответ: `{"goal_id": "GOAL-1", "status": "..."}`.

`goal_get`: метаданные цели + массив ID входящих задач.
`goal_delete`: предусловие `archived`; задачи не удаляются, только отвязываются.

### 2.6. `task_graph`

Единый источник истины для UI и агентов (заменяет `item_list`, `task_list`,
`goal_list`, `goal_tasks`, `task_children`, `task_ready`).

Вход: `{"goal_id": "string (optional)"}`.

Ответ — только summary-поля (компактность; полные карточки — через `task_get`):

```json
{
  "goals": [ { "goal_id", "title", "status", "priority", "tasks_total",
               "tasks_done", "created_at", "metrics" } ],
  "tasks": [ { "id", "parent_id", "goal_id", "title", "status", "attempt",
               "depends_on", "assigned_profile", "assignee", "model",
               "session_id", "created_at", "metrics" } ]
}
```

- `session_id` обязателен в ответе: на нём строится восстановление привязок
  после рестарта Zed (см. Часть 2, §2.3).
- Статус `blocked` — **производный**: TGS помечает `ready`-задачи с
  незакрытыми зависимостями как `blocked` в ответе; через `task_update`
  статус `blocked` не принимается (соответствует текущему поведению панели).
- Без параметра возвращаются все цели и задачи проекта, включая задачи без
  цели. Пагинация не вводится (альфа, объёмы малы); расширение —
  опциональные фильтры `status`/`limit` при необходимости.

### 2.7. `events_list`

Вход: `{"limit": "integer (optional, default 100)"}`. Ответ — массив
`AgentTaskEvent` (`seq`, `timestamp_millis`, `task_id`, `kind`, `message`).
Клиент Zed запрашивает 200 (текущее поведение `refresh`).

### 2.8. Артефакты

- `artifact_publish`: `{"task_id", "kind", "content", "summary" (optional)}`
  -> `{"id": "..."}`.
- `artifact_get`: `{"artifact_id", "offset" (optional), "limit" (optional)}`
  -> объект артефакта; без `offset`/`limit` — полный контент (пагинация для
  больших артефактов, чтобы не раздувать контекст читателя).
- `artifact_list`: `{"task_id"}` -> массив артефактов задачи.

### 2.9. `task_record_turn` (нативная телеметрия, Rust-to-Go)

Вызывается рантаймом Zed один раз в конце хода сабагента. Заменяет
`task_turn_start` + `task_turn_finish` + публикацию `execution_report`.

```json
{
  "task_id": "string (required)",
  "session_id": "string (optional)",
  "model": "string (optional)",
  "duration_ms": "integer (required)",
  "tokens": {
    "input": "integer",
    "output": "integer",
    "cache_read": "integer",
    "cache_creation": "integer"
  },
  "error": "string (optional)",
  "branch": "string (optional)",
  "commit_sha": "string (optional)",
  "output_summary": "string (optional, первые ~20 строк финального ответа агента)"
}
```

Логика (одна транзакция SQLite):

1. `turn_index += 1`; запись строки в таблицу `turns`.
2. Пересчёт кумулятивных метрик задачи (`duration_ms`, токены,
   `turns_count`).
3. Рендер и upsert артефакта `kind = "execution_report"` из метаданных
   задачи + записи хода + переданных `branch`/`commit_sha`/`output_summary`.
   Отчёт создаётся и для неуспешных ходов (при наличии `error`) — аудит-след
   остаётся всегда.

Ответ: `{"turn_index": 2}`.

## 3. FSM задач

Переходы:

```
ready      -> running
running    -> completed | failed | cancelled
completed  -> running   (reopen, attempt += 1, событие task.reopened)
failed     -> running   (retry-start, attempt += 1)
failed     -> ready     (ручной retry из панели; семантика текущего task_retry)
cancelled  -> running   (возобновление)
любое      -> archived
archived   -> ready     (unarchive — восстановление в исходное состояние)
```

Правила:

- Повторный вызов с тем же статусом (`running -> running` и т.п.) —
  идемпотентный no-op, ответ `Ok`.
- Reopen не сбрасывает историю `turns` и метрики — накопление кумулятивно.
- При Reopen переданный `session_id` перезаписывает привязку (новая сессия
  может законно принять задачу).
- `blocked` — производный статус (см. §2.6), в FSM не хранится.

## 4. События

В таблицу `events` пишутся: смена статуса задачи/цели, `task.reopened`,
создание/удаление. События append-only; удаление задачи их не затирает.

## 5. Удаляемые эндпоинты (21)

`task_start`, `task_complete`, `task_fail`, `task_cancel`, `task_retry`,
`task_archive`, `task_unarchive`, `task_add_dependency`,
`task_remove_dependency`, `task_dependencies`, `task_dependents`,
`task_children`, `task_ready`, `task_list`, `goal_tasks`, `goal_list`,
`goal_archive`, `goal_unarchive`, `item_list`, `task_turn_start`,
`task_turn_finish`.

## 6. Схема БД

Альфа: схема пересоздаётся с нуля, миграции и сохранение данных не
требуются. Таблицы: `tasks` (включая `session_id`, `attempt`, `parent_id`,
`priority`), `goals` (включая `priority`, полный строковый статус),
`dependencies` (task_id, depends_on_task_id), `turns` (turn_index, duration,
tokens, error, branch, commit_sha), `events`, `artifacts`.

## 7. Осознанные отказы (не потери по недосмотру)

| Поле/механизм | Причина отказа |
|---|---|
| `idempotency_key` | Дублируется ensure-семантикой `task_id`; батч атомарен |
| `contract` (objective/inputs/expected_outputs/constraints), `metadata` | Zed-модель (`AgentTaskDetail`) потребляет только `description` + `acceptance_criteria`; контракт инжектится из них |
| `dependency_type` у рёбер | Не используется ни Zed, ни текущими оркестраторами |
| `cost_usd` в метриках | Ценообразование — клиентская забота; TGS хранит только токены/время |
| `task_children`/`task_dependents` | Вычисляются клиентом из `task_graph` |

## 8. Тесты TGS (Go)

1. FSM: все переходы из §3, включая no-op идемпотентность и отказ
   `blocked` в `task_update`.
2. Reopen: `completed -> running` инкрементирует `attempt`, пишет
   `task.reopened`, метрики кумулятивны.
3. `task_create`: ensure-семантика `task_id`; батч в одной транзакции
   (откат по сломанной ссылке); цикл отвергается; `temp_id` с префиксом
   `TASK-` отвергается; лимит 50.
4. `task_record_turn`: `turn_index`, агрегаты, артефакт `execution_report`
   создан (в т.ч. при `error`).
5. `goal_update`: полный enum, alias `active`.
6. `task_delete`/`goal_delete`: предусловие `archived`, каскады, `events`
   сохраняются.

---

# Часть 2. Zed (Rust)

## 1. `crates/agent/src/tools/spawn_agent_tool.rs`

1. **Старт хода** (L565-573): без изменений — при `task_id` вызывается
   `set_task_status_registered(task_id, Running)`. Благодаря FSM TGS это
   бесшовно переоткрывает `completed`-задачу (reopen по `session_id`).
2. **Телеметрия** (L596-618, L658-676): удалить `record_turn_start` и
   `record_turn_finish`. Локальный замер `Instant::now()` + дельта токенов
   (уже есть, L620-656) сохраняется. В конце хода — один вызов
   `record_turn` со структурой `AgentTurnRecord` (см. §4).
3. **Финиш хода** (L743-804): сохранить перевод в `Completed` при
   `send_result.is_ok()` и авто-`fail` при definitive-ошибке (L692-707).
   **Удалить** блок форматирования `execution_report` и вызов
   `publish_artifact` (L775-804) — отчёт теперь рендерит TGS в
   `task_record_turn`.
4. **Контракт задачи** (L580-594, L623-637): контракт инжектится только
   рантаймом через `get_task_registered`. Удалить fallback-фразу «Fetch the
   details of this task yourself via the task-management tools» — у
   сабагентов больше нет taskgraph-инструментов.
5. **Worktree**: жизненный цикл не меняется — `commit_subagent_worktree`,
   `auto_cleanup_subagent_worktree` (L710-841) и весь механизм
   `task_worktree.rs` остаются как есть. Привязка сессии
   `register_task_session` на spawn (L516-519) сохраняется.

## 2. `crates/agent/src/mcp_agent_task_provider.rs`

1. **`set_task_status`** (L507-589): удалить маппинг на 7 тулов и весь
   каскад фоллбеков. Единственный вызов:

   ```rust
   let mut args = serde_json::Map::new();
   args.insert("task_id".to_string(), serde_json::Value::String(id.to_string()));
   args.insert("status".to_string(), serde_json::Value::String(status.as_str().to_string()));
   let task = self.call_tool("task_update", Some(args), cx);
   cx.spawn(async move |_cx| { task.await?; Ok(()) })
   ```

2. **`ensure_task`** (L106-157): схема сохраняется (`task_get` -> при
   отсутствии `task_create`), из аргументов create убрать
   `idempotency_key`, оставить `task_id`.
3. **Статусные методы**: `complete_task`, `fail_task` (передаёт
   `error: reason`), `archive_task`, `unarchive_task`, `set_goal_status`,
   `archive_goal`, `unarchive_goal` — делегируют в `task_update` /
   `goal_update`.
4. **Телеметрия**: `record_turn` вызывает `task_record_turn` (MCP-эндпоинт,
   доступный только рантайму; в профилях агентов отключён на уровне
   настроек).
5. **Удалить** `fetch_items` и разбор `item_list` (L479-505).

## 3. `crates/agent/src/agent_task_store.rs`

1. Удалить `merge_items_into_graph` (L15-120) и всё, что относится к
   слиянию items.
2. **`refresh`** (L190-262): запрашивать `fetch_graph` +
   `list_events(200)`; присваивать `store.graph = graph` напрямую. **Без
  условно** восстановить привязки сессий — для каждой задачи с
   `session_id`:

   ```rust
   for task in &graph.tasks {
       if let Some(session_id) = &task.session_id {
           crate::task_worktree::register_task_session(&task.id, session_id.clone());
       }
   }
   ```

   Без этого после рестарта Zed теряется task→session-привязка и «бесшовный
   reopen» молча деградирует до новой сессии. Поле `session_id` в
   `AgentTaskSummary` добавить при отсутствии.

## 4. `crates/agent/src/agent_task.rs`

1. Удалить из трейта `AgentTaskProvider`: `fetch_items`,
   `record_turn_start`, `record_turn_finish`.
2. Добавить:

   ```rust
   pub struct AgentTurnRecord {
       pub duration_ms: u64,
       pub tokens: Option<language_model::TokenUsage>,
       pub model: Option<SharedString>,
       pub session_id: Option<SharedString>,
       pub error: Option<SharedString>,
       pub branch: Option<SharedString>,
       pub commit_sha: Option<SharedString>,
       pub output_summary: Option<SharedString>,
   }

   fn record_turn(
       &self,
       task_id: &AgentTaskId,
       record: AgentTurnRecord,
       cx: &mut App,
   ) -> Task<Result<u32>>;
   ```

3. Удалить `AgentUnifiedItem` и связанные типы, если вне пути `item_list`
   они не используются (проверить `agent_task_panel.rs`).

## 5. Тесты (`crates/agent/src/tests/mod.rs` и store-тесты)

1. `FakeThreadEnvironment`: `recorded_turn_starts`/`recorded_turn_finishes`
   -> `recorded_turns`.
2. `test_spawn_agent_turn_recording_and_metrics`: один `record_turn` с
   корректной `AgentTurnRecord`.
3. `test_spawn_agent_updates_task_metadata_and_status_lifecycle`: ход ->
   `Running` -> `Completed`; повторный `spawn_agent` с тем же
   `task_id`+`session_id` -> `Running` (reopen).
4. `set_task_status`: ровно один MCP-вызов `task_update`, без фоллбеков.
5. `refresh`: после сброса реестра привязка сессий восстанавливается из
   `task_graph`.
6. Удалить тесты `merge_items_into_graph` и разбора `item_list`
   (`test_agent_task_store_merge_items`,
   `test_agent_task_store_synthesize_goals_when_graph_goals_empty`,
   `test_agent_task_store_items_failure_non_fatal`, `test_merge_items_*`,
   `test_parse_real_item_list_payload`).

---

# Часть 3. `.zed/settings.json`

Оценка экономии корректна только **на диалог** (схемы инструментов живут в
системном промпте и кэшируются), не «на каждое сообщение».

## 1. Оркестратор (`orchestrator`)

`context_servers.taskgraph.tools` — ровно 6 инструментов:

```json
"taskgraph": {
  "tools": {
    "goal_create": true,
    "goal_update": true,
    "task_create": true,
    "task_update": true,
    "task_graph": true,
    "task_get": true
  }
}
```

В `tool_permissions` — `default: allow` для этих 6; остальные
`mcp:taskgraph:*` — `false`.

## 2. Сабагенты

Статусы и телеметрия идут через рантайм — MCP-инструменты жизненного цикла
сабагентам не нужны. Полный перечень профилей с taskgraph-конфигурацией
(сегодня): `orchestrator`, `agent_architect`, `agent_fleet_planner`,
`agent_tuner`, `agent_reviewer`, `agent_engineer`, `editor_engineer`,
`ui_engineer`, `collab_engineer`, `reviewer`, `git_committer`,
`git_merger`, `merge_resolver`, `fork_merger`, `ci_release`, `e2e_tester`,
`researcher`, `github_researcher`.

Всем, кроме перечисленных ниже, — `"taskgraph": false` (или исключить блок):

- `researcher`, `github_researcher`: `{ "artifact_publish": true,
  "artifact_get": true, "artifact_list": true }`
- `reviewer`: `{ "task_get": true, "task_graph": true, "artifact_get": true,
  "artifact_list": true }` — верификация критериев приёмки и артефактов.

`project_qa` и `code_mechanic` уже без taskgraph — не трогать.

---

# Часть 4. Порядок внедрения и приёмка

Совместимость не требуется (альфа): сначала TGS (новая схема БД с нуля),
затем Zed, одним cutover. На время между деплоями панель задач будет
оффлайн — приемлемо.

Критерии приёмки:

1. TGS экспонирует ровно 14 эндпоинтов (13 MCP + телеметрия); список §5
   отсутствует.
2. `set_task_status` в Zed порождает ровно один вызов `task_update`
   (тест 4 Части 2).
3. Повторный `spawn_agent` с тем же `task_id`+`session_id` переводит
   задачу `completed -> running`, `attempt` растёт, метрики кумулятивны.
4. После условного рестарта (сброс реестра) `refresh` восстанавливает
   `register_task_session` из `task_graph` (тест 5 Части 2).
5. Жизненный цикл worktree не изменён: существующие тесты
   `task_worktree.rs` зелёные без правок.
6. Артефакт `execution_report` появляется после каждого хода, включая
   неуспешные; Zed не содержит кода его форматирования.
7. Все статусные действия панели (включая `blocked`/`failed`/`cancelled`
   для целей) проходят без ошибок.
8. Системный промпт оркестратора содержит ровно 6 taskgraph-схем;
   researcher/github_researcher — 3; reviewer — 4; остальные профили — 0.
