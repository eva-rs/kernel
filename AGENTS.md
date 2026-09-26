# Правила для агентов

Дизайн обязателен: `../docs/design/` (`00` инварианты I1–I14, `11` слои и операции, `06` протокол, `10` схема). Расходишься с ним — сначала правка дизайна.

## Сборка

- Только в devShell (у системного cargo нет линкера): `nix develop -c cargo <check|test|clippy|fmt> …` из корня; `-p <crate>` для одного крейта.
- Гейты перед сдачей: `cargo fmt --all`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`.
- Параллельные сборки ждут `Blocking waiting for file lock` — это норма. `cargo clean` не звать.
- Манифесты и `Cargo.lock` менять только при прямой необходимости.

## Крейты

| Путь | Крейт | Содержит |
|---|---|---|
| `protocol/` | `eva-protocol` | типы, `requests/<ns>.rs` (`Args/Ok/Err`), `errors/`, wire; без I/O |
| `core/` | `eva-core` | `Session`, шаги, трейты неймспейсов, `Caller`, `Provider`, `prelude` |
| `rpc/` | `eva-rpc` | кадры msgpack, uds/ws/wss, пир, сервер-диспетчер, `RpcSession`, `ModuleProvider` |
| `sdk/` | `eva-sdk` | `Module` → `Connected`, `Kernel = Session<RpcSession>` |
| `providers/{openai,mock}` | `eva-provider-*` | `impl Provider` одного вендора |
| `providers/all/` | `eva-providers` | `ProviderConfig` (тег `kind`), `AnyProvider`, `build` |
| `services/kernel/` | `eva-kernel` | бинарь и библиотека ядра |

## Слои (`11 §1`, `§9`)

- Рёбра: `protocol ← core ← {rpc, providers/*}`, `rpc ← sdk`, `kernel ← {core, rpc, eva-providers, eva-hands}`. Обратных нет.
- Авторизация только в `state/policy/`; транспорт лишь выводит `Caller` из токена.
- SQL только в `db/`, хендлеры (`#[eva::endpoint]` + `#[fastrace::trace]`) только в `handling/`, строки БД → типы в `convert/`.
- Одна реализация операции: RPC-метод и тул — обёртки. Новая операция: `protocol` → `core` → `handling`/`impls`/`policy` → строка в `tests/contract.rs`.
- Vendor- и surface-строки только в `providers/*` (I14, `tests/vendor_lint.rs`).
- Операции поздних этапов отвечают `Aux::Unavailable` с `M2_PENDING`/`M3_PENDING`/`M4_PENDING`.

## Код

- Фреймворк `eva`: `#[data]` на все типы данных, `eva::time`, `log` с kv (`log::info!(k = v; "msg")`), `eva::cli::App` + `Supervisor` в бинарях.
- Без `unwrap`/`expect` на данных провода, БД и сети; паника — только инвариант.
- Идентификаторы, логи, промпты — английский; комментарии — русский, одна фраза «почему», только неочевидное, без истории.

## Коммиты

- Коммитить только по просьбе хозяина. Трейлер `Co-Authored-By: Eva`.
