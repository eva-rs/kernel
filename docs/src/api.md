# Библиотечное API

Основной интерфейс ядра — Rust-библиотека: крейт `eva-core` (сессия и трейты неймспейсов) над типами `eva-protocol`. Код внутри ядра, модуль на [`eva-sdk`](sdk.md) и тест вызывают операции одинаково; RPC-метод `<namespace>.<op>` — обёртка над той же операцией ([Протокол модулей](protocol.md)).

## Форма вызова

```rust
use eva_core::prelude::*;
use eva_protocol::requests::surfaces;

let mut session = kernel.make_session(Caller::Kernel).await?;
let surfaces::get::Ok { surface } = session
    .surfaces()
    .get("local".into())
    .await?;
```

- `SessionMaker::make_session(caller)` выдаёт `Session<S>`: у ядра это `Kernel`, у модуля — `Listening::kernel()` или `Connected::kernel()` (caller фиксирует соединение). Отказ `make_session` — только `Aux`.
- `session.<ns>()` возвращает `Client<&mut S>`, `.<op>(arguments).await` вызывает операцию. Импорт трейта для такого вызова не нужен. Если `Args` уже собран, используется `.inner_mut().<op>().call(args)` и импорт `CallStep`.
- Операция `chats.move` в Rust — `session.chats().r#move(chat, surface)`.

Каждая операция — модуль `eva_protocol::requests::<ns>::<op>` с тремя типами:

| Тип | Что это |
|---|---|
| `Args` | Аргументы; на проводе — msgpack-карта с именами полей. У загрузки — `{ meta: Meta, body: ByteStream }`: по проводу идёт `Meta`, у `Args` serde-формы нет |
| `Ok` | Успешный ответ; у потока — `Ok(pub Items<Item, Err>)`, `Item` — тип элемента |
| `Err` | Собственные ошибки операции; пустой enum, если своих нет |

Шаг любой операции — `CallStep<Args, Output = Response<Ok, Err>>`, вызов — `.call(args)`. Вид операции — атрибут `#[flags(...)]` у метода `#[service]` в `protocol/src/requests/<ns>.rs`:

| Вид | Флаг | Итог |
|---|---|---|
| read | — | `Response<Ok, Err>` |
| mut | `mutating` | `Response<Ok, Err>`; повтор того же запроса в окне идемпотентности отдаёт сохранённый ответ |
| stream | `stream` | `Response<Ok, Err>`, где `Ok` оборачивает `Items<Item, Err>`; отказ до первого элемента — `Err` самого вызова, `Err` внутри потока — последний элемент, drop `Items` — отмена |
| upload | `upload` | `Response<Ok, Err>`; `body: ByteStream` — поток чанков `Result<Bytes, Aux>`, обрыв источника — `Aux` |

`Items` и `ByteStream` — типы `eva_protocol::requests`.

```rust
use futures::StreamExt as _;
use eva_protocol::requests::blobs;

let mut items = session.blobs().get(id, 0).await?.0;
while let Some(item) = items.next().await {
    match item? {
        blobs::get::Item::Meta { size, .. } => log::info!(size; "blob"),
        blobs::get::Item::Chunk(bytes) => sink.write_all(&bytes).await?,
    }
}
```

При загрузке тело передаётся отдельным аргументом:

```rust
use eva_protocol::{requests::{ByteStream, blobs::put}, types::blob::Kind};

let body: ByteStream = Box::pin(futures::stream::iter(chunks.into_iter().map(Ok)));
let put::Ok { id, .. } = session.blobs().put(
    put::Meta {
        chat,
        name: "photo.jpg".into(),
        media_type: "image/jpeg".into(),
        kind: Kind::Image,
        size,
        hash: None,
    },
    body,
).await?;
```

## Ошибки

`Response<O, E> = Result<O, Generic<E>>`. `Generic` отделяет честный отказ операции от беды подсистемы:

```rust
pub enum Generic<S> {
    Aux(Aux),  // общая беда, одинаковая для всех операций
    Spec(S),   // `Err` этой операции
}
```

`Aux` и `Spec` реализуют `HasCode`: код ошибки на проводе ставит сторона, знающая причину, по тексту он не угадывается.

