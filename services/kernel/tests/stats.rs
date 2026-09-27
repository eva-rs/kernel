//! `stats.*` (`09 §1.4`, `§2`, `§3`, `§6`, `§7`): разрезы по текущему
//! положению чата, группы расхода без смешения валют, метрики кэша по caps
//! реестра, сжатие публичного `view`, gauge'и и матрица `Policy`.

mod common;

use std::time::Duration;

use common::{Harness, OTHER, Outcome, SVC, outcome};
use eva::{
    time::{Clock as _, Date},
    utils::SecretString,
};
use eva_core::{CallStep as _, service::stats::Stats as _};
use eva_kernel::{
    config::modules::TokenEntry,
    db::{Db as _, DbResult},
};
use eva_protocol::{
    errors::{Aux, Generic},
    requests::stats::{Top, chat_spend, health, spend, spend_series, tools_usage},
    types::{
        ChatId, FolderId, ModelId, ModuleId, SurfaceId, ToolId,
        model::{ModelCaps, ModelDecl, Price},
        module::{State, TokenKind},
        stats::{BreakReason, GroupBy, Scope, Target},
        turn::{Audience, TurnView},
    },
};

const SEC: i64 = 1_000_000_000;
const HOUR: i64 = 3600 * SEC;
const D0: &str = "2026-09-19";
const D1: &str = "2026-09-20";
const D2: &str = "2026-09-21";

fn date(s: &str) -> Date {
    let (y, rest) = s.split_at(4);
    let (m, d) = (&rest[1..3], &rest[4..6]);
    format!("{d}.{m}.{y}").parse().expect("date")
}

/// Полдень UTC суток: в любой зоне хоста ±11h это те же сутки.
fn noon(day: &str) -> i64 {
    (i64::from(date(day).days()) * 86_400 + 43_200) * SEC
}

async fn exec(h: &Harness, sql: String) {
    h.kernel
        .shared()
        .db()
        .write(move |tx| -> DbResult<()> {
            tx.execute_batch(&sql)?;
            Ok(())
        })
        .await
        .expect("seed");
}

fn approx(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-9
}

struct World {
    h: Harness,
    unsorted: FolderId,
}

/// Чаты: 1 — `local/Unsorted`, 2 — `local/agents`, 3 — `svc/Unsorted`; 99 — вычищенный.
/// Тулы: 1 живой `fs_read`, 2 ретирован, 3 пропал.
async fn world() -> World {
    // Мок-модель с кэшем и ценой: холодные старты и экономия считаются по реестру.
    let mock = eva_providers::mock::Config {
        models: vec![ModelDecl {
            name: ModelId::from(common::MODEL),
            context_window: 200_000,
            max_output: 8192,
            caps: ModelCaps {
                cache_breakpoints: 4,
                cache_min_prefix: 1024,
                ..ModelCaps::default()
            },
            price: Some(Price {
                input: 3.0,
                output: 15.0,
                cache_read: 0.3,
                cache_write_short: 3.75,
                cache_write_long: 6.0,
            }),
            embed: None,
        }],
        ..common::mock_config()
    };
    let h = common::start_with_mock(|_| {}, mock).await;
    h.register_module(common::CLIENT).await;
    h.service_with_surface(SVC).await;
    h.service_with_surface(OTHER).await;
    let t = i64::try_from(h.clock.get().as_nanos().get()).expect("nanos fit");

    let chat = |id: i64, surface: &str, folder: &str| {
        format!(
            "INSERT INTO chats (id, folder_id, kind, created_at, last_activity_at)
               SELECT {id}, id, 'persistent', {t}, {t} FROM folders WHERE surface_id = '{surface}' AND name = '{folder}';"
        )
    };
    let tool_stats = |day: &str, chat: i64, tool: i64, calls: i64| {
        format!(
            "INSERT INTO tool_stats (day, chat_id, tool_id, calls, turns_with_call, loads, errors, ends_turn, last_at)
             VALUES ('{day}', {chat}, {tool}, {calls}, 1, {tool}, 1, 0, {t});"
        )
    };
    let chat_stats = |day: &str, chat: i64, turns: i64| {
        format!("INSERT INTO chat_stats (day, chat_id, turns) VALUES ('{day}', {chat}, {turns});")
    };

    let sql = [
        chat(1, "local", "Unsorted"),
        chat(2, "local", "agents"),
        chat(3, SVC, "Unsorted"),
        format!(
            "INSERT INTO tools (tool_id, module, path, canonical, decl, spec_hash, registered_at, last_seen_at, retired_at)
             VALUES (1, 'kernel', 'fs.read', 'fs_read', '{{}}', x'00', {t}, {t}, NULL),
                    (2, 'kernel', 'old.tool', 'old_tool', '{{}}', x'01', {t}, {t}, {t});"
        ),
        tool_stats(D1, 1, 1, 3),
        tool_stats(D2, 1, 2, 5),
        tool_stats(D1, 2, 1, 2),
        tool_stats(D1, 3, 1, 7),
        tool_stats(D1, 99, 3, 1),
        tool_stats(D0, 1, 1, 100),
        chat_stats(D1, 1, 4),
        chat_stats(D2, 1, 2),
        chat_stats(D1, 2, 3),
        chat_stats(D1, 3, 5),
        chat_stats(D1, 99, 1),
        chat_stats(D0, 1, 50),
        spend_rows(),
    ]
    .concat();
    exec(&h, sql).await;

    let unsorted = h
        .query(|c| {
            Ok(c.query_row(
                "SELECT id FROM folders WHERE surface_id = 'local' AND name = 'Unsorted'",
                [],
                |r| r.get(0).map(FolderId),
            )?)
        })
        .await;

    World { h, unsorted }
}

