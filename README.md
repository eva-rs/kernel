# Eva kernel

Ядро Евы — персонального агента, который работает постоянно. Один процесс поверх одного SQLite-файла — единственный владелец её состояния: чатов, поверхностей (surface), личностей, памяти, заметок, статистики и расхода. Всё остальное — модули: отдельные процессы, которые говорят с ядром по msgpack-RPC и работают с ним тем же библиотечным API, что и код внутри ядра. Сервисы (мосты в мессенджеры, MCP-обвязки) слушают unix-сокет или TCP, и ядро само звонит им по реестру под Noise; клиенты хозяина (телефон, TUI) звонят ядру сами — через unix-сокет, WebSocket или WebSocket поверх TLS. Инференс — встроенные провайдеры за трейтом `Provider`: OpenAI-совместимый (OpenRouter, либо vLLM и llama-server в диалекте `plain`) и сценарный `mock` для запуска без ключей.

Построено на фреймворке [eva](https://git.desu.church/VienDesu/eva) (конфиг, `Supervisor`, логи, время); руки — библиотека [eva-hands](https://git.desu.church/eva/hands).

## Состояние

Основные подсистемы соединены в ядре. Текущие изменения проходят проверку
компиляции и точечные тесты; полная проверка релиза и интеграция с клиентом
ещё не завершены.

| Этап | Что | Статус |
|---|---|---|
| M1 | библиотечное API, схема SQLite и миграции, RPC-сервер модулей, SDK, провайдеры | готов |
| M2 | туры: диалоговый движок, хуки, `turns.*`, `chats.ingest`/`compact`/`notify` | реализован |
| M3 | память, заметки, гигиена контекста, поиск по чатам (`chats.search`) | реализован |
| M4 | каталог тулов (`tools.catalog`) и вызов тулов модулей, статистика, крон, сон, GC | реализован |
| M5 | рабочие туры: подагенты, руки, план, `ask`, разрешения и вложения | реализован |

Текущее поведение — в главах [Туры](docs/src/turns.md),
[Инструменты](docs/src/tools.md) и [Рабочие операции](docs/src/work.md).

## Крейты

| Крейт | Путь | Назначение |
|---|---|---|
| `eva-protocol` | [`protocol/`](protocol/README.md) | типы операций (`Args`/`Ok`/`Err`), ошибки, проводные кадры; без I/O |
| `eva-kernel-macros` | [`macros/`](macros/README.md) | единственный список операций (`endpoints!` на `eva-api-builder`) |
| `eva-core` | [`core/`](core/README.md) | библиотечное API: `Session`, трейты неймспейсов, `Caller`, трейт `Provider` |
| `eva-rpc` | [`rpc/`](rpc/README.md) | msgpack-кадры: клиенты по uds/ws/wss, сервисы по uds/tcp под Noise (дозвон ядра); сервер-диспетчер, клиентская `RpcSession` |
| `eva-sdk` | [`sdk/`](sdk/README.md) | модуль ядра: сервис слушает звонок ядра, клиент подключается сам; смена соединения, тулы, события, хуки |
| `eva-provider-openai` | [`providers/openai/`](providers/openai/README.md) | OpenAI-совместимый Chat Completions |
| `eva-provider-mock` | [`providers/mock/`](providers/mock/README.md) | сценарный провайдер для тестов и смоука |
| `eva-providers` | [`providers/all/`](providers/all/README.md) | `ProviderConfig` под тегом `kind`, диспетчер `AnyProvider` |
| `eva-kernel` | [`services/kernel/`](services/kernel/README.md) | бинарь и библиотека ядра |

Зависимости (стрелка — «зависит от», транзитивные рёбра опущены); обратных рёбер нет:

```mermaid
graph LR
  core --> protocol
  core --> macros
  macros --> builder[eva-api-builder]
  rpc --> core
  sdk --> rpc
  openai[providers/openai] --> core
  mock[providers/mock] --> core
  all[providers/all] --> openai
  all --> mock
  kernel[services/kernel] --> rpc
  kernel --> core
  kernel --> all
  kernel --> hands[eva-hands]
```

## Сборка и запуск

Сборка — только через Nix: у системного `cargo` нет нужного линкера.

```sh
nix develop                                # devShell: toolchain, sqlite, cargo-insta, mdbook
nix develop -c cargo test --workspace      # тесты
nix build .#eva-kernel                     # бинарь: ./result/bin/eva-kernel
nix develop -c mdbook build docs           # книга: docs/book/index.html
nix develop -c mdbook serve docs           # книга с живой пересборкой
```

```sh
eva-kernel --config eva.yaml [--config more.yaml …]
```

Форматы — YAML, JSON, TOML (по расширению); несколько `--config` сливаются, поздний сильнее. Переменные `EVA_…` сильнее файлов, вложенность — через `__`: `EVA_DB__READERS=8` — это `db.readers`.

## Минимальный конфиг

```yaml
system:
  state_dir: /var/lib/eva            # БД, blob'ы, сокет, песочницы модулей
llm:
  providers:
    router:
      kind: openai                   # base_url по умолчанию — OpenRouter
      api_key: {file: /run/secrets/router-key}
  default_model: vendor/model        # id модели из каталога провайдера
modules:
  listen:
    - uds: modules.sock              # относительный путь — от state_dir
  tokens:                            # клиенты хозяина звонят сами, с токеном
    phone:                           # ключ записи — id модуля
      token: {file: /run/secrets/eva-phone}
  services:                          # сервисы слушают, ядро звонит им само
    bridge:
      address: uds:///run/eva-bridge/module.sock   # или tcp://host:port
      key: {file: /run/secrets/eva-bridge}         # тот же секрет — module.key у модуля
```

Без ключей: `llm: {providers: {mock: {kind: mock}}, default_model: mock}`.

До запуска создайте файл персоны `persona.md` в `system.state_dir`. Его путь
задаёт `context.persona_file`; отсутствие или нечитаемый файл останавливает старт.

Секреты (`llm.providers.*.api_key`, `modules.tokens.*.token`, `modules.services.*.key`, `hands.endpoints.*.key`, `module.token` и `module.key` модуля на SDK) — `{file: /путь}` или `{plaintext: "…"}`. Читаются один раз на старте, хвостовой перевод строки срезается; нечитаемый секрет — отказ старта с путём ключа. Форму `plaintext` держите вне файлов, которые видят другие. Id `kernel`, `system`, `local` зарезервированы.

## NixOS

Флейк — `eva.lib.mkServiceFlake` фреймворка: `nixosModules.default` даёт `services.eva-kernel`, где несекретное — в `settings`, а секреты — `secrets."<ключ>" = <файл>`. systemd отдаёт файл сервису через `LoadCredential`, в конфиг встаёт `{file = "/run/credentials/eva-kernel.service/<ключ>";}`. Пользователя `eva-kernel` и группу `eva` модуль не создаёт — их объявляет хост.

```nix
users.users.eva-kernel = { isSystemUser = true; group = "eva-kernel"; };
users.groups = { eva-kernel = { }; eva = { }; };

services.eva-kernel = {
  enable = true;
  settings = {
    llm.providers.router.kind = "openai";
    llm.default_model = "vendor/model";
    modules.services.bridge.address = "uds:///run/eva-bridge/module.sock";
  };
  secrets = {
    "llm.providers.router.api_key" = "/run/secrets/router-key";
    "modules.tokens.phone.token" = "/run/secrets/eva-phone";
    "modules.services.bridge.key" = "/run/secrets/eva-bridge";
  };
};
```

`settings` попадают в мирочитаемый Nix store, поэтому сборка отказывает, если секрет (`llm.providers.*.api_key`, `modules.tokens.*.token`, `modules.services.*.key`, `hands.endpoints.*.key`) стоит в `settings` формой `plaintext`, и если ключ `secrets` не совпадает ни с одним из этих полей. Права файлов задаёт не код, а модуль: база (в ней ключи сервисов из `modules.add`), blob'ы и песочницы — только пользователю ядра (`StateDirectoryMode = 0700`, `UMask = 0077`); сокет клиентов `/run/eva-kernel/modules.sock` ядро связывает само в своём `RuntimeDirectory`, и при таком `UMask` он доступен только пользователю ядра; сервисы машины слушают `/run/eva-<id>/module.sock` с группой `eva`, ядро в ней состоит. Подробно — [Быстрый старт](docs/src/quickstart.md#nixos).

## Документация

Книга (mdBook) — в [`docs/`](docs/src/SUMMARY.md):

- [Введение](docs/src/introduction.md)
- [Быстрый старт](docs/src/quickstart.md)
- [Архитектура](docs/src/architecture.md)
- [Конфигурация](docs/src/config.md)
- [Библиотечное API](docs/src/api.md)
- [Протокол модулей](docs/src/protocol.md)
- [SDK модулей](docs/src/sdk.md)
- [Провайдеры инференса](docs/src/providers.md)
- [Безопасность](docs/src/security.md)
- [Разработка](docs/src/development.md)
- [Дорожная карта](docs/src/roadmap.md)

Правила для агентов, работающих в репозитории, — [`AGENTS.md`](AGENTS.md).
