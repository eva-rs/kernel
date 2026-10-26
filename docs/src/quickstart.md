# Быстрый старт

## Сборка

Сборка идёт через Nix: у системного `cargo` нет нужного линкера, окружение даёт devShell флейка.

```sh
nix build .#eva-soul                     # бинарь: ./result/bin/eva-soul
nix develop -c cargo build -p eva-soul   # то же из devShell
nix develop -c cargo test --workspace      # тесты всего воркспейса
nix develop -c mdbook build docs           # эта книга: docs/book/index.html
```

Флейк — `eva.lib.mkServiceFlake` фреймворка [`eva`](https://github.com/nerodono/eva): `packages.<system>.eva-soul` (он же `default`), `devShells.default` (с `sqlite`, `cargo-insta`, `mdbook`, `mdbook-mermaid`) и `nixosModules.default`.

## Запуск

```sh
eva-soul --config eva.yaml [--config secrets.yaml …]
```

- Форматы файлов: YAML (`.yaml`/`.yml`), JSON, TOML — по расширению.
- Несколько `--config` сливаются, более поздний файл перекрывает ранний.
- Переменные окружения с префиксом `EVA_` перекрывают все файлы; вложенность — через `__`: `EVA_DB__READERS=8` — это `db.readers`.
- Секция `llm` обязательна, остальные имеют значения по умолчанию. Все ключи — в главе [Конфигурация](config.md).

Остановка: первый SIGINT/SIGTERM штатно сворачивает компоненты (закрывает сессии модулей, затем сбрасывает WAL в основной файл БД), второй обрывает компоненты сразу, без сброса WAL. Код выхода в обоих случаях 0; процесс завершается, когда допишутся уже начатые блокирующие операции (запросы БД, файлы).

## Минимальный конфиг с `mock`

Провайдер `mock` не ходит в сеть и не требует ключей: на любой запрос он отвечает эхом последней реплики. Модели по умолчанию — `mock` (чат) и `mock-embed` (эмбеддинги).

```sh
mkdir -p /tmp/eva-dev
openssl rand -hex 32 > /tmp/eva-dev/phone.token
chmod 600 /tmp/eva-dev/phone.token
cat > /tmp/eva-dev/persona.md <<'EOF'
Ты — Ева, персональный помощник. Отвечай по существу и проверяй результат работы.
EOF
```

```yaml
# eva.yaml
system:
  state_dir: /tmp/eva-dev        # БД, blob'ы, сокет, песочницы модулей
modules:
  listen:
    - uds: modules.sock          # относительный путь — от state_dir
  tokens:
    phone:                       # ключ записи — id клиента
      token: {file: /tmp/eva-dev/phone.token}
llm:
  providers:
    mock: {kind: mock}
  default_model: mock
```

```sh
./result/bin/eva-soul --config eva.yaml
```

Файл персоны обязателен. По умолчанию soul читает `persona.md` внутри
`system.state_dir`; другой путь задаётся `context.persona_file`. Если файл
отсутствует или недоступен, запуск завершается ошибкой.

Слушатель по умолчанию — `modules.sock` в `state_dir`, как и в примере. Soul не ставит права файла сокета: их задают umask процесса или менеджер служб, и тогда soul берёт готовым его сокет с этим путём ([NixOS](#nixos)).

Путь Unix-сокета ограничен ядром ОС (на Linux — 108 байт вместе с завершающим нулём). Относительный `uds` склеивается с `state_dir`, и под длинным `state_dir` путь может не влезть: старт тогда падает с ошибкой `failed to listen on …`. Выход — короче `state_dir` или абсолютный `uds`.

При первом старте soul создаёт `/tmp/eva-dev/eva.sqlite`, применяет миграции и засевает встроенные surface `local` и `system`. В логе появится `soul started`.

## Конфиг с OpenRouter

Провайдер `kind: openai` по умолчанию смотрит в OpenRouter и при старте читает оттуда каталог моделей.

```yaml
llm:
  providers:
    router:
      kind: openai
      api_key: {file: /run/secrets/openrouter}
      title: Eva                 # заголовок X-Title, необязателен
  default_model: vendor/model    # id модели из каталога OpenRouter
  roles:
    cheap: vendor/cheap-model    # для inference.complete модулей
```

Тот же ключ через окружение:

```sh
export EVA_LLM__PROVIDERS__ROUTER__API_KEY__FILE=/run/secrets/openrouter
```

Локальный OpenAI-совместимый сервер (vLLM, llama-server) подключается тем же `kind: openai` с `base_url` и `dialect: plain`; подробности — в главе [Провайдеры инференса](providers.md).

## Секреты

Каждый секрет — `eva::utils::SecretString` фреймворка [`eva`](https://github.com/nerodono/eva) в одной из двух форм: `{file: "/путь"}` или `{plaintext: "…"}`. Читается один раз при старте, хвостовые пробельные символы (перевод строки в конце файла) срезаются; ошибка чтения — отказ старта с путём ключа (`modules.tokens.phone.token: …`), пустой токен клиента, ключ сервиса или ключ рук — тоже.

Предпочтительна форма `file`: в конфиге остаётся только путь. Форма `plaintext` годится лишь там, где значение не попадёт в мирочитаемое место, то есть вне Nix: в Nix store читать может любой пользователь машины, и NixOS-модуль её отвергает (см. ниже).

| Секрет | Ключ | Переменная окружения |
|---|---|---|
| Токен клиента в конфиге soul | `modules.tokens.<module>.token` | `EVA_MODULES__TOKENS__<MODULE>__TOKEN__FILE` |
| Ключ сервиса в конфиге soul | `modules.services.<module>.key` | `EVA_MODULES__SERVICES__<MODULE>__KEY__FILE` |
| Ключ API провайдера `openai` | `llm.providers.<name>.api_key` | `EVA_LLM__PROVIDERS__<NAME>__API_KEY__FILE` |
| Ключ удалённого endpoint'а рук | `hands.endpoints.<name>.key` | `EVA_HANDS__ENDPOINTS__<NAME>__KEY__FILE` |
| Токен в конфиге клиента на SDK | `module.token` | `EVA_MODULE__TOKEN__FILE` (если модуль читает env с префиксом `EVA_`) |
| Ключ в конфиге сервиса на SDK | `module.key` | `EVA_MODULE__KEY__FILE` (так же) |

Имена переменных приводятся к нижнему регистру, поэтому `<MODULE>` и `<NAME>` соответствуют строчным ключам конфига. Вне Nix вместо `…__FILE` допустимо `…__PLAINTEXT`.

`Debug` и `Display` секрета — `<secret>`: значение не попадает в логи. Soul держит в памяти только blake3-хэши токенов клиентов и выведенный из ключей сервисов PSK.

## NixOS

```nix
{
  inputs.eva-soul.url = "github:eva-rs/soul";

  outputs = { nixpkgs, eva-soul, ... }: {
    nixosConfigurations.host = nixpkgs.lib.nixosSystem {
      system = "x86_64-linux";
      modules = [
        eva-soul.nixosModules.default
        {
          # модуль не создаёт ни пользователя сервиса, ни группу eva сокетов сервисов
          users.users.eva-soul = { isSystemUser = true; group = "eva-soul"; };
          users.groups = { eva-soul = { }; eva = { }; };

          services.eva-soul = {
            enable = true;
            settings = {
              llm = {
                providers.router.kind = "openai";
                default_model = "vendor/model";
              };
              # сервис этой машины слушает сокет в своём RuntimeDirectory с группой eva
              modules.services.bridge.address = "uds:///run/eva-bridge/module.sock";
            };
            # файл → LoadCredential; в settings встаёт
            # <ключ> = { file = "/run/credentials/eva-soul.service/<ключ>"; }
            secrets = {
              "llm.providers.router.api_key" = "/run/secrets/openrouter";
              "modules.tokens.phone.token" = "/run/secrets/eva-phone";
              "modules.services.bridge.key" = "/run/secrets/eva-bridge";
            };
          };
        }
      ];
    };
  };
}
```

Опции `services.eva-soul`:

| Опция | По умолчанию | Назначение |
|---|---|---|
| `enable` | `false` | Включить сервис |
| `package` | пакет флейка | Какой бинарь запускать |
| `settings` | `{}` | Несекретная часть конфига; рендерится в YAML в Nix store |
| `secrets` | `{}` | Файлы секретов по ключу конфига через точку (`"hands.endpoints.laptop.key" = "/run/secrets/…"`); ключ — одно из полей `llm.providers.*.api_key`, `modules.tokens.*.token`, `modules.services.*.key`, `hands.endpoints.*.key`. systemd отдаёт каждый через `LoadCredential` (файлу хватает прав root), в `settings` встаёт `{file = "/run/credentials/eva-soul.service/<ключ>";}` |
| `extraConfigFiles` | `[]` | Дополнительные `--config` поверх `settings`, читаются при старте (пути sops-nix/agenix); должны быть доступны пользователю сервиса |
| `environment` | `{}` | Переменные окружения |
| `environmentFiles` | `[]` | `EnvironmentFile` systemd |
| `user` / `group` | `eva-soul` | Пользователь и группа сервиса; модуль их **не создаёт** — объявите на хосте |

Что задаёт модуль:

- `StateDirectory = eva-soul`: systemd передаёт `$STATE_DIRECTORY`, и `system.state_dir` по умолчанию становится `/var/lib/eva-soul`.
- `StateDirectoryMode = 0700` и `UMask = 0077`: база (в ней лежат ключи сервисов из `modules.add`), blob'ы и песочницы модулей доступны только пользователю soul. Soul сам не ставит права файлов — ни базе, ни сокетам.
- `RuntimeDirectory = eva-soul` и слушатель по умолчанию `modules.listen = [{uds = "/run/eva-soul/modules.sock";}]`: soul сам связывает сокет клиентов, и при `UMask = 0077` файл сокета доступен только пользователю soul. Нужен сокет для группы — хост объявляет socket-юнит с тем же путём (`systemd.sockets.eva-soul`: `ListenStream`, `SocketMode = 0660`, `SocketGroup`), soul берёт готовый сокет по адресу (`LISTEN_FDS`) вместо своего bind. Если задать `settings.modules.listen`, список заменяется целиком.
- `SupplementaryGroups = [ "eva" ]`: по группе `eva` (её объявляет хост) soul дозванивается до сервисов машины. Сервис слушает `/run/eva-<id>/module.sock`, доступный группе `eva`, а права задаёт его окружение — юнит (`RuntimeDirectory = eva-<id>`, `Group = eva`, `UMask = 0007`; `module.listen = {uds: /run/eva-<id>/module.sock}`) или socket-юнит (`SocketGroup = eva`, `SocketMode = 0660`, `Accept = no`; SDK берёт унаследованный сокет по адресу). Один файл ключа отдаётся обоим юнитам: юниту soul — `secrets."modules.services.<id>.key"`, модулю — `LoadCredential` в `module.key`.
- `TimeoutStopSec = 180`, `Restart = on-failure`, `RestartSec = 5`.

Assertions фреймворка — сборка системы отказывает:

| Что | Почему отказ |
|---|---|
| `settings.llm.providers.*.api_key`, `settings.modules.tokens.*.token`, `settings.modules.services.*.key`, `settings.hands.endpoints.*.key` в форме `plaintext` | `settings` рендерятся в мирочитаемый Nix store; нужен `secrets."<ключ>"` |
| Ключ `secrets`, не совпадающий ни с одним из этих полей | опечатка в ключе тихо оставила бы секрет без адресата |

## Подключение клиентского модуля

Клиент хозяина (телефон, TUI) сам звонит в soul, с токеном. Сервис — наоборот: слушает, а soul набирает его ([ниже](#подключение-сервисного-модуля)).

1. Выдайте клиенту токен. Два способа:
   - **в конфиге soul**: `modules.tokens.<module> = {token}`, `token` — `{file: …}` (на NixOS — через `secrets`). Такой токен сильнее выданного операцией: если конфиг называет модуль, принимается только конфиговый токен;
   - **операцией** `modules.token_issue {module}` от уже подключённого клиента хозяина. Токен показывается один раз, soul хранит только его blake3-хэш. Повторная выдача — ротация: старый токен отзывается, его сессии закрываются. Отказы: id, который называет `modules.tokens` или `modules.services` или занял `modules.add`, — `conflict`; собственный модуль вызывающего — `conflict` (ротация закрыла бы сессию, которая получает единственную копию нового токена); зарезервированный или невалидный id — `invalid_params`.
2. Ключ записи токена — id клиента: soul узнаёт клиента по токену (имени в `hello` нет) и возвращает id в `welcome.module`. Формат `[a-z][a-z0-9_-]{0,63}`; `soul`, `system`, `local` зарезервированы. Зарезервированный или невалидный ключ `modules.tokens`, пустой токен и один токен у двух ключей не дают soul стартовать; токен короче 32 символов — предупреждение в логе.
3. Адрес soul для клиента: `uds:///run/eva-soul/modules.sock`, `ws://host:port/v1/modules` или `wss://host:port/v1/modules`.

Модуль на `eva-sdk`, клиент хозяина на встроенной поверхности `local`:

```yaml
# конфиг модуля
module:
  soul: uds:///tmp/eva-dev/modules.sock
  token: {file: /tmp/eva-dev/phone.token}
```

```rust
use eva_sdk::prelude::*;
use eva_sdk::{ClientConfig, protocol::requests::Surfaces};

let connected = Module::new()
    .ask(true)                     // рисует кнопки вопросов и разрешений
    .connect_config(&cfg)          // cfg: ClientConfig из секции `module`
    .await?;
log::info!(module:% = connected.module(); "connected");   // "phone" — ключ записи токена

let mut soul = connected.soul();
let Surfaces::get::Ok { surface } = soul
    .surfaces()
    .get("local".into())
    .await?;

connected.run(shutdown).await?;
```

Surface не объявляются: soul выводит, кто их обслуживает, из живых сессий. Клиент обслуживает `local`. Служебный модуль создаёт свою surface сам — `soul.surfaces().create()` сразу после подключения (`Surfaces::create::Args {id, name, rules}`); soul записывает модуль-создателя в `surface.module`, и обслуживает surface его живая сессия. Для создателя вызов идемпотентен, поэтому безопасен на каждом старте; `name`/`rules` меняет `surfaces.update`.

`connect_config` ждёт соединения с soul до `module.connect_wait` (по умолчанию 3 минуты); `run` переподключается с `hello.resume`, и регистрировать после реконнекта нечего: surface уже в базе soul, а soul выводит обслуживание из новой сессии. Подробно — в главе [SDK модулей](sdk.md).

## Подключение сервисного модуля

Сервис (мост, MCP) адреса soul не знает: он слушает `uds` или `tcp`, а soul звонит ему по записи реестра и шифрует соединение Noise с общим ключом.

```sh
openssl rand -hex 32 > /tmp/eva-dev/bridge.key
chmod 600 /tmp/eva-dev/bridge.key
```

1. Зарегистрируйте сервис у soul. Два способа:
   - **в конфиге soul**: `modules.services.<module> = {address, key}`, например `{address: "uds:///tmp/eva-dev/bridge.sock", key: {file: /tmp/eva-dev/bridge.key}}`. Такая запись сильнее добавленной операцией;
   - **операцией** `modules.add {module, address, key?}` от клиента хозяина. Без `key` soul генерирует его и возвращает один раз в ответе. Ключ хранится в базе soul как есть — soul сам им дозванивается. `modules.remove {module}` убирает такую запись и закрывает сессию.
2. Ключ записи — id сервиса; модулю его знать не нужно, он приходит в `welcome.module`. Ключ короче 32 символов — предупреждение на старте (у `modules.add` — отказ).
3. Конфиг модуля — адрес, где слушать, и тот же ключ:

```yaml
# конфиг модуля
module:
  listen: {uds: /tmp/eva-dev/bridge.sock}
  key: {file: /tmp/eva-dev/bridge.key}
```

```rust
let listening = Module::new()
    .listen(&cfg)                  // cfg: ServiceConfig из секции `module`
    .await?;
let mut soul = listening.soul().await?;   // ждёт первого звонка до `module.soul_wait`
soul
    .surfaces()
    .create("bridge".into(), "Bridge".into(), None)
    .await?;
listening.run(shutdown).await?;
```

Порядок запуска не важен: soul звонит сразу при старте и при `modules.add`, а не дозвонившись — повторяет с паузой от 1 s до 60 s; причина провала видна в `modules.list` (`dial.error`). Рестарт soul — новое соединение вытесняет прежнее, `Soul` модуля продолжает работать. Подробно — в главе [SDK модулей](sdk.md#сервис-listen--listening).
