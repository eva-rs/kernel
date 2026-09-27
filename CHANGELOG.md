# Changelog

## M1 — 2026-09-26

Библиотечный слой, транспорт модулей и SDK.

### Ядро (`eva-kernel`)

Операции и хранилище:

- Все операции M1 без туров: surfaces, folders, chats (кроме `ingest`/`compact`/`notify`/`search`), persons, memory, notes, models, cron, sleep, agents, stats, modules, hands_grants, blobs, inference, fs, sqlite, kv. Отложенные отвечают `Aux::Unavailable` с `M2_PENDING`/`M3_PENDING`/`M4_PENDING`: `turns.*`, `chats.ingest/compact/notify` (M2), `chats.search` (M3), `tools.catalog` (M4).
- SQLite: схема миграциями `0001_init` и `0002_module_kv_usage`, раннер с blake3-чексуммами, один писатель, сид `local`/`system`/хозяина из конфига.
- Читатели БД — жёсткий потолок `db.readers`: соединение берётся на один запрос, ожидание свободного не дольше `db.busy_timeout` (иначе `Aux::Db`), отменённый запрос держит слот до конца.
- `folders.delete`, `folders.update{trusted}` и `surfaces.update{trusted}`, меняющие действующее доверие чата, ставят ему `reset_pending`.
- `memory.restore` публикует `memory.changed`.
- `inference.complete` доступен ядру и пишет `spend` с `source = system`; `max_tokens` урезается до остатка окна контекста модели (оценка входа по калиброванным `token_ratios`), не влезает ничего — `Aux::Invalid`.
- Строки ограничены (`Aux::Invalid`): имена, заголовки и handle — ≤ 256 символов, `external_id` — ≤ 256 байт, `project`/`workdir` — ≤ 4096 байт, `rules` surface — ≤ 16 KiB, `media_type` блоба — ≤ 255 байт.

Политика и изоляция:

- `Policy` по доменам; `Service` видит только свои surface — фильтр в `WHERE`.
- `chats.list`/`chats.get`/`chats.create`/`persons.resolve` не выдают сервису чужие surface, папки и алиасы: чужое и несуществующее отвечают одинаковым `forbidden`, курсор `after` проверяется той же видимостью.
- Правила surface и описания тулов отвергают управляющие и невидимые символы Unicode (Cf, U+2028/U+2029), которыми можно спрятать служебную разметку.

Модули:

- Подсистема `modules`: слушатели uds/ws/wss; id модуля — из токена: имя найденной записи `modules.tokens` или `module_tokens` (blake3, сравнение за постоянное время), его возвращает `welcome.module`; ключ `kernel`/`system`/`local`, пустой токен и один токен у двух ключей — отказ старта, токен короче 32 символов — предупреждение. Вытеснение сессии того же модуля, строка `modules` (connected/offline/forgotten, перевод в forgotten — ежечасно), провайдер запасной двери, рассылка событий `chat.*`/`module.*` по `Policy::events()` — включая события, случившиеся во время handshake. Capability `tools`/`hooks` проверяются и хранятся; вызовы — с M4/M2.
- Неверный токен отвечает `unauthorized` после паузы 250 мс; ошибка БД до принятия токена уходит пиру как `internal error`, подробности — только в лог.
- Отзыв токена во время handshake не даёт сессии; запросы уже закрытой сессии не исполняются; handshake, завершившийся после начала остановки, получает отказ `unavailable`.
- `modules.token_issue`: зарезервированный id — `Aux::Invalid`; модуль из `modules.tokens` («конфиг сильнее») и собственный модуль вызывающего (ротация закрыла бы сессию, получающую единственную копию) — `Aux::Conflict`, ничего не пишется.
- Модуль-провайдер не перехватывает имена моделей, уже обслуживаемые другим провайдером: принимаются только `welcome.models_accepted`.
- Surface — сущность ядра: `surfaces.create {id, name, rules?}` запоминает создателя в `surface.module`, для него идемпотентен (строка не меняется), чужой `id` — `Taken`, `local`/`system` — `forbidden` любому. Обслуживание и `ask_ui` выводятся из живых сессий: создатель обслуживает свои surface, каждый `client` — ещё `local`; реестра нет. `surfaces.update` создателя после реконнекта снова включает обслуживание.
- `chats.persons.master[].module` привязывает surface адреса хозяина к модулю: чужой `surfaces.create` — `forbidden`, пара на surface другого модуля не сидится, пара без привязки — предупреждение на старте.