| `Aux` | Код провода | Когда |
|---|---|---|
| `Db(String)` | `internal` | База недоступна или запрос к ней не прошёл |
| `Forbidden(String)` | `forbidden` | Caller не вправе делать это. Чужой объект — `forbidden`, а не `not_found`; сервису за чужой и за несуществующий чат, папку, личность, blob — один и тот же `forbidden` (кроме `surfaces.get`/`surfaces.update`: несуществующая surface — `not_found`) |
| `NotFound(String)` | `not_found` | Объекта нет, а своей ошибки у операции для этого нет |
| `Unavailable(String)` | `unavailable` | Внешняя машина, провайдер или модуль недостижимы; подсистема выключена; обрыв соединения модуля, после которого исход мутации неизвестен |
| `Invalid(String)` | `invalid_params` | Аргументы не прошли валидацию: формат id, длина текста, зарезервированное имя, диапазон |
| `Quota(String)` | `quota` | Лимит или квота: `fs`/`sqlite`/`kv`, blob-store, лимиты сессии |
| `Conflict(String)` | `conflict` | Состояние изменилось под вызовом: пропал якорь страницы, внешний id уже занят другим чатом, ответ мутации не сохранён в окне идемпотентности |
| `Internal(String)` | `internal` | Баг ядра |

`Spec` — атомарные ошибки домена (`eva_protocol::errors::<ns>`), каждая со своим кодом. Примеры: `surfaces::Taken` (`conflict`), `surfaces::BadRules` (`invalid_params`), `chats::NotFound` (`not_found`), `chats::TrustChange` (`conflict`, повторить с `confirm: true`).

```rust
match session.chats().inner_mut().r#move().call(args).await {
    Ok(chats::r#move::Ok { chat }) => { /* … */ }
    Err(Generic::Spec(chats::r#move::Err::TrustChange(e))) => { /* спросить хозяина */ }
    Err(Generic::Aux(Aux::Forbidden(why))) => { /* … */ }
    Err(other) => return Err(other.into()),
}
```

