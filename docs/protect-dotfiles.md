# SPEC-AGENT-DOTFILES — Защита скрытых файлов и директорий (dotfiles) от записи агентами

| Атрибут | Значение |
|---|---|
| Статус | Draft |
| Версия | 2.0 (рефакторинг ТЗ v1) |
| Дата | 2026-09-23 |
| Область | `crates/agent`, `crates/acp_thread`, `crates/sandbox` |
| Зависимости | `write_scopes` (профили агентов), терминальная песочница |

---

## 1. Контекст и модель угроз

### 1.1. Текущее состояние

Сегодня защита от несанкционированной записи агентом действует точечно
(см. `sensitive_settings_kind` в `crates/agent/src/tools/tool_permissions.rs`):

| Защищаемый путь | Классификация |
|---|---|
| `.zed/` (настройки воркспейса) | `SensitiveSettingsKind::Local` |
| `~/.agents/skills/` (глобальные навыки) | `SensitiveSettingsKind::AgentSkills` |
| `paths::config_dir()` (конфиг приложения) | `SensitiveSettingsKind::Global` |
| `.git/` и git common dirs | только в песочнице терминала (`sandbox_git_dirs`) |

Все остальные скрытые пути (компоненты, начинающиеся с `.`) остаются незащищёнными,
если профиль имеет общие права на запись.

### 1.2. Модель угроз

| Вектор | Пример цели | Риск |
|---|---|---|
| Подмена CI/CD | `.github/workflows/*.yml`, `.gitlab-ci.yml` | Кража секретов, RCE в пайплайне (pwn-request) |
| Хуки Git | `.husky/`, `.githooks/` | RCE на машине разработчика при коммите |
| Переопределение сборщика | `.cargo/config.toml`, `.npmrc`, `.yarnrc` | RCE при `cargo test`, подмена реестра пакетов (supply chain) |
| Секреты | `.env`, `.env.local` | Перезапись/порча секретов, влияние на окружение следующих команд |
| Конфиг IDE/редактора | `.vscode/tasks.json` | Автозапуск команд при открытии проекта |

### 1.3. Цель

Любая **мутирующая** операция агента над скрытым путём (файл или директория,
любой компонент которой начинается с `.`) требует явного разрешения:

- **Интерактивный режим** — обязательный запрос подтверждения через UI
  (`event_stream.authorize_always_prompt`), даже если для инструмента задано
  `default: "allow"`.
- **Автономный режим** — путь обязан входить в `write_scopes` профиля; иначе
  немедленный отказ `PolicyDenied`.
- **Терминальная песочница** — скрытые пути верхнего уровня воркспейса
  передаются в `protected_paths` (read-only на уровне ОС).

---

## 2. Термины

- **Скрытый путь (dot-path)** — путь, у которого хотя бы один компонент
  `Component::Normal` начинается с `.` и длиннее одного символа.
- **Мутирующие инструменты** — `edit_file`, `write_file`, `delete_path`,
  `create_directory`, `move_path`, `copy_path`.
- **Читающие инструменты** — `read_file`, `grep`, `find_path`, `list_directory`
  и др.; их поведение эта спецификация **не меняет**.
- **Режимы** — `AgentPermissionMode::Interactive` / `Autonomous` / `Unrestricted`.

---

## 3. Цели и не-цели

### Цели

1. Единый предикат обнаружения скрытых компонентов пути.
2. Классификация скрытых путей как чувствительных с защитой от обхода
   (`..`-traversal, внутрипроектные симлинки).
3. Единый сценарий авторизации для всех мутирующих инструментов.
4. Расширение терминальной песочницы на скрытые пути.
5. Конфигурирование легитимного доступа только через существующий механизм
   `write_scopes`.

### Не-цели (Non-goals)

1. Ограничение **чтения** скрытых файлов — не меняется.
2. Изменение поведения режима `Unrestricted` — сохраняет текущую семантику
   чувствительных путей (см. §9, вопрос Q1).
3. Защита скрытых путей **вне** воркспейсов проекта (кроме уже покрытых
   `AgentSkills`/`Global`).
4. Защита от создания **новых** скрытых файлов командами терминала в песочнице
   (см. §8 «Остаточные риски»).
5. Блокировка действий, выполняемых самим пользователем через UI Zed.

---

## 4. Функциональные требования

Приоритеты: **P0** — обязательно для первой итерации; **P1** — желательно;
**P2** — последующие итерации.

