# Правила для агентов

Источник истины — код; книга `docs/` описывает его. Меняешь поведение — правь соответствующую главу книги в том же изменении.

До первого релиза обратную совместимость не сохраняем: при изменении API обновляем существующие модули и вызывающий код.

## Сборка

- Только в devShell (у системного cargo нет линкера): `nix develop -c cargo <check|test|clippy|fmt> …` из корня.
- Гейты перед сдачей: `cargo fmt --all`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`.
- Параллельные сборки ждут `Blocking waiting for file lock` — это норма. `cargo clean` не звать.
- Манифесты и `Cargo.lock` менять только при прямой необходимости.

## Крейты

| Путь | Крейт | Содержит |
|---|---|---|
| `protocol/` | `eva-protocol` | типы, `requests/<ns>.rs` (`Args/Ok/Err`), `errors/`, wire; без I/O |
| `core/` | `eva-core` | `Session`, трейты и `for_each_op!` из списка, `Caller`, `Provider`, `prelude` |
| `rpc/` | `eva-rpc` | кадры msgpack, клиенты uds/ws/wss, сервисы uds/tcp под Noise (`Psk`, дозвон), пир, сервер-диспетчер, `RpcSession`, `ModuleProvider` |
| `sdk/` | `eva-sdk` | `module!()` + `Group`/`Tool` + `run` (конфиг, `init`, `Supervisor`); ручной `Module` → `Listening` / `Connected`; `Soul = Session<LiveSession>` (переживает смену соединения) |
| `sdk/macros/` | `eva-sdk-macros` | `msg!`, `include_translations!` (PO в бинарь), бинарь `eva-sdk-i18n` (extract/merge) |
| `providers/{openai,mock}` | `eva-provider-*` | `impl Provider` одного вендора |
| `providers/all/` | `eva-providers` | `ProviderConfig` (тег `kind`), `AnyProvider`, `build` |
| `services/soul/` | `eva-soul` | бинарь и библиотека soul |

## Слои

- Рёбра: `protocol ← core ← {rpc, providers/*}`, `rpc ← sdk`, `soul ← {core, rpc, eva-providers, eva-hands}`. Обратных нет.
- Авторизация только в `state/policy/`; транспорт лишь выводит `Caller` из соединения.
- SQL только в `db/`, хендлеры (`#[eva::endpoint]` + `#[fastrace::trace]`) только в `handling/`, строки БД → типы в `convert/` (`tests/k3_layer_lint.rs`).
- Одна реализация операции: RPC-метод и тул — обёртки. Операции объявлены в `#[service]` в `protocol/src/requests/<ns>.rs`; `#[export]` в `core/src/service.rs` объединяет сервисы в `eva_core::for_each_op!`.
- Новая операция: метод в `#[service]` в `protocol/src/requests/<ns>.rs` (мутация — `#[flags(mutating)]`, тесты его не ловят) → хендлер в `handling/`, правило в `state/policy/`, автоматическая проекция в `impls/` → строки в `tests/k3_policy_all.rs`, `tests/contract.rs`, `rpc/tests/it/errors.rs`.
- Vendor- и surface-строки только в `providers/*` (`tests/vendor_lint.rs`).

## Код

- Фреймворк `eva`: `#[data]` на все типы данных, `eva::time`, `log` с kv (`log::info!(k = v; "msg")`), `eva::cli::App` + `Supervisor` в бинарях.
- Без `unwrap`/`expect` на данных провода, БД и сети; паника — только инвариант.
- Права и владельца файлов (сокеты, база) код не ставит — только окружение: NixOS-модуль во `flake.nix` (`UMask`, `StateDirectoryMode`) или socket-юнит хоста.
- Идентификаторы, логи, промпты — английский; комментарии — русский, одна фраза «почему», только неочевидное, без истории.

## Коммиты

- Коммитить только по просьбе хозяина. Трейлер `Co-Authored-By: Eva`.
