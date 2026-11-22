## 1. Жизненный цикл задач и изоляция Git Worktree

| Параметр в `settings.json`                      | Тип                      | По умолчанию               | Описание и влияние на Harness                                                                                                                                                                                           |
| ----------------------------------------------- | ------------------------ | -------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `agent.task_lifecycle_mode`                     | `"runtime"` \| `"agent"` | `"runtime"`                | **Главный тумблер автоматики**: `"runtime"` включает автоматический коммит, слияние в goal, обновление TGS и очистку ворктри. В режиме `"agent"` вся автоматика выключена (агент сам управляет своим жизненным циклом). |
| `agent.task_worktree_limit`                     | `usize`                  | `3` (мин. `1`)             | Максимальное число каталогов ворктри на диске в пуле проекта. При превышении запускает LRU-вытеснение завершенных ворктри.                                                                                              |
| `agent.task_worktree_retention_timeout_seconds` | `u64` (сек)              | `0` (мин. `60` при $>0$)   | Задержка в секундах перед удалением/вытеснением ворктри завершенной задачи. `0` отключает задержку, значения от `1` до `59` округляются до `60`.                                                                       |
| `agent.task_worktree_language_servers`          | `bool`                   | `true`                     | Запускать ли Language Server (Rust Analyzer и др.) в изолированных ворктри субагентов по умолчанию.                                                                                                                    |
| `agent.commit_on_session_end`                   | `bool`                   | `true`                     | Автоматический git commit в ветку `agent-task/{task_id}` по завершении работы субагента (при ошибке — WIP salvage commit).                                                                                              |
| `agent.cleanup`                                 | `bool`                   | `true`                     | Автоматическое удаление папки ворктри с диска при завершении таска (блокируется при наличии зависимых задач).                                                                                                           |
| `agent.register_metadata`                       | `bool`                   | `true`                     | Автоматическая регистрация метаданных субагента (модель, сессия) и выставление статуса в Task Graph Server (TGS).                                                                                                       |
| `agent.publish_ref_artifact`                    | `bool`                   | `true`                     | Автоматическая публикация ref-артефакта со статусом слияния задачи в TGS.                                                                                                                                               |
| `agent.delete_task_branches_on_graduation`      | `bool`                   | `true`                     | Удаление всех веток задач `agent-task/{task_id}` из Git при выпуске цели (`cleanup_graduated_goal`).                                                                                                                     |
| `agent.delete_goal_branches_on_graduation`      | `bool`                   | `true`                     | Удаление ветки цели `agent-goal/{goal_id}` из Git при выпуске цели.                                                                                                                                                     |
| `agent.goal_graduation_base_branch`             | `string` \| `null`       | `null`                     | Базовая ветка репозитория для выпуска цели (если отличается от дефолтной ветки репозитория).                                                                                                                            |

---

## 2. Параллелизм и глубина вложенности субагентов (`nested_sub_agents`)

| Параметр | Тип | По умолчанию | Описание и влияние на Harness |
|---|---|---|---|
| `agent.nested_sub_agents.enabled` | `bool` | `true` | Разрешает субагентам рекурсивно вызывать `spawn_agent` для создания дочерних субагентов. При `false` инструмент доступен только корневому агенту. |
| `agent.nested_sub_agents.max_depth` | `u32` | `1` (диапазон `1..=5`) | Максимальная глубина иерархии субагентов под корневым агентом (при `1` субагенты не могут делегировать дальше). |
| `agent.nested_sub_agents.max_concurrent` | `u32` | `16` (диапазон `1..=32`) | Лимит одновременно работающих субагентов во всем дереве сессий проекта. |

---

## 3. Декларативные профили агентов (`agent.profiles.<profile_id>`)

