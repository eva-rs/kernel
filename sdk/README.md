# eva-sdk

SDK модулей Евы: сервис слушает и принимает ядро (Noise с общим ключом), клиент хозяина звонит ядру сам с токеном; обработчики вызовов ядро → модуль (тулы, хуки, события, запасной вход провайдера). Клиент ядра `Kernel` — та же `Session`, что и внутри ядра, и ручка переживает смену соединения.

## Место в слоях

Зависит от `eva-rpc`, `eva-core`, `eva-protocol` и фреймворка [eva](https://git.desu.church/VienDesu/eva) (`cli`, `tokio`). Не зависит от `eva-kernel`, `eva-hands` и провайдеров: модуль видит ядро только через протокол.

## Точки входа

| Путь | Что |
|---|---|
| `Module::new()` | билдер: `version`, `about`, `ask`, `group`, `tool` (+ `description`, `ends_turn`, `master_only`, `trusted_only`, `needs`, `parallel_safe`, `idempotent`, `timeout`), `on_event`, `hook`, `hooks_timeout`, `hooks_fail_mode`, `provider` |
| `Module::listen(&ServiceConfig)` | сервис: bind или сокет от `LISTEN_FDS`, приём ядра в фоне → `Listening` |
| `Listening` | `kernel().await` (ждёт первого `welcome` до `kernel_wait`), `module()`, `welcome()`, `client()`, `address()`, `run(shutdown)`; новое соединение ядра вытесняет прежнее |
| `Module::connect_config(&ClientConfig)` / `connect(url, token)` | клиент: ждёт ядро до `connect_wait`, отказ токена или версии — сразу ошибка → `Connected` |
| `Module::handshake(conn, token)` | клиент поверх готового `Conn` (тесты), без реконнекта |
| `Module::capabilities()` | что уйдёт в `hello` |
| `Connected` | `kernel()`, `module()` (id из `welcome`), `welcome()`, `caller()`, `run(shutdown)` с реконнектом |
| `Kernel` | `Session<LiveSession>`: все неймспейсы (собраны по `eva_core::for_each_op!`), вызов идёт через текущее соединение; поток и загрузка — тот же `call` |
| `ServiceConfig`, `ClientConfig` | секция `module`; `register(app)` для `eva::cli::App` |
| `ToolCtx`, `ToolError` | контекст вызова тула (`kernel`, `turn`, `call_id`, `progress`); `respond`/`fatal` |
| `prelude` | `CallStep`, трейты неймспейсов, `Kernel`, `Module`, `Listening`, `Connected`, `ToolCtx`, `ToolError` |

Секция `module` сервиса:

| Ключ | По умолчанию | Что |
|---|---|---|
| `listen` | — | `{uds: /run/eva-<id>/module.sock, mode: 0o660, group?}` или `{tcp: "127.0.0.1:7001"}` |
| `key` | — | `{file: /путь}` или `{plaintext: "…"}`; тот же секрет, что `key` записи `modules.services.<id>` у ядра |
| `kernel_wait` | `180s` | сколько `kernel()` ждёт первого соединения ядра |
| ключи `eva_rpc::Config` | как у ядра | плоско в той же секции; `max_hello` сервис не проверяет |

Секция `module` клиента:

| Ключ | По умолчанию | Что |
|---|---|---|
| `kernel` | — | `uds:///run/eva-kernel/modules.sock`, `ws://host:port/v1/modules`, `wss://…` |
| `token` | — | `{file: /путь}` или `{plaintext: "…"}`; хвостовой перевод строки срезается |
| `connect_wait` | `180s` | сколько ждать ядро на старте |
| `tls.extra_ca` | — | PEM CA ядра с самоподписанным сертификатом |
| ключи `eva_rpc::Config` | как у ядра | плоско в той же секции: `request_timeout`, `ping_interval`, … |

Модель идентичности:

- Имени в `hello` нет. Id сервиса — ключ записи реестра, которую набрало ядро; id клиента — ключ записи его токена (`modules.tokens.<id>`). Приходит в `welcome` (`Listening::module`, `Connected::module`).
- Surface модуль создаёт сам: `kernel().surfaces().create()`. Вызов идемпотентен для создателя — безопасно делать на каждом старте; `name`/`rules` меняет `surfaces.update`. Ядро помнит создателя и выводит из его живой сессии, кто surface обслуживает, поэтому после смены соединения регистрировать нечего.
- `ask` — capability модуля: он рисует кнопки pending-запросов в чатах surface, которые обслуживает.

## Что работает сейчас

В M1 работают приём ядра сервисом (uds и tcp под Noise, вытеснение прежнего соединения, socket activation), подключение клиента с ожиданием ядра и реконнектом (пауза `1s..60s`), `hello.resume`, события (ядро рассылает `chat.*` и `module.*`) и запасной вход `.provider()`. Билдеры `.tool()` и `.hook()` уже объявляют capability и регистрируют обработчики, но ядро их пока только хранит: хуки оно зовёт с M2, тулы — с M4. Повтор `turns.attach` после смены соединения — M2.

## Пример

```rust
use eva_sdk::{Module, ServiceConfig, prelude::*, protocol::requests::surfaces};

// cfg: ServiceConfig из секции `module`
let listening = Module::new()
    .version(env!("CARGO_PKG_VERSION"))
    .ask(true)
    .listen(&cfg)
    .await?;

let mut kernel = listening.kernel().await?; // первый дозвон ядра
log::info!(module:? = listening.module(); "kernel connected");
kernel
    .surfaces()
    .create()
    .call(surfaces::create::Args { id: "bridge".into(), name: "Bridge".into(), rules: None })
    .await?;
listening.run(shutdown).await
```

`Listening::run` живёт до `shutdown`: потеря ядра его не завершает, ядро перезвонит само. `Connected::run` клиента завершается ошибкой, если ядро отвергло токен или версию при реконнекте либо сессию вытеснила другая сессия того же модуля.

## Тесты

```sh
nix develop -c cargo test -p eva-sdk
```

`tests/it/` гоняет SDK против фейкового ядра на `eva-rpc` (`fake.rs`): билдер → capability (`builder.rs`), разбор секции `module` (`config.rs`), обработчики (`dispatch.rs`), приём ядра сервисом, вытеснение и Noise (`listen.rs`), реконнект клиента и `resume` (`reconnect.rs`). Контракт с настоящим ядром — `services/kernel/tests/contract.rs`.

## Документация

[SDK модулей](../docs/src/sdk.md), [Протокол модулей](../docs/src/protocol.md).
