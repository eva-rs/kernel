# Eva kernel

Ядро Евы — персонального агента: один процесс поверх одного SQLite-файла. Хранит чаты, память, заметки, личности и расход; говорит с модулями (клиенты хозяина, мосты мессенджеров, MCP) по msgpack-RPC; инференс — встроенные провайдеры за трейтом `Provider` (OpenAI-совместимый, первым OpenRouter; `mock` для смоука без ключей). Дизайн: `../docs/design/`.

Сейчас готов этап M1: библиотечное API (все операции `11 §3`, кроме туров, поиска и каталога тулов), RPC-сервер модулей, SDK модулей. Туры — M2.

## Сборка и запуск

```sh
nix develop -c cargo test --workspace   # devShell: у системного cargo нет линкера
nix build .#eva-kernel                  # бинарь ./result/bin/eva-kernel
eva-kernel --config eva.yaml            # несколько --config сливаются; env сильнее файлов
```

Минимальный конфиг:

```yaml
system:
  state_dir: /var/lib/eva
llm:
  providers:
    router: {kind: openai}          # api_key — только через env/credential
  default_model: some-vendor/some-model
modules:
  tokens:
    phone: {token: "...", kind: client}
```

Секреты — только окружением или файлами вне Nix store: `EVA_LLM__PROVIDERS__<name>__API_KEY`, `EVA_MODULES__TOKENS__<module>__TOKEN`, `EVA_HANDS__ENDPOINTS__<name>__KEY`. NixOS-модуль — `nixosModules.default` (`services.eva-kernel`).

## Конфиг

Секции (YAML/JSON/TOML, env-префикс `EVA_`, вложенность через `__`):

| Секция | Что | Глава |
|---|---|---|
| `system` | `state_dir` (дефолт `$STATE_DIRECTORY`, иначе `/var/lib/eva`), `tz`; фреймворк: `system.rt`, `system.logging` | `00 §5` |
| `db` | `path` (`<state_dir>/eva.sqlite`), `busy_timeout`, `readers` | `10 §7` |
| `llm` | обязательна: `providers.<name> = {kind: openai \| mock \| module, …}`, `default_model`, `models`, `roles`, `fallback` | `01 §8.2` |
| `modules` | `listen` (`{uds, mode, group}` \| `{ws}` \| `{wss, tls}`), `tokens`, тайминги RPC, `fs_quota`, `sqlite_*` | `06 §11` |
| `chats` | `default_kind`, `persons.master` (личности хозяина), `persons.master_name` | `05` |
| `hands` | `endpoints.<name> = {local: true, limits}` \| `{address, key}`, `default_endpoint` | `07 §3` |
| `work` | `blob_dir`, `blob_keep`, `blob_max` | `07 §6` |
| `notes` | `enabled`, `budget_percent`, `budget_max_tokens` | `04 §2` |

Провайдер `openai`: `base_url` (дефолт OpenRouter), `api_key`, `proxy`, `timeout`, `dialect: openrouter | plain`, `allow`, `models` (поправки деклараций), `referer`/`title`. Каталог моделей читается при старте.

## Модуль на SDK

```rust
let connected = eva_sdk::Module::new("bridge")
    .surface(eva_sdk::Surface::new("bridge").name("Bridge").ask(true))
    .connect(&cfg.kernel, cfg.token.expose())
    .await?;
connected.run(shutdown).await
```

Правила для агентов, работающих в репозитории, — `AGENTS.md`.
