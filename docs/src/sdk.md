# SDK модулей

`eva-sdk` — модуль для soul на Rust поверх `eva-rpc`: handshake, смена соединения с продолжением сессии, обработчики вызовов от soul (тулы, хуки, события, провайдер) и клиент `Soul`, который реализует то же [библиотечное API](api.md), что и сам soul. `module!().run()` берёт на себя конфиг, runtime и `Supervisor` фреймворка `eva`. Провод, который SDK прячет, описан в [Протоколе модулей](protocol.md).

Модуль бывает двух видов, и вид задаёт, кто звонит:

| Вид | Кто | Соединение | Вход SDK |
|---|---|---|---|
| сервис | мост мессенджера, MCP, экзотический провайдер | модуль слушает, soul набирает его по записи реестра; Noise с общим ключом | `Module::listen(&ServiceConfig)` → `Listening` |
| клиент | клиент хозяина: телефон, TUI | модуль сам звонит в soul с токеном | `Module::connect_config(&ClientConfig)` → `Connected` |

Сервис не знает адреса soul. Клиенты хозяина за NAT и приходят-уходят, поэтому звонят сами.

## Минимальный сервис

Сервис ниже загружает типизированный конфиг, хранит общее состояние и объявляет обработчик прямо в группе. Это `sdk/examples/echo.rs` — он собирается вместе с крейтом:

```sh
nix develop -c cargo build -p eva-sdk --examples
nix develop -c cargo run -p eva-sdk --example echo -- --config echo.yaml
```

```rust
{{#include ../../sdk/examples/echo.rs}}
```

Сервис регистрирует хозяин — запись реестра у soul с адресом и ключом: `modules.services.<id> = {address, key}` в конфиге soul или операция `modules.add {module, address, key?}` (без `key` soul сгенерирует ключ и вернёт его один раз). Ключ записи и есть id модуля; модулю его знать не нужно — он приходит в `welcome` и доступен как `listening.module()`.

## Конфигурация

При запуске через `module!().run()` секция `module` необязательна. `init("echo", async |cfg: EchoConfig| …)` регистрирует обязательную секцию `echo` и один раз создаёт состояние до открытия слушателя. Дефолты полей своего конфига задаются обычным `#[serde(default)]`.

Загрузка — как у soul: повторяемый `--config` (YAML, JSON, TOML), затем окружение. Префикс берётся из окончательного имени модуля: `weather-bridge` → `WEATHER_BRIDGE_`; вложенность через `__`. Например, `ECHO_ECHO__PREFIX` переопределяет `echo.prefix`. Имя для `run` начинается с латинской буквы, содержит до 64 букв, цифр, `_` или `-`.

Минимальный конфиг примера:

```yaml
echo:
  prefix: "Eva: "
```

Сервис, дефолты `run()` (`ServiceDefaults`):

| Ключ | Тип | По умолчанию | Смысл |
|---|---|---|---|
| `listen` | слушатель | `$RUNTIME_DIRECTORY/module.sock`, иначе `/run/eva-<name>/module.sock` | `{uds: <путь>}` или `{tcp: "<ip>:<port>"}`; при socket activation — адрес унаследованного сокета. Права файла сокета SDK не ставит — их задаёт окружение (umask процесса или socket-юнит) |
| `key` | секрет | `{credential: soul-key}` | Тот же PSK, что `key` записи реестра у soul; доступны формы `credential`, `file`, `plaintext`, хвостовой перевод строки срезается |
| `soul_wait` | длительность | `3m` | Сколько `soul()` ждёт первого соединения от soul |
| транспорт | | как у soul | те же ключи, что ниже у клиента; `max_hello` сервис не проверяет — его `hello` держит лишь `max_frame` |

```yaml
module:
  listen: { uds: /run/eva-x-bridge/module.sock }
  key: { credential: soul-key }
```

`RUNTIME_DIRECTORY` должен задавать один абсолютный каталог; несколько каталогов требуют явного `module.listen`. Каталог и права создаёт окружение. `soul-key` — имя systemd credential с общим ключом Noise; `SecretString::read()` берёт его из `$CREDENTIALS_DIRECTORY/soul-key`. API-ключ внешнего сервиса задаётся отдельно, например `weather.api_key: {credential: owm-api-key}`. Имена credentials — один компонент из букв, цифр, `.`, `_`, `-`, кроме `.` и `..`.