| Параметр профиля | Тип | По умолчанию | Описание и влияние на Harness |
|---|---|---|---|
| `name` | `string` | *(обязательное)* | Отображаемое имя роли в интерфейсе. |
| `description` | `string` \| `null` | `null` | Описание роли, передаваемое родительскому агенту в каталоге делегирования. |
| `task_isolation` | `"required"` \| `"optional"` \| `"disabled"` | `"optional"` | Режим изоляции: `"required"` (всегда отдельный Git worktree), `"optional"` (ворктри создается только при явных параметрах ветки), `"disabled"` (запуск только в общем каталоге). |
| `permission_mode` | `"autonomous"` \| `"interactive"` \| `"unrestricted"` \| `null` | `null` | Режим подтверждений. При `null` вычисляется динамически: `"autonomous"`, если задан блок `tool_permissions`, иначе `"interactive"`. |
| `default_model` | `LanguageModelSelection` \| `null` | `null` | Модель, закрепленная за профилем (`provider`, `model`, `enable_thinking`, `effort`, `speed`). |
| `custom_prompt_path` | `string` \| `null` | `null` | Путь к Markdown-файлу с системными инструкциями профиля (алиасы: `custom_prompt_file`, `prompt_path`, `prompt_file`). |
| `system_prompt_template` | `string` \| `null` | `null` | Путь к файлу шаблона Handlebars/Markdown, переопределяющему системный промпт. |
| `tools` | `map<string, bool>` | `{}` | Белый список доступных встроенных инструментов (`read_file`, `edit_file`, `terminal`, `spawn_agent` и др.). |
| `enable_all_context_servers` | `bool` \| `null` | `null` | Разрешить ли по умолчанию доступ ко всем MCP-серверам для данного профиля. |
| `context_servers` | `map<string, bool \| { tools: map<string, bool> }>` | `{}` | Подключенные MCP-серверы и инструменты. Сервер можно включить целиком (`true`/`false`) либо выбрать отдельные инструменты. |
| `skills` | `string[]` \| `null` | `null` | Список разрешенных скиллов. При `null` встроенные профили видят все скиллы, а кастомные — ни одного. При явном `[]` скиллы блокируются полностью. |
| `delegation` | `object` \| `null` | `null` | Блок разрешения спавна. Без этого блока профиль является solo-агентом (`spawn_agent` недоступен). |
| `delegation.allowed` | `string[]` | `[]` | Список идентификаторов профилей, которые этот агент имеет право порождать. |
| `delegation.max_depth` | `u32` | `1` (диапазон `1..=5`) | Лимит глубины делегирования для данного профиля. |
| `tool_permissions` | `ToolPermissions` \| `null` | `null` | Правила разрешений инструментов и `write_scopes` для профиля. |
| `task_worktree_language_servers` | `bool` \| `null` | `null` | Переопределение запуска Language Server для ворктри этого профиля (наследует `agent.task_worktree_language_servers`). |
| `terminal_wrapper_command` | `string` \| `null` | `null` | Переписчик команд терминала (`rtk`). Три состояния: `null` (наследует глобальный), `""` (отключен), строка (переопределяет). |

---

## 4. Безопасность, периметры записи и Песочница ОС

| Параметр | Тип | По умолчанию | Описание и влияние на Harness |
|---|---|---|---|
| `agent.tool_permissions.default` | `"allow"` \| `"deny"` \| `"confirm"` | `"confirm"` | Глобальная политика по умолчанию для инструментов при отсутствии совпадений по правилам. |
| `agent.tool_permissions.tools.<tool>.default` | `"allow"` \| `"deny"` \| `"confirm"` \| `null` | `null` | Дефолтная политика для конкретного инструмента (наследует глобальный `default`). |
| `agent.tool_permissions.tools.<tool>.write_scopes` | `string[]` | `[]` | Glob-шаблоны файлового периметра для модифицирующих инструментов (`edit_file`, `write_file`). |
| `agent.tool_permissions.tools.<tool>.always_allow` | `(string \| RegexRule)[]` | `[]` | Регулярные выражения аргументов для автоподтверждения вызова инструмента. |
| `agent.tool_permissions.tools.<tool>.always_deny` | `(string \| RegexRule)[]` | `[]` | Регулярные выражения для безусловной блокировки (наивысший приоритет среди всех слоев настроек). |
| `agent.tool_permissions.tools.<tool>.always_confirm` | `(string \| RegexRule)[]` | `[]` | Регулярные выражения, требующие обязательного ручного подтверждения пользователем. |
| `agent.sandbox_permissions.allow_unsandboxed` | `bool` | `false` | Полное отключение песочницы ОС для терминала субагента (вместо песочницы запускается стандартный терминал). |
| `agent.sandbox_permissions.allow_all_hosts` | `bool` | `false` | Разрешить процессам в песочнице сетевой доступ к любым хостам без запроса подтверждения. |
| `agent.sandbox_permissions.network_hosts` | `string[]` | `[]` | Белый список доменов/хостов, доступных из песочницы (поддерживает маски вида `*.npmjs.org`). |
| `agent.sandbox_permissions.allow_fs_write_all` | `bool` | `false` | Разрешить процессам в песочнице запись в любые пути файловой системы без запроса подтверждения. |
| `agent.sandbox_permissions.write_paths` | `(string \| { requested: string, resolved: string })[]` | `[]` | Разрешенные для записи поддеревья файловой системы. |
| `agent.sandbox_permissions.warn_confusable_unicode` | `bool` | `true` | Предупреждать при запросе прав к доменам или путям с невидимыми/омоглифическими Unicode-символами. |
| `agent.sandbox_permissions.warn_ntfs_grants` | `bool` | `true` | Предупреждать (Windows/WSL) о снижении гарантий изоляции при выдаче прав на запись в Windows DrvFs. |