/// Строки `spend` (см. комментарии тестов с ожиданиями).
fn spend_rows() -> String {
    let row = |id: i64,
               ts: i64,
               model: &str,
               source: &str,
               op: &str,
               module: &str,
               chat: &str,
               turn: &str,
               tokens: [i64; 7],
               brk: &str,
               cost: &str,
               source_of_cost: &str| {
        let [input, cr, cw, cwl, output, reasoning, edits] = tokens;
        format!(
            "INSERT INTO spend (id, ts, model, source, op, module, chat_id, turn_id, input, cache_read,
                cache_write, cache_write_long, output, reasoning, context_edits_applied,
                cache_break_reason, cost, currency, cost_source)
             VALUES ({id}, {ts}, '{model}', '{source}', '{op}', {module}, {chat}, {turn}, {input}, {cr},
                {cw}, {cwl}, {output}, {reasoning}, {edits}, {brk}, {cost}, '{source_of_cost}');"
        )
    };
    [
        row(
            1,
            noon(D1),
            "mock-model",
            "surface",
            "step",
            "NULL",
            "1",
            "'t-a'",
            [100, 900, 0, 0, 50, 10, 1],
            "NULL",
            "0.5, 'USD'",
            "pricebook",
        ),
        row(
            2,
            noon(D1) + HOUR,
            "mock-model",
            "surface",
            "step",
            "NULL",
            "1",
            "'t-a'",
            [1000, 0, 500, 100, 10, 0, 0],
            "'ttl_expired'",
            "0.2, 'USD'",
            "provider",
        ),
        row(
            3,
            noon(D2),
            "other-model",
            "surface",
            "step",
            "NULL",
            "1",
            "'t-b'",
            [10, 0, 0, 0, 5, 0, 0],
            "'first_request'",
            "NULL, NULL",
            "none",
        ),
        row(
            4,
            noon(D2) + HOUR,
            "mock-model",
            "surface",
            "keep_alive",
            "NULL",
            "1",
            "NULL",
            [0, 1000, 0, 0, 0, 0, 0],
            "NULL",
            "0.01, 'USD'",
            "pricebook",
        ),
        row(
            5,
            noon(D2) + 2 * HOUR,
            "mock-model",
            "surface",
            "step",
            "NULL",
            "1",
            "'t-b'",
            [50, 50, 0, 0, 20, 0, 0],
            "'unknown'",
            "0.3, 'EUR'",
            "provider",
        ),
        row(
            6,
            noon(D1),
            "mock-model",
            "module",
            "other",
            "'svc'",
            "NULL",
            "NULL",
            [20, 0, 0, 0, 5, 0, 0],
            "NULL",
            "0.05, 'USD'",
            "provider",
        ),
        row(
            7,
            noon(D0),
            "mock-model",
            "surface",
            "step",
            "NULL",
            "1",
            "'t-z'",
            [999, 0, 0, 0, 1, 0, 0],
            "NULL",
            "0.1, 'USD'",
            "pricebook",
        ),
        row(
            8,
            noon(D1),
            "mock-model",
            "surface",
            "step",
            "NULL",
            "3",
            "'t-c'",
            [30, 0, 0, 0, 3, 0, 0],
            "NULL",
            "0.03, 'USD'",
            "pricebook",
        ),
    ]
    .concat()
}