Для встраивания в свой процесс остаются `ServiceConfig::register(app)` и `ClientConfig::register(app)` с обязательной секцией `module`. У явного `ServiceConfig` поля `listen` и `key` обязательны; дефолты выше применяет именно `Module::run`.

Клиент, `ClientConfig`:

| Ключ | Тип | По умолчанию | Смысл |
|---|---|---|---|
| `soul` | URL | — (обязателен) | `uds:///run/eva-soul/modules.sock`, `ws://host:port/v1/modules`, `wss://…`; пустой путь ws дополняется до `/v1/modules` |
| `token` | секрет | — (обязателен) | `{credential: token}`, `{plaintext: "…"}` или `{file: "/path"}`; читается один раз, хвостовой перевод строки отбрасывается |
| `connect_wait` | длительность | `3m` | сколько ждать soul при первом подключении |
| `tls.extra_ca` | путь | нет | PEM CA для soul с самоподписанным сертификатом: задан — доверие только ему, иначе публичные корни webpki |
| `handshake_timeout`, `ping_interval`, `ping_timeout`, `request_timeout`, `stream_idle_timeout`, `stream_buffer`, `max_inflight`, `max_streams`, `max_frame`, `max_hello`, `blob_chunk`, `idempotency_window` | | как у soul | транспорт — те же ключи и умолчания, что в секции `modules` конфига soul ([Конфигурация](config.md#транспорт)), плоско в секции `module` |

```yaml
module:
  soul: wss://soul.example:7443/v1/modules
  token: { file: /run/credentials/phone.service/token }
  tls: { extra_ca: /etc/phone/soul-ca.pem }
```

Ключ и токен держите файлом вне Nix store: под systemd — через `LoadCredential`, как это делает NixOS-модуль soul ([Безопасность](security.md#токены)). Ключ сервиса — секрет высокой энтропии (32+ символов): Noise `NNpsk0` с угадываемым ключом подбирается офлайн по перехваченному handshake.

## Билдер `Module`

Каноничный вход — `module!()`: имя и версия берутся из `CARGO_PKG_NAME` и `CARGO_PKG_VERSION` вызывающего крейта. Их можно заменить: `module!().version("1.4.3").name("owm")`. Имя управляет дефолтами запуска и доменом переводов; авторизованный id по-прежнему приходит от soul.

`Module::new()` остаётся низкоуровневым входом с именем `module` и версией `0.0.0`.

| Метод | Что задаёт |
|---|---|
| `name(v)`, `version(v)` | Переопределяют имя приложения и версию для `modules.list` |
| `init(section, initializer)` | Загружает типизированный конфиг и создаёт состояние при `run` |
| `state(value)` | Готовое состояние для `listen`, `connect*` или `run`; задаётся до тулов |
| `translations(include_translations!("locales"))` | Встроенные PO-каталоги |
| `run()` | Запуск сервиса через `eva::cli::App` и `Supervisor`, остановка по SIGINT/SIGTERM |
| `about(text)` | Описание для логов модуля; в `hello` не уходит |
| `ask(bool)` | Capability `ask`: модуль рисует кнопки pending-запросов в чатах своих surface (клиенту — ещё `local`) |
| `group(Group<S>)` | Группа с локальными именами тулов и вложенных групп |
| `group_path(path, about)`, `tool_path(path, handler)` | Явные полные пути для миграции старого кода; модификаторы на `Module` правят последний тул |
| `on_event(topic, handler)` | Подписка на топик; обработчик `Fn(Soul, Event) -> impl Future`, его ошибка только пишется в лог |
| `hook(point, handler)` | Хук точки; тип входа (`BeforeTurn`, `BeforeTool`, `AfterTool`) обязан совпасть с точкой |
| `hooks_timeout(d)`, `hooks_fail_mode(mode)` | Потолок ответа хука (soul режет до `modules.hooks.timeout`) и поведение при отказе: `Open` (по умолчанию) или `Closed` |
| `provider(p)` | Запасной вход провайдера: любой `impl eva_core::Provider` |
| `options(ClientOptions)` | Транспорт, TLS и счётчик id запросов вручную |
| `connect_wait(d)` | Клиент: сколько ждать soul на старте; по умолчанию 3 min |
| `reconnect_backoff(min, max)` | Клиент: пауза между попытками, от `min` вдвое до `max`; по умолчанию 1 s..60 s |
| `capabilities()` | Что уйдёт в `hello` — удобно для тестов |

## Сервис: `listen` → `Listening`

`Module::listen(&ServiceConfig)` проверяет ключ, привязывает слушатель и возвращается сразу; соединения от soul принимаются в фоне. Нечитаемый или пустой `key` — `ConnectError::Config`, занятый адрес — `ConnectError::Transport`.

| Метод | Что даёт |
|---|---|
| `soul().await` | `Soul` через текущее соединение; до первого ждёт его до `soul_wait`, не дождался — `ConnectError` с причиной последней неудачной попытки soul (например, чужой ключ) |
| `module()` | Id модуля из `welcome`; до первого соединения — `None` |
| `welcome()`, `client()` | Последний `welcome` и текущий `RpcClient`; до первого соединения — `None` |
| `address()` | Адрес, по которому звонит soul: `uds:///…` или `tcp://ip:port` (порт `0` из конфига здесь уже выбран) |
| `run(shutdown)` | Живёт до `shutdown` |

`Listening` клонируется; `run` съедает один клон, а с последним брошенным клоном приём останавливается. Поведение:

- **Соединение от soul** — Noise responder, затем `hello` без токена и `welcome`; всё вместе — за один `handshake_timeout` от accept. Чужой ключ — сокет закрывается без ответа, у soul это провал дозвона. Медленный или молчащий сокет не задерживает приём остальных; до конца Noise таких держится не больше 64 (и 8 из одной сети: IPv4-адрес или IPv6 /64), сверх лимита новичок вытесняет старейшее соединение, ещё не доказавшее ключ. Провал до Noise пишется в лог как `debug`, после — `warn`.
- **Новое соединение от soul** после своего `welcome` становится текущим, прежнее закрывается `conflict "superseded"` — так модуль переживает рестарт soul и полуоткрытое старое соединение. `hello.resume` — `session_id` последней сессии процесса, счётчик id запросов продолжается.
- **Потеря soul** `run` не завершает: soul перезвонит сам, по своей паузе `1s..60s`. Модуль не звонит никогда.
- **`shutdown`** — приём останавливается, файл своего uds-сокета удаляется, текущее соединение закрывается с `unavailable`, `run` выходит с `Ok`.
- Предупреждения `welcome.warnings` (отклонённые тулы, модели, capability) SDK пишет в лог.

Socket activation: если `LISTEN_PID` — pid модуля, SDK ищет среди унаследованных сокетов (`LISTEN_FDS`, с fd 3) тот, чей адрес совпадает с `listen` (путь `uds` или `ip:port`), и берёт его вместо bind. Ошибка старта: унаследованный сокет с другим адресом (его некому обслужить) или не слушающий — при `Accept=yes` менеджер отдаёт уже принятое соединение, а нужен слушатель (`Accept=no`, значение по умолчанию). Переменные `LISTEN_*` SDK не трогает: дочерний процесс отсеет чужие сокеты по `LISTEN_PID`. Файл унаследованного сокета принадлежит менеджеру служб и не удаляется.

Локальный сервис на NixOS слушает `/run/eva-<id>/module.sock`, доступный группе `eva` — soul в этой группе. Права файла задаёт окружение, не SDK: либо юнит сервиса (`RuntimeDirectory = eva-<id>`, `Group = eva`, `UMask = 0007`), либо socket-юнит (`SocketGroup = eva`, `SocketMode = 0660`, `Accept = no`).

## Клиент: `connect` → `Connected`

| Метод | Когда |
|---|---|
| `connect_config(&ClientConfig)` | Обычный путь: адрес, токен, TLS и транспорт из секции `module` |
| `connect(&Url, &SecretString)` | Адрес и токен из своего конфига |
| `handshake(Conn, &SecretString)` | Готовое соединение (`Conn::pair` в тестах); без адреса `run` не переподключается |

Токен клиенту выдаёт хозяин: запись `modules.tokens.<id>` в конфиге soul или операция `modules.token_issue`. `connect*` возвращает `Connected` после `welcome`. До этого SDK ждёт подключения к soul до `connect_wait`: обрыв транспорта и временный отказ (`unavailable`, `timeout`, `cancelled`, `internal`, `quota`) — повтор с паузой; отказ по существу (`unauthorized`, `proto_version`, `invalid_params`…) и нечитаемый секрет — сразу `ConnectError`.

| Метод `Connected` | Что даёт |
|---|---|
| `soul()` | `Soul` — клиент библиотечного API через текущее соединение |
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

## Вызов soul

`Soul = Session<LiveSession>`: те же неймспейсы и форма вызова, что в [Библиотечном API](api.md), и те же права, что у caller'а модуля. Ручку можно клонировать и держать всё время жизни модуля: каждый вызов берёт текущее соединение — после реконнекта клиента и после нового дозвона от soul к сервису тоже.

```rust
use eva::handling::CallStep as _;
use eva_sdk::prelude::*;
use eva_sdk::protocol::requests::Chats;

let mut soul = listening.soul().await?; // у клиента — connected.soul()
let Chats::list::Ok { found } = soul
    .chats()
    .inner_mut()
    .list()
    .call(Chats::list::Args { surface: Some("x".into()), ..Default::default() })
    .await?;
```

Поток и загрузка вызываются методами клиента с обычными аргументами: у `blobs.get` и `turns.attach` `Ok` — поток `Items`, у `blobs.put` тело `ByteStream` едет в `Args` рядом с `Meta` ([Форма вызова](api.md#форма-вызова)). SDK шлёт `Meta` в `params` запроса, а тело — чанками потока.

Если соединение оборвалось, а новое возможно (у сервиса идёт приём, у клиента жив `run`):

| Вызов | Поведение |
|---|---|
| Новый | Ждёт нового соединения до 30 s; не дождался — `Aux::Unavailable` с причиной обрыва |
| Чтение, открытие потока в полёте | Повторяется на новом соединении |
| Мутация в полёте | Повторяется тем же id запроса, если soul продолжил окно (`welcome.resumed`); иначе исход неизвестен — `Aux::Unavailable` |
| Загрузка в полёте | `Aux::Unavailable`: тело уже прочитано |
| Уже открытый поток | Последний элемент — `Aux::Unavailable`; переоткрыть — дело модуля |

## Surface

Surface — лицо модуля, в котором живут его чаты. Модуль создаёт её сам:

```rust
soul.surfaces().create(
    "x".into(),                     // slug [a-z][a-z0-9_]{0,31}
    "Messenger X".into(),
    Some(RULES.into()),              // prompt-блок канала
).await?;
```

- Soul записывает создателя в `surface.module`. Для него `create` идемпотентен: свой `id` возвращается как есть, поэтому вызов безопасен на каждом старте. Чужой `id` — `Taken`, `local`/`system` — `forbidden`.
- Имя и правила потом меняет `surfaces.update`; trust и модель — только хозяин.
- Soul выводит обслуживание surface из живой сессии создателя: после смены соединения регистрировать ничего не нужно.
- Сервис видит только чаты, личности, blob'ы и события своих surface ([Безопасность](security.md)).

## Песочница: `fs`, `sqlite`, `kv`

Скоуп — id модуля из `welcome`; чужое хранилище не адресуется. Лимиты — в [Безопасности](security.md#песочница-модуля).

```rust
use eva_sdk::protocol::{
    requests::{Kv, Statement},
    types::{Bytes, Value},
};

soul.fs().write(
    "cache/state.json".into(),
    Bytes(serde_json::to_vec(&state)?),
    true,
).await?;

soul.sqlite().exec(Statement {
    sql: "INSERT INTO seen (external_id) VALUES (?1)".into(),
    params: vec![Value::String(external_id)],
}).await?;

let Kv::get::Ok { value } = soul.kv().get("cursor".into()).await?;
```

`sqlite.query` отдаёт `columns` и `rows` ячеек `Cell` (`Null`, `Integer`, `Real`, `Text`, `Blob`); несколько операторов атомарно — `sqlite.tx`.

## Тулы и состояние

```rust
module!()
    .state(bridge)
    .group(groups::messenger())
    .group(groups::weather());

fn messenger() -> Group<Bridge> {
    Group::new("messenger")
        .about("Messenger actions")
        .tool(
            Tool::new("send", async |ctx: ToolCtx<Bridge>, input: SendInput| {
                ctx.progress(msg!("Sending to {chat}", chat = &input.chat)).await;
                let id = ctx.state().send(&input.chat, &input.text).await?;
                Ok::<_, ToolError>(format!("sent {id}"))
            })
            .describe(Description {
                text: Some("Send a message to the chat".into()),
                idempotent: false,
                parallel_safe: false,
                ends_turn: true,
                ..Default::default()
            }),
        )
}
```

Группа задаёт namespace: локальный `send` становится `messenger.send` на проводе и `messenger_send` для модели. Имена сегментов — `[a-z][a-z0-9_]{0,23}`. Вложенные группы объявляются через `Group::group`. Повторы путей и коллизии каноничных имён отвергаются при сборке модуля. Soul проверяет глубину и остальные ограничения при handshake.

Все группы одного `Module<S>` получают одно `Arc<S>`; обработчик видит `&S` через `ctx.state()`. Если клиент уже готов, используется `state(client)`; если его надо создать из конфига — `init("weather", async |cfg: WeatherConfig| …)` и затем `run()`. Состояние задаётся до регистрации тулов. При ручном запуске через `listen` или `connect*` нужно передать уже созданное состояние через `state`.

Схема входа берётся из `#[data]`-типа: draft 2020-12, вложенные схемы раскрыты, без `title`. `Description` экспортируется из `eva_sdk` и его `prelude`. `.describe(Description { … })` полностью заменяет настройки одного тула, сохраняя его имя, схему входа и обработчик. Короткие методы `description`, `ends_turn`, `master_only`, `trusted_only`, `needs`, `parallel_safe`, `idempotent`, `waits_on_cancel`, `result`, `timeout` обновляют через `describe` только своё поле.

| Поле `Description` | По умолчанию |
|---|---|
| `text` | `None`: doc-комментарий типа входа; `Some(text)` переопределяет его |
| `idempotent` | `true` |
| `parallel_safe` | `true` |
| `master_only`, `trusted_only`, `ends_turn`, `waits_on_cancel` | `false` |
| `needs` | Пустой список |
| `result` | `ResultForm::Windowed` |
| `timeout` | `None`: таймаут soul |

Порядок имеет значение: `.idempotent(false).describe(Description::default())` вернёт `idempotent` в `true`, а `.describe(description).idempotent(false)` сохранит остальные поля переданного описания. `text: None` возвращает описание из типа входа; `.description("")` явно задаёт пустое описание. При объявлении через `Module::tool_path` те же методы применяются к последнему тулу.

`idempotent` обещает, что повтор с теми же аргументами не создаст дополнительного эффекта по сравнению с одним вызовом; ответы при этом могут различаться. Отправка сообщения, списание денег или увеличение счётчика требуют явного `false`. Soul пока не использует этот флаг для автоматических повторов; SDK не делает обработчик идемпотентным.

`parallel_safe` разрешает исполнять подряд идущие вызовы одного ответа модели параллельно в пределах `tools.parallel.max`. Если важен порядок, например при отправке нескольких сообщений или изменении одного устройства, задайте `false`: такой вызов исполняется отдельно, по порядку ответа. Это не блокировка между турами: синхронизацией общего состояния и внешних ресурсов занимается модуль.

- Возврат обработчика — `Result<O, E>`: `O: IntoToolResult` (`String`, `&str`, `ToolContent`, `Vec<ToolContent>` или `ToolResult`), `E: Into<ToolError>`.
- Числа и булевы, присланные строкой, приводятся к схеме. Неразобранный вход возвращается модели как `is_error`.
- `ToolCtx<S>`: `state()`, `soul()`, `turn()`, `call_id()`, `progress(text)`.
- Любая `Display`-ошибка через `?` становится ответом `is_error`; `ToolError::fatal(reason)` роняет тур.
- Повтор `tools.call` с тем же `call_id` после смены соединения не запускает обработчик второй раз.

## Локализация прогресса

`ctx.progress` отправляет описательный статус в soul во время вызова тула. Soul передаёт его подписчикам ленты тура; это отдельное событие, не результат для модели. Обычная строка остаётся допустима. Для перевода используется английский оригинал и именованные параметры:

```rust
ctx.progress(msg!("Fetching weather for {city}", city = &input.city)).await;
ctx.progress(msg!(
    "Found {count} file",
    plural = "Found {count} files",
    count = total,
)).await;
let button = msg!("Open", context = "button");
```

`msg!` проверяет имена параметров при компиляции, значения форматирует через `Display`. `count` для plural — `u64`; `{{` и `}}` обозначают буквальные скобки. `context` и `plural` — зарезервированные аргументы макроса.

Каталоги — обычные UTF-8 `.po` с `msgid`, `msgctxt`, `msgid_plural`, `msgstr[n]` и заголовками `Language`, `Plural-Forms`. Пример перевода:

```po
msgid "Fetching weather for {city}"
msgstr "Получаю погоду для {city}"
```

`include_translations!("locales")` читает `locales/*.po` относительно `Cargo.toml` вызывающего крейта, проверяет каталоги и встраивает их в бинарь. Изменение существующего PO запускает пересборку; после добавления нового файла нужно также изменить исходник, содержащий макрос. Язык берётся из заголовка `Language`, иначе из имени файла. Непереведённые и `fuzzy`-строки используют оригинал, неизвестные параметры перевода — ошибка сборки. Контекст и plural-формы следуют gettext; поддержаны арифметика, сравнения, логические операторы и тернарное выражение `Plural-Forms`.

В потоке всегда есть английский fallback `text`, а для переводимых сообщений — ещё `message: LocalizedText` с оригиналом, параметрами, именем домена, хэшем каталога и переводами только этой строки. Для plural SDK заранее выбирает форму каждого языка по `count`. Полный каталог по RPC не передаётся. Получатель вызывает `message.render("ru-RU")`: сначала ищется регион, затем язык, затем оригинал. Глобальной locale процесса нет; разные подписчики могут показывать один статус на разных языках. Старый клиент использует `text`. Скраб секретов проверяет fallback, параметры и переводы; при совпадении soul удаляет метаданные локализации и оставляет очищенный текст. Раскрытый текст ограничен 64 КиБ.

Извлечение строк в POT и обновление PO (GNU `msgmerge` включён в devShell):

```sh
nix develop -c cargo run -p eva-sdk-macros --bin eva-sdk-i18n -- extract sdk/examples /tmp/echo.pot
nix develop -c cargo run -p eva-sdk-macros --bin eva-sdk-i18n -- merge sdk/examples/locales/ru.po /tmp/echo.pot
```

Экстрактор читает Rust-файлы, собирает вызовы `msg!`, контексты и plural-формы, объединяет одинаковые строки. Он не раскрывает пользовательские макросы, генерирующие вызовы `msg!`.

## Хуки

```rust
Module::new().hook(HookPoint::BeforeTool, |_soul, input: BeforeTool| async move {
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
| Сервис: приём соединений от soul под Noise, вытеснение, socket activation; клиент: подключение, ожидание soul, реконнект; `resume` | работает |
| Вызов операций soul | Работает через общий библиотечный API |
| Surface, `fs`/`sqlite`/`kv`, blob'ы, `inference.complete` | работает |
| События (`on_event`) | События чатов, памяти, модулей, туров, инструментов, крона и сна согласно подписке |
| Провайдер (`provider`) | работает, если модуль назван в `llm.providers` с `kind: module` ([Провайдеры](providers.md#kind-module)) |
| Тулы (`tool`) | Объявляются и проверяются на handshake; soul вызывает их в турах |
| Хуки (`hook`) | Soul вызывает объявленные точки тура |
| `ask` | Объявляет возможность показать вопросы и разрешения |
| Переподписка на ленты открытых туров после смены соединения | Выполняется вызывающим кодом |

После реконнекта клиент читает `chats.list {live: true}` и заново открывает нужные
`turns.attach`, передавая сохранённый `since_seq`. SDK восстанавливает соединение,
но не хранит курсоры лент. UI применяет завершённые items по ID и учитывает
`ItemCompleted.replaces`, чтобы живой вывод заменился историческим без дубликата.
