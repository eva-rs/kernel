# Конфигурация

Справочник всех секций и ключей, которые читает бинарь `eva-kernel`. Источник истины — структуры в `services/kernel/src/config/`, `rpc/src/config.rs` и `providers/*/src`; значения по умолчанию ниже взяты оттуда.

## Загрузка

```sh
eva-kernel --config base.yaml --config secrets.toml
```

| Правило | Поведение |
|---|---|
| Форматы | по расширению: `.yaml`/`.yml` (YAML с merge-ключами `<<`), `.json`, `.toml`; иное расширение или каталог — ошибка старта |
| Несколько `--config` | сливаются в порядке аргументов: более поздний файл сильнее; словари сливаются рекурсивно по ключам, скаляры и списки заменяются целиком |
| Окружение | префикс `EVA_`, вложенность через `__`; применяется последним и сильнее любого файла |
| Регистр | имена переменных приводятся к нижнему регистру: `EVA_DB__BUSY_TIMEOUT` → `db.busy_timeout` |
| Значения из env | разбираются как структурные: числа, `true`/`false`, списки `[a, b]`, словари `{k=v}`; строки с числами приводятся к нужному типу |
| Обязательность | секция `llm` обязательна; остальные необязательны и целиком берут умолчания |
| Незнакомые ключи | молча игнорируются (опечатка не даёт ошибки); исключение — `models.<id>` провайдера `openai` и его `caps` |

Из-за приведения регистра ключ словаря, заданный в файле с заглавными буквами (например, имя провайдера `Router`), из окружения не адресуется. Имена в `llm.providers`, `modules.tokens`, `hands.endpoints` лучше писать строчными.

```sh
EVA_LLM__DEFAULT_MODEL=vendor/model
EVA_MODULES__LISTEN='[{ws="127.0.0.1:7700"}]'
EVA_SYSTEM__LOGGING__DEFAULT_LEVEL=debug
```

### Форматы значений

| Тип | Запись |
|---|---|
| длительность | строка humantime: `500ms`, `30s`, `5m`, `2h`, `7days`, `1h 30m` |
| размер | строка `64MiB`, `1GiB`, `16 MB` или число байт |
| путь | строка; относительный путь сокета UDS отсчитывается от `system.state_dir` |
| перечисления | `snake_case`: `ephemeral`, `single_threaded`, `openrouter` |

## Секреты

Все секреты — `eva::utils::SecretString`: `{file: "/путь"}` или `{plaintext: "…"}`. Читаются один раз при старте; у ключей ядра хвостовые пробельные символы (перевод строки в конце файла) срезаются; ошибка чтения — отказ старта с путём ключа.

| Ключ | Что |
|---|---|
| `llm.providers.<name>.api_key` (`kind: openai`) | ключ API; ошибка — `llm.providers.<name>: api_key: …` |
| `modules.tokens.<module>.token` | токен модуля; пустой — отказ старта |
| `hands.endpoints.<name>.key` | pre-shared key демона рук; пустой — отказ старта |
| `system.logging.append[].otel.basic_auth` | учётка OTel-экспорта (читает фреймворк; только при сборке с `otel`) |