### FR-1 (P0). Предикат `is_dot_path`

**Файл:** `crates/agent/src/tools/tool_permissions.rs`

```rust
/// Returns true when any normal component of the path is dot-prefixed
/// (e.g. `.github/`, `.env`). `.` and `..` components never match.
pub fn is_dot_path(path: &Path) -> bool {
    path.components().any(|component| match component {
        std::path::Component::Normal(name) => {
            let s = name.to_string_lossy();
            s.starts_with('.') && s.len() > 1
        }
        _ => false,
    })
}
```

Требования:

- FR-1.1. Предикат покрывает и директории (`.github/`), и конечные файлы
  (`.env`, `.gitignore`) — проверяется **каждый** компонент, не только последний.
- FR-1.2. `Component::CurDir` (`.`), `Component::ParentDir` (`..`), префиксы и
  корни (`Component::Prefix`/`RootDir`, например `C:\`) не считаются скрытыми.
  Условие `s.len() > 1` — дополнительная защита: «нормальный» компонент,
  состоящий из одной точки, трактуется как не скрытый.
- FR-1.3. Предикат сам по себе регистронезависим по построению: у точки нет
  регистровых вариантов, поэтому `.ENV` и `.env` распознаются одинаково на всех
  платформах без дополнительной нормализации. Отдельная регистронезависимость
  нужна только при **посимвольном сравнении имён** (как это уже делает
  `component_matches_ignore_ascii_case` для `.zed`), здесь не требуется.
- FR-1.4. Предикат чистый (без I/O) и пригоден для быстрого пути (FR-2.1).

### FR-2 (P0). Классификация скрытых путей как чувствительных

**Файл:** `crates/agent/src/tools/tool_permissions.rs`,
функция `sensitive_settings_kind`.

**Решение по моделированию:** расширить перечень чувствительных путей новым
вариантом `SensitiveSettingsKind::Hidden` (dot-path общего вида). Это
автоматически переиспользует существующие механизмы: обход через
`write_scopes`, `PolicyDenied` в автономном режиме, принудительный промпт в
интерактивном.

Алгоритм (двухфазный, как сегодня для `.zed`):

- FR-2.1. **Быстрый путь (без I/O):** лексическая проверка
  `is_dot_path(raw_path)` до каноникализации. Покрывает обычный случай, когда
  агент передаёт путь, буквально содержащий скрытый компонент.
- FR-2.2. **Медленный путь (canonicalize):** при отсутствии совпадения —
  каноникализация через существующую `canonicalize_with_ancestors`, затем:
  - относительный путь вычисляется относительно корней воркспейсов
    (`canonical_path.strip_prefix(worktree_root)`);
  - к относительному пути применяется `is_dot_path`.
- FR-2.3. Перехват обходов, которые обязан ловить медленный путь:
  - `..`-traversal: `src/../.env`, `crates/editor/../../.github/workflows/ci.yml`;
  - внутрипроектные симлинки: `link_dir -> .github` (целевой путь становится
    скрытым после каноникализации).
- FR-2.4. **Приоритет конкретных видов:** если путь одновременно подпадает под
  `Local` (`.zed`), `AgentSkills` или `Global`, возвращается именно этот вид, а
  не `Hidden`. Существующие, более специфичные заголовки промптов и тексты
  ошибок сохраняются без изменений.
- FR-2.5. `Hidden` вычисляется только для путей **внутри** воркспейсов
  (как и сегодняшняя проверка `.zed` в медленном пути); абсолютные пути вне
  проекта классифицируются существующей логикой (`AgentSkills`/`Global`) и
  `Hidden` не получают.

### FR-3 (P0). Интеграция с мутирующими инструментами

#### FR-3.1. Инструменты редактирования: `edit_file`, `write_file`

Оба проходят через `EditSessionContext::authorize` →
`authorize_file_edit`. Требуется, чтобы классификация FR-2 учитывалась в
существующем блоке `is_sensitive`:

| Ситуация | Поведение |
|---|---|
| Скрытый путь + попадание в `write_scopes` | Разрешить без промпта (любой режим) |
| Скрытый путь + автономный режим + нет в `write_scopes` | Отказ (см. текст ниже), даже при `default: "allow"` и даже если `write_scopes` не настроены вовсе |
| Скрытый путь + интерактивный режим + нет в `write_scopes` | Всегда `authorize_always_prompt` (в т.ч. при `default: "allow"`) |
| Нескрытый путь | Без изменений (существующая логика) |

Текст отказа в автономном режиме — в стиле существующих сообщений:

```
PolicyDenied: Editing hidden path '{}' is disallowed for autonomous profile '{}' without explicit write_scope
```

Заголовок промпта в интерактивном режиме:

```
{title} (hidden / configuration file)
```

(аналогично существующим суффиксам `(local settings)`, `(settings)`,
`(agent skills)`).

#### FR-3.2. Файловые операции: `delete_path`, `create_directory`, `copy_path`, `move_path`

Сегодня эти инструменты вызывают `check_profile_write_scope` (которая возвращает
`Ok` при отсутствии настроенных `write_scopes`) и собственные блоки
`is_path_in_profile_write_scope` для чувствительных путей. Требование
«отказ даже при `default: allow` и не настроенных `write_scopes`» означает, что
проверка скрытости должна выполняться **до/независимо** от опциональных
`write_scopes`:

- FR-3.2.1. `delete_path` — проверяется удаляемый путь.
- FR-3.2.2. `create_directory` — проверяется создаваемый путь (включая
  `.github/workflows` целиком).
- FR-3.2.3. `move_path` — проверяются **и** источник, **и** назначение
  (перемещение легитимного файла в `.github/` — тоже атака).
- FR-3.2.4. `copy_path` — проверяется **назначение** (источник читается, запись
  идёт только в назначение).
- FR-3.2.5. Поведение по режимам — идентично матрице FR-3.1.
- FR-3.2.6 (рекомендация). Логику FR-3.1/FR-3.2 вынести в общий помощник
  (например, `authorize_path_mutation`) в `tool_permissions.rs`, чтобы все
  шесть инструментов использовали один и тот же порядок проверок вместо
  копирования блоков; порядок проверок внутри инструмента сохранить
  (симлинк-эскейпы → write_scopes → чувствительность).

### FR-4 (P1). Терминальная песочница

**Файлы:** `crates/agent/src/sandboxing.rs`, вызов в `crates/agent/src/thread.rs`
(импорт `sandbox_git_dirs`), `crates/acp_thread/src/terminal.rs`
(`SandboxWrap::protected_paths`).

- FR-4.1. Переименовать `sandbox_git_dirs` в
  `sandbox_protected_paths(project: &Project, cx: &App) -> Vec<PathBuf>`.
  Обновить вызов в `thread.rs` и doc-комментарий `ThreadSandbox::with_protected_paths`
  (сейчас упоминает только `.git`).
- FR-4.2. Новая функция возвращает объединение:
  1. всех путей, собираемых текущей `sandbox_git_dirs` (`.git` воркспейсов,
     включая несуществующие, common dirs репозиториев — поведение сохраняется);
  2. всех **существующих** скрытых файлов и директорий **верхнего уровня**
     каждого воркспейса.
- FR-4.3. Поиск скрытых записей — по snapshot воркспейса (без прямого I/O):
  дочерние элементы корня, для которых `is_dot_path` истинен.
- FR-4.4. Передача в `acp_thread::SandboxWrap::protected_paths` без изменений
  (существующий путь данных). Эффект на уровне ОС:
  - Linux (bwrap): `--ro-bind` для каждого существующего protected path —
    запись вернёт `Permission denied`;
  - macOS (seatbelt): правило `(deny file-write* (subpath "..."))`;
  - Windows: песочница доступна только через WSL (bwrap внутри WSL) —
    поведение следует платформенной доступности песочницы.
- FR-4.5. Семантика best-effort сохраняется: пути, которые не удалось
  захватить (`HostFilesystemLocation::capture`), молча отбрасываются
  (см. §8).

### FR-5 (P0). Конфигурационная модель

Легитимный доступ к dotfiles настраивается **только** существующим механизмом
`write_scopes`; новые ключи настроек не вводятся.

Схема (соответствует `agent.profiles.<id>.tool_permissions.tools.<tool>` в
`settings.json`, см. `crates/settings_content/src/agent.rs`):

```jsonc
"agent": {
  "profiles": {
    "ci_engineer": {
      "name": "CI/CD Engineer",
      "tool_permissions": {
        "default": "deny",
        "tools": {
          "edit_file": {
            "default": "allow",
            "write_scopes": [
              ".github/**"   // явное разрешение на CI/CD
            ]
          },
          "write_file": {
            "default": "allow",
            "write_scopes": [".github/**"]
          }
        }
      }
    }
  }
}
```

- FR-5.1. Glob-ы `write_scopes` матчатся против пути, относительного корня
  воркспейса (и варианта с именем корня) — как в
  `is_path_in_profile_write_scope` / `check_profile_write_scope`. Примеры
  корректных скоупов: `.github/**`, `.env*`, `.vscode/**`.
- FR-5.2. Устоявшиеся запреты сохраняются: скоупы вида `.zed/**` для
  рабочих профилей по-прежнему не выдаются (правила профилей проекта).
- FR-5.3. Некорректный glob в `write_scopes` отключает инструмент
  (существующее поведение `invalid_patterns`).

---

## 5. Матрица решений авторизации

Для мутирующих инструментов и скрытого целевого пути:

| Режим | Путь в `write_scopes` | Результат |
|---|---|---|
| Interactive | да | Разрешено, без промпта |
| Interactive | нет | `authorize_always_prompt` «{title} (hidden / configuration file)» |
| Autonomous | да | Разрешено |
| Autonomous | нет (включая «`write_scopes` не настроены») | `PolicyDenied: Editing hidden path '...' ... without explicit write_scope` |
| Unrestricted | — | Без изменений относительно текущей семантики чувствительных путей |

Для нескрытых путей поведение всех режимов не меняется.

---

## 6. План тестирования

Инфраструктура: существующие помощники `init_test`, `worktree_roots`,
`setup_thread_and_project` в `tool_permissions.rs`; образцы тестов —
действующие `test_authorize_file_edit_*` (покрывают `.zed/settings.json`).

### 6.1. Unit-тесты предиката (FR-1)

| Тест | Вход | Ожидание |
|---|---|---|
| `is_dot_path_matches_files_and_dirs` | `.env`; `.github/workflows/ci.yml` | `true` |
| `is_dot_path_ignores_normal_paths` | `src/main.rs` | `false` |
| `is_dot_path_matches_nested_dot_components` | `foo/.bar/baz` | `true` |
| `is_dot_path_ignores_current_and_parent` | `.`; `..` | `false` |
| `is_dot_path_matches_through_parent_dir` | `src/../.env` | `true` (`ParentDir` игнорируется, `Normal(".env")` матчится) |
| `is_dot_path_ignores_windows_prefix` | `C:\repo\.env` | `true` (префикс не мешает, компонент `.env` матчится) |

### 6.2. Unit-тесты авторизации (FR-2, FR-3)

По аналогии с существующими `test_authorize_file_edit_*`:

1. `test_authorize_file_edit_dotfile_interactive_prompts` — редактирование
   `.env` в интерактивном режиме вызывает промпт подтверждения; в заголовке —
   суффикс `(hidden / configuration file)`.
2. `test_authorize_file_edit_dotdir_autonomous_without_scope_fails` —
   редактирование `.github/ci.yml` в автономном режиме → `PolicyDenied`.
3. `test_authorize_file_edit_dotdir_autonomous_with_explicit_scope_succeeds` —
   при `.github/**` в `write_scopes` операция проходит без промпта.
4. `test_authorize_file_edit_dotdir_autonomous_default_allow_without_scope_fails` —
   автономный режим, `default: "allow"`, `write_scopes` отсутствуют → всё равно
   `PolicyDenied` (регрессия главного инварианта).
5. `test_authorize_dotfile_traversal_blocked` — запись в `src/../.env` или
   `foo/bar/../../.vscode/tasks.json` распознаётся как скрытый путь
   (медленный путь FR-2.2).
6. `test_authorize_dotfile_symlink_blocked` — симлинк на скрытую папку
   (по образцу `test_resolve_project_path_allows_intra_project_symlinks`) не
   позволяет обойти проверку.
7. `test_sensitive_settings_kind_prefers_specific_kinds` — для `.zed/...`
   возвращается `Local`, а не `Hidden` (FR-2.4).
8. Для FR-3.2 — аналогичные сценарии 1–4 минимум для одного файлового
   инструмента (`delete_path`), плюс отдельный тест `move_path` со скрытым
   назначением и нескрытым источником.

### 6.3. Тесты терминальной песочницы (FR-4)

1. `sandbox_protected_paths` возвращает `.git` (регрессия) и существующие
   скрытые записи верхнего уровня; несуществующие dotfiles — не возвращает.
2. Генерация политики: protected path `.env` даёт `--ro-bind` в аргументах
   bwrap (по образцу `test_build_bwrap_args_allow_fs_write_binds_root_read_write`)
   и `(deny file-write* (subpath ...))` в конфиге seatbelt (по образцу
   `test_generate_seatbelt_config_denies_protected_path_writes`).
3. Интеграционно (платформы с доступной песочницей): `echo 1 > .env` и запись в
   `.github/` внутри песочницы завершаются системной ошибкой доступа
   (`Permission denied`).

---

## 7. Критерии приёмки

1. Все тесты §6.1–6.3 проходят; `./script/clippy` без новых предупреждений.
2. Автономный профиль без явных `write_scopes` не может изменить ни один
   скрытый путь ни одним из шести мутирующих инструментов — в том числе при
   `default: "allow"` и при полностью отсутствующих `write_scopes`.
3. Интерактивный режим всегда показывает промпт для скрытых путей.
4. Явные `write_scopes` (например, `.github/**`) полностью снимают блокировку
   для указанного инструмента.
5. Поведение для `.zed`, `~/.agents/skills`, `paths::config_dir()` и `.git`
   не изменилось (заголовки промптов и тексты ошибок — как раньше).
6. Читающие инструменты работают со скрытыми путями как раньше.

---

## 8. Ограничения и остаточные риски (принимаются осознанно)

| Риск | Причина | Митигация |
|---|---|---|
| Терминал может **создать новый** dotfile (`.env` не существовал на момент сборки песочницы) | `protected_paths` захватываются best-effort; inode несуществующего пути закрепить нельзя (см. doc-комментарий `SandboxWrap::to_policy`) | Инструментные проверки (FR-3) перехватывают запись агентом; для терминала риск аналогичен принятому `git init`-loophole |
| Вложенные скрытые директории (не верхний уровень) не защищены на уровне ОС в терминале | FR-4.2 сознательно ограничен верхним уровнем (стоимость полного обхода дерева) | Инструментные проверки (FR-3) покрывают вложенные пути полностью |
| Скрытие через переименование: `mv ordinary/.env .env` в терминале | read-only bind не запрещает создание записи в родителе | Вне объёма; `move_path`-инструмент перехватывается FR-3.2.3 |
| `Unrestricted`-профиль | Не-цель №2 | См. Q1 |

---

## 9. Открытые вопросы

| # | Вопрос | Дефолт при отсутствии решения |
|---|---|---|
| Q1 | Должен ли `Unrestricted` обходить `Hidden`-защиту так же, как обходят её существующие чувствительные пути, или защищать строже? | Сохранить текущую семантику чувствительных путей |
| Q2 | Нужен ли «мягкий» список общеупотребительных dotfiles (`.gitignore`, `.dockerignore`), промпт для которых упрощён? | Нет — промпт для всех скрытых путей единый |
| Q3 | Расширять ли FR-4.2 на вложенные уровни при малой стоимости (snapshot уже в памяти)? | Только верхний уровень (P2 — пересмотреть по метрикам) |

---

## 10. Якоря в коде (проверено на момент рефакторинга)

| Символ | Файл |
|---|---|
| `SensitiveSettingsKind` (Local/Global/AgentSkills), `sensitive_settings_kind`, `canonicalize_with_ancestors`, `authorize_file_edit`, `is_path_in_profile_write_scope`, `check_profile_write_scope`, `component_matches_ignore_ascii_case` | `crates/agent/src/tools/tool_permissions.rs` |
| `EditSessionContext::authorize` (вызывает `authorize_file_edit` для `edit_file`/`write_file`) | `crates/agent/src/tools/edit_session.rs` |
| `check_profile_write_scope` в `run()` инструментов | `crates/agent/src/tools/{delete_path,create_directory,copy_path,move_path}_tool.rs` |
| `sandbox_git_dirs`, `ThreadSandbox::with_protected_paths` | `crates/agent/src/sandboxing.rs`; вызов — `crates/agent/src/thread.rs` |
| `SandboxWrap::protected_paths`, `to_policy` (best-effort capture) | `crates/acp_thread/src/terminal.rs` |
| `--ro-bind` protected paths / `(deny file-write* (subpath ...))` | `crates/sandbox/src/linux_bubblewrap.rs`, `crates/sandbox/src/macos_seatbelt.rs` |
| Схема `write_scopes` (`ToolRulesContent`) | `crates/settings_content/src/agent.rs`; компиляция — `crates/agent_settings/src/agent_settings.rs` (`ToolRules`) |