---

## 5. Маршрутизация и выбор моделей языкового ядра

| Параметр | Тип | По умолчанию | Описание и влияние на Harness |
|---|---|---|---|
| `agent.default_model` | `LanguageModelSelection` \| `null` | `claude-sonnet-4` *(в default.json)* | Основная модель интерактивного диалога пользователя с агентом. |
| `agent.subagent_model` | `LanguageModelSelection` \| `null` | `null` | Модель субагентов по умолчанию (при `null` наследует модель, effort и thinking родительского треда). |
| `agent.commit_message_model` | `LanguageModelSelection` \| `null` | `null` | Модель для генерации сообщений коммитов в Git (при `null` наследует `default_model`). |
| `agent.compaction_model` | `LanguageModelSelection` \| `null` | `null` | Модель для сжатия контекста диалога (при `null` используется текущая модель треда). |
| `agent.thread_summary_model` | `LanguageModelSelection` \| `null` | `null` | Модель для генерации заголовков и резюме сессий (при `null` наследует `default_model`). |
| `agent.inline_assistant_model` | `LanguageModelSelection` \| `null` | `null` | Модель для инлайн-ассистента в редакторе (при `null` наследует `default_model`). |
| `agent.inline_assistant_use_streaming_tools` | `bool` | `true` | Использовать ли потоковые инструменты для инлайн-ассистента при поддержке провайдером. |
| `agent.inline_alternatives` | `LanguageModelSelection[]` | `[]` | Дополнительные модели для одновременной генерации альтернативных вариантов в инлайн-ассистенте. |
| `agent.favorite_models` | `LanguageModelSelection[]` | `[]` | Список избранных моделей, отображаемых вверху меню выбора модели. |
| `agent.model_parameters` | `LanguageModelParameters[]` | `[]` | Индивидуальные параметры моделей (`provider`, `model`, `temperature`, `system_prompt_template`, `custom_instructions`). |

---

## 6. Шаблонизация контекста и промптов

| Параметр | Тип | По умолчанию | Описание и влияние на Harness |
|---|---|---|---|
| `agent.task_contract_template` | `string` \| `null` | `null` | Шаблон Handlebars для блока контракта задачи, инжектируемого субагентам при старте. |
| `agent.system_prompt_template` | `string` \| `null` | `null` | Глобальный базовый шаблон Handlebars/Markdown для сборки системного промпта. |
| `agent.thread_title_template` | `string` \| `null` | `null` | Шаблон/промпт для автоматической генерации названий тредов. |
| `agent.commit_message_instructions` | `string` \| `null` | `null` | Дополнительные пользовательские инструкции для промпта генерации Git-коммитов. |
| `agent.commit_message_include_project_rules` | `bool` | `true` | Включать ли правила проекта (`AGENTS.md`, `.rules` и др.) в контекст генерации сообщения коммита. |

---

## 7. Сторожевые таймеры (Watchdogs) и таймауты шагов

*(Примечание: таймауты от `1` до `4` секунд округляются до `5s`; значение `0` отключает таймаут)*