fn usage_args(scope: Scope, target: Option<Target>) -> tools_usage::Args {
    tools_usage::Args {
        scope,
        target,
        from: date(D1),
        to: date(D2),
        top: Top::default(),
        view: None,
    }
}

async fn usage(h: &Harness, args: tools_usage::Args) -> (Vec<(i64, Option<String>, u64)>, u64) {
    let tools_usage::Ok { rows, turns } = h
        .client_session()
        .await
        .stats()
        .tools_usage()
        .call(args)
        .await
        .expect("tools_usage");
    let rows = rows
        .into_iter()
        .map(|r| (r.tool_id.get(), r.name, r.calls))
        .collect();
    (rows, turns)
}

fn fs_read() -> Option<String> {
    Some(String::from("fs_read"))
}

#[tokio::test]
async fn tools_usage_by_scope() {
    let World { h, unsorted } = world().await;

    let chat = usage(&h, usage_args(Scope::Chat, Some(Target::Chat(ChatId(1))))).await;
    assert_eq!(
        chat,
        (vec![(2, None, 5), (1, fs_read(), 3)], 6),
        "retired tool has no name"
    );

    let folder = usage(
        &h,
        usage_args(Scope::Folder, Some(Target::Folder(unsorted))),
    )
    .await;
    assert_eq!(folder, chat, "only chat 1 lives in local/Unsorted");

    let surface = usage(
        &h,
        usage_args(
            Scope::Surface,
            Some(Target::Surface(SurfaceId::from("local"))),
        ),
    )
    .await;
    assert_eq!(
        surface,
        (vec![(1, fs_read(), 5), (2, None, 5)], 9),
        "ties by tool_id"
    );

    let global = usage(&h, usage_args(Scope::Global, None)).await;
    assert_eq!(
        global,
        (vec![(1, fs_read(), 12), (2, None, 5), (3, None, 1)], 15),
        "purged chats count only globally"
    );

    let top = usage(
        &h,
        tools_usage::Args {
            top: Top::new(1).expect("top in range"),
            ..usage_args(Scope::Global, None)
        },
    )
    .await;
    assert_eq!(top.0, [(1, fs_read(), 12)]);

    let one_day = usage(
        &h,
        tools_usage::Args {
            to: date(D1),
            ..usage_args(Scope::Chat, Some(Target::Chat(ChatId(1))))
        },
    )
    .await;
    assert_eq!(
        one_day,
        (vec![(1, fs_read(), 3)], 4),
        "to is inclusive, D2 excluded"
    );
}

#[tokio::test]
async fn tools_usage_rows_carry_every_counter() {
    let World { h, .. } = world().await;
    let tools_usage::Ok { rows, .. } = h
        .client_session()
        .await
        .stats()
        .tools_usage()
        .call(usage_args(Scope::Chat, Some(Target::Chat(ChatId(1)))))
        .await
        .expect("tools_usage");
    let fs = rows
        .iter()
        .find(|r| r.tool_id == ToolId(1))
        .expect("fs_read");
    assert_eq!(
        (fs.turns_with_call, fs.loads, fs.errors, fs.ends_turn),
        (1, 1, 1, 0)
    );
}

fn view(chat: i64, master_turn: bool, trusted_chat: bool) -> Option<TurnView> {
    Some(TurnView {
        chat: ChatId(chat),
        actor: None,
        master_turn,
        trusted_chat,
        audience: Audience::Mixed,
    })
}