Квоты модулей:

- `sqlite`: `modules.sqlite_quota` (1 GiB) ограничивает файл базы и временные файлы каждого вызова (сортировки, промежуточные b-tree, журналы) через VFS ядра `eva-module`, сверх — `quota`; PRAGMA вне allow-list, `ATTACH`/`DETACH`/`VACUUM` — `forbidden`, управление транзакцией — `invalid_params`.
- `sqlite.query` принимает только читающие инструкции, ответ ограничен кадром (иначе `quota`), BLOB возвращается как msgpack `bin`; отмена и дедлайн прерывают инструкцию.
- `kv`: `modules.kv_value_max` (1 MiB) на значение и `modules.kv_quota` (64 MiB) на модуль; расход ведёт таблица `module_kv_usage` на триггерах (миграция `0002` заполняет её из существующих строк).
- `blobs`: `work.blob_quota` (4 GiB) на владельца surface, каждый живой блоб стоит не меньше 4 KiB; загрузка принимается в `blob_dir/.staging` и не оставляет файлов при отмене, остатки подметаются на старте.
- `fs`: `modules.fs_max_entries` (100 000) и `modules.fs_quota` без гонок параллельных записей; `fs.read` укладывается в кадр с конвертом.

Конфигурация и запуск:

- Секреты `llm.providers.*.api_key`, `modules.tokens.*.token`, `hands.endpoints.*.key` — `eva::utils::SecretString` (`{plaintext}` | `{file}`): читаются один раз на старте, хвостовой перевод строки срезается, ошибка — отказ старта с путём ключа (`llm.providers.<name>: api_key: …`).
- Провайдеры `llm.providers`: встроенные через `eva_providers::build`, `kind: module` — на handshake модуля; поздний список моделей встроенного провайдера (`settle`) доходит до работающего реестра.
- `system.tz`: `UTC`/`Etc/UTC`, `±HH:MM` или имя IANA из встроенной базы зон; сутки счётчиков и `spend` учитывают DST (`eva_day`), неизвестная зона — отказ старта.
- Бинарь: `eva::cli::App` (env `EVA_`), `Supervisor` с компонентами `db` и `modules`, штатная остановка по SIGINT/SIGTERM с `PRAGMA optimize` и чекпойнтом WAL — после того, как `modules` закрыл сессии и дождался их уборки (строки `offline` записаны).
- Счётчики здоровья модулей (включая `modules.rpc_errors` и `modules.forbidden`), личностей, переносов trust и инференса.
- NixOS-модуль: `services.eva-kernel.secrets` отдаёт файлы через `LoadCredential`; assertion отвергает `plaintext` в `settings`, переменные `EVA_*__PLAINTEXT` и файлы из nix store в `extraConfigFiles`/`environmentFiles`. Слушатель по умолчанию — `/run/eva-kernel/modules.sock` с группой `eva`: локальным модулям нужен `SupplementaryGroups = [ "eva" ]`.

### Протокол и транспорт