`Debug` и `Display` секрета — `<secret>`, в логи значение не попадает. Секрет держат вне мирочитаемых мест (Nix store, репозиторий): формой `{file: …}` (например, credential systemd `/run/credentials/<unit>/<name>` — так делает NixOS-модуль, см. [NixOS](#nixos)), отдельным `--config`-файлом с правами только для сервиса или окружением:

```sh
EVA_LLM__PROVIDERS__ROUTER__API_KEY__FILE=/run/credentials/eva-kernel.service/router
EVA_MODULES__TOKENS__PHONE__TOKEN__FILE=/run/credentials/eva-kernel.service/phone
EVA_HANDS__ENDPOINTS__LAPTOP__KEY__FILE=/run/credentials/eva-kernel.service/laptop
```

Форма `plaintext` (`…__PLAINTEXT` в окружении) допустима только вне Nix: NixOS-модуль отвергает её и в `settings`, и в `environment`.

## Минимальный конфиг

```yaml
llm:
  providers:
    router: {kind: openai, api_key: {file: /run/credentials/eva-kernel.service/router}}
  default_model: vendor/model
modules:
  tokens:
    phone: {token: {file: /run/credentials/eva-kernel.service/phone}, kind: client}
```

Без ключей и сети — провайдер `mock` (см. [Провайдеры](providers.md)):

```yaml
llm:
  providers:
    local: {kind: mock}
  default_model: mock
```

## `system`

| Ключ | Тип | По умолчанию | Смысл |
|---|---|---|---|
| `state_dir` | путь | первый элемент `$STATE_DIRECTORY` (systemd, список через `:`), иначе `/var/lib/eva` | корень состояния: база, blob-store, сокет модулей, песочницы модулей |
| `tz` | строка \| null | null — зона хоста | зона календарных суток для счётчиков и разрезов `stats`: `UTC`/`Etc/UTC`, фиксированное смещение `±HH:MM` (не больше 18 ч) или имя зоны IANA (`Europe/Berlin`) из встроенной базы зон, с переходами на летнее время. Иное значение — ошибка старта |

### `system.rt` (фреймворк)

| Ключ | По умолчанию | Смысл |
|---|---|---|
| `flavor` | `{worker_threads: {threads: thread_per_cpu}}` | `single_threaded` — однопоточный рантайм tokio; `{worker_threads: {threads: N}}` — N рабочих потоков (`N = 1` равносильно однопоточному); `thread_per_cpu` — по потоку на CPU |

### `system.logging` (фреймворк)

| Ключ | Тип | По умолчанию | Смысл |
|---|---|---|---|
| `enabled` | bool | `true` | `false` отключает весь вывод |
| `default_level` | уровень | `info` | `trace`, `debug`, `info`, `warn`, `error`, `disabled` |
| `components` | `{имя: уровень}` | `{}` | уровень записей внутри компонента супервизора; у ядра компоненты `db` и `modules` |
| `targets` | `{target: уровень}` | `{}` | жёсткий потолок для log target (путь модуля Rust, например `eva_kernel::db`); сильнее `components` |
| `append` | список | `[stderr]` | приёмники: `stderr`, `stdout`, `{file: {base_dir, file_name}}` (JSON-строки, суффикс `.log`), `{otel: {name, endpoint, labels?, basic_auth?}}` (OTLP HTTP; бинарь `eva-kernel` собран без feature `otel` фреймворка: такой приёмник валит старт) |

Ошибка построения любого приёмника валит старт.

## `db`

| Ключ | Тип | По умолчанию | Смысл |
|---|---|---|---|
| `path` | путь \| null | `<state_dir>/eva.sqlite` | файл SQLite; `:memory:` — база в памяти (тесты) |
| `busy_timeout` | длительность | `5s` | ожидание блокировки SQLite |
| `readers` | целое 1..=64 | `4` | потолок читающих соединений; сверх него запрос ждёт свободное до `busy_timeout`, затем получает ошибку БД. Писатель один |

## `llm`

Секция обязательна. Реестр моделей строится из деклараций провайдеров (`Provider::models()`), конфиг лишь выбирает провайдеров и фильтрует модели — см. [Провайдеры](providers.md).

| Ключ | Тип | По умолчанию | Смысл |
|---|---|---|---|
| `providers` | `{имя: провайдер}` | — (обязателен) | провайдеры; вид задаёт `kind`: `openai`, `mock` или `module` |
| `default_model` | id модели | — (обязателен) | модель по умолчанию; отсутствие её в реестре на старте — предупреждение, не ошибка |
| `default_effort` | `low` \| `medium` \| `high` \| `max` | `medium` | глубина размышления по умолчанию для туров (M2) |
| `default_thinking` | `adaptive` \| `off` | `adaptive` | режим размышления по умолчанию для туров (M2) |
| `models` | список id \| null | null — все | глобальный allow-list поверх деклараций всех провайдеров |
| `roles.summary` | id \| null | null | модель суммаризации (M2) |
| `roles.title` | id \| null | null | модель заголовков чатов (M2) |
| `roles.cheap` | id \| null | null | модель `inference.complete` без явной `model`; null — самая дешёвая по `price.input` не-эмбеддинговая модель реестра, без цен — `default_model` |
| `roles.smartest` | id \| null | null | самая сильная модель (M2) |
| `fallback` | список id | `[]` | цепочка запасных моделей при `auth`/`not_found`/`quota` (M2) |
| `retries` | u32 | `3` | повторы шага при `transient` (M2) |
| `retry_backoff` | длительность | `1s` | база экспоненциальной паузы повторов (M2) |
| `provider_wait` | длительность | `2m` | сколько тур и планировщик ждут провайдера модели, которого ещё нет в реестре (M2) |
| `currency` | строка | `USD` | валюта цен `ModelDecl.price` и строк расхода; ядро не конвертирует, это метка |

Ключи с пометкой M2 читаются и проверяются уже сейчас, но действуют с появлением движка туров.

### `llm.providers.<name>`

| `kind` | Ключи | Глава |
|---|---|---|
| `openai` | `base_url`, `api_key`, `proxy`, `timeout`, `dialect`, `allow`, `models`, `referer`, `title` | [Провайдеры → openai](providers.md#провайдер-openai) |
| `mock` | `models`, `decl`, `on_exhausted` | [Провайдеры → mock](providers.md#провайдер-mock) |
| `module` | `allow` (список id \| null) | [Провайдеры → kind: module](providers.md#kind-module) |

Неизвестный `kind` — ошибка старта с его именем.

## `modules`

Слушатели и транспорт RPC модулей, токены и лимиты пространств модулей. Протокол — в главе [Протокол модулей](protocol.md).

| Ключ | Тип | По умолчанию | Смысл |
|---|---|---|---|
| `listen` | список слушателей | `[{uds: modules.sock, mode: 0o660, group: eva}]` | см. ниже; заданный список заменяет умолчание целиком. NixOS-модуль ставит своё умолчание (см. [NixOS](#nixos)) |
| `tokens` | `{модуль: {token, kind}}` | `{}` | статические токены; ключ — id модуля (ядро узнаёт модуль по токену), `token` — секрет (см. [Секреты](#секреты)), `kind`: `client` (клиент хозяина) или `service`. Токен из конфига сильнее выданного операцией `modules.token_issue`. Ошибка старта: ключ вне `[a-z][a-z0-9_-]{0,63}`, ключ `kernel`/`system`/`local`, пустой токен, один токен у двух ключей. Токен короче 32 символов — предупреждение |
| `call_reconnect_grace` | длительность | `30s` | сколько незавершённый `tools.call` ждёт реконнекта модуля (M4) |
| `offline_forget` | длительность | `7d` | модуль offline дольше — `forgotten`; проверяется на старте и раз в час |
| `hooks.timeout` | длительность | `2s` | потолок таймаута хука модуля (хуки вызываются турами, M2) |
| `fs_quota` | размер | `1GiB` | байтовая квота файлового пространства модуля (`fs.*`) |
| `fs_max_entries` | u32 | `100000` | файлов и каталогов в пространстве модуля: пустые записи байтовая квота не видит |
| `sqlite_max_rows` | u32 | `10000` | потолок строк ответа `sqlite.*`; перебор — `quota` |
| `sqlite_busy_timeout` | длительность | `5s` | busy timeout базы модуля `<state_dir>/modules/<module>/state.sqlite` (`sqlite.*`) |
| `sqlite_quota` | размер | `1GiB` | потолок базы модуля; столько же — её временной базе и временным файлам каждого вызова (сортировки, промежуточные b-tree, журналы операторов); сверх — `quota` |
| `kv_quota` | размер | `64MiB` | ключи и значения `kv.*` одного модуля вместе, плюс 64 байта на запись |
| `kv_value_max` | размер | `1MiB` | потолок одного значения `kv.set` |

### Слушатели `modules.listen`

Вид слушателя задаётся ключом адреса:

| Форма | Поля |
|---|---|
| `{uds: <путь>, mode?, group?}` | Unix-сокет; относительный путь — от `system.state_dir`. `mode` — число или восьмеричная строка (`"0660"`, `"0o660"`), по умолчанию `0o660`; `group` — имя или gid, ставится после bind; неизвестная группа — ошибка старта |
| `{ws: <addr:port>}` | WebSocket без TLS; HTTP нужен только для Upgrade на пути `/v1/modules` |
| `{wss: <addr:port>, tls: {cert, key}}` | WebSocket поверх TLS; `cert` — PEM-цепочка, `key` — PEM-ключ |

Занятый адрес валит старт процесса. Путь сокета ограничен ядром ОС (на Linux — 108 байт вместе с завершающим нулём): относительный `uds` под длинным `state_dir` может не влезть, и старт падает с `failed to listen on …`.

### Транспорт

Ключи лежат плоско в секции `modules` (те же типы использует SDK модуля).

| Ключ | Тип | По умолчанию | Смысл |
|---|---|---|---|
| `handshake_timeout` | длительность | `10s` | TLS, Upgrade и `hello` должны уложиться в это время |
| `ping_interval` | длительность | `15s` | период `ping` и heartbeat долгих потоков |
| `ping_timeout` | длительность | `45s` | тишина дольше — соединение закрывается, сессия offline |
| `request_timeout` | длительность | `60s` | ответ на не-потоковый запрос и первый кадр потока |
| `stream_idle_timeout` | длительность | `2m` | поток без кадров дольше — `cancel` и `timeout` |
| `stream_buffer` | целое 1..=1024 | `64` | буфер получателя на поток; переполнение рвёт только этот поток |
| `max_inflight` | целое 1..=4096 | `64` | одновременных не-потоковых запросов и загрузок сессии; сверх — `quota` |
| `max_streams` | целое 1..=4096 | `256` | одновременных потоков сессии; сверх — `quota` |
| `max_frame` | размер | `16MiB` | кадр больше — соединение закрывается с `invalid_params` |
| `max_hello` | размер | `4MiB` | потолок кадра `hello`, не больше `max_frame`; больше — отказ `invalid_params`, на который SDK не переподключается |
| `blob_chunk` | размер | `1MiB` | потолок чанка `blobs.put`; больше — `invalid_params`. Непрочитанные чанки всех загрузок сессии вместе — не больше `stream_buffer × blob_chunk`, сверх — `quota` этой загрузке |
| `idempotency_window` | длительность | `10m` | сколько помнится ответ мутирующего запроса по `(session_id, request.id)` |
| `proto_min_major` | u16 \| null | null — только текущий | наименьший принимаемый MAJOR версии протокола в `hello` |

Фиксированные лимиты, не настраиваемые конфигом, — в главе [Безопасность](security.md#транспорт).

## `chats`

| Ключ | Тип | По умолчанию | Смысл |
|---|---|---|---|
| `default_kind` | `ephemeral` \| `persistent` | `ephemeral` | вид чата `chats.create` без явного `kind` |
| `persons.master` | список `{surface, external_id, module?}` | `[]` | адреса хозяина на поверхностях; сидируются в личность хозяина при старте. Адрес на ещё не созданной surface ждёт её `surfaces.create` |
| `persons.master_name` | строка | `Master` | имя личности хозяина при сиде |

`module` в адресе хозяина привязывает его surface к модулю: создать её может только этот модуль, чужой `surfaces.create` получает `forbidden`, а адрес на surface, уже созданной другим модулем, не сидится (предупреждение в логе). Адрес без `module` на не-встроенной surface — предупреждение на старте: такую surface может создать любой служебный модуль. `module` у встроенных `local`/`system` — ошибка старта.

## `hands`

Машины, на которых работают «руки» агента: локальная (`Hands::local`) или удалённый демон [`eva-hands`](https://git.desu.church/eva/hands).

| Ключ | Тип | По умолчанию | Смысл |
|---|---|---|---|
| `endpoints` | `{имя: endpoint}` | `{}` | машины рук |
| `default_endpoint` | строка \| null | null | машина чатов без явной рабочей машины; имя вне `endpoints` — предупреждение на старте, у таких чатов рук нет |

Формы endpoint:

| Форма | Смысл |
|---|---|
| `{local: true, limits?}` | руки в процессе ядра; `local: false` — ошибка старта |
| `{address: "host:port", key: {file: "…"}}` | демон `eva-hands` по Noise с pre-shared key (`key` — секрет, см. [Секреты](#секреты)); недоступный демон старт не держит |

`limits` локальной машины:

| Ключ | Тип | По умолчанию | Смысл |
|---|---|---|---|
| `max_read` | u64, байты | `67108864` (64 MiB) | потолок чтения |
| `max_write` | u64, байты | `268435456` (256 MiB) | потолок записи |
| `max_hits` | u64 | `4096` | потолок совпадений поиска |
| `max_output` | u64, байты | `1048576` (1 MiB) | потолок вывода команды |
| `exec` | bool | `true` | разрешён ли запуск команд |
| `exec_timeout` | длительность | `60s` | таймаут команды по умолчанию |
| `exec_max_timeout` | длительность | `1h` | предельный таймаут команды |
| `fetch` | bool | `true` | разрешён ли выход в сеть |
| `roots` | список путей | `[]` — вся ФС | лексическая ограда путей, не песочница: симлинки не разрешаются |

## `work`

| Ключ | Тип | По умолчанию | Смысл |
|---|---|---|---|
| `blob_dir` | путь \| null | `<state_dir>/blobs` | каталог blob-store |
| `blob_keep` | длительность | `30d` | срок жизни blob'а, загруженного `blobs.put` |
| `blob_max` | размер | `64MiB` | дисковый лимит одного blob'а |
| `blob_quota` | размер | `4GiB` | суммарный вес живых blob'ов чатов одного владельца surface; каждый blob весит не меньше 4 KiB; surface без модуля делят одну квоту |

## `notes`

| Ключ | Тип | По умолчанию | Смысл |
|---|---|---|---|
| `enabled` | bool | `true` | `false` — блокнота нет, `notes.list` отвечает `unavailable` |
| `budget_percent` | целое 1..=50 | `1` | бюджет блокнота в процентах от `context_window` модели чата |
| `budget_max_tokens` | u32 | `4000` | абсолютный потолок бюджета |

## NixOS

Флейк экспортирует `nixosModules.default` с опциями `services.eva-kernel`:

| Опция | Смысл |
|---|---|
| `settings` | несекретная часть конфига; рендерится в YAML в Nix store (мирочитаемо) |
| `secrets` | `{"<ключ.через.точку>" = "<файл>";}`: systemd отдаёт файл через `LoadCredential`, в `settings` встаёт `{file = "/run/credentials/eva-kernel.service/<ключ>";}` |
| `extraConfigFiles` | дополнительные `--config`, сливаются поверх `settings` в рантайме |
| `environment`, `environmentFiles` | переменные окружения и `EnvironmentFile` systemd |
| `user`, `group` | по умолчанию `eva-kernel`; модуль их не создаёт |

Модуль задаёт `StateDirectory=eva-kernel` (отсюда `state_dir` по умолчанию — `/var/lib/eva-kernel`), `RuntimeDirectory=eva-kernel`, `TimeoutStopSec=180`, значение по умолчанию `modules.listen = [{uds = "/run/eva-kernel/modules.sock"; group = "eva";}]`, группу `users.groups.eva` и `SupplementaryGroups = [ "eva" ]` сервиса. Assertions отвергают `plaintext`-секреты в `settings`, переменные `EVA_*__PLAINTEXT` в `environment` и пути из Nix store в секретах, `extraConfigFiles` и `environmentFiles`. Подробно — в [Быстром старте](quickstart.md#nixos).