#[tokio::test]
async fn view_clamps_public_turns_to_their_chat() {
    let World { h, .. } = world().await;
    let chat1 = usage(&h, usage_args(Scope::Chat, Some(Target::Chat(ChatId(1))))).await;

    for (master_turn, trusted_chat) in [(false, true), (true, false)] {
        let got = usage(
            &h,
            tools_usage::Args {
                view: view(1, master_turn, trusted_chat),
                ..usage_args(Scope::Global, None)
            },
        )
        .await;
        assert_eq!(got, chat1, "public turn asks global and gets its own chat");
    }

    // Хозяйский тур в доверенном чате: опущенный target — текущий чат, его папка, его surface.
    let own = |scope| tools_usage::Args {
        view: view(1, true, true),
        ..usage_args(scope, None)
    };
    assert_eq!(usage(&h, own(Scope::Chat)).await, chat1);
    assert_eq!(usage(&h, own(Scope::Folder)).await, chat1);
    assert_eq!(usage(&h, own(Scope::Surface)).await.1, 9);
    assert_eq!(
        usage(&h, own(Scope::Global)).await.1,
        15,
        "master sees everything"
    );
}

#[tokio::test]
async fn bad_scope_and_period_are_invalid() {
    let World { h, .. } = world().await;
    let mut session = h.client_session().await;
    let cases = [
        usage_args(Scope::Chat, Some(Target::Folder(FolderId(1)))),
        usage_args(Scope::Global, Some(Target::Chat(ChatId(1)))),
        usage_args(Scope::Surface, None),
        tools_usage::Args {
            from: date(D2),
            to: date(D1),
            ..usage_args(Scope::Global, None)
        },
    ];
    for args in cases {
        let dbg = format!("{args:?}");
        let res = session.stats().tools_usage().call(args).await;
        assert!(
            matches!(res, Err(Generic::Aux(Aux::Invalid(_)))),
            "{dbg}: {res:?}"
        );
    }
}

fn spend_args(scope: Scope, target: Option<Target>, group_by: GroupBy) -> spend::Args {
    spend::Args {
        scope,
        target,
        from: date(D1),
        to: date(D2),
        group_by,
        view: None,
    }
}

async fn spend(h: &Harness, args: spend::Args) -> spend::Ok {
    h.client_session()
        .await
        .stats()
        .spend()
        .call(args)
        .await
        .expect("spend")
}

#[tokio::test]
async fn spend_groups_by_key_and_currency() {
    let World { h, .. } = world().await;
    let spend::Ok { rows, cache } = spend(
        &h,
        spend_args(Scope::Chat, Some(Target::Chat(ChatId(1))), GroupBy::Model),
    )
    .await;

    let keys: Vec<_> = rows
        .iter()
        .map(|r| (r.key.as_str(), r.currency.as_deref()))
        .collect();
    assert_eq!(
        keys,
        [
            ("mock-model", Some("EUR")),
            ("mock-model", Some("USD")),
            ("other-model", None)
        ],
        "currencies are never added together; the D0 row is out of the period"
    );
    let usd = &rows[1];
    assert_eq!(
        (
            usd.input,
            usd.cache_read,
            usd.cache_write,
            usd.cache_write_long,
            usd.output,
            usd.reasoning
        ),
        (1100, 1900, 500, 100, 60, 10)
    );
    assert!(usd.cost.is_some_and(|c| approx(c, 0.71)));
    assert_eq!(rows[2].cost, None, "unpriced rows stay unpriced");

    // Шаги 1, 2, 3, 5: cache_read 950, cache_write 500, prompt_total 2610.
    assert!(approx(cache.hit_ratio, 950.0 / 2610.0));
    assert!(approx(cache.write_ratio, 500.0 / 2610.0));
    assert_eq!(
        cache.cold_starts, 1,
        "row 2 only: other-model has no caps in the registry"
    );
    let breaks: Vec<_> = cache.breaks.iter().map(|b| (b.reason, b.count)).collect();
    assert_eq!(
        breaks,
        [(BreakReason::TtlExpired, 1), (BreakReason::Unknown, 1)]
    );
    assert_eq!(cache.context_edits, 1);
    assert_eq!(cache.keep_alive, 1);
    assert!(cache.keep_alive_cost.is_some_and(|c| approx(c, 0.01)));
    // Прайс-бук: Σ cache_read 1900 · (3.0 − 0.3) / 1e6.
    assert!(
        cache
            .cost_saved
            .is_some_and(|c| approx(c, 1900.0 * 2.7 / 1e6))
    );
}