| Параметр | Тип | По умолчанию | Описание и влияние на Harness |
|---|---|---|---|
| `agent.spawn_step_timeout_seconds.commit` | `u64` (сек) | `300` | Таймаут операций коммита и слияния ветки задачи во время спавна. |
| `agent.spawn_step_timeout_seconds.task_graph` | `u64` (сек) | `120` | Таймаут RPC-запросов к Task Graph Server во время спавна. |
| `agent.spawn_step_timeout_seconds.cleanup` | `u64` (сек) | `60` | Таймаут операций удаления каталога ворктри с диска во время спавна. |
| `agent.spawn_step_timeout_seconds.diff` | `u64` (сек) | `30` | Таймаут вычисления Git-диффа для телеметрии субагента во время спавна. |
| `agent.send_watchdog_seconds` | `u64` (сек) | `30` | Таймаут отправки входящего сообщения в субагент до прерывания хода. |
| `agent.turn_watchdog_seconds` | `u64` (сек) | `120` | Таймаут ожидания одного хода генерации модели до прерывания по зависанию. |
| `language_models.<provider>.rate_limit.initial_wait_seconds` | `u64` (сек) | `60` | Пауза перед первым повтором при ответе HTTP 429 от провайдера LLM без заголовка сброса. |
| `language_models.<provider>.rate_limit.max_wait_seconds` | `u64` (сек) | `300` | Верхняя граница одного интервала экспоненциального ожидания при 429. |
| `language_models.<provider>.rate_limit.max_total_wait_seconds` | `u64` (сек) | `18000` (5ч) | Суммарный лимит времени ожидания при Rate Limit до ошибки (`0` — ждать до отмены пользователем). |

---

## 8. Терминал и выполнение команд

| Параметр | Тип | По умолчанию | Описание и влияние на Harness |
|---|---|---|---|
| `agent.terminal_watchdog.enabled` | `bool` | `true` | Включение сторожевого таймера для процессов терминального инструмента агента. |
| `agent.terminal_watchdog.hard_timeout_ms` | `u64` \| `null` (мс) | `3600000` (1ч) | Жесткий таймаут работы команды инструмента `terminal` (`null` отключает лимит). |
| `agent.terminal_watchdog.idle_timeout_ms` | `u64` \| `null` (мс) | `300000` (5мин) | Таймаут простоя команды при потреблении CPU ниже порога (`null` отключает детектор простоя). |
| `agent.terminal_watchdog.idle_cpu_threshold_percent` | `f32` | `5.0` | Порог потребления CPU дерева процессов (в %), ниже которого процесс считается неактивным. |
| `agent.terminal_watchdog.poll_interval_ms` | `u64` (мс) | `5000` (мин. `250`) | Периодичность опроса активности процессов терминала в миллисекундах. |
| `agent.terminal_watchdog.idle_timeout_no_probe_ms` | `u64` (мс) | `600000` (10мин) | Таймаут простоя команды в окружениях, где мониторинг CPU/IO недоступен. |
| `agent.terminal_wrapper_command` | `string` \| `null` | `null` | Глобальная утилита-переписчик команд терминала (контракт: `<cmd> rewrite <command-to-run>`). |
| `agent.cancel_generation_on_terminal_stop` | `bool` | `true` | Прерывать генерацию ответа LLM при нажатии кнопки остановки запущенного терминального инструмента. |
| `agent.terminal_init_command` | `string` | `""` | Команда, автоматически отправляемая при создании интерактивной вкладки терминала в панели агента. |

---

## 9. Контроль контекста, UI и интеграция с сервисами

| Параметр | Тип | По умолчанию | Описание и влияние на Harness |
|---|---|---|---|
| `agent.auto_compact.enabled` | `bool` | `true` | Автоматическое сжатие контекста диалога при приближении к лимиту токенов. |
| `agent.auto_compact.threshold` | `string` \| `integer` | `"90%"` | Порог запуска сжатия: проценты (`"90%"`), использованные токены (`100000`) или остаток (`-20000`). |
| `agent.prevent_idle_sleep` | `bool` | `true` | Запрет ухода операционной системы в спящий режим во время работы тредов агента. |
| `agent.show_turn_stats` | `bool` | `false` | Отображение времени выполнения хода и статистики генерации в UI. |
| `agent.show_merge_conflict_indicator` | `bool` | `true` | Отображение в строке состояния (status bar) индикатора предложения разрешить конфликты слияния агентом. |
| `agent.task_graph_server_id` | `string` | `"tgs"` | Ключ в `context_servers`, указывающий на MCP-сервер Task Graph Service для панели задач. |
| `agent.fetch_max_bytes` | `usize` (байт) | `102400` (100 КБ) | Ограничение объема данных, возвращаемых инструментом `fetch` перед обрезкой ответа. |

