# Changelog

## M1 — 2026-09-26

Библиотечный слой, транспорт модулей и SDK.

### Ядро (`eva-kernel`)

- Все операции `11 §3` без туров: surfaces, folders, chats (кроме `ingest`/`compact`/`notify`/`search`), persons, memory, notes, models, cron, sleep, agents, stats, modules, hands_grants, blobs, inference, fs, sqlite, kv. Отложенные отвечают `Aux::Unavailable` с `M2_PENDING`/`M3_PENDING`/`M4_PENDING`: `turns.*`, `chats.ingest/compact/notify` (M2), `chats.search` (M3), `tools.catalog` (M4).
- SQLite: схема `10 §2` одной миграцией, раннер с blake3-чексуммами, один писатель и пул читателей, сид `local`/`system`/хозяина из конфига.
- `Policy` по доменам `11 §4`; `Service` видит только свои surface — фильтр в `WHERE`.
- Подсистема `modules`: слушатели uds/ws/wss, токены из конфига и `module_tokens` (blake3), зарезервированные имена, вытеснение сессии того же имени, строка `modules` (connected/offline/forgotten), провайдер запасной двери, рассылка событий `chat.*`/`module.*` по `Policy::events()`. Capability `tools`/`hooks` проверяются и хранятся; вызовы — с M4/M2.
- Провайдеры `llm.providers`: встроенные через `eva_providers::build`, `kind: module` — на handshake модуля.
- Бинарь: `eva::cli::App` (env `EVA_`), `Supervisor` с компонентами `db` и `modules`, штатная остановка по SIGINT/SIGTERM.
- Счётчики здоровья `09 §5` для модулей, личностей, переносов trust и инференса.

### Протокол и транспорт

- `eva-protocol`: wire-кадры, handshake, capabilities, коды ошибок `HasCode` для всех `Err`, терпимость к незнакомым вариантам, байтовый newtype `Bytes`.
- `eva-rpc`: симметричный пир (вызовы, потоки, загрузки, отмена, ping, буфер на поток), окно идемпотентности с `hello.resume`, причина закрытия доходит до пира, незнакомые ключи capability — в `warnings`.
- `eva-core`: `prelude` со всеми трейтами неймспейсов.

### SDK (`eva-sdk`)

- `Module` → `Connected`: ожидание ядра до `connect_wait`, регистрация surface после каждого `welcome`, реконнект `1s..60s` с `hello.resume`, остановка при вытеснении; билдеры тулов, хуков, событий и провайдера.

### Провайдеры

- `eva-provider-openai`: OpenAI-совместимый Chat Completions, первым OpenRouter; `eva-provider-mock`: сценарии и запись запросов; `eva-providers`: конфиг под тегом `kind`.

### Тесты

- Контракт `tests/contract.rs`: набор операций M1 в процессе и по UDS через SDK даёт побайтно равные ответы; изоляция сервиса по UDS; два клиента на `local`; вытеснение; реконнект; чужой токен.
- Схема = снапшот, STRICT, CHECK ⇔ enum; lint I14; матрицы `Policy`.
