//! `stats.*` (`09 §1.4`, `§2`, `§3`, `§6`, `§7`): разрезы по текущему
//! положению чата, группы расхода без смешения валют, метрики кэша по caps
//! реестра, сжатие публичного `view`, gauge'и и матрица `Policy`.

mod common;

use std::time::Duration;

use common::{CLIENT, Harness, OTHER, Outcome, SVC, outcome};
use eva::{
    handling::CallStep as _,
    time::{Clock as _, Date},
    utils::SecretString,
};
use eva_kernel::{
    config::modules::TokenEntry,
    db::{
        Db as _, DbResult,
        stats::{Metric, Op as StatsOp},
    },
    subsystems::stats::{Call, Origin, Recorder},
};
use eva_protocol::{
    errors::{Aux, Generic},
    requests::{
        Stats::{Trait as _, chat_spend, health, spend, spend_series, tools_usage},
        Top,
    },
    types::{
        ChatId, FolderId, ModelId, ModuleId, SurfaceId, ToolId, TurnId,
        generate::{RateLimits, Usage},
        model::{ModelCaps, ModelDecl, Price},
        module::{Dial, ModuleKind, ServiceAddress, State},
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
/// Мок-модель с кэшем и ценой: холодные старты и экономия считаются по реестру.
fn priced_mock() -> eva_providers::mock::Config {
    eva_providers::mock::Config {
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
    }
}

async fn world() -> World {
    let h = common::start_with_mock(|_| {}, priced_mock()).await;
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
             VALUES ('{day}', {chat}, {tool_id}, {calls}, 1, {tool}, 1, 0, {t});", tool_id = tool + 10000
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
             VALUES (10001, 'kernel', 'fixture.read', 'fixture_read', '{{}}', x'00', {t}, {t}, NULL),
                    (10002, 'kernel', 'fixture.old', 'fixture_old', '{{}}', x'01', {t}, {t}, {t});"
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
        .inner_mut()
        .tools_usage()
        .call(args)
        .await
        .expect("tools_usage");
    let rows = rows
        .into_iter()
        .map(|r| (r.tool_id.get() - 10000, r.name, r.calls))
        .collect();
    (rows, turns)
}

fn fs_read() -> Option<String> {
    Some(String::from("fixture_read"))
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
        .inner_mut()
        .tools_usage()
        .call(usage_args(Scope::Chat, Some(Target::Chat(ChatId(1)))))
        .await
        .expect("tools_usage");
    let fs = rows
        .iter()
        .find(|r| r.tool_id == ToolId(10001))
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
        let res = session.stats().inner_mut().tools_usage().call(args).await;
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
        .inner_mut()
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
        .spend_series(
            Scope::Chat,
            Some(Target::Chat(ChatId(1))),
            date(D1),
            date(D2),
            GroupBy::Model,
        )
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
        .chat_spend(ChatId(chat))
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
        .chat_spend(ChatId(999))
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
             INSERT INTO module_tokens (token_hash, module, issued_via, issued_at)
               VALUES (x'01', '{CLIENT}', 'phone', {t});
             INSERT INTO module_services (module, address, key, added_via, added_at)
               VALUES ('{SVC}', 'uds:///run/eva-svc/module.sock', 'svc-key', 'phone', {t}),
                      ('bridge', 'tcp://bridge.lan:7000', 'bridge-key', 'phone', {later});
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
        .health(date(D1), date(D2))
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
        [
            ("bridge", ModuleKind::Service, State::Offline),
            (CLIENT, ModuleKind::Client, State::Connected),
            (SVC, ModuleKind::Service, State::Connected),
        ],
        "neither a registry entry nor a live token: no kind, left out; a never dialed entry is offline"
    );
    let bridge = &gauges.modules[0];
    assert_eq!(
        bridge.since.as_nanos().get(),
        u64::try_from(t + 60 * SEC).expect("positive"),
        "since = added_at"
    );
    let svc = &gauges.modules[2];
    assert!(svc.capabilities.is_empty());
    assert_eq!(
        svc.dial,
        Some(Dial {
            address: ServiceAddress::Uds("/run/eva-svc/module.sock".into()),
            config: false,
            error: None,
        })
    );
    assert_eq!(gauges.modules[1].dial, None);
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
async fn configured_registrations_name_the_kind() {
    let h = common::start_with(|cfg| {
        cfg.modules.tokens.insert(
            ModuleId::from(OTHER),
            TokenEntry::new(SecretString::Plaintext(String::from("secret"))),
        );
        common::with_service(cfg, SVC);
    })
    .await;
    h.register_module(OTHER).await;
    h.register_module(SVC).await;

    let health::Ok { gauges, .. } = h
        .kernel_session()
        .await
        .stats()
        .health(date(D1), date(D1))
        .await
        .expect("health");
    let modules: Vec<_> = gauges
        .modules
        .iter()
        .map(|m| (m.module.as_str(), m.kind, m.dial.as_ref().map(|d| d.config)))
        .collect();
    assert_eq!(
        modules,
        [
            (OTHER, ModuleKind::Client, None),
            (SVC, ModuleKind::Service, Some(true)),
        ]
    );
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
            let mut stats_client = s.stats();
            let stats = stats_client.inner_mut();
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

// --- запись: `stats::Recorder` (`09 §1.3`, `§2`, `§3`, `§4`) ---

fn call(op: StatsOp, reason: Option<BreakReason>) -> Call {
    Call {
        model: ModelId::from(common::MODEL),
        origin: Origin::Surface,
        op,
        chat: Some(ChatId(1)),
        turn: Some(TurnId::from("turn-1")),
        run: None,
        cache_break_reason: reason,
    }
}

fn chunk(input: u64, cache_read: u64, output: u64) -> Usage {
    Usage {
        input,
        cache_read,
        output,
        ..Usage::default()
    }
}

async fn table(h: &Harness, sql: &'static str) -> Vec<String> {
    h.query(move |c| {
        let mut stmt = c.prepare(sql)?;
        let n = stmt.column_count();
        let rows = stmt
            .query_map([], |r| {
                (0..n)
                    .map(|i| {
                        r.get::<_, rusqlite::types::Value>(i).map(|v| match v {
                            rusqlite::types::Value::Null => String::from("-"),
                            rusqlite::types::Value::Integer(n) => n.to_string(),
                            rusqlite::types::Value::Real(x) => x.to_string(),
                            rusqlite::types::Value::Text(s) => s,
                            rusqlite::types::Value::Blob(b) => format!("{b:?}"),
                        })
                    })
                    .collect::<Result<Vec<_>, _>>()
                    .map(|cols| cols.join("|"))
            })?
            .collect::<Result<_, _>>()?;
        Ok(rows)
    })
    .await
}

#[tokio::test]
async fn spend_writes_a_row_per_chunk_and_the_cache_counters() {
    let h = common::start_with_mock(
        |cfg| cfg.system.tz = Some(String::from("UTC")),
        priced_mock(),
    )
    .await;
    let shared = h.kernel.shared();
    let rec = Recorder::new(shared);
    let limits = RateLimits {
        requests_remaining: Some(7),
        tokens_remaining: None,
        reset_at: None,
    };
    let cold_tail = Usage {
        output: 50,
        rate_limits: Some(limits),
        ..Usage::default()
    };
    rec.spend(
        call(StatsOp::Step, Some(BreakReason::ToolsChange)),
        &[chunk(2000, 0, 0), cold_tail],
    )
    .await
    .expect("cold step in two chunks");
    rec.spend(
        call(StatsOp::Step, Some(BreakReason::FirstRequest)),
        &[chunk(10, 5000, 20)],
    )
    .await
    .expect("warm step");
    let provided = Usage {
        cost: Some(0.25),
        ..chunk(1, 0, 0)
    };
    rec.spend(call(StatsOp::KeepAlive, None), &[provided])
        .await
        .expect("keep-alive");
    let module = Call {
        model: ModelId::from("unpriced"),
        origin: Origin::Module(ModuleId::from(SVC)),
        op: StatsOp::Other,
        chat: None,
        turn: None,
        run: None,
        cache_break_reason: None,
    };
    rec.spend(module, &[chunk(1, 0, 1)])
        .await
        .expect("module call");
    rec.spend(call(StatsOp::Step, None), &[])
        .await
        .expect("no usage");

    assert_eq!(
        table(&h, "SELECT turn_id, source, op, module, cache_break_reason, cost_source FROM spend ORDER BY id").await,
        [
            "turn-1|surface|step|-|tools_change|pricebook",
            "turn-1|surface|step|-|-|pricebook",
            "turn-1|surface|step|-|first_request|pricebook",
            "turn-1|surface|keep_alive|-|-|provider",
            format!("-|module|other|{SVC}|-|none").as_str(),
        ],
        "a row per chunk; the break rides on the first one"
    );
    assert_eq!(
        table(
            &h,
            "SELECT metric, key, SUM(count) FROM health_stats GROUP BY 1, 2 ORDER BY 1, 2"
        )
        .await,
        [
            "cache.break|tools_change|1",
            "cache.cold_start|mock-model|1",
            "cache.keep_alive|mock-model|1",
            "inference.usage_missing|mock-model|1",
            "spend.unpriced|unpriced|1",
        ]
    );

    let today = h.clock.get().date().expect("date");
    let health::Ok { rate_limits, .. } = h
        .client_session()
        .await
        .stats()
        .health(today, today)
        .await
        .expect("health");
    let got: Vec<_> = rate_limits
        .iter()
        .map(|r| (r.model.as_str(), r.requests_remaining))
        .collect();
    assert_eq!(
        got,
        [(common::MODEL, Some(7))],
        "the last snapshot per model"
    );
}

#[tokio::test]
async fn a_turn_tally_flushes_one_bucket_per_tool() {
    let h = common::start_with(|cfg| cfg.system.tz = Some(String::from("UTC"))).await;
    let shared = h.kernel.shared();
    let mut tally = Recorder::new(shared).tally(ChatId(1), shared.now());
    tally.note_step();
    tally.note_step();
    tally.note_user_message();
    tally.note_assistant_message();
    tally.note_call(ToolId(1), false, false);
    tally.note_call(ToolId(1), true, true);
    tally.note_call(ToolId(2), false, true);
    tally.note_load(ToolId(3));
    tally.finish(false).await.expect("flush");

    assert_eq!(
        table(&h, "SELECT tool_id, calls, turns_with_call, errors, ends_turn, loads FROM tool_stats ORDER BY tool_id").await,
        ["1|2|1|1|0|0", "2|1|1|0|1|0", "3|0|0|0|0|1"],
        "an error never ends the turn; a load alone is not coverage"
    );
    assert_eq!(
        table(
            &h,
            "SELECT turns, steps, user_messages, assistant_messages, cancelled FROM chat_stats"
        )
        .await,
        ["1|2|1|1|0"]
    );
}

#[tokio::test]
async fn a_dropped_tally_flushes_as_cancelled() {
    let h = common::start_with(|cfg| cfg.system.tz = Some(String::from("UTC"))).await;
    let shared = h.kernel.shared();
    let mut tally = Recorder::new(shared).tally(ChatId(1), shared.now());
    tally.note_call(ToolId(1), false, false);
    drop(tally);

    let mut rows = Vec::new();
    for _ in 0..100 {
        rows = table(&h, "SELECT turns, cancelled FROM chat_stats").await;
        if !rows.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        rows,
        ["1|1"],
        "cancellation and panics still count the turn"
    );
    assert_eq!(table(&h, "SELECT calls FROM tool_stats").await, ["1"]);
}

fn at(iso: &str) -> eva::time::Timestamp {
    let ts: jiff::Timestamp = iso.parse().expect("timestamp");
    let nanos = u64::try_from(ts.as_nanosecond()).expect("after 1970");
    eva::time::Timestamp::from_nanos(std::num::NonZeroU64::new(nanos).expect("non-zero"))
}

#[tokio::test]
async fn a_turn_belongs_to_the_day_it_started_in_system_tz() {
    let h = common::start_with(|cfg| cfg.system.tz = Some(String::from("America/New_York"))).await;
    let shared = h.kernel.shared();
    // 2025-03-09 — переход на летнее время: сутки в 23 часа.
    for started in [
        "2025-03-08T23:30:00-05:00",
        "2025-03-09T00:30:00-05:00",
        "2025-03-09T23:59:00-04:00",
    ] {
        let mut tally = Recorder::new(shared).tally(ChatId(5), at(started));
        tally.note_step();
        tally.finish(false).await.expect("flush");
    }
    assert_eq!(
        table(
            &h,
            "SELECT day, turns FROM chat_stats WHERE chat_id = 5 ORDER BY day"
        )
        .await,
        ["2025-03-08|1", "2025-03-09|2"],
        "a turn past midnight stays with its start; the boundary is 00:00 local"
    );
}

#[test]
fn tally_buckets_keep_their_invariants() {
    use proptest::prelude::*;

    #[derive(Debug, Clone)]
    enum Note {
        Call { tool: i64, error: bool, ends: bool },
        Load { tool: i64 },
    }

    let note = prop_oneof![
        (1..4i64, any::<bool>(), any::<bool>()).prop_map(|(tool, error, ends)| Note::Call {
            tool,
            error,
            ends
        }),
        (1..4i64).prop_map(|tool| Note::Load { tool }),
    ];
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let h = rt.block_on(common::start_with(|cfg| {
        cfg.system.tz = Some(String::from("UTC"))
    }));
    let chats = std::sync::atomic::AtomicI64::new(0);
    proptest!(ProptestConfig::with_cases(32), |(notes in prop::collection::vec(note, 0..24))| {
        let chat_id = ChatId(chats.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1);
        rt.block_on(async {
            let shared = h.kernel.shared();
            let mut tally = Recorder::new(shared).tally(chat_id, shared.now());
            for n in &notes {
                match *n {
                    Note::Call { tool, error, ends } => tally.note_call(ToolId(tool), error, ends),
                    Note::Load { tool } => tally.note_load(ToolId(tool)),
                }
            }
            tally.finish(false).await.expect("flush");
        });
        let rows: Vec<(i64, u32, u32, u32, u32, u32)> = rt.block_on(h.query(move |c| {
            let mut stmt = c.prepare(
                "SELECT tool_id, calls, turns_with_call, loads, errors, ends_turn FROM tool_stats WHERE chat_id = ?1",
            )?;
            let rows = stmt
                .query_map([chat_id.get()], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)))?
                .collect::<Result<_, _>>()?;
            Ok(rows)
        }));
        let calls = notes.iter().filter(|n| matches!(n, Note::Call { .. })).count();
        prop_assert_eq!(rows.iter().map(|r| r.1 as usize).sum::<usize>(), calls);
        for (tool, calls, with_call, loads, errors, ends) in rows {
            prop_assert_eq!(with_call, u32::from(calls > 0), "coverage is 0/1 per turn");
            prop_assert!(ends <= calls - errors);
            let want_loads = notes.iter().filter(|n| matches!(n, Note::Load { tool: t } if *t == tool)).count();
            prop_assert_eq!(loads as usize, want_loads, "loads do not depend on calls");
        }
    });
}

async fn live_gauges(h: &Harness) -> eva_protocol::types::stats::Gauges {
    let today = h.clock.get().date().expect("date");
    let health::Ok { gauges, .. } = h
        .client_session()
        .await
        .stats()
        .health(today, today)
        .await
        .expect("health");
    gauges
}

#[tokio::test]
async fn live_gauges_reach_health() {
    let h = common::start().await;
    let gauges = h.kernel.shared().gauges();
    let turn = gauges.turn_started();
    gauges.set_hot_tokens(321);
    gauges.latency("tools.call", Duration::from_millis(40));
    let g = live_gauges(&h).await;
    assert_eq!((g.turns_in_flight, g.memory_hot_tokens), (1, 321));
    assert_eq!(
        g.latency
            .iter()
            .map(|l| (l.method.as_str(), l.p50.0))
            .collect::<Vec<_>>(),
        [("tools.call", Duration::from_millis(40))]
    );
    drop(turn);
    assert_eq!(live_gauges(&h).await.turns_in_flight, 0);
}

#[test]
fn metric_registry() {
    let names: Vec<_> = Metric::ALL.iter().map(|m| m.as_str()).collect();
    insta::assert_snapshot!(names.join("\n"));
}

/// `tool_stats` пишет только `db/stats.rs` (`09 §9`).
#[test]
fn tool_stats_has_a_single_writer() {
    fn walk(dir: &std::path::Path, out: &mut Vec<String>) {
        for entry in std::fs::read_dir(dir).expect("read dir").flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, out);
            } else if path.extension().is_some_and(|e| e == "rs")
                && std::fs::read_to_string(&path)
                    .is_ok_and(|s| s.contains("INSERT INTO tool_stats"))
            {
                out.push(path.display().to_string());
            }
        }
    }
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut writers = Vec::new();
    walk(&src, &mut writers);
    assert_eq!(writers, [src.join("db/stats.rs").display().to_string()]);
}
