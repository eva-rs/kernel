# eva-rpc

msgpack-RPC Евы: кадры поверх unix-сокета, WebSocket и WebSocket поверх TLS у клиентов, поверх Noise `NNpsk0` на uds и tcp у сервисов; симметричный мультиплексированный пир с потоками, загрузками, отменой и ping; серверный диспетчер операций библиотеки и клиентская `RpcSession`, реализующая те же трейты неймспейсов, что и ядро. Обратное направление ядро → модуль (`tools.call`, хуки, события, `provider.*`) — тоже здесь.

## Место в слоях

Зависит от `eva-protocol`, `eva-core`, фреймворка [eva](https://git.desu.church/VienDesu/eva), `tokio`, `tokio-tungstenite`, `rustls` и `snow` с `hkdf`/`sha2`. Не зависит от `eva-kernel`, `eva-sdk`, `eva-hands` и провайдеров. Авторизации здесь нет: сервер лишь выводит `Caller` из `welcome` (`kind` и `module`, которые `Authenticator` решил по токену клиента или по записи набранного сервиса), права проверяет реализация `SessionMaker`.

Слои снизу вверх: `transport` (`Conn`, сырые кадры; `noise` и `service` — сервисное соединение) → `peer` (вызовы, потоки, загрузки, отмены, ping) → `server`/`client` (handshake и операции `<namespace>.<op>`) → `module` (ядро → модуль).

Кто набрал соединение, ролей не меняет: `hello` всегда шлёт модуль, `welcome` — ядро. Клиент звонит ядру (`RpcClient::connect` → `Server::serve`/`accept`), ядро набирает сервис (`Server::dial` → `ServiceListener::accept` → `Incoming::secure` → `RpcClient::answer`).

Таблица диспетчера (`server/dispatch.rs`) и `RpcSession` (`client.rs`) — колбэки над `eva_core::for_each_op!`: RPC-метод, форма кадров и окно идемпотентности берутся из флага операции (`mutating`, `stream`, `upload`), своего списка операций здесь нет.

## Точки входа

| Путь | Что |
|---|---|
| `Server::new(maker, auth, cfg)` | сервер над любым `SessionMaker`; клиенты — `accept(conn)`, `serve(listener, shutdown, on_session)`; сервисы — `dial(address, psk, dialed)` → `Accepted` или `DialError`, `accept_dialed(conn, dialed)` |
| `Authenticator` | проверка `hello` ядром: `Origin::Listener` (токен клиента) или `Origin::Dialed(&A::Dialed)` (запись, которую набрало ядро) → `Welcome` или отказ |
| `Accepted` | сессия после `welcome`: `peer`, `hello`, `welcome`, `caller`, `module()` → `ModuleClient` |
| `RpcClient::connect(url, hello)` | клиент звонит ядру → `(RpcClient, Welcome)`; `session()` → `Session<RpcSession>` |
| `RpcClient::answer(conn, hello, options, handler)` | сервис отвечает на соединение ядра: `hello` без токена |
| `ClientOptions` | `Config`, `ClientTls`, счётчик id для `hello.resume` |
| `ModuleClient` / `ModuleProvider` | сторона ядра: вызовы в модуль; провайдер модуля как `impl Provider` |
| `ModuleHandler`, `NoHandler` | сторона модуля: обработчики `tools.call` и хуков, по умолчанию `unknown_method` |
| `Listener`, `Conn` | слушатель клиентов из `config::Listen` и соединение; `Conn::pair` — для тестов |
| `ServiceListener`, `Incoming` | слушатель сервиса из `config::ServiceListen` (`bind` или унаследованный fd — `adopt`); `Incoming::secure` — Noise responder |
| `Psk`, `transport::dial` | PSK из `SecretString` (`Psk::read`); connect и Noise initiator к `ServiceAddress` |
| `config::{Config, Listen, ServiceListen, ClientTls}` | тайминги и лимиты; `{uds, mode, group}` \| `{ws}` \| `{wss, tls: {cert, key}}`; у сервиса `{uds, mode, group}` \| `{tcp}`; `extra_ca` клиента |
| `install_crypto_provider()` | ring провайдером rustls процесса; бинарь зовёт на старте |

Адрес клиента: `uds:///path/to/modules.sock` (или `unix://`), `ws://host:port/v1/modules`, `wss://host:port/v1/modules`. Адрес сервиса (`ServiceAddress`): `uds:///<абсолютный путь>`, `tcp://<host>:<port>`.

Сервисное соединение: Noise `Noise_NNpsk0_25519_ChaChaPoly_BLAKE2s`, ядро — initiator, модуль — responder; сообщение — `u16 BE` длина и шифротекст (≤ 65535 байт, открытого текста ≤ 65519), внутри — кадрирование uds. PSK = HKDF-SHA256(секрет без хвостового перевода строки, без salt, info `eva-module psk v1`); чужой PSK модуль рвёт без ответа. Connect, Noise, `hello` и `welcome` укладываются в один `handshake_timeout`; `hello` сервиса держит лишь `max_frame`, лимиты до `welcome` (`max_hello`, места на сеть) — только у слушателя клиентов.

`Config` одинаков у ядра (плоско в секции `modules`) и у модуля (плоско в секции `module`):

| Ключ | По умолчанию | Что |
|---|---|---|
| `handshake_timeout` | `10s` | всё до `welcome`: у клиента от accept, у сервиса от connect ядра |
| `ping_interval` / `ping_timeout` | `15s` / `45s` | период ping и heartbeat; тишина дольше — закрытие |
| `request_timeout` | `60s` | ответ на запрос и первый кадр потока |
| `stream_idle_timeout` | `120s` | поток без кадров дольше — `cancel` и `timeout` |
| `stream_buffer` | `64` | буфер получателя на поток (1..=1024) |
| `max_inflight` / `max_streams` | `64` / `256` | одновременных запросов и потоков сессии; сверх — `quota` |
| `max_frame` / `max_hello` | `16 MiB` / `4 MiB` | потолок кадра; кадра `hello` клиента |
| `blob_chunk` | `1 MiB` | потолок чанка `blobs.put` |
| `idempotency_window` | `600s` | сколько помнится ответ мутации по `(session_id, request.id)` |
| `proto_min_major` | текущий MAJOR | наименьший принимаемый MAJOR `hello.proto_version` |

## Пример

Клиент, по `tests/it/loopback.rs`:

```rust
use eva_core::prelude::*;
use eva_protocol::{requests::surfaces::create, wire::{Capabilities, Hello, handshake::ProtoVersion}};
use eva_rpc::RpcClient;

let hello = Hello {
    version: "0.1.0".into(),
    proto_version: ProtoVersion::CURRENT,
    token: Some(token.into()),
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

Сервис, по `tests/it/service.rs`:

```rust
let listener = ServiceListener::bind(&listen, Psk::read(&key)?, &config).await?;
let conn = listener.accept().await?.secure().await?;
let (client, welcome) = RpcClient::answer(conn, Hello { token: None, ..hello }, options, handler).await?;
```

Модулю обычно удобнее `eva-sdk`: он добавляет ожидание ядра, реконнект с `hello.resume` и обработчики.

## Тесты

```sh
nix develop -c cargo test -p eva-rpc
```

`tests/it/` поднимает сервер над тестовым ядром (`kernel.rs`: несколько операций по-настоящему, остальное — заглушки из `for_each_op!`) на uds и ws и набирает им сервисы на uds и tcp (`harness.rs`); каждый тест `loopback.rs` гоняется по обоим клиентским транспортам. Остальное: сервисное соединение под Noise (`service.rs`; сам Noise и PSK — юнит-тесты `transport/noise.rs`), handshake, ping и идемпотентность сырыми кадрами (`protocol.rs`), пределы против чужого пира (`limits.rs`), RPC-метод у каждой операции (`methods.rs`), каждый `Err` каждой операции на проводе (`errors.rs`), ядро → модуль у набранного сервиса (`reverse.rs`), форма секции (`config.rs`), wss с тестовым CA из `tests/fixtures` (`tls.rs`).

## Документация

[Протокол модулей](../docs/src/protocol.md), [Безопасность](../docs/src/security.md).
