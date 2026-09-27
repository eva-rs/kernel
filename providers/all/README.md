# eva-providers

Встроенные провайдеры инференса одной точкой: конфиг `llm.providers.<name>` под тегом `kind` и диспетчер `AnyProvider` над скомпилированными крейтами провайдеров. Vendor-строку `kind` разбирает этот крейт, а не ядро.

## Место в слоях

Зависит от `eva-core`, `eva-protocol`, фреймворка [eva](https://git.desu.church/VienDesu/eva) и крейтов провайдеров, включённых фичами. Ядро зависит от него и не знает ни одного вендора по имени. Не зависит от `eva-kernel` и `eva-rpc`.

| Фича | Крейт | `kind` |
|---|---|---|
| `openai` (по умолчанию) | [`eva-provider-openai`](../openai/README.md) | `openai` |
| `mock` (по умолчанию) | [`eva-provider-mock`](../mock/README.md) | `mock` |

Третий вид записи, `kind: module` (провайдер — подключённый модуль с capability `provider`), разбирает само ядро, не этот крейт.

## Точки входа

| Путь | Что |
|---|---|
| `ProviderConfig` | `#[serde(tag = "kind")]`: `Openai(openai::Config)` \| `Mock(mock::Config)` |
| `build(name, config)` | собирает `AnyProvider`; `openai` при этом один раз читает каталог моделей |
| `AnyProvider` | `impl Provider`; `settle()` дочитывает каталог, `true` — `models()` сменился |
| `BuildError` | `llm.providers.<name>: <причина>` — ошибка читается путём ключа |
| `openai`, `mock` | реэкспорт крейтов провайдеров |

## Пример

По `tests/config.rs`:

```rust
use eva_providers::{ProviderConfig, build, openai};

let config: ProviderConfig = serde_json::from_value(json!({
    "kind": "openai",
    "base_url": "http://127.0.0.1:8080/v1",
    "dialect": "plain",
}))?;
let ProviderConfig::Openai(ref cfg) = config else { unreachable!() };
assert_eq!(cfg.dialect, openai::Dialect::Plain);

let provider = build("local", config).await?;
```

## Тесты

```sh
nix develop -c cargo test -p eva-providers
```

`tests/config.rs` проверяет разбор по `kind` и диспетчер без JSON и рантайма: конфиг читается десериализатором карты serde, future без I/O — одним poll.

## Документация

[Провайдеры инференса](../../docs/src/providers.md), [Конфигурация](../../docs/src/config.md).
