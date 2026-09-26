//! Единый список операций `11 §3` для транспорта. Повторяет объявления
//! `service_trait!` ядра: метка — форма шага (`read`, `mut`, `stream`,
//! `upload`). Из него собираются клиентская сессия (каждый трейт целиком —
//! пропуск операции не скомпилируется) и таблица диспетчера; сверку с
//! `NAMESPACES` ядра держит тест.

macro_rules! for_each_op {
    ($callback:ident) => {
        $callback! {
            surfaces: Surfaces { mut create, read get, read list, mut update },
            folders: Folders { mut create, mut update, mut delete, read list },
            chats: Chats {
                mut ingest, mut create, read get, read list, mut update, mut r#move,
                mut delete, mut restore, read search, read history_read, mut compact,
                mut notify,
            },
            turns: Turns {
                mut start, stream attach, mut append_silent, mut delivered, mut cancel,
                mut respond,
            },
            persons: Persons { mut resolve, read list, mut rename, mut merge, mut whitelist },
            memory: Memory { read list, read get, read history, mut restore },
            notes: Notes { read list },
            tools: Tools { read catalog },
            models: Models { read list },
            cron: Cron { read list, read runs },
            sleep: Sleep { read runs, read phase_runs },
            agents: Agents { read list },
            stats: Stats {
                read tools_usage, read spend, read spend_series, read chat_spend, read health,
            },
            modules: Modules { read list, mut token_issue, mut token_revoke },
            hands_grants: HandsGrants { read list, mut revoke },
            blobs: Blobs { upload put, stream get, read stat },
            inference: Inference { mut complete },
            fs: Fs { read read, mut write, read list, read stat, mut remove },
            sqlite: Sqlite { mut exec, read query, mut tx },
            kv: Kv { read get, mut set, mut del, read list },
        }
    };
}

pub(crate) use for_each_op;

/// RPC-метод операции: `<namespace>.<op>`.
pub(crate) fn method(ns: &str, op: &str) -> String {
    format!("{ns}.{op}")
}