Через RPC ошибка приходит `RpcError` с кодом, текстом и `data` — msgpack того же `Generic` ([Коды и данные ошибок](protocol.md#коды-и-данные-ошибок)). Клиентская сессия разбирает `data` обратно в `Generic<Err>`; без `data` восстанавливает `Aux` по коду.

## Идентификаторы

Правило одно: на проводе и в базе id хранится в одной форме, а `Display` даёт форму, которую видит модель.

**Целочисленные** — на проводе число; на вход принимается и `42`, и `"42"`, и строка с префиксом:

| Тип | Префикс | Пример `Display` |
|---|---|---|
| `ChatId` | `c` | `c42` |
| `FolderId` | `f` | `f7` |
| `MemoryId` | `m` | `m15` |
| `EventId` (он же `MessageId`, `seq` ленты items) | `e` | `e901` |
| `SegmentId` | `s` | `s3` |
| `Tseq` (результат вызова тула в чате) | `t` | `t17` |
| `ToolId` | `tool` | `tool5` |
| `HandsGrantId` | `g` | `g2` |

**Строковые** — форма хранения и есть человеческая:

| Тип | Форма |
|---|---|
| `SurfaceId` | slug `[a-z][a-z0-9_]{0,31}`: `local`, `system`, `x` |
| `ModuleId` | `[a-z][a-z0-9_-]{0,63}`; `kernel`, `system`, `local` зарезервированы |
| `PersonId` | `p<N>`; `p0` — служебная личность ядра |
| `ModelId` | имя модели, как его объявил провайдер |
| `BlobId` | непрозрачная строка |
| `TurnId`, `RunId`, `PendingId`, `CronJobId`, `SessionId` | uuid v7 |

Чат и папку можно адресовать селектором: `chats::Selector` — `42` или `{surface, external_id}`, `folders::Selector` — `7` или `{surface, name}`.

Прочие формы провода: `Timestamp` — наносекунды Unix-эпохи (`u64`), `Date` — дни от Unix-эпохи (`u16`), длительность — строка humantime (`"30s"`, `"10m"`), `Bytes` — msgpack `bin`.

## Пагинация

Списки — keyset: `after` — последний элемент прошлой страницы (его id), `limit` — размер страницы. Короткая страница (меньше `limit`) — последняя.

```rust
let mut after = None;
loop {
    let chats::list::Ok { found } = session
        .chats()
        .inner_mut()
        .list()
        .call(chats::list::Args { limit, after, ..Default::default() })
        .await?;
    let last = found.len() < usize::from(limit.into_inner());
    after = found.last().map(|c| c.id);
    handle(found);
    if last { break; }
}
```

| Тип лимита | Диапазон | По умолчанию | Где |
|---|---|---|---|
| `Limit` | 1..=200 | 50 | `surfaces.list`, `folders.list`, `chats.list`, `persons.list`, `memory.list`, `kv.list` |
| `RunsLimit` | 1..=200 | 20 | `cron.runs`, `sleep.runs` |
| `SearchLimit` | 1..=50 | 10 | `chats.search` |
| `HistoryCount` | 1..=200 | 50 | `chats.history_read`: вперёд с `from_seq`, назад с `before`; `next_seq: None` — конец |
| `Top` | 1..=100 | 20 | `stats.tools_usage` |
| `Depth` | 1..=16 | 1 | `tools.catalog` |

Лимит вне диапазона не разбирается (`invalid_params`); в Rust — `Limit::new(n)`. Порядок страниц: `chats.list` — по последней активности, новые первыми; `surfaces`, `folders`, `persons`, `memory` — по id убывая; `kv.list` — ключи по возрастанию; `cron.runs`, `sleep.runs` — новейшие первыми. Если якорь `after` в `chats.list` исчез или не виден caller'у — `conflict`: листать заново.

## Patch

Операции `*.update` принимают `update: Update`, где каждое поле — `Patch<T>`:

| На проводе | Rust | Эффект |
|---|---|---|
| поле опущено, `null` или `"keep"` | `Patch::Keep` | не трогать |
| `{"change": v}` | `Patch::Change(v)` | записать `v` |
| `{"change": null}` у `Patch<Option<T>>` | `Patch::Change(None)` | очистить (вернуть наследование) |

```rust
session.chats().update(
    chat,
    chats::update::Update {
        title: Patch::Change(Some("Отпуск".into())),
        trusted: Patch::Change(None),
        ..Default::default()
    },
).await?;
```

`trusted`, `model`, `effort`, `thinking`, `compact_at` на surface, папке и чате — `Option`: `None` — наследовать от уровня выше. Менять их может только хозяин (ядро или клиент).

## Операции

Колонки: **вид** — read / mut / stream / upload; **ядро** — `Caller::Kernel`; **клиент** — клиент хозяина, пришедший на слушатель с токеном; **сервис** — модуль, которого набрало ядро по реестру `modules.services` / `modules.add`. «свои» — только объекты surface, созданных этим модулем; «—» — `forbidden`. Права проверяются до выполнения операции.

### `surfaces`

| Операция | Вид | Ядро | Клиент | Сервис | Смысл |
|---|---|---|---|---|---|
| `create` | mut | — | ✓ | ✓ | Создать surface; создателем записывается модуль сессии. Свой `id` возвращается как есть (безопасно на каждом старте), чужой — `Taken`; `local`/`system` — `forbidden` |
| `get` | read | ✓ | ✓ | свои | Surface по id |
| `list` | read | ✓ | ✓ | свои | Список surface |
| `update` | mut | ✓ | ✓ | свои: `name`, `rules` | Имя, правила канала, trust и модель |

### `folders`

| Операция | Вид | Ядро | Клиент | Сервис | Смысл |
|---|---|---|---|---|---|
| `create` | mut | ✓ | ✓ | — | Папка на surface |
| `update` | mut | ✓ | ✓ | — | Имя, trust и модель; встроенные папки — `Builtin` |
| `delete` | mut | ✓ | ✓ | — | Удалить; чаты уезжают в `Unsorted` (`moved`) |
| `list` | read | ✓ | ✓ | — | Папки, опционально одной surface |

### `chats`

| Операция | Вид | Ядро | Клиент | Сервис | Смысл |
|---|---|---|---|---|---|
| `ingest` | mut | ✓ | ✓ | свои | Вход речи с поверхности: сообщение и, если надо, тур; повтор `external_id` идемпотентен |
| `create` | mut | ✓ | ✓ | свои | Новый чат; сервису — только в папке с trust её `Unsorted` |
| `get` | read | ✓ | ✓ | свои | Чат по id или по alias `{surface, external_id}` |
| `list` | read | ✓ | ✓ | свои | Фильтры `surface`, `folder`, `deleted`, `live` |
| `update` | mut | ✓ | ✓ | свои, без trust-полей | Заголовок, `pinned`, `alerted`, `kind`, `project`, `workdir`, `workdir_on`; хозяину ещё trust и модель |
| `move` | mut | ✓ | ✓ | — | В другую папку той же surface; смена эффективного trust требует `confirm` |
| `delete` | mut | ✓ | ✓ | — | В корзину или сразу стереть (`purge`); `undeletable` — `Undeletable` |
| `restore` | mut | ✓ | ✓ | — | Вернуть из корзины |
| `search` | read | ✓ | ✓ | — | Гибридный поиск (полнотекстовый ∪ векторный) по речи и названиям чатов; `view` сужает выдачу видимостью тура; сниппеты однострочные |
| `history_read` | read | ✓ | ✓ | свои | Страница `count`: вперёд с `from_seq` либо последние реплики строго раньше `before` |
| `compact` | mut | ✓ | ✓ | — | Ручная компакция: новый сегмент |
| `notify` | mut | ✓ | — | — | Уведомление хозяину в чат `notify` |

### `turns`

Лента и восстановление подписки описаны в главе [Туры](turns.md).

| Операция | Вид | Ядро | Клиент | Сервис | Смысл |
|---|---|---|---|---|---|
| `start` | mut | ✓ | ✓ | свои | Сообщение в чат и старт тура; `extra_tools` — тулы на один тур |
| `attach` | stream | ✓ | ✓ | свои | Лента items тура с `since_seq` |
| `append_silent` | mut | ✓ | ✓ | свои | Сообщение без тура |
| `delivered` | mut | ✓ | ✓ | свои | Поверхность доставила реплику: её внешний id |
| `cancel` | mut | ✓ | ✓ | свои | Отменить живой тур чата |
| `respond` | mut | ✓ | ✓ | свои | Ответ на pending-запрос; разрешение требует хозяина, вопрос — подходящего актора |

### `persons`

| Операция | Вид | Ядро | Клиент | Сервис | Смысл |
|---|---|---|---|---|---|
| `resolve` | mut | ✓ | ✓ | свои | Адрес `(surface, external_id)` → личность; неизвестный заводится |
| `list` | read | ✓ | ✓ | — | Личности, фильтры `surface`, `whitelisted` |
| `rename` | mut | ✓ | ✓ | — | Переименовать; `p0` — `System` |
| `merge` | mut | ✓ | ✓ | — | Слить `from` в `into`; смена `master` требует `confirm` |
| `whitelist` | mut | ✓ | ✓ | — | Включить или снять whitelist |

### `memory`, `notes`, `tools`, `models`

| Операция | Вид | Ядро | Клиент | Сервис | Смысл |
|---|---|---|---|---|---|
| `memory.list` | read | ✓ | ✓ | — | Текущие версии записей, фильтры `kind`, `visibility`, `since` |
| `memory.get` | read | ✓ | ✓ | — | Запись и её архивная версия; с `view` недоступная туру запись — `not_found` |
| `memory.history` | read | ✓ | ✓ | — | Цепочка версий от корня; `view` прореживает её видимостью тура |
| `memory.restore` | mut | ✓ | ✓ | — | Вернуть забытую запись |
| `notes.list` | read | ✓ | ✓ | — | Блокнот чата: заметки, режим, токены и бюджет; `unavailable` при `notes.enabled: false` |
| `tools.catalog` | read | ✓ | ✓ | — | Поддерево каталога инструментов; `chat` добавляет видимость по последнему туру |
| `models.list` | read | ✓ | ✓ | — | Реестр моделей с caps, ценой и провайдером |

### `cron`, `sleep`, `agents`, `hands_grants`

Таблицы наполняются работающими подсистемами крона, сна, подагентов и разрешений
рабочих машин. Подробности рук — в главе [Рабочие операции](work.md).

| Операция | Вид | Ядро | Клиент | Сервис | Смысл |
|---|---|---|---|---|---|
| `cron.list` | read | ✓ | ✓ | — | Джобы крона |
| `cron.runs` | read | ✓ | ✓ | — | Журнал запусков, опционально одной джобы |
| `sleep.runs` | read | ✓ | ✓ | — | Журнал ночей сна |
| `sleep.phase_runs` | read | ✓ | ✓ | — | Фазы одной ночи |
| `agents.list` | read | ✓ | ✓ | — | Подагенты, опционально одного лида |
| `hands_grants.list` | read | ✓ | ✓ | — | Постоянные разрешения рук |
| `hands_grants.revoke` | mut | ✓ | ✓ | — | Отозвать разрешение |

### `stats`

| Операция | Вид | Ядро | Клиент | Сервис | Смысл |
|---|---|---|---|---|---|
| `tools_usage` | read | ✓ | ✓ | — | Использование тулов за период `from..=to` |
| `spend` | read | ✓ | ✓ | — | Расход за период с группировкой |
| `spend_series` | read | ✓ | ✓ | — | Расход по дням |
| `chat_spend` | read | ✓ | ✓ | свои | Расход одного чата |
| `health` | read | ✓ | ✓ | — | Счётчики здоровья и rate limit'ы |

### `modules`

| Операция | Вид | Ядро | Клиент | Сервис | Смысл |
|---|---|---|---|---|---|
| `list` | read | ✓ | ✓ | — | Модули, их состояние и capabilities; у записи реестра сервисов — `dial {address, config, error}`, ещё не дозвонённая — `offline` |
| `add` | mut | — | ✓ | — | Добавить сервис в реестр `{module, address, key?}` и сразу начать дозвон. Без `key` ядро генерирует его и возвращает один раз; ключ короче 32 символов, id зарезервирован или невалиден — `invalid_params`; id занят сервисом или токеном клиента (конфиг или база) — `Taken` (`conflict`); `address`, который уже набирает другая запись реестра, — `conflict` с её id |
| `remove` | mut | — | ✓ | — | Убрать сервис, добавленный `add`: дозвон встаёт, сессия закрывается `unauthorized "module removed"`. Запись `modules.services` — `conflict`, неизвестный id — `ServiceNotFound` (`not_found`) |
| `token_issue` | mut | ✓ | ✓ | — | Выдать токен клиенту (показывается один раз, прежний отзывается). `issued_by` задаёт только ядро; токен своей сессии — `conflict`; id из `modules.tokens`, `modules.services` или `modules.add` — `conflict` |
| `token_revoke` | mut | ✓ | ✓ | — | Отозвать выданные токены и закрыть сессии клиента |

### `blobs`

| Операция | Вид | Ядро | Клиент | Сервис | Смысл |
|---|---|---|---|---|---|
| `put` | upload | ✓ | ✓ | свои | Загрузить вложение чата; `size` и `hash` (blake3 hex) сверяются по ходу приёма |
| `get` | stream | ✓ | ✓ | свои | Сначала `Meta`, затем чанки; `offset` — докачка |
| `stat` | read | ✓ | ✓ | свои | Метаданные blob'а |

Blob'ы подчиняются правам своего чата; blob без чата сервису недоступен.

### `inference`

| Операция | Вид | Ядро | Клиент | Сервис | Смысл |
|---|---|---|---|---|---|
| `complete` | mut | ✓ | ✓ | ✓ | Разовый вызов модели без чата и тулов; `model: None` — роль `cheap`, `schema` — структурированный ответ. Блоки `image`/`file` входа проверяются правами их чатов |

### `fs`, `sqlite`, `kv`

Песочница модуля: скоуп — модуль сессии, чужое имя указать нельзя. У ядра песочницы нет. Лимиты — в [Безопасности](security.md#песочница-модуля).

| Операция | Вид | Ядро | Клиент | Сервис | Смысл |
|---|---|---|---|---|---|
| `fs.read` | read | — | своё | своё | Файл целиком (`Bytes`) |
| `fs.write` | mut | — | своё | своё | Записать файл; `create_dirs` — создать каталоги |
| `fs.list` | read | — | своё | своё | Каталог; `""` — корень |
| `fs.stat` | read | — | своё | своё | Тип, размер, время изменения |
| `fs.remove` | mut | — | своё | своё | Удалить; каталог — с `recursive` |
| `sqlite.exec` | mut | — | своё | своё | Один оператор; `changes` |
| `sqlite.query` | read | — | своё | своё | Строки `columns` + `rows` ячеек `Cell` |
| `sqlite.tx` | mut | — | своё | своё | Несколько операторов одной транзакцией |
| `kv.get` | read | — | своё | своё | Значение ключа или `None` |
| `kv.set` | mut | — | своё | своё | Записать, last-write-wins |
| `kv.del` | mut | — | своё | своё | Удалить ключ |
| `kv.list` | read | — | своё | своё | Ключи с `prefix`, по возрастанию |

Параметры `sqlite` — `Value` (`?1`, `?2`, … по порядку): `Bytes` — BLOB, `Bool` — 0/1, массив и карта — JSON-текст.
