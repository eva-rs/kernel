# eva-sdk

SDK модулей Евы: подключение к ядру, ожидание его на старте, реконнект с `hello.resume` и обработчики вызовов ядро → модуль (тулы, хуки, события, запасной вход провайдера). Клиент ядра `Kernel` — та же `Session`, что и внутри ядра, и ручка переживает реконнект.

## Место в слоях

Зависит от `eva-rpc`, `eva-core`, `eva-protocol` и фреймворка [eva](https://git.desu.church/VienDesu/eva) (`cli`, `tokio`). Не зависит от `eva-kernel` и провайдеров: модуль видит ядро только через протокол.

## Точки входа

| Путь | Что |
|---|---|
| `Module::new()` | билдер: `version`, `about`, `ask`, `group`, `tool` (+ `description`, `ends_turn`, `master_only`, `trusted_only`, `needs`, `parallel_safe`, `idempotent`, `timeout`), `on_event`, `hook`, `hooks_timeout`, `hooks_fail_mode`, `provider` |
| `Module::connect_config(&ModuleConfig)` / `connect(url, token)` | ждёт ядро до `connect_wait`, отказ токена или версии — сразу ошибка |
| `Module::handshake(conn, token)` | поверх готового `Conn` (тесты), без реконнекта |
| `Module::capabilities()` | что уйдёт в `hello` |
| `Connected` | `kernel()`, `module()` (id из `welcome`), `welcome()`, `caller()`, `run(shutdown)` |
| `Kernel` | `Session<LiveSession>`: все неймспейсы, вызов идёт через текущее соединение |
| `ModuleConfig` | секция `module`; `ModuleConfig::register(app)` для `eva::cli::App` |
| `ToolCtx`, `ToolError` | контекст вызова тула (`kernel`, `turn`, `call_id`, `progress`); `respond`/`fatal` |
| `prelude` | шаги, трейты неймспейсов, `Kernel`, `Module`, `Connected` |

Секция `module`:

| Ключ | По умолчанию | Что |
|---|---|---|
| `kernel` | — | `uds:///run/eva-kernel/modules.sock`, `ws://host:port/v1/modules`, `wss://…` |
| `token` | — | `{file: /путь}` или `{plaintext: "…"}`; хвостовой перевод строки срезается |
| `connect_wait` | `180s` | сколько ждать ядро на старте |
| `tls.extra_ca` | — | PEM CA ядра с самоподписанным сертификатом |
| ключи `eva_rpc::Config` | как у ядра | плоско в той же секции: `request_timeout`, `ping_interval`, … |

Модель идентичности:

- Имени в `hello` нет. Id модуля — ключ записи его токена у ядра (`modules.tokens.<id>`), приходит в `welcome` (`Connected::module`).
- Surface модуль создаёт сам: `kernel().surfaces().create()`. Вызов идемпотентен для создателя — безопасно делать на каждом старте; `name`/`rules` меняет `surfaces.update`. Ядро помнит создателя и выводит из его живой сессии, кто surface обслуживает, поэтому после реконнекта регистрировать нечего.
- `ask` — capability модуля: он рисует кнопки pending-запросов в чатах surface, которые обслуживает.

## Что работает сейчас

В M1 работают подключение и ожидание ядра, реконнект с `hello.resume` (пауза `1s..60s`), события (ядро рассылает `chat.*` и `module.*`) и запасной вход `.provider()`. Билдеры `.tool()` и `.hook()` уже объявляют capability и регистрируют обработчики, но ядро их пока только хранит: хуки оно зовёт с M2, тулы — с M4. Повтор `turns.attach` после реконнекта — M2.

## Пример

```rust
use eva_sdk::{Module, ModuleConfig, prelude::*, protocol::requests::surfaces};

// cfg: ModuleConfig из секции `module`
let connected = Module::new()
    .version(env!("CARGO_PKG_VERSION"))
    .ask(true)
    .connect_config(&cfg)
    .await?;
log::info!(module:% = connected.module(); "connected");

connected
    .kernel()
    .surfaces()
    .create()
    .call(surfaces::create::Args { id: "bridge".into(), name: "Bridge".into(), rules: None })
    .await?;
connected.run(shutdown).await
```

`run` живёт до `shutdown` и завершается ошибкой, если ядро отвергло токен или версию при реконнекте либо сессию вытеснила другая сессия того же модуля.

## Тесты

```sh
nix develop -c cargo test -p eva-sdk
```

`tests/it/` гоняет SDK против фейкового ядра на `eva-rpc` (`fake.rs`): билдер → capability (`builder.rs`), разбор секции `module` (`config.rs`), обработчики (`dispatch.rs`), реконнект и `resume` (`reconnect.rs`). Контракт с настоящим ядром — `services/kernel/tests/contract.rs`.

## Документация

[SDK модулей](../docs/src/sdk.md), [Протокол модулей](../docs/src/protocol.md).
