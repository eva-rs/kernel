# SDK модулей

`eva-sdk` — модуль ядра на Rust поверх `eva-rpc`: handshake, смена соединения с продолжением сессии, обработчики вызовов ядра (тулы, хуки, события, провайдер) и клиент ядра `Kernel`, который реализует то же [библиотечное API](api.md), что и само ядро. Конфиг и запуск процесса — на фреймворке [`eva`](https://git.desu.church/VienDesu/eva) (`eva::cli::App`). Провод, который SDK прячет, описан в [Протоколе модулей](protocol.md).

Модуль бывает двух видов, и вид задаёт, кто звонит:

| Вид | Кто | Соединение | Вход SDK |
|---|---|---|---|
| сервис | мост мессенджера, MCP, экзотический провайдер | модуль слушает, ядро набирает его по записи реестра; Noise с общим ключом | `Module::listen(&ServiceConfig)` → `Listening` |
| клиент | клиент хозяина: телефон, TUI | модуль звонит ядру сам с токеном | `Module::connect_config(&ClientConfig)` → `Connected` |

Сервис адреса ядра не знает. Клиенты хозяина за NAT и приходят-уходят, поэтому звонят сами.

## Минимальный сервис

Сервис ниже создаёт свою surface, считает запуски в KV, слушает создание чатов своей surface и объявляет тул. Это `sdk/examples/echo.rs` — он собирается вместе с крейтом:

```sh
cargo build -p eva-sdk --examples
cargo run -p eva-sdk --example echo -- --config echo.yaml
```

```rust
{{#include ../../sdk/examples/echo.rs}}
```

Сервис регистрирует хозяин — запись реестра у ядра с адресом и ключом: `modules.services.<id> = {address, key}` в конфиге ядра или операция `modules.add {module, address, key?}` (без `key` ядро сгенерирует ключ и вернёт его один раз). Ключ записи и есть id модуля; модулю его знать не нужно — он приходит в `welcome` и доступен как `listening.module()`.

## Конфигурация

`ServiceConfig::register(app)` (сервис) или `ClientConfig::register(app)` (клиент) добавляет обязательную секцию `module`; свои секции модуль регистрирует рядом в том же `App`. Загрузка — как у ядра: `--config` (YAML, JSON, TOML), переменные окружения с префиксом, который задал модуль, вложенность через `__`.

Сервис, `ServiceConfig`:

| Ключ | Тип | По умолчанию | Смысл |
|---|---|---|---|
| `listen` | слушатель | — (обязателен) | `{uds: <путь>}` или `{tcp: "<ip>:<port>"}`; при socket activation — адрес унаследованного сокета. Права файла сокета SDK не ставит — их задаёт окружение (umask процесса или socket-юнит) |
| `key` | секрет | — (обязателен) | Тот же секрет, что `key` записи реестра у ядра: `{plaintext: "…"}` или `{file: "/path"}`; хвостовой перевод строки в ключ не входит |
| `kernel_wait` | длительность | `3m` | Сколько `kernel()` ждёт первого соединения ядра |
| транспорт | | как у ядра | те же ключи, что ниже у клиента; `max_hello` сервис не проверяет — его `hello` держит лишь `max_frame` |

```yaml
module:
  listen: { uds: /run/eva-x-bridge/module.sock }
  key: { file: /run/credentials/eva-x-bridge.service/key }
```

Клиент, `ClientConfig`:

| Ключ | Тип | По умолчанию | Смысл |
|---|---|---|---|
| `kernel` | URL | — (обязателен) | `uds:///run/eva-kernel/modules.sock`, `ws://host:port/v1/modules`, `wss://…`; пустой путь ws дополняется до `/v1/modules` |
| `token` | секрет | — (обязателен) | `{plaintext: "…"}` или `{file: "/path"}`; читается один раз, хвостовой перевод строки отбрасывается |
| `connect_wait` | длительность | `3m` | сколько ждать ядро при первом подключении |
| `tls.extra_ca` | путь | нет | PEM CA ядра с самоподписанным сертификатом: задан — доверие только ему, иначе публичные корни webpki |
| `handshake_timeout`, `ping_interval`, `ping_timeout`, `request_timeout`, `stream_idle_timeout`, `stream_buffer`, `max_inflight`, `max_streams`, `max_frame`, `max_hello`, `blob_chunk`, `idempotency_window` | | как у ядра | транспорт — те же ключи и умолчания, что в секции `modules` ядра ([Конфигурация](config.md#транспорт)), плоско в секции `module` |

```yaml
module:
  kernel: wss://kernel.example:7443/v1/modules
  token: { file: /run/credentials/phone.service/token }
  tls: { extra_ca: /etc/phone/kernel-ca.pem }
```

Ключ и токен держите файлом вне Nix store: под systemd — через `LoadCredential`, как это делает NixOS-модуль ядра ([Безопасность](security.md#токены)). Ключ сервиса — секрет высокой энтропии (32+ символов): Noise `NNpsk0` с угадываемым ключом подбирается офлайн по перехваченному handshake.

## Билдер `Module`

`Module::new()` — модуль без capabilities. Имени у модуля нет: идентичность даёт запись реестра (сервис) или токен (клиент).

| Метод | Что задаёт |
|---|---|
| `version(v)` | Версия для `modules.list`; по умолчанию `0.0.0` |
| `about(text)` | Описание для логов модуля; в `hello` не уходит |
| `ask(bool)` | Capability `ask`: модуль рисует кнопки pending-запросов в чатах своих surface (клиенту — ещё `local`) |
| `group(path, about)` | Группа тулов |
| `tool(path, handler)` | Тул; вход — `#[data]`-тип, схема из его `JsonSchema`, описание — doc-комментарий типа. Повтор пути — паника при сборке |
| `description`, `ends_turn`, `master_only`, `trusted_only`, `needs`, `parallel_safe`, `idempotent`, `timeout` | Правят последний объявленный тул; до первого `tool` — паника |
| `on_event(topic, handler)` | Подписка на топик; обработчик `Fn(Kernel, Event) -> impl Future`, его ошибка только пишется в лог |
| `hook(point, handler)` | Хук точки; тип входа (`BeforeTurn`, `BeforeTool`, `AfterTool`) обязан совпасть с точкой |
| `hooks_timeout(d)`, `hooks_fail_mode(mode)` | Потолок ответа хука (ядро режет до `modules.hooks.timeout`) и поведение при отказе: `Open` (по умолчанию) или `Closed` |
| `provider(p)` | Запасной вход провайдера: любой `impl eva_core::Provider` |
| `options(ClientOptions)` | Транспорт, TLS и счётчик id запросов вручную |
| `connect_wait(d)` | Клиент: сколько ждать ядро на старте; по умолчанию 3 min |
| `reconnect_backoff(min, max)` | Клиент: пауза между попытками, от `min` вдвое до `max`; по умолчанию 1 s..60 s |
| `capabilities()` | Что уйдёт в `hello` — удобно для тестов |

## Сервис: `listen` → `Listening`

`Module::listen(&ServiceConfig)` проверяет ключ, привязывает слушатель и возвращается сразу; соединения ядра принимаются в фоне. Нечитаемый или пустой `key` — `ConnectError::Config`, занятый адрес — `ConnectError::Transport`.

| Метод | Что даёт |
|---|---|
| `kernel().await` | `Kernel` через текущее соединение; до первого ждёт его до `kernel_wait`, не дождался — `ConnectError` с причиной последней неудачной попытки ядра (например, чужой ключ) |
| `module()` | Id модуля из `welcome`; до первого соединения — `None` |
| `welcome()`, `client()` | Последний `welcome` и текущий `RpcClient`; до первого соединения — `None` |
| `address()` | Адрес, который набирает ядро: `uds:///…` или `tcp://ip:port` (порт `0` из конфига здесь уже выбран) |
| `run(shutdown)` | Живёт до `shutdown` |

`Listening` клонируется; `run` съедает один клон, а с последним брошенным клоном приём останавливается. Поведение:

- **Соединение ядра** — Noise responder, затем `hello` без токена и `welcome`; всё вместе — за один `handshake_timeout` от accept. Чужой ключ — сокет закрывается без ответа, у ядра это провал дозвона. Медленный или молчащий сокет не задерживает приём остальных; до конца Noise таких держится не больше 64 (и 8 из одной сети: IPv4-адрес или IPv6 /64), сверх лимита новичок вытесняет старейшее соединение, ещё не доказавшее ключ. Провал до Noise пишется в лог как `debug`, после — `warn`.
- **Новое соединение ядра** после своего `welcome` становится текущим, прежнее закрывается `conflict "superseded"` — так модуль переживает рестарт ядра и полуоткрытое старое соединение. `hello.resume` — `session_id` последней сессии процесса, счётчик id запросов продолжается.
- **Потеря ядра** `run` не завершает: ядро перезвонит само, по своей паузе `1s..60s`. Модуль не звонит никогда.
- **`shutdown`** — приём останавливается, файл своего uds-сокета удаляется, текущее соединение закрывается с `unavailable`, `run` выходит с `Ok`.
- Предупреждения `welcome.warnings` (отклонённые тулы, модели, capability) SDK пишет в лог.

Socket activation: если `LISTEN_PID` — pid модуля, SDK ищет среди унаследованных сокетов (`LISTEN_FDS`, с fd 3) тот, чей адрес совпадает с `listen` (путь `uds` или `ip:port`), и берёт его вместо bind. Ошибка старта: унаследованный сокет с другим адресом (его некому обслужить) или не слушающий — при `Accept=yes` менеджер отдаёт уже принятое соединение, а нужен слушатель (`Accept=no`, значение по умолчанию). Переменные `LISTEN_*` SDK не трогает: дочерний процесс отсеет чужие сокеты по `LISTEN_PID`. Файл унаследованного сокета принадлежит менеджеру служб и не удаляется.

Локальный сервис на NixOS слушает `/run/eva-<id>/module.sock`, доступный группе `eva` — ядро в этой группе. Права файла задаёт окружение, не SDK: либо юнит сервиса (`RuntimeDirectory = eva-<id>`, `Group = eva`, `UMask = 0007`), либо socket-юнит (`SocketGroup = eva`, `SocketMode = 0660`, `Accept = no`).

## Клиент: `connect` → `Connected`

| Метод | Когда |
|---|---|
| `connect_config(&ClientConfig)` | Обычный путь: адрес, токен, TLS и транспорт из секции `module` |
| `connect(&Url, &SecretString)` | Адрес и токен из своего конфига |
| `handshake(Conn, &SecretString)` | Готовое соединение (`Conn::pair` в тестах); без адреса `run` не переподключается |

Токен клиенту выдаёт хозяин: запись `modules.tokens.<id>` в конфиге ядра или операция `modules.token_issue`. `connect*` возвращает `Connected` после `welcome`. До этого ядро ждётся до `connect_wait`: обрыв транспорта и временный отказ (`unavailable`, `timeout`, `cancelled`, `internal`, `quota`) — повтор с паузой; отказ по существу (`unauthorized`, `proto_version`, `invalid_params`…) и нечитаемый секрет — сразу `ConnectError`.

| Метод `Connected` | Что даёт |
|---|---|
| `kernel()` | `Kernel` — клиент библиотечного API через текущее соединение |
| `module()` | Id модуля из `welcome` |
| `caller()` | `Caller::Client`, который решил токен |
| `welcome()` | Последний `welcome`: `warnings`, `models_accepted`, `resumed`… |
| `client()` | Текущий `RpcClient` — сырой пир |
| `run(shutdown)` | Живёт до `shutdown`, переподключаясь |

`Connected` клонируется; `run` съедает один клон. Поведение `run`:

- **Обрыв** — реконнект с паузой `reconnect_backoff` (1 s, 2 s, 4 s… до 60 s; после успеха снова с 1 s) и `hello.resume` прошлой сессии. Счётчик id запросов продолжается, поэтому оборванную мутацию можно повторить тем же id ([Идемпотентность](protocol.md#идемпотентность)).
- **Вытеснение** другой сессией того же модуля (`conflict`) — `run` завершается ошибкой, не переподключаясь.
- **Отказ по существу** при реконнекте (отозванный токен, чужая версия протокола) — `run` завершается ошибкой.
- **`shutdown`** — модуль закрывает соединение с `unavailable` и выходит с `Ok`.
- Предупреждения `welcome.warnings` SDK пишет в лог.

## Вызов ядра

`Kernel = Session<LiveSession>`: те же неймспейсы и форма вызова, что в [Библиотечном API](api.md), и те же права, что у caller'а модуля. Ручку можно клонировать и держать всё время жизни модуля: каждый вызов берёт текущее соединение — после реконнекта клиента и после нового дозвона ядра к сервису тоже.

```rust
use eva_sdk::prelude::*;
use eva_sdk::protocol::requests::chats;

let mut kernel = listening.kernel().await?; // у клиента — connected.kernel()
let chats::list::Ok { found } = kernel
    .chats()
    .list()
    .call(chats::list::Args { surface: Some("x".into()), ..Default::default() })
    .await?;
```

Поток и загрузка зовутся тем же `.call(args)`: у `blobs.get` и `turns.attach` `Ok` — поток `Items`, у `blobs.put` тело `ByteStream` едет в `Args` рядом с `Meta` ([Форма вызова](api.md#форма-вызова)). SDK шлёт `Meta` в `params` запроса, а тело — чанками потока.

Если соединение оборвалось, а новое возможно (у сервиса идёт приём, у клиента жив `run`):

| Вызов | Поведение |
|---|---|
| Новый | Ждёт нового соединения до 30 s; не дождался — `Aux::Unavailable` с причиной обрыва |
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
- Обслуживание surface ядро выводит из живой сессии создателя: после смены соединения регистрировать ничего не нужно.
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
- Повтор `tools.call` с тем же `call_id` после смены соединения (с `hello.resume`) SDK не исполняет второй раз, а дожидается первого исполнения.

## Хуки

```rust
Module::new().hook(HookPoint::BeforeTool, |_kernel, input: BeforeTool| async move {
    if input.canonical == "shell_exec" {
        Decision::Cancel { reason: "quiet hours".into() }
    } else {
        Decision::Continue
    }
});
```

Решения: `Continue`, `Cancel { reason }`, `Patch { payload }`. Патч есть только у `after_tool` (`result`), он идёт следующему хуку цепочки; у `before_turn` и `before_tool` тип патча `NoPatch` — реплика и вызов уже в логе, менять нечего.

## Возможности SDK

| Возможность | Поведение |
|---|---|
| Сервис: приём ядра под Noise, вытеснение, socket activation; клиент: подключение, ожидание ядра, реконнект; `resume` | работает |
| Вызов операций ядра | Работает через общий библиотечный API |
| Surface, `fs`/`sqlite`/`kv`, blob'ы, `inference.complete` | работает |
| События (`on_event`) | События чатов, памяти, модулей, туров, инструментов, крона и сна согласно подписке |
| Провайдер (`provider`) | работает, если модуль назван в `llm.providers` с `kind: module` ([Провайдеры](providers.md#kind-module)) |
| Тулы (`tool`) | Объявляются и проверяются на handshake; ядро вызывает их в турах |
| Хуки (`hook`) | Ядро вызывает объявленные точки тура |
| `ask` | Объявляет возможность показать вопросы и разрешения |
| Переподписка на ленты открытых туров после смены соединения | Выполняется вызывающим кодом |

После реконнекта клиент читает `chats.list {live: true}` и заново открывает нужные
`turns.attach`, передавая сохранённый `since_seq`. SDK восстанавливает соединение,
но не хранит курсоры лент. UI применяет завершённые items по ID и учитывает
`ItemCompleted.replaces`, чтобы живой вывод заменился историческим без дубликата.
