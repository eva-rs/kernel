# eva-provider-openai

OpenAI-совместимый Chat Completions за трейтом `Provider`. Диалект `openrouter` (по умолчанию) — OpenRouter с его расширениями: `reasoning`, `reasoning_details`, `cache_control`, `usage.cost`. Диалект `plain` — голый Chat Completions: vLLM, llama-server, OpenAI.

## Место в слоях

Реализация `eva_core::Provider` для одного семейства API; зависит от `eva-core`, `eva-protocol`, фреймворка [eva](https://git.desu.church/VienDesu/eva), `reqwest` и `tokio`. Не зависит от `eva-kernel` и `eva-rpc`: ядро видит провайдер только через трейт и `eva-providers`. Vendor-строки живут только в `providers/*`.

## Точки входа

| Путь | Что |
|---|---|
| `OpenAiProvider::connect(Config)` | клиент и реестр моделей; один раз читает `GET /models` |
| `OpenAiProvider::settle()` | дочитывает каталог, если на старте он не ответил: повторы с паузой `5s`..`300s` |
| `OpenAiProvider::refresh()` | перечитать `GET /models`; при ошибке реестр прежний |
| `OpenAiProvider::from_listing(Config, Option<&Value>)` | реестр из готового ответа каталога, без сети |
| `Config`, `Dialect`, `ModelOverride`, `CapsOverride` | секция `llm.providers.<name>` с `kind: openai` |
| `declare`, `render`, `normalize_schema`, `Decoder`, `pump`, `classify` | каталог → `ModelDecl`, запрос → тело, SSE → `GenItem`, статус → `ProviderError` |

Запросы: `POST <base_url>/chat/completions` (поток SSE), `POST <base_url>/embeddings`, `GET <base_url>/models`. `count_tokens` не поддерживается; `decl().local` — только при loopback-адресе в `base_url`.

До `connect` процесс обязан поставить провайдер криптографии rustls (`eva_rpc::install_crypto_provider()` в бинаре ядра), иначе сборка HTTP-клиента паникует.

## Конфиг

| Ключ | По умолчанию | Что |
|---|---|---|
| `base_url` | `https://openrouter.ai/api/v1` | корень API без `/chat/completions` |
| `api_key` | — | `{file: /путь}` или `{plaintext: "…"}`; читается один раз при сборке |
| `proxy` | — | `http://`, `https://` или `socks5://` |
| `timeout` | `300s` | подключение и тишина между чтениями, не весь ответ |
| `dialect` | `openrouter` | `openrouter` \| `plain` |
| `allow` | все | allow-list id моделей |
| `models.<id>` | — | поправки к каталогу: `context_window`, `max_output`, `price`, `embed`, `caps` |
| `referer` / `title` | — | заголовки `HTTP-Referer` / `X-Title` (атрибуция у OpenRouter) |

`caps` принимает только то, что Chat Completions умеет выразить: `vision`, `image_out`, `thinking`, `effort_levels`, `structured_output`, `system_in_messages`, `thinking_binding`, `cache_breakpoints` (только `openrouter`), `cache_min_prefix`. Модель, которой нет в каталоге, обязана задать `context_window` (или `embed`), иначе не объявляется.

Недоступный каталог сборку не валит: реестр стартует с `models` из конфига, остальное дочитывает `settle`. Ошибка сборки — только у `openrouter` с отказом, который повтор не исправит (ключ, путь).

## Пример

Локальный vLLM, модели — из конфига (по `tests/models.rs`):

```rust
use eva_provider_openai::{Config, Dialect, OpenAiProvider};

let config: Config = serde_json::from_value(json!({
    "base_url": "http://127.0.0.1:8080/v1",
    "dialect": "plain",
    "models": {"qwen3-32b": {"context_window": 40960, "max_output": 8192}}
}))?;
assert_eq!(config.dialect, Dialect::Plain);
let provider = OpenAiProvider::connect(config).await?;
```

## Тесты

```sh
nix develop -c cargo test -p eva-provider-openai
```

Сеть не нужна: `tests/http.rs` поднимает loopback-сервер с заготовленными ответами; `models.rs` — каталог (`tests/fixtures`) и поправки; `request.rs` — тела запросов снимками `insta`; `stream.rs` — SSE-фикстуры в точные последовательности `GenItem`; `errors.rs` — классификация не-2xx; `prefix.rs` — рендер длинного запроса начинается с рендера короткого. Снимки обновляет `cargo insta review` из devShell.

## Документация

[Провайдеры инференса](../../docs/src/providers.md), [Конфигурация](../../docs/src/config.md).