- `eva-protocol`: wire-кадры (payload вызова — отдельно закодированный msgpack `bin`: битый payload валит только свой вызов), handshake (`hello` без имени модуля; `welcome` с `module`, `kind`, `resumed`), capabilities (`provider`, `tools`, `events`, `hooks`, `ask`; surface — не capability), коды ошибок `HasCode` для всех `Err`, терпимость к незнакомым вариантам, байтовый newtype `Bytes`. `Effort` читается именем и номером 1..=4, пишется именем, иное — ошибка; `types::Value` (msgpack с `bin`) — параметры `sqlite` и значения `kv`: BLOB пишется и читается без base64.
- `eva-rpc`: симметричный пир (вызовы, потоки, загрузки, отмена, ping, буфер на поток), окно идемпотентности с `hello.resume` (продолжено ли оно — `welcome.resumed`), причина закрытия доходит до пира раньше концов его потоков, незнакомые ключи capability — в `warnings`.
- `eva-rpc`, лимиты: `hello` больше `max_hello` (4 MiB, не больше `max_frame`) — `close{invalid_params}` вместо обрыва, клиент проверяет свой `hello` до сети; до `welcome` — не больше 64 соединений и 8 из одной сети (IPv4 — адрес, IPv6 — /64), новичок вытесняет старейшее молчащее; подключение транспорта ограничено `handshake_timeout`.
- `eva-rpc`, окна: новый `hello` без `resume` отвязывает прежние окна модуля (повтор в них — `conflict`, не второе исполнение); хранимые ответы всех окон ≤ 64 MiB, сверх забываются старейшие у самого толстого окна.
- `eva-rpc`, загрузки: чанк больше `blob_chunk` — `invalid_params`; непрочитанные чанки всех загрузок сессии ≤ `stream_buffer × blob_chunk`, сверх — `quota` этой загрузке.
- `eva-rpc`: повторная запись в соединение после ошибки записи возвращает ошибку, а не паникует.
- `eva-rpc`, wss: клиент всегда строит свой rustls-конфиг на ring — с `extra_ca` или с корнями `webpki-roots`, — и не зависит от провайдера процесса по умолчанию.
- `eva-core`: `prelude` со всеми трейтами неймспейсов.

### SDK (`eva-sdk`)

- `Module` → `Connected`: ожидание ядра до `connect_wait`, реконнект `1s..60s` с `hello.resume`, остановка при вытеснении и при `hello` больше своего `max_hello`; билдеры тулов, хуков, событий, провайдера и `.ask(bool)`. `Connected::module()` — id из `welcome`. Surface модуль создаёт сам — `kernel().surfaces().create()` (идемпотентно), после реконнекта регистрировать нечего. `module.token` — `SecretString`.
- `Kernel = Session<LiveSession>`: он и `ToolCtx::kernel()` следуют за реконнектом: вызов ждёт новую сессию до 30s; оборванные чтения и открытия потоков повторяются, оборванные мутации — с тем же id запроса, если ядро продолжило окно идемпотентности (`welcome.resumed`), иначе (рестарт ядра) — `Aux::Unavailable` с неизвестным исходом; вызов, оборванный закрытием соединения (включая вытеснение), падает как `Aux::Unavailable`. Паника тула — `internal`, повтор того же `call_id` получает тот же итог.

### Провайдеры

- `eva-provider-openai`: OpenAI-совместимый Chat Completions, первым OpenRouter; ошибка `api_key` не зависит от TLS-провайдера процесса.
- `eva-provider-mock`: сценарии, запись запросов, поздний список моделей через `settle`.
- `eva-providers`: конфиг под тегом `kind`; ошибка сборки называет ключ `llm.providers.<name>`.

### Тесты

- Контракт `tests/contract.rs`: набор операций M1 в процессе, по UDS через SDK и через SDK `Kernel` даёт побайтно равные ответы; изоляция сервиса по UDS (алиасы, папки, курсоры, персоны); два клиента на `local`; вытеснение; реконнект; чужой токен.
- Матрица `Policy` полна: каждая операция × {kernel, client, свой сервис, чужой сервис, сервис против `local`/`system`}; тип шага каждой операции закреплён таблицей в `core`.
- Lint слоёв: SQL только в `db/`, эндпоинты только в `handling/`, `Caller` только в политике и handshake; lint вендор-строк.
- `eva-protocol`: образцы покрывают все варианты кадров, событий и элементов по JSON-схеме; proptest-круги msgpack; каждый вариант `Aux` проходит через `RpcError`.
- Схема = снапшот, STRICT, CHECK ⇔ enum; счётчик `module_kv_usage` равен полному пересчёту (proptest).
