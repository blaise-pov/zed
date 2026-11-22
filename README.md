# Zed

[![Zed](https://img.shields.io/endpoint?url=https://raw.githubusercontent.com/zed-industries/zed/main/assets/badge/v0.json)](https://zed.dev)
[![CI](https://github.com/zed-industries/zed/actions/workflows/run_tests.yml/badge.svg)](https://github.com/zed-industries/zed/actions/workflows/run_tests.yml)

Zed White-box Agent Harness - в этом форке реализована архитектурная обвязка для ИИ-агентов, в которой внутренний цикл исполнения, маршрутизация задач, права доступа и управление контекстом полностью прозрачны, декларативно заданы и детерминированно контролируются системой, а не отданы на откуп самой языковой модели.

### Цели
* Обеспечить изоляцию агентов для параллельной работы и обеспечения безопасности.
* Получить чистый контекст при выполнении задачи. В контексте должен быть необходимый минимум для выполнения задачи. Никаких лишних инструкций, mcp, tools.
* Возможность выстраивать иерархию из агентов. Агент получает задание, декомпозирует его и выдает задания специализированным агентам, которые в свою очередь могут также вызвать сабагентов. Пример: Агент Оркестратор получает задание, декомпозирует его на задачи для фронтенда и бекенда, отдает задачи специализированным агентам, они в свою очередь декомпозируют полученные задачи в рамках своей архитектуры и дают задачи сабагентам которые занимаются реализацией кода в своём домене, слое.
* Проектный конфиг: задает параметры работы в рамках конкретного проекта. Настройки профилей, подключение MCP-серверов, скиллов. Настройки харнеса являются частью проекта.

---
### Основные изменения
**1. Профили агента**
Профиль агента определяет поведение и права доступа ИИ агента.
Профили задают специализированные роли для решения разных задач.
Каждый профиль настраивает:
- модель
- описание роли агента (промпт)
- доступные системные инструменты и инструменты MCP-серверов
- доступные скиллы
- правила делегирования (перечень профилей агентов которые он может использовать для запуска сабагентов)
- режим изоляции
Подробнее: [Профили агентов](./docs/harness/Профили%20агентов.md)

**2. Изоляция агентов**
Изоляция обеспечивает безопасность и независимое параллельное выполнение задач.
Она предотвращает конфликты изменений и защищает основное рабочее дерево проекта.
Механизмы изоляции включают:
- изоляцию рабочего пространства (отдельные Git worktree под задачу)
- ограничения файловой системы (области записи `write_scopes` для каждого агента)
- изоляцию контекста (чистый сеанс без шума из родительской истории и нерелевантных инструкций)
- политики безопасности терминала
- изоляция инструментов
Подробнее: [Изоляция агентов](./docs/harness/Изоляция%20агентов.md)

**3. Оркестрация агентов**
Оркестрация управляет жизненным циклом задач и координацией иерархии агентов.
Она позволяет декомпозировать сложные цели на специализированные подзадачи.
Оркестрация включает:
- управление графом задач (дерево целей, подзадачи, граф зависимостей)
- делегирование и запуск сабагентов согласно правилам профиля
- параллельное исполнение независимых веток задач
- контракты задач (четкие цели, ограничения и критерии приемки)
- контроль результатов и этапы верификации (код-ревью, проверка тестами, слияние изменений)
Подробнее: [Оркестрация агентов](./docs/harness/Оркестрация%20агентов.md)

**4. Проектный конфиг**
Проектный конфиг задает параметры работы в рамках конкретного проекта.
Конфигурация определяет:
- настройки профилей агентов
- настройки подключения MCP-серверов
Подробнее: [Проектный конфиг](./docs/harness/Проектный%20конфиг.md)

- [Список изменений](./docs/harness/Список%20изменений.md)
- [Параметры settings.json](./docs/harness/Параметры%20settings.json.md)
- [Пример рабочего процесса](./docs/harness/Описание%20примера%20рабочего%20процесса.md)

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