#[tokio::test]
async fn spend_group_keys() {
    let World { h, .. } = world().await;
    let keys = |ok: spend::Ok| -> Vec<String> {
        let mut keys: Vec<_> = ok.rows.into_iter().map(|r| r.key).collect();
        keys.dedup();
        keys
    };

    let by_chat = keys(spend(&h, spend_args(Scope::Global, None, GroupBy::Chat)).await);
    assert_eq!(by_chat, ["c1", "c3", "none"]);
    let by_source = keys(spend(&h, spend_args(Scope::Global, None, GroupBy::Source)).await);
    assert_eq!(by_source, ["module", "surface"]);
    let by_op = keys(spend(&h, spend_args(Scope::Global, None, GroupBy::Op)).await);
    assert_eq!(by_op, ["keep_alive", "other", "step"]);
    let by_day = keys(spend(&h, spend_args(Scope::Global, None, GroupBy::Day)).await);
    assert_eq!(by_day, [D1, D2]);

    let svc = spend(
        &h,
        spend_args(
            Scope::Surface,
            Some(Target::Surface(SurfaceId::from(SVC))),
            GroupBy::Chat,
        ),
    )
    .await;
    assert_eq!(
        keys(svc),
        ["c3"],
        "module inference without a chat is global only"
    );
}

#[tokio::test]
async fn spend_series_by_day() {
    let World { h, .. } = world().await;
    let spend_series::Ok { series } = h
        .client_session()
        .await
        .stats()
        .spend_series()
        .call(spend_series::Args {
            scope: Scope::Chat,
            target: Some(Target::Chat(ChatId(1))),
            from: date(D1),
            to: date(D2),
            group_by: GroupBy::Model,
        })
        .await
        .expect("series");

    let points: Vec<_> = series
        .iter()
        .map(|p| (p.day, p.key.as_str(), p.prompt_total, p.output))
        .collect();
    assert_eq!(
        points,
        [
            (date(D1), "mock-model", 2500, 60),
            (date(D2), "mock-model", 1100, 20),
            (date(D2), "other-model", 10, 5),
        ]
    );
    assert!(series[0].cost.is_some_and(|c| approx(c, 0.7)));
    assert_eq!(series[2].cost, None);
}

async fn chat_spend(h: &Harness, chat: i64) -> chat_spend::Ok {
    h.client_session()
        .await
        .stats()
        .chat_spend()
        .call(chat_spend::Args { chat: ChatId(chat) })
        .await
        .expect("chat_spend")
}

#[tokio::test]
async fn chat_spend_totals_in_the_latest_currency() {
    let World { h, .. } = world().await;
    let chat_spend::Ok {
        total,
        by_model,
        last_turn,
        hit_ratio,
    } = chat_spend(&h, 1).await;

    // Новейшая оценённая строка — EUR: USD в итог не подмешивается.
    let total = total.expect("priced");
    assert_eq!(total.currency, "EUR");
    assert!(approx(total.cost, 0.3));

    let models: Vec<_> = by_model
        .iter()
        .map(|m| (m.model.as_str(), m.prompt_total, m.output))
        .collect();
    assert_eq!(models, [("mock-model", 4599, 81), ("other-model", 10, 5)]);
    assert!(by_model[1].cost.is_none());

    let last = last_turn.expect("last turn");
    assert_eq!(last.turn_id.as_str(), "t-b");
    assert_eq!((last.prompt_total, last.cache_read), (110, 50));
    assert!(last.cost.is_some_and(|m| approx(m.cost, 0.3)));

    assert!(approx(hit_ratio, 950.0 / 3609.0));
}