---

## 10. Дополнительные параметры UI, компоновки панели и уведомлений

| Параметр | Тип | По умолчанию | Описание и влияние на Harness |
|---|---|---|---|
| `agent.enabled` | `bool` | `true` | Глобальное включение/выключение Agent в редакторе. |
| `agent.default_profile` | `string` | `"write"` | Профиль агента, выбираемый по умолчанию при создании новых тредов. |
| `agent.button` | `bool` | `true` | Отображение кнопки открытия панели агента в строке состояния. |
| `agent.task_button` | `bool` | `true` | Отображение кнопки панели задач агента (Agent Task Panel) в строке состояния. |
| `agent.dock` | `"left"` \| `"right"` \| `"bottom"` | `"left"` / `"right"` | Позиция док-панели агента (по умолчанию слева в Agentic layout, справа в Classic). |
| `agent.task_dock` | `"left"` \| `"right"` | `"left"` | Позиция док-панели задач агента. |
| `agent.flexible` | `bool` | `true` | Пропорциональное (flexible) изменение ширины панели агента при боковом расположении. |
| `agent.task_flexible` | `bool` | `false` | Пропорциональное изменение ширины панели задач агента. |
| `agent.default_width` | `number` (px) | `640` | Фиксированная ширина панели агента в пикселях при `flexible: false`. |
| `agent.default_height` | `number` (px) | `320` | Фиксированная высота панели агента в пикселях при расположении снизу. |
| `agent.limit_content_width` | `bool` | `true` | Ограничивать ли максимальную ширину текста сообщений для удобства чтения. |
| `agent.max_content_width` | `number` (px) | `850` | Максимальная ширина текстового контейнера в пикселях при `limit_content_width: true`. |
| `agent.threads_sidebar.position` | `"left"` \| `"right"` | `"left"` | Расположение боковой панели списка тредов. |
| `agent.threads_sidebar.default_width` | `number` (px) | `300` | Ширина панели списка тредов по умолчанию. |
| `agent.threads_sidebar.auto_open` | `bool` | `true` | Автоматически открывать сайдбар тредов при открытии проекта. |
| `agent.max_idle_retained_threads` | `usize` | `5` | Количество неактивных тредов, удерживаемых загруженными в памяти (`0` выгружает все). |
| `agent.notify_when_agent_waiting` | `"primary_screen"` \| `"all_screens"` \| `"never"` | `"primary_screen"` | Экран для показа всплывающего уведомления, когда агент ожидает ввода или подтверждения. |
| `agent.play_sound_when_agent_done` | `"never"` \| `"when_hidden"` \| `"always"` | `"never"` | Воспроизведение звука при завершении ответа агента или необходимости ввода пользователя. |
| `agent.thinking_display` | `"auto"` \| `"preview"` \| `"always_expanded"` \| `"always_collapsed"` | `"auto"` | Режим отображения блоков рассуждений модели (thinking blocks) в панели агента. |
| `agent.expand_edit_card` | `bool` | `true` | Автоматически разворачивать карточки правок файлов (`edit_file`) с предпросмотром диффа. |
| `agent.expand_terminal_card` | `bool` | `true` | Автоматически разворачивать карточки вывода терминальных команд. |
| `agent.use_modifier_to_send` | `bool` | `false` | Отправлять сообщения только по сочетанию клавиш Cmd/Ctrl+Enter вместо одиночного Enter. |
| `agent.message_editor_min_lines` | `usize` | `4` | Минимальная высота поля ввода сообщений агенту в строках. |
| `agent.enable_feedback` | `bool` | `true` | Отображение кнопок голосования («палец вверх»/«вниз») для оценки работы агента. |
| `agent.single_file_review` | `bool` | `false` | Отображать правки агента в обычных редакторах файлов помимо мультибуфера ревью. |
| `agent.context_servers` | `map<string, ContextServerSettings>` | `{}` | Глобальные MCP-серверы агента (имеют приоритет над `project.context_servers`). |