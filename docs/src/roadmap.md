# Дорожная карта

| Этап | Содержание | Состояние |
|---|---|---|
| M1 | Библиотечное API, схема SQLite и миграции, RPC-сервер модулей, `eva-sdk`, провайдеры `openai` и `mock` | готов |
| M2 | Движок туров: диалог end-to-end, картинки в диалоге | следующий |
| M3 | Память, заметки, гигиена контекста, поиск по чатам | запланирован |
| M4 | Каталог инструментов, статистика, крон, сон, GC | запланирован |
| M5 | Рабочие туры: подагенты, руки, вопросы кнопками | запланирован |

## Операции, которые пока отвечают «pending»

Операции поздних этапов уже есть в API и в протоколе, но их хендлер отвечает ошибкой `Aux::Unavailable` (на проводе — код `unavailable`). Строка `Aux::Unavailable` начинается с маркера этапа; на проводе `message` — `unavailable: M2_PENDING: …`, `data` — msgpack `{"unavailable": "M2_PENDING: …"}`:

| Операция | Маркер | Текст `Aux::Unavailable` |
|---|---|---|
| `turns.start` | `M2_PENDING` | `M2_PENDING: turns.start arrives with M2: turn engine` |
| `turns.attach` | `M2_PENDING` | `M2_PENDING: turns.attach arrives with M2: turn engine` |
| `turns.append_silent` | `M2_PENDING` | `M2_PENDING: turns.append_silent arrives with M2: turn engine` |
| `turns.delivered` | `M2_PENDING` | `M2_PENDING: turns.delivered arrives with M2: turn engine` |
| `turns.cancel` | `M2_PENDING` | `M2_PENDING: turns.cancel arrives with M2: turn engine` |
| `turns.respond` | `M2_PENDING` | `M2_PENDING: turns.respond arrives with M2: turn engine` |
| `chats.ingest` | `M2_PENDING` | `M2_PENDING: chats.ingest arrives with M2: turn engine` |
| `chats.compact` | `M2_PENDING` | `M2_PENDING: chats.compact arrives with M2: turn engine` |
| `chats.notify` | `M2_PENDING` | `M2_PENDING: chats.notify arrives with M2: turn engine` |
| `chats.search` | `M3_PENDING` | `M3_PENDING: chats.search arrives with M3: chat search index` |
| `tools.catalog` | `M4_PENDING` | `M4_PENDING: tools.catalog arrives with M4: tool registry` |

Rust-клиенту (`eva-sdk`/`eva-rpc`) достаточно префикса строки `Aux::Unavailable`; клиенту провода — разобрать `data` или искать вхождение `M<N>_PENDING` в `message`. Ещё места, где M1 принимает, но не исполняет:

- capability `tools` и `hooks` модуля проверяются на handshake и сохраняются, но ядро не зовёт тулы модулей до M4 и хуки до M2;
- capability `ask` принимается и хранится, но pending-запросов, для которых она рисует кнопки, до M5 нет;
- ключи конфига, помеченные M2 или M4 в главе [Конфигурация](config.md), читаются и проверяются, но не действуют;
- после переподключения `eva-sdk` не переподписывается на ленты открытых туров (`turns.attach`) — это появится вместе с турами в M2.

Отдельно от этапов: `notes.list` отвечает `unavailable`, если в конфиге `notes.enabled: false`.

## M2 — движок туров

- Тур — реакция ядра на входящее событие: цикл «инференс → инструменты → инференс» до конца ответа. Лог событий чата, сегменты между компакциями, детерминированный рендер сегмента в запрос провайдера, учёт токенов, компакция, повторы и fallback-модели.
- Вход речи с поверхности (`chats.ingest`), актор тура и маркер спикера, названия чатов, лента items тура с `turns.attach`, доставка и отмена.
- Хуки модулей и события туров (`turn.*`, `tool.called`).
- **Картинки в диалоге.** Сообщение уже хранит блоки `image {blob, media_type}` со ссылкой в blob-store. Тур будет подставлять их в запрос: модели с `vision` — байтами, остальным — текстовым маркером `[image <id>] <name>`.

Что для картинок готово уже в M1:

- `blobs.put` / `blobs.get` / `blobs.stat` — загрузка и выдача байтов потоками, с лимитами и проверкой blake3-хэша;
- `inference.complete` принимает на вход блоки `image`: при `vision` у модели — байты, иначе — маркер; файлы — всегда маркер `[file <id>] <name> (<size> bytes)`;
- провайдер `openai` передаёт картинки как `image_url` с data URL; `vision` и `image_out` модели берутся из каталога провайдера или из поправок `models.<id>.caps` в конфиге;
- у моделей с `image_out` провайдер запрашивает `modalities: ["image", "text"]` и разбирает сгенерированные картинки из потока. `inference.complete` их не возвращает: ответ этой операции — только текст или JSON.

## M3 — память, заметки, поиск

- Память: записи `core`/`fact`/`event`/`procedure` с видимостью `public`/`trusted`/`private`, полнотекстовый и векторный поиск, hot-блок в system prompt, архив версий. В M1 доступны только чтение (`memory.list`, `get`, `history`) и `memory.restore`.
- Заметки: блокнот чата с бюджетом (`notes.budget_percent`, `notes.budget_max_tokens`), режимы и давление. В M1 — `notes.list`.
- Гигиена контекста: события, которые меняют промпт без правки прошлого, дедупликация результатов инструментов, context editing, keep-alive кэша.
- `chats.search`: гибридный поиск по речи чатов.
- Локальный провайдер эмбеддингов.

## M4 — каталог инструментов, статистика, крон, сон, GC

- Реестр инструментов, дерево групп, видимость на шаге, каталог резерва и его загрузка по запросу, warm-набор; вызовы инструментов модулей; `tools.catalog`.
- Статистика: бакеты, расход, health-метрики. Операции `stats.*` есть уже в M1; сейчас в них попадают расход `inference.complete` модулей и счётчики здоровья модулей, личностей, переносов trust и ошибок инференса, остальное добавят туры и эти подсистемы.
- Крон с персистентным расписанием, ночной сон по фазам, недельная сборка мусора. В M1 — только чтение `cron.list`, `cron.runs`, `sleep.runs`, `sleep.phase_runs`.

## M5 — рабочие туры

- Подагенты в папке `agents` со своей моделью и перепиской с ведущим туром (`agents.list` уже есть).
- Pending-запросы: вопросы кнопками и разрешения, ответ через `turns.respond`. Кнопки рисуют модули с capability `ask`, которые обслуживают surface чата; признак, что такая живая сессия есть, ядро выводит уже в M1.
- План тура.
- Руки через `eva-hands`: инструменты `fs_*`/`shell_*` на endpoint'ах `hands.endpoints`, гранты рук (`hands_grants.list`/`revoke` уже есть).
- Вложения в ленте тура.

## Что получает клиент хозяина

| Этап | Что появляется для клиента |
|---|---|
| M1 | Транспорт (WebSocket + msgpack, `hello` клиента с токеном, дозвон ядра до сервисов под Noise, ping, переподключение с `hello.resume`), surface `local`, чаты и папки, `blobs.put`/`blobs.get`, чтение памяти, заметок, статистики, крона и сна |
| M2 | Диалог: отправка сообщений через `turns.start`, лента items тура через `turns.attach` с сохранённым `seq`, доставка и отмена |
| M3 | Поиск по чатам `chats.search`, живая память и заметки |
| M4 | Каталог инструментов `tools.catalog`, наполненные крон, сон и статистика |
| M5 | Вопросы и разрешения кнопками через `turns.respond`, подагенты, руки |
