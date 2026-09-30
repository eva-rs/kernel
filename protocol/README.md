# eva-protocol

Типы протокола Евы: аргументы, результаты и ошибки каждой операции, сущности домена и проводные формы обмена ядро ↔ модуль. Один и тот же модуль операции читают библиотека ядра, RPC-сервер, RPC-клиент модуля и тул модели, поэтому форма вызова у всех одна.

## Место в слоях

Нижний слой воркспейса: от него зависят все остальные крейты, включая клиентов. Сам зависит только от фреймворка [eva](https://git.desu.church/VienDesu/eva) и крейтов `serde`, `serde_json`, `serde_with`, `serde_bytes`, `futures-core` (трейт `Stream`). Никакого I/O и рантайма: без `tokio`, сети, файлов и БД; не зависит от `eva-core` и выше. Строк конкретных вендоров и поверхностей здесь нет — их место в `providers/*`.

## Точки входа

| Путь | Что |
|---|---|
| `requests/<ns>.rs` | модуль на операцию: `Args`, `Ok`, `Err`; у потока `Ok = Items<Item, Err>`, у загрузки `Args { meta, body }` и по проводу — `Meta` |
| `requests::{Patch, Limit, RunsLimit, Response}` | частичное обновление, границы страниц, `Result<O, Generic<E>>` |
| `requests::{Items, ByteStream}` | поток к вызывающему (`Response<Item, Err>` поштучно) и тело загрузки (`Result<Bytes, Aux>`); только типы |
| `errors::{Generic, Aux, HasCode}` | `Generic<E>` = `Spec(E)` \| `Aux(Aux)`: отказ операции против беды подсистемы |
| `errors/<ns>.rs` | атомарные ошибки домена; код провода — `code!(T => Code)` |
| `types/` | сущности и идентификаторы (`ids`), по файлу на домен |
| `wire::{Frame, RpcError, ErrorCode}` | кадры RPC и коды ошибок провода |
| `wire::handshake::{Hello, Welcome, ProtoVersion}` | handshake; `ProtoVersion::CURRENT` — `MAJOR.MINOR` версии крейта |
| `wire::Capabilities` | `provider`, `tools`, `events`, `hooks`, `ask` из `hello` |
| `wire::{events, hooks, tools, provider}` | направление ядро → модуль |
| `wire::method` | методы вне операций: `hello`, `ping`, `close`, `tools.call`, `events.publish`, `provider.*` |

Правила формы:

- Кодировка провода — msgpack с именами полей (`rmp_serde::to_vec_named`).
- `hello` не несёт имени модуля: клиенту идентичность даёт токен (id — ключ записи `modules.tokens` или строки `module_tokens`), сервису — запись реестра, которую набрало ядро (`modules.services` или `module_services`); id возвращает `Welcome::module`, вид — `Welcome::kind`.
- Surface — не capability, а сущность ядра: модуль создаёт её операцией `surfaces.create`.
- Enum'ы, которые принимает пир, терпят незнакомый вариант (`Unknown` или ближайший общий); незнакомые поля структур пропускаются. Аргументы запросов строгие: непонятое — `invalid_params`.
- `Patch<T>`: отсутствие поля, `null` и `"keep"` — не трогать; `{change: v}` — записать; `{change: null}` у `Patch<Option<T>>` — очистить.

## Пример

По `tests/msgpack.rs`:

```rust
use eva_protocol::{requests::{Patch, surfaces}, types::Effort};
use serde_json::json;

let update: surfaces::update::Update =
    rmp_serde::from_slice(&rmp_serde::to_vec_named(&json!({"effort": {"change": 2}}))?)?;
assert_eq!(update.effort, Patch::Change(Some(Effort::Medium)));
assert!(update.rules.is_keep());
```

## Тесты

```sh
nix develop -c cargo test -p eva-protocol
```

`tests/msgpack.rs` — roundtrip проводных типов, терпимость к пиру новее и `params` `blobs.put` как голая `Meta`; `tests/wire.rs` — roundtrip операций, `Patch`, различение `Aux`/`Spec`; `tests/errors.rs` — каждый `Aux` через msgpack и `RpcError` и его код; `tests/bytes.rs` — `Bytes` как msgpack `bin` и base64 в JSON.

## Документация

[Протокол модулей](../docs/src/protocol.md), [Библиотечное API](../docs/src/api.md).
