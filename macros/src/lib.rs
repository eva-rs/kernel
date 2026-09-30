//! Единственный список операций библиотеки (`11 §3`): из него `eva-core`
//! получает трейты неймспейсов и X-макро `for_each_op!`, по которому
//! собираются сессия, RPC-диспетчер, клиент и SDK.

use proc_macro::TokenStream;

use eva_api_builder::{ApiBuilder, MethodBuilder, parse_quote};

/// Трейты неймспейсов и список операций под именем из входа, в грамматике
/// `eva_api_builder`. Флаги: без флага — чтение, `mutating`, `stream`
/// (`Ok` — `Items`), `upload` (`Args` несёт `ByteStream`, serde-формы нет).
#[proc_macro]
pub fn endpoints(args: TokenStream) -> TokenStream {
    ApiBuilder::new("::eva_protocol::requests")
        .result_type(|ok, err| parse_quote!(::eva_protocol::requests::Response<#ok, #err>))
        .service("surfaces", |svc| {
            svc.method_with("create", mutating)
                .methods(["get", "list"])
                .method_with("update", mutating)
        })
        .service("folders", |svc| {
            svc.method_with("create", mutating)
                .method_with("update", mutating)
                .method_with("delete", mutating)
                .method("list")
        })
        .service("chats", |svc| {
            svc.method_with("ingest", mutating)
                .method_with("create", mutating)
                .methods(["get", "list"])
                .method_with("update", mutating)
                .method_with("r#move", mutating)
                .method_with("delete", mutating)
                .method_with("restore", mutating)
                .methods(["search", "history_read"])
                .method_with("compact", mutating)
                .method_with("notify", mutating)
        })
        .service("turns", |svc| {
            svc.method_with("start", mutating)
                .method_with("attach", |m| m.flag("stream"))
                .method_with("append_silent", mutating)
                .method_with("delivered", mutating)
                .method_with("cancel", mutating)
                .method_with("respond", mutating)
        })
        .service("persons", |svc| {
            // `resolve` заводит неизвестного актора.
            svc.method_with("resolve", mutating)
                .method("list")
                .method_with("rename", mutating)
                .method_with("merge", mutating)
                .method_with("whitelist", mutating)
        })
        .service("memory", |svc| {
            svc.methods(["list", "get", "history"])
                .method_with("restore", mutating)
        })
        .service("notes", |svc| svc.method("list"))
        .service("tools", |svc| svc.method("catalog"))
        .service("models", |svc| svc.method("list"))
        .service("cron", |svc| svc.methods(["list", "runs"]))
        .service("sleep", |svc| svc.methods(["runs", "phase_runs"]))
        .service("agents", |svc| svc.method("list"))
        .service("stats", |svc| {
            svc.methods([
                "tools_usage",
                "spend",
                "spend_series",
                "chat_spend",
                "health",
            ])
        })
        .service("modules", |svc| {
            svc.method("list")
                .method_with("add", mutating)
                .method_with("remove", mutating)
                .method_with("token_issue", mutating)
                .method_with("token_revoke", mutating)
        })
        .service("hands_grants", |svc| {
            svc.method("list").method_with("revoke", mutating)
        })
        .service("blobs", |svc| {
            svc.method_with("put", |m| m.flag("upload"))
                .method_with("get", |m| m.flag("stream"))
                .method("stat")
        })
        // `complete` пишет spend: повтор оплатился бы дважды.
        .service("inference", |svc| svc.method_with("complete", mutating))
        .service("fs", |svc| {
            svc.method("read")
                .method_with("write", mutating)
                .methods(["list", "stat"])
                .method_with("remove", mutating)
        })
        .service("sqlite", |svc| {
            // `query` пропускает только `sqlite3_stmt_readonly`-операторы (`06 §5`).
            svc.method_with("exec", mutating)
                .method("query")
                .method_with("tx", mutating)
        })
        .service("kv", |svc| {
            svc.method("get")
                .method_with("set", mutating)
                .method_with("del", mutating)
                .method("list")
        })
        .export(args, true)
        .into()
}

/// Чтение SDK повторяет после реконнекта с новым id, поэтому мутация без
/// этого флага исполнилась бы дважды.
fn mutating(m: MethodBuilder) -> MethodBuilder {
    m.flag("mutating")
}
