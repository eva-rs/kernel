# eva-provider-mock

Сценарный провайдер инференса для тестов и смоука без ключей: отдаёт заготовленные `GenItem`, записывает каждый запрос и отвергает ошибкой `invalid` то, на что живой вендор ответил бы 400, — поэтому баг рендера ядра валит любой тест, идущий через мок. Без сценария отвечает эхом последней реплики.

## Место в слоях

Реализация `eva_core::Provider`; зависит от `eva-core`, `eva-protocol`, фреймворка [eva](https://git.desu.church/VienDesu/eva) и `tokio`. Сети нет. Не зависит от `eva-kernel`: ядро подключает его через `eva-providers` (`kind: mock`), тесты ядра — напрямую.

## Точки входа

| Путь | Что |
|---|---|
| `MockProvider::new(Config)` | провайдер с моделями конфига |
| `MockProvider::strict()` | модели по умолчанию; запрос сверх сценариев — ошибка `invalid` |
| `push(Script)` | сценарий на один `generate`; расходуются по порядку |
| `requests()`, `last_request()`, `scripts_left()` | записанные запросы, включая отвергнутые |
| `seen()` | `watch`-счётчик принятых запросов: тест ждёт событие, а не таймер |
| `awaiting_listing()`, `list(models)`, `settle()` | каталог, прочитанный позже старта — как у `openai` |
| `Script` | `reply(text)`, `call(id, name, input)`, `error(e)`; билдер `text`, `thinking`, `tool_use`, `usage`, `stop`, `fail`, `item`, `gate` |
| `CHAT_MODEL`, `EMBED_MODEL` | модели конфига по умолчанию: `mock` и `mock-embed` |

## Конфиг

`llm.providers.<name>` с `kind: mock`:

| Ключ | По умолчанию | Что |
|---|---|---|
| `models` | `mock`, `mock-embed` | объявленные модели (`ModelDecl`); пустой список — ни одной |
| `decl` | `{local: true, count_tokens: true}` | декларация провайдера |
| `on_exhausted` | `echo` | ответ сверх сценариев: `echo` — `[mock:<model>] echo: …` или образец по `format`; `fail` — ошибка `invalid` |

## Пример

По `tests/mock.rs`:

```rust
use eva_provider_mock::{MockProvider, Script};

let mock = MockProvider::strict();
mock.push(Script::reply("one"))
    .push(Script::new().text("par").fail(transient()));

let (tx, rx) = futures::channel::mpsc::unbounded();
mock.generate(request, GenSink::new(tx)).await?;   // TextDelta "one", Stop(EndTurn)
assert_eq!(mock.requests().len(), 1);
```

## Тесты

```sh
nix develop -c cargo test -p eva-provider-mock
```

## Документация

[Провайдеры инференса](../../docs/src/providers.md), [Разработка](../../docs/src/development.md).
