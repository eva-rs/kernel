# eva-rpc

msgpack-RPC Евы: кадры поверх unix-сокета, WebSocket и WebSocket поверх TLS; симметричный мультиплексированный пир с потоками, загрузками, отменой и ping; серверный диспетчер операций библиотеки и клиентская `RpcSession`, реализующая те же трейты неймспейсов, что и ядро. Обратное направление ядро → модуль (`tools.call`, хуки, события, `provider.*`) — тоже здесь.

## Место в слоях

Зависит от `eva-protocol`, `eva-core`, фреймворка [eva](https://git.desu.church/VienDesu/eva), `tokio`, `tokio-tungstenite` и `rustls`. Не зависит от `eva-kernel`, `eva-sdk` и провайдеров. Авторизации здесь нет: сервер лишь выводит `Caller` из `welcome` (`kind` и `module` токена), права проверяет реализация `SessionMaker`.

Слои снизу вверх: `transport` (`Conn`, сырые кадры) → `peer` (вызовы, потоки, загрузки, отмены, ping) → `server`/`client` (handshake и операции `<namespace>.<op>`) → `module` (ядро → модуль).

## Точки входа

| Путь | Что |
|---|---|
| `Server::new(maker, auth, cfg)` | сервер над любым `SessionMaker`; `accept(conn)`, `serve(listener, shutdown, on_session)` |
| `Authenticator` | проверка `hello` ядром: токен → `Welcome` или отказ |
| `Accepted` | сессия после `welcome`: `peer`, `hello`, `welcome`, `caller`, `module()` → `ModuleClient` |
| `RpcClient::connect(url, hello)` | подключение модуля → `(RpcClient, Welcome)`; `session()` → `Session<RpcSession>` |
| `ClientOptions` | `Config`, `ClientTls`, счётчик id для `hello.resume` |
| `ModuleClient` / `ModuleProvider` | сторона ядра: вызовы в модуль; провайдер модуля как `impl Provider` |
| `ModuleHandler`, `NoHandler` | сторона модуля: обработчики `tools.call` и хуков, по умолчанию `unknown_method` |
| `Listener`, `Conn` | слушатель из `config::Listen` и соединение; `Conn::pair` — для тестов |
| `config::{Config, Listen, ClientTls}` | тайминги и лимиты; `{uds, mode, group}` \| `{ws}` \| `{wss, tls: {cert, key}}`; `extra_ca` клиента |
| `install_crypto_provider()` | ring провайдером rustls процесса; бинарь зовёт на старте |

Адрес подключения: `uds:///path/to/modules.sock` (или `unix://`), `ws://host:port/v1/modules`, `wss://host:port/v1/modules`.

`Config` одинаков у ядра (плоско в секции `modules`) и у модуля (плоско в секции `module`):

| Ключ | По умолчанию | Что |
|---|---|---|
| `handshake_timeout` | `10s` | `hello` обязан прийти за это время |
| `ping_interval` / `ping_timeout` | `15s` / `45s` | период ping и heartbeat; тишина дольше — закрытие |
| `request_timeout` | `60s` | ответ на запрос и первый кадр потока |
| `stream_idle_timeout` | `120s` | поток без кадров дольше — `cancel` и `timeout` |
| `stream_buffer` | `64` | буфер получателя на поток (1..=1024) |
| `max_inflight` / `max_streams` | `64` / `256` | одновременных запросов и потоков сессии; сверх — `quota` |
| `max_frame` / `max_hello` | `16 MiB` / `4 MiB` | потолок кадра; кадра `hello` |
| `blob_chunk` | `1 MiB` | потолок чанка `blobs.put` |
| `idempotency_window` | `600s` | сколько помнится ответ мутации по `(session_id, request.id)` |
| `proto_min_major` | текущий MAJOR | наименьший принимаемый MAJOR `hello.proto_version` |

## Пример

По `tests/it/loopback.rs`:

```rust
use eva_core::prelude::*;
use eva_protocol::{requests::surfaces::create, wire::{Capabilities, Hello, handshake::ProtoVersion}};
use eva_rpc::RpcClient;

let hello = Hello {
    version: "0.1.0".into(),
    proto_version: ProtoVersion::CURRENT,
    token: token.into(),
    capabilities: Capabilities::default(),
    resume: None,
};
let (client, welcome) = RpcClient::connect(&url, hello).await?;
let mut session = client.session();
let create::Ok { surface } = session
    .surfaces()
    .create()
    .call(create::Args { id: "x".into(), name: "X".into(), rules: None })
    .await?;
```

Модулю обычно удобнее `eva-sdk`: он добавляет ожидание ядра, реконнект с `hello.resume` и обработчики.

## Тесты

```sh
nix develop -c cargo test -p eva-rpc
```

`tests/it/` поднимает сервер над тестовым ядром (`kernel.rs`: несколько операций по-настоящему, остальное — заглушки) на uds и ws (`harness.rs`); каждый тест `loopback.rs` гоняется по обоим транспортам. Остальное: handshake, ping и идемпотентность сырыми кадрами (`protocol.rs`), пределы против чужого пира (`limits.rs`), RPC-метод у каждой операции (`methods.rs`), каждый `Err` каждой операции на проводе (`errors.rs`), ядро → модуль (`reverse.rs`), форма секции (`config.rs`), wss с тестовым CA из `tests/fixtures` (`tls.rs`).

## Документация

[Протокол модулей](../docs/src/protocol.md), [Безопасность](../docs/src/security.md).