#[tokio::test]
async fn chat_spend_of_a_quiet_chat_and_errors() {
    let World { h, .. } = world().await;
    let quiet = chat_spend(&h, 2).await;
    assert_eq!(quiet.total, None);
    assert!(quiet.by_model.is_empty());
    assert_eq!(quiet.last_turn, None);
    assert!(approx(quiet.hit_ratio, 0.0));

    let res = h
        .client_session()
        .await
        .stats()
        .chat_spend()
        .call(chat_spend::Args { chat: ChatId(999) })
        .await;
    assert!(
        matches!(res, Err(Generic::Spec(chat_spend::Err::NotFound(ref e))) if e.chat == ChatId(999).into()),
        "{res:?}"
    );
}

#[tokio::test]
async fn health_counters_and_gauges() {
    let World { h, .. } = world().await;
    let t = i64::try_from(h.clock.get().as_nanos().get()).expect("nanos fit");
    exec(
        &h,
        format!(
            "INSERT INTO health_stats (day, metric, key, count) VALUES
               ('{D1}', 'cron.ok', 'daily', 2), ('{D2}', 'cron.ok', 'daily', 3),
               ('{D1}', 'tools.denied', 'forbidden', 0), ('{D0}', 'cron.failed', 'daily', 1),
               ('{D2}', 'sleep.runs', '', 1);
             INSERT INTO module_tokens (token_hash, module, kind, issued_via, issued_at)
               VALUES (x'01', '{SVC}', 'service', 'phone', {t});
             INSERT INTO agents (id, parent_chat_id, name, task, tools, master_turn, status, started_at)
               VALUES (2, 1, 'scout', 'look', '[]', 1, 'running', {t});
             INSERT INTO pending (id, chat_id, turn_id, call_id, kind, payload, created_at, expires_at, resolved_at, outcome)
               VALUES ('p-open', 1, 't', 'c', 'ask', '{{}}', {t}, {t}, NULL, NULL),
                      ('p-done', 1, 't', 'c', 'ask', '{{}}', {t}, {t}, {t}, 'answered');
             INSERT INTO sleep_runs (run_id, started, dry_run) VALUES ('n1', {t}, 0), ('n2', {later}, 0);
             INSERT INTO gc_runs (run_id, started, finished, ok) VALUES ('g1', {t}, {gc_end}, 1);
             INSERT INTO memory (kind, text, visibility, created_at, updated_at, valid_from, last_reinforced_at)
               VALUES ('fact', 'waits for a vector', 'public', {t}, {t}, {t}, {t});",
            later = t + 60 * SEC,
            gc_end = t + 5 * SEC,
        ),
    )
    .await;

    let health::Ok {
        gauges,
        counters,
        rate_limits,
    } = h
        .client_session()
        .await
        .stats()
        .health()
        .call(health::Args {
            from: date(D1),
            to: date(D2),
        })
        .await
        .expect("health");

    let counters: Vec<_> = counters
        .iter()
        .map(|c| (c.metric.as_str(), c.key.as_str(), c.count))
        .collect();
    assert_eq!(
        counters,
        [("cron.ok", "daily", 5), ("sleep.runs", "", 1)],
        "non-zero, in period"
    );
    assert!(rate_limits.is_empty());

    let modules: Vec<_> = gauges
        .modules
        .iter()
        .map(|m| (m.module.as_str(), m.kind, m.state))
        .collect();
    assert_eq!(
        modules,
        [(SVC, TokenKind::Service, State::Connected)],
        "modules without a live token have no kind and are left out"
    );
    assert!(gauges.modules[0].capabilities.is_empty());
    assert_eq!(gauges.agents_running, 1);
    assert_eq!(gauges.pending_open, 1);
    assert!(gauges.db_bytes > 0);
    assert_eq!(
        gauges.last_sleep_at.map(|t| t.as_nanos().get()),
        Some(u64::try_from(t + 60 * SEC).expect("positive"))
    );
    assert_eq!(
        gauges.gc_duration_last.map(|d| d.0),
        Some(Duration::from_secs(5))
    );
    assert_eq!(gauges.memory_embed_pending, 1);
    assert_eq!((gauges.index_pending, gauges.index_lag), (0, None));
}

