# eva-kernel

Ядро Евы: библиотека `Kernel<C>: SessionMaker` и бинарь `eva-kernel`. Один процесс поверх одного SQLite-файла реализует все операции библиотечного API над своей БД, проверяет права вызывающего и обслуживает модули по RPC. Построено на фреймворке [eva](https://git.desu.church/VienDesu/eva); руки — библиотека [eva-hands](https://git.desu.church/eva/hands).

## Место в слоях

Верхний слой: зависит от `eva-protocol`, `eva-core`, `eva-rpc`, `eva-providers` и `eva-hands`; от него не зависит ни один крейт (`eva-sdk` — только dev-зависимость контрактных тестов). Vendor- и surface-строк здесь нет: провайдеры приходят готовыми из `eva-providers`.

Раскладка внутри:

| Путь | Что |
|---|---|
| `handling/` | хендлер на операцию (`#[eva::endpoint]` + `#[fastrace::trace]`), файл на домен |
| `impls/` | `impl <Namespace> for Session<C>`: строка на операцию, пропуск — ошибка компиляции |
| `state/policy/` | `Policy` — единственное место авторизации; чужой объект — `forbidden`, не `not_found` |
| `db/` | весь SQL, по файлу на домен; `migrate`, `seed`, пул `sqlite`, `quota_vfs` |
| `convert/` | строка БД → тип протокола |
| `subsystems/modules/` | слушатели, handshake (`KernelAuth`), живые сессии, рассылка событий |
| `config/` | секции конфига, по файлу на секцию |
| `providers.rs`, `registry.rs` | `KernelProvider` (встроенный или модуль) и реестр моделей из `Provider::models()` |
| `hands.rs` | endpoint'ы рук из `hands.endpoints` |
| `components.rs`, `world.rs` | `Components` (что подменяет тест) и `Demiurge` — часы и rng операции |
| `markers.rs` | реестр служебного текста ядра |

Операции поздних этапов отвечают `unavailable` с маркером в тексте: `turns.*`, `chats.ingest`, `chats.compact`, `chats.notify` — `M2_PENDING`; `chats.search` — `M3_PENDING`; `tools.catalog` — `M4_PENDING`.

## Бинарь

```sh
eva-kernel --config eva.yaml [--config more.yaml …]
```

- `--config`/`-c` повторяется; YAML, JSON, TOML по расширению; поздний файл сильнее.
- Переменные `EVA_…` сильнее файлов, вложенность — через `__`: `EVA_MODULES__TOKENS__PHONE__TOKEN__FILE=/run/secrets/eva-phone`.
- Старт: провайдер rustls процесса → открытие БД → встроенные провайдеры (каталоги моделей) → endpoint'ы рук из `hands.endpoints` (чтение ключей) → проверка `system.tz`, токенов и `chats.persons.master` → миграции → сид → сброс состояний модулей прошлого процесса → greeting рук → чистка брошенных загрузок blob'ов → реестры → привязка слушателей `modules.listen`. Занятый сокет роняет процесс до старта компонентов.
- Остановка (`Supervisor` фреймворка): первый SIGINT/SIGTERM сворачивает компоненты штатно, второй обрывает их сразу; код выхода в обоих случаях 0, процесс ждёт уже начатые блокирующие операции (БД, файлы). Под systemd модуль NixOS ставит `TimeoutStopSec = 180`.

## Конфиг

Секция на компонент; `llm` обязательна, остальные с дефолтами. Все ключи — [Конфигурация](../../docs/src/config.md).

| Секция | Ключи |
|---|---|
| `system` | `state_dir` (`$STATE_DIRECTORY`, иначе `/var/lib/eva`), `tz`; `rt`, `logging` читает фреймворк |
| `db` | `path` (`<state_dir>/eva.sqlite`; `:memory:` — в памяти), `busy_timeout` (`5s`), `readers` (`4`) |
| `llm` | `providers.<name>`, `default_model` — обязательны; `default_effort`, `default_thinking`, `models`, `roles`, `fallback`, `retries` (`3`), `retry_backoff` (`1s`), `provider_wait` (`120s`), `currency` (`USD`) |
| `modules` | `listen`, ключи транспорта `eva-rpc`, `tokens.<id> = {token, kind}`, `call_reconnect_grace`, `offline_forget`, `hooks.timeout`, квоты `fs_*`, `sqlite_*`, `kv_*` |
| `chats` | `default_kind` (`ephemeral`), `persons.master` (`[{surface, external_id, module?}]`), `persons.master_name` |
| `hands` | `endpoints.<name> = {local: true, limits?}` \| `{address, key}`, `default_endpoint` |
| `work` | `blob_dir` (`<state_dir>/blobs`), `blob_keep` (`30d`), `blob_max` (`64 MiB`), `blob_quota` (`4 GiB`) |
| `notes` | `enabled` (`true`), `budget_percent` (`1`), `budget_max_tokens` (`4000`) |

`llm.providers.<name>` с `kind: module` (провайдер — подключённый модуль с capability `provider`, опционально `allow`) разбирает ядро; остальные `kind` — [`eva-providers`](../../providers/all/README.md).

`modules.tokens`: ключ записи — id модуля (`[a-z][a-z0-9_-]{0,63}`; `kernel`, `system`, `local` зарезервированы), `kind` — `client` или `service`. Секреты (`token`, `api_key`, `key`) — `{file: …}` или `{plaintext: …}`; читаются один раз на старте, в памяти от токенов остаётся только blake3. Пустой токен, общий токен у двух записей, нечитаемый файл — отказ старта с путём ключа.

## Компоненты `Supervisor`

| Компонент | Секция | Что делает |
|---|---|---|
| `modules` | `modules` | принимает соединения на слушателях, handshake по токену, RPC-сервер над `Kernel`, живые сессии, вытеснение, провайдеры модулей, рассылка событий |
| `db` | `db` | держит базу до остановки; финальный checkpoint WAL — только после завершения `modules`, потому что сессии модулей пишут в базу до своего закрытия |

Миграции, сид и реестры выполняет `Kernel::start` до компонентов.

## База и миграции

- SQLite в режиме WAL: один писатель (`BEGIN IMMEDIATE`, за async-мьютексом) и пул до `db.readers` читателей (`query_only`); каждый запрос — в `spawn_blocking`. Трейт `db::Db` позволяет тесту подменить базу.
- Запросы домена — синхронные методы над `&Connection` (`conn.surfaces().get(..)`): одни и те же работают на читателе и в транзакции писателя.
- Миграции — `db/migrations/NNNN_<name>.sql`, встроены в бинарь и перечислены в `migrate::MIGRATIONS`; только вперёд. Применённые обязаны быть префиксом встроенных с тем же blake3, иначе старт отказывает: бинарь старее базы, файл правили задним числом или база чужая.
- Сид идемпотентен и гоняется на каждом старте: встроенные surface `local` и `system` с их папками, служебная личность и личности хозяина из `chats.persons`.
- `sqlite.*` модулей живут в отдельных файлах под квотой (`quota_vfs`), не в базе ядра.

## Тесты

```sh
nix develop -c cargo test -p eva-kernel
nix develop -c cargo insta review          # обновить снимки
```

Харнесс — `tests/common/mod.rs`: `Kernel<Test>` на временном каталоге без транспорта.

| Часть | Что |
|---|---|
| `Test: Components` | часы, которые двигает тест; `FaultyDb` с отказом по флагу; `MockProvider`; `CountingHands` — `Hands::local` в песочнице со счётчиком и инъекцией `Unavailable` |
| `common::start()`, `start_with(tweak)`, `start_with_mock`, `start_with_provider` | ядро с конфигом по умолчанию или поправленным |
| `h.session(caller)`, `h.client_session()`, `h.kernel_session()` | сессии под `Caller`; `client(name)`, `service(name)` |
| `h.register_module(name)`, `h.service_with_surface(name)` | модуль в `modules` и сервис со своим surface |
| `h.listen()` | слушатели поверх того же ядра и `uds://` URL для модулей на `eva-sdk`; токены — `with_module_tokens`, `token(name)` |
| `h.clock`, `h.db_fail`, `h.provider` | время, отказ БД, сценарии и записанные запросы провайдера |

Наборы: по файлу на домен (`surfaces.rs`, `chats.rs`, …) и правила доступа (`policy_*.rs`); `k3_policy_all.rs` — полная матрица операция × caller, новая операция без строки роняет тест; `contract.rs` — одни и те же операции в процессе и по UDS через `eva-sdk` дают побайтно одинаковый msgpack; `k2_handshake.rs` — гонки handshake; `k2_runtime.rs` — компонент `modules` вне handshake: сутки `system.tz`, проверки конфига на старте, забывание offline-модулей; `k1_readers.rs` — пул читателей; `k3_db_failure.rs` — отказ SQLite доезжает ровно `Aux::Db`; `migrations.rs`, `schema.rs` (снимок схемы), `seed.rs`, `secrets.rs`, `config.rs`; линтеры `k3_layer_lint.rs` (SQL только в `db/`, хендлеры только в `handling/`, авторизация только в `Policy`) и `vendor_lint.rs` (ни одной vendor- или surface-строки вне `providers/*`, включая этот файл).

## Документация

[Архитектура](../../docs/src/architecture.md), [Конфигурация](../../docs/src/config.md), [Безопасность](../../docs/src/security.md), [Разработка](../../docs/src/development.md).
