# eva-core

Библиотечное API Евы: `Session` с неймспейсом на домен и шагом на операцию, `Caller` — кто зовёт, и трейт `Provider` для инференса. Soul реализует эти трейты над своей БД, `eva-rpc` — поверх сети, поэтому код модуля и код внутри soul вызывают операции одинаково.

## Место в слоях

Зависит только от `eva-protocol`, `eva-soul-macros` (список операций), фреймворка [eva](https://github.com/nerodono/eva) и трейтов `futures`. Без рантайма, транспорта, SQL и конкретных провайдеров: не зависит от `eva-rpc`, `eva-sdk`, `providers/*` и `eva-soul` — они зависят от него.

## Точки входа

| Путь | Что |
|---|---|
| `Session<S>` | неймспейсы (`session.surfaces()`, `.chats()`, …) плюс `caller()` |
| `SessionMaker` | выдаёт `Session` для `Caller`: soul, RPC-клиент, тестовый мок |
| `CallStep` | шаг операции (`eva::handling::CallStep`): `call(args)` → `Response<Ok, Err>`; у потока `Ok` — `Items`, у загрузки `Args` несёт `body: ByteStream` |
| `service::<ns>::<Ns>` | трейт неймспейса из списка операций |
| `for_each_op!` | X-макро списка операций: по нему собраны RPC-диспетчер, клиент и SDK |
| `NAMESPACES` | все операции: RPC-метод — `<namespace>.<op>` |
| `OpKind` | `Read`, `Mutating` (окно идемпотентности), `Stream`, `Upload` |
| `Caller` | `Soul`, `Client { module }`, `Service { module }`; `from_kind(kind, module)`, `kind()`, `module()` |
| `Provider`, `GenSink` | трейт провайдера (`decl`, `models`, `generate`, `embed`, `count_tokens`) и приёмник потока `GenItem` |
| `prelude` | `CallStep` и все трейты неймспейсов одним `use` |

Неймспейсы: `surfaces`, `folders`, `chats`, `turns`, `persons`, `memory`, `notes`, `tools`, `models`, `cron`, `sleep`, `agents`, `stats`, `modules`, `hands_grants`, `blobs`, `inference`, `fs`, `sqlite`, `kv`.

`Caller` — единственная ось авторизации библиотеки. `Client` действует от лица хозяина; `Service` работает только с surface, которые создал сам. Проверяет права реализация (`eva-soul`), не транспорт.

## Пример

```rust
use eva_core::prelude::*;
use eva_protocol::requests::Surfaces;

let mut session = soul.make_session(Caller::Soul).await?;
let Surfaces::get::Ok { surface } = session
    .surfaces()
    .get("local".into())
    .await?;
```

Тот же вызов работает на `Session<RpcSession>` из `eva-rpc` и на `eva_sdk::Soul`.

## Тесты

```sh
nix develop -c cargo test -p eva-core
```

`tests/namespaces.rs`: RPC-метод raw-операции идёт без `r#`, glob-импорт `prelude` открывает методы неймспейсов у `Session`.

## Документация

[Библиотечное API](../docs/src/api.md), [Архитектура](../docs/src/architecture.md).