#[tokio::test]
async fn configured_token_kind_wins() {
    let h = common::start_with(|cfg| {
        cfg.modules.tokens.insert(
            ModuleId::from(OTHER),
            TokenEntry {
                token: SecretString::Plaintext(String::from("secret")),
                kind: TokenKind::Client,
            },
        );
    })
    .await;
    h.register_module(OTHER).await;

    let health::Ok { gauges, .. } = h
        .kernel_session()
        .await
        .stats()
        .health()
        .call(health::Args {
            from: date(D1),
            to: date(D1),
        })
        .await
        .expect("health");
    let modules: Vec<_> = gauges
        .modules
        .iter()
        .map(|m| (m.module.as_str(), m.kind))
        .collect();
    assert_eq!(modules, [(OTHER, TokenKind::Client)]);
}

#[tokio::test]
async fn stats_matrix() {
    use Outcome::{Forbidden as F, Ok as K};

    let World { h, .. } = world().await;
    // (случай, [kernel, client, service-own, service-foreign])
    let cases: [(&str, [Outcome; 4]); 7] = [
        ("tools_usage", [K, K, F, F]),
        ("spend", [K, K, F, F]),
        ("spend_series", [K, K, F, F]),
        ("health", [K, K, F, F]),
        ("chat_spend svc chat", [K, K, K, F]),
        ("chat_spend local chat", [K, K, F, F]),
        (
            "chat_spend missing chat",
            [Outcome::Spec, Outcome::Spec, Outcome::Spec, Outcome::Spec],
        ),
    ];

    let mut failures = Vec::new();
    for (case, expected) in cases {
        for ((who, caller), want) in common::matrix_callers().into_iter().zip(expected) {
            let mut s = h.session(caller).await;
            let mut stats = s.stats();
            let chat_spend = |chat| chat_spend::Args { chat: ChatId(chat) };
            let got = match case {
                "tools_usage" => outcome(
                    &stats
                        .tools_usage()
                        .call(usage_args(Scope::Global, None))
                        .await,
                ),
                "spend" => outcome(
                    &stats
                        .spend()
                        .call(spend_args(Scope::Global, None, GroupBy::Model))
                        .await,
                ),
                "spend_series" => outcome(
                    &stats
                        .spend_series()
                        .call(spend_series::Args {
                            scope: Scope::Global,
                            target: None,
                            from: date(D1),
                            to: date(D2),
                            group_by: GroupBy::Day,
                        })
                        .await,
                ),
                "health" => outcome(
                    &stats
                        .health()
                        .call(health::Args {
                            from: date(D1),
                            to: date(D2),
                        })
                        .await,
                ),
                "chat_spend svc chat" => outcome(&stats.chat_spend().call(chat_spend(3)).await),
                "chat_spend local chat" => outcome(&stats.chat_spend().call(chat_spend(1)).await),
                _ => outcome(&stats.chat_spend().call(chat_spend(999)).await),
            };
            if got != want {
                failures.push(format!("stats {case} as {who}: want {want:?}, got {got:?}"));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// Сутки `spend` в зоне IANA — и в ключе группы, и в фильтре периода.
#[tokio::test]
async fn spend_days_follow_an_iana_zone() {
    let h = common::start_with(|cfg| cfg.system.tz = Some(String::from("America/New_York"))).await;
    // 2026-07-01T02:00Z — ещё 30 июня в Нью-Йорке (EDT, −4).
    let ts = (i64::from(date("2026-07-01").days()) * 86_400 + 7_200) * SEC;
    exec(
        &h,
        format!(
            "INSERT INTO spend (ts, model, source, op, input, cache_read, cache_write, output, cost_source)
             VALUES ({ts}, 'mock-model', 'system', 'other', 1, 0, 0, 1, 'none');"
        ),
    )
    .await;

    let day = date("2026-06-30");
    let ok = spend(
        &h,
        spend::Args {
            scope: Scope::Global,
            target: None,
            from: day,
            to: day,
            group_by: GroupBy::Day,
            view: None,
        },
    )
    .await;
    let keys: Vec<_> = ok.rows.iter().map(|r| r.key.as_str()).collect();
    assert_eq!(keys, ["2026-06-30"]);
}
