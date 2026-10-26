# eva-sdk

Высокоуровневый SDK модулей Eva: группы с inline-обработчиками, типизированное состояние, конфиг и локализованный прогресс. Сервис слушает, soul звонит ему под Noise; клиент хозяина подключается сам с токеном. `Soul` предоставляет библиотечное API soul и переживает смену соединения.

```rust
use eva::data;
use eva_sdk::{Group, Tool, ToolCtx, ToolError, module};

#[data]
struct Say {
    text: String,
}

fn echo() -> Group {
    Group::new("echo").about("Echo tools").tool(
        Tool::new("say", async |_: ToolCtx, input: Say| {
            Ok::<_, ToolError>(input.text)
        })
        .description("Repeat the given text"),
    )
}

fn main() -> eyre::Result<()> {
    module!().group(echo()).run()
}
```

`module!()` берёт имя и версию из Cargo вызывающего пакета. Переопределение: `module!().version("1.4.3").name("owm")`. Имена тулов внутри группы локальны: `say` в группе `echo` получает путь `echo.say`. Вложенные группы — `Group::group`.

`init("weather", async |cfg: WeatherConfig| …)` загружает типизированный конфиг и создаёт общее состояние; `state(client)` передаёт готовое. Обработчик читает его через `ctx.state()`. Успешный результат — текст, `ToolContent` или `ToolResult`; ошибки преобразуются в `ToolError`.

Настройки тула можно задать целиком через `.describe(Description { idempotent: false, parallel_safe: false, ..Default::default() })` или отдельными сеттерами. `describe` заменяет весь набор; сеттер меняет одно поле, сохраняя остальные. По умолчанию `idempotent = parallel_safe = true`. Операции с накапливающимся эффектом, например отправку сообщения, помечайте `.idempotent(false)`; если важен порядок вызовов — `.parallel_safe(false)`. Автоматические повторы по `idempotent` soul пока не выполняет. `parallel_safe: false` сохраняет порядок внутри одного ответа модели, но не блокирует вызовы из других туров.

При `run()` секция `module` необязательна:

| Настройка | Дефолт |
|---|---|
| Имя и версия | `CARGO_PKG_NAME`, `CARGO_PKG_VERSION` |
| `listen` | `$RUNTIME_DIRECTORY/module.sock`, иначе `/run/eva-<name>/module.sock` |
| `key` | `{credential: soul-key}` — `$CREDENTIALS_DIRECTORY/soul-key` |
| `soul_wait` | `3m` |
| Транспорт | Дефолты `eva_rpc::Config` |
| Префикс окружения | Имя в верхнем регистре, `-` → `_`, затем `_` |

`run` использует `eva::cli::App` и `Supervisor`, поддерживает `--config` и SIGINT/SIGTERM. Каталоги и права задаёт окружение. Авторизованный id сервиса задаёт реестр soul, id клиента — токен; оба приходят в `welcome` независимо от имени приложения.

Для ручного запуска остаются `listen(&ServiceConfig)`, `connect_config(&ClientConfig)` и `handshake`. При таком запуске состояние создаётся заранее через `state`; `init` выполняется только в `run`. Старые вызовы с полными путями переименованы в `group_path` и `tool_path`.

Переводы подключаются через `.translations(include_translations!("locales"))`, статус отправляется как `ctx.progress(msg!("Fetching weather for {city}", city = &input.city)).await`. Поддержаны gettext PO, контекст, plural-формы и fallback. Каждый получатель выбирает свой язык через `LocalizedText::render(locale)`; soul очищает секреты также в параметрах и переводах. Для POT и обновления PO есть `eva-sdk-i18n extract` / `merge` в пакете `eva-sdk-macros`.

Полный собираемый пример: [examples/echo.rs](examples/echo.rs). API, конфиг, локализация и миграция: [глава SDK](../docs/src/sdk.md). Провод: [протокол модулей](../docs/src/protocol.md).

Проверка:

```sh
nix develop -c cargo test -p eva-sdk -p eva-sdk-macros -p eva-protocol -p eva-rpc
```

SDK зависит от `eva-rpc`, `eva-core`, `eva-protocol`, `eva-sdk-macros` и `eva`; не зависит от soul, рук или провайдеров.
