# SDK модулей

`eva-sdk` — модуль ядра на Rust поверх `eva-rpc`: handshake, реконнект с продолжением сессии, обработчики вызовов ядра (тулы, хуки, события, провайдер) и клиент ядра `Kernel`, который реализует то же [библиотечное API](api.md), что и само ядро. Конфиг и запуск процесса — на фреймворке [`eva`](https://git.desu.church/VienDesu/eva) (`eva::cli::App`). Провод, который SDK прячет, описан в [Протоколе модулей](protocol.md).

## Минимальный модуль

Модуль ниже создаёт свою surface, считает запуски в KV, слушает создание чатов своей surface и объявляет тул. Это `sdk/examples/echo.rs` — он собирается вместе с крейтом:

```sh
cargo build -p eva-sdk --examples
cargo run -p eva-sdk --example echo -- --config echo.yaml
```

```rust
{{#include ../../sdk/examples/echo.rs}}
```

Токен модуля выдаёт хозяин: запись `modules.tokens.<id>` в конфиге ядра (ключ записи и есть id модуля) или операция `modules.token_issue`. Id модулю знать не нужно — он приходит в `welcome` и доступен как `connected.module()`.

## Конфигурация

`ModuleConfig::register(app)` добавляет обязательную секцию `module`; свои секции модуль регистрирует рядом в том же `App`. Загрузка — как у ядра: `--config` (YAML, JSON, TOML), переменные окружения с префиксом, который задал модуль, вложенность через `__`.

| Ключ | Тип | По умолчанию | Смысл |
|---|---|---|---|
| `kernel` | URL | — (обязателен) | `uds:///run/eva-kernel/modules.sock`, `ws://host:port/v1/modules`, `wss://…`; пустой путь ws дополняется до `/v1/modules` |
| `token` | секрет | — (обязателен) | `{plaintext: "…"}` или `{file: "/path"}`; читается один раз, хвостовой перевод строки отбрасывается |
| `connect_wait` | длительность | `3m` | сколько ждать ядро при первом подключении |
| `tls.extra_ca` | путь | нет | PEM CA ядра с самоподписанным сертификатом: задан — доверие только ему, иначе публичные корни webpki |
| `handshake_timeout`, `ping_interval`, `ping_timeout`, `request_timeout`, `stream_idle_timeout`, `stream_buffer`, `max_inflight`, `max_streams`, `max_frame`, `max_hello`, `blob_chunk`, `idempotency_window`, `proto_min_major` | | как у ядра | транспорт — те же ключи и умолчания, что в секции `modules` ядра ([Конфигурация](config.md#транспорт)), плоско в секции `module` |

```yaml
module:
  kernel: wss://kernel.example:7443/v1/modules
  token: { file: /run/credentials/x-bridge.service/token }
  tls: { extra_ca: /etc/x-bridge/kernel-ca.pem }
```

Токен держите файлом вне Nix store: под systemd — через `LoadCredential`, как это делает NixOS-модуль ядра ([Безопасность](security.md#токены)).

## Билдер `Module`

`Module::new()` — модуль без capabilities. Имени у модуля нет: идентичность даёт токен.

| Метод | Что задаёт |
|---|---|
| `version(v)` | Версия для `modules.list`; по умолчанию `0.0.0` |
| `about(text)` | Описание для логов модуля; в `hello` не уходит |
| `ask(bool)` | Capability `ask`: модуль рисует кнопки pending-запросов в чатах своих surface (клиенту — ещё `local`) |
| `group(path, about)` | Группа тулов |
| `tool(path, handler)` | Тул; вход — `#[data]`-тип, схема из его `JsonSchema`, описание — doc-комментарий типа. Повтор пути — паника при сборке |
| `description`, `ends_turn`, `master_only`, `trusted_only`, `needs`, `parallel_safe`, `idempotent`, `timeout` | Правят последний объявленный тул; до первого `tool` — паника |
| `on_event(topic, handler)` | Подписка на топик; обработчик `Fn(Kernel, Event) -> impl Future`, его ошибка только пишется в лог |
| `hook(point, handler)` | Хук точки; тип входа (`BeforeTurn`, `BeforeTool`, `AfterTool`, `BeforeSend`) обязан совпасть с точкой |
| `hooks_timeout(d)`, `hooks_fail_mode(mode)` | Потолок ответа хука (ядро режет до `modules.hooks.timeout`) и поведение при отказе: `Open` (по умолчанию) или `Closed` |
| `provider(p)` | Запасной вход провайдера: любой `impl eva_core::Provider` |
| `options(ClientOptions)` | Транспорт, TLS и счётчик id запросов вручную |
| `connect_wait(d)` | Сколько ждать ядро на старте; по умолчанию 3 min |
| `reconnect_backoff(min, max)` | Пауза между попытками: от `min`, вдвое до `max`; по умолчанию 1 s..60 s |
| `capabilities()` | Что уйдёт в `hello` — удобно для тестов |

## Подключение

| Метод | Когда |
|---|---|
| `connect_config(&ModuleConfig)` | Обычный путь: адрес, токен, TLS и транспорт из секции `module` |
| `connect(&Url, &SecretString)` | Адрес и токен из своего конфига |
| `handshake(Conn, &SecretString)` | Готовое соединение (`Conn::pair` в тестах); без адреса `run` не переподключается |

`connect*` возвращает `Connected` после `welcome`. До этого ядро ждётся до `connect_wait`: обрыв транспорта и временный отказ (`unavailable`, `timeout`, `cancelled`, `internal`, `quota`) — повтор с паузой; отказ по существу (`unauthorized`, `proto_version`, `invalid_params`…) и нечитаемый секрет — сразу `ConnectError`.

## `Connected`

| Метод | Что даёт |
|---|---|
| `kernel()` | `Kernel` — клиент библиотечного API через текущее соединение |
| `module()` | Id модуля из `welcome` |
| `caller()` | `Caller::Client` или `Caller::Service`, который решил токен |
| `welcome()` | Последний `welcome`: `warnings`, `models_accepted`, `resumed`… |
| `client()` | Текущий `RpcClient` — сырой пир |
| `run(shutdown)` | Живёт до `shutdown`, переподключаясь |

`Connected` клонируется; `run` съедает один клон. Поведение `run`:

- **Обрыв** — реконнект с паузой `reconnect_backoff` (1 s, 2 s, 4 s… до 60 s; после успеха снова с 1 s) и `hello.resume` прошлой сессии. Счётчик id запросов продолжается, поэтому оборванную мутацию можно повторить тем же id ([Идемпотентность](protocol.md#идемпотентность)).
- **Вытеснение** другой сессией того же модуля (`conflict`) — `run` завершается ошибкой, не переподключаясь.
- **Отказ по существу** при реконнекте (отозванный токен, чужая версия протокола) — `run` завершается ошибкой.
- **`shutdown`** — модуль закрывает соединение с `unavailable` и выходит с `Ok`.
- Предупреждения `welcome.warnings` (отклонённые тулы, модели, capability) SDK пишет в лог.

## Вызов ядра

`Kernel = Session<LiveSession>`: те же неймспейсы и форма вызова, что в [Библиотечном API](api.md), и те же права, что у caller'а модуля. Ручку можно клонировать и держать всё время жизни модуля: каждый вызов берёт текущее соединение.

```rust
use eva_sdk::prelude::*;
use eva_sdk::protocol::requests::chats;

let mut kernel = connected.kernel();
let chats::list::Ok { found } = kernel
    .chats()
    .list()
    .call(chats::list::Args { surface: Some("x".into()), ..Default::default() })
    .await?;
```

Поток и загрузка зовутся тем же `.call(args)`: у `blobs.get` и `turns.attach` `Ok` — поток `Items`, у `blobs.put` тело `ByteStream` едет в `Args` рядом с `Meta` ([Форма вызова](api.md#форма-вызова)). SDK шлёт `Meta` в `params` запроса, а тело — чанками потока.

Если соединение оборвалось, а `run` жив:

| Вызов | Поведение |
|---|---|
| Новый | Ждёт реконнекта до 30 s; не дождался — `Aux::Unavailable` с причиной обрыва |
| Чтение, открытие потока в полёте | Повторяется на новом соединении |
| Мутация в полёте | Повторяется тем же id запроса, если ядро продолжило окно (`welcome.resumed`); иначе исход неизвестен — `Aux::Unavailable` |
| Загрузка в полёте | `Aux::Unavailable`: тело уже прочитано |
| Уже открытый поток | Последний элемент — `Aux::Unavailable`; переоткрыть — дело модуля |

## Surface

Surface — лицо модуля, в котором живут его чаты. Модуль создаёт её сам:

```rust
kernel.surfaces().create().call(surfaces::create::Args {
    id: "x".into(),                 // slug [a-z][a-z0-9_]{0,31}
    name: "Messenger X".into(),
    rules: Some(RULES.into()),       // prompt-блок канала
}).await?;
```

- Ядро записывает создателя в `surface.module`. Для него `create` идемпотентен: свой `id` возвращается как есть, поэтому вызов безопасен на каждом старте. Чужой `id` — `Taken`, `local`/`system` — `forbidden`.
- Имя и правила потом меняет `surfaces.update`; trust и модель — только хозяин.
- Обслуживание surface ядро выводит из живой сессии создателя: после реконнекта регистрировать ничего не нужно.
- Сервис видит только чаты, личности, blob'ы и события своих surface ([Безопасность](security.md)).

## Песочница: `fs`, `sqlite`, `kv`

Скоуп — id модуля из `welcome`; чужое хранилище не адресуется. Лимиты — в [Безопасности](security.md#песочница-модуля).

```rust
use eva_sdk::protocol::{
    requests::{fs, kv, sqlite::{self, Statement}},
    types::{Bytes, Value},
};

kernel.fs().write().call(fs::write::Args {
    path: "cache/state.json".into(),
    data: Bytes(serde_json::to_vec(&state)?),
    create_dirs: true,
}).await?;

kernel.sqlite().exec().call(sqlite::exec::Args {
    statement: Statement {
        sql: "INSERT INTO seen (external_id) VALUES (?1)".into(),
        params: vec![Value::String(external_id)],
    },
}).await?;

let kv::get::Ok { value } = kernel.kv().get().call(kv::get::Args { key: "cursor".into() }).await?;
```

`sqlite.query` отдаёт `columns` и `rows` ячеек `Cell` (`Null`, `Integer`, `Real`, `Text`, `Blob`); несколько операторов атомарно — `sqlite.tx`.

## Тулы

```rust
/// Send a message to the chat.
#[data]
pub struct SendInput {
    /// External chat id.
    pub chat: String,
    pub text: String,
}

async fn send(ctx: ToolCtx, input: SendInput) -> Result<ToolResult, ToolError> {
    ctx.progress("sending").await;
    let id = bridge.send(&input.chat, &input.text).await?; // ошибка → is_error модели
    Ok(ToolResult::ok(vec![ToolContent::text(format!("sent {id}"))]))
}

Module::new()
    .group("x", "Messenger X actions")
    .tool("x.send", send)
    .ends_turn(true);
```

- Путь — `group.sub.tool`, каноничное имя для модели — через `_` (`x_send`). Схема входа — draft 2020-12, вложенные схемы раскрыты, без `title`.
- Числа и булевы, которые модель прислала строкой, приводятся к типу схемы; неразобранный вход уходит модели как `is_error` («invalid input: …»), чтобы она исправила аргументы.
- `ToolCtx`: `kernel()`, `turn()` — контекст тура, `call_id()`, `progress(text)`.
- `ToolError`: любая `Display`-ошибка через `?` — `respond`, текст уходит модели, тур продолжается; `ToolError::fatal(reason)` роняет тур.
- Повтор `tools.call` с тем же `call_id` после реконнекта SDK не исполняет второй раз, а дожидается первого исполнения.

## Хуки

```rust
Module::new().hook(HookPoint::BeforeSend, |_kernel, input: BeforeSend| async move {
    if input.text.contains("secret") {
        Decision::Cancel { reason: "leak".into() }
    } else {
        Decision::Continue
    }
});
```

Решения: `Continue`, `Cancel { reason }`, `Patch { payload }` — патч поля точки (`content`, `input`, `result`, `text`), он идёт следующему хуку цепочки.

## Что работает в M1

| Возможность | M1 |
|---|---|
| Подключение, ожидание ядра, реконнект с `resume` | работает |
| Вызов операций ядра | работает; операции поздних этапов отвечают `unavailable` с `M<N>_PENDING` ([Дорожная карта](roadmap.md)) |
| Surface, `fs`/`sqlite`/`kv`, blob'ы, `inference.complete` | работает |
| События (`on_event`) | работает: `chat.created`, `chat.deleted`, `chat.restored`, `chat.purged`, `memory.changed`, `module.connected`, `module.disconnected` |
| Провайдер (`provider`) | работает, если модуль назван в `llm.providers` с `kind: module` ([Провайдеры](providers.md#kind-module)) |
| Тулы (`tool`) | объявляются и проверяются на handshake; ядро зовёт их с M4 |
| Хуки (`hook`) | объявляются; ядро зовёт их с M2 |
| `ask` | объявляется; кнопки pending-запросов — с M5 |
| Переподписка на ленты открытых туров после реконнекта | M2 |
