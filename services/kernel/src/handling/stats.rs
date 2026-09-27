//! Хендлеры `stats.*` (`09 §7`): только чтение; глобальные — хозяину,
//! `chat_spend` — по праву чтения чата. `view` публичного тура сжимает
//! разрез до его чата (`09 §6`).

use std::time::Duration;

use eva::time::{HumanDuration, Timestamp};
use eva_protocol::{
    errors::{self, Aux, AuxResult, ext::*},
    requests::{
        Response,
        stats::{chat_spend, health, spend, spend_series, tools_usage},
    },
    types::{
        stats::{Gauges, Scope, Target},
        turn::TurnView,
    },
};

use crate::{
    components::Components,
    convert,
    db::{
        DbError, Queries as _,
        stats::{Period, ScopeFilter},
    },
    state::{Policy, State},
};

fn period(from: eva::time::Date, to: eva::time::Date) -> AuxResult<Period> {
    (from <= to).true_or(Aux::Invalid(format!(
        "period starts after it ends: {from} > {to}",
        from = crate::db::stats::day_str(from),
        to = crate::db::stats::day_str(to),
    )))?;
    Ok(Period { from, to })
}

/// `Target` обязан совпадать по виду со `Scope`; опущенный — чат тура из `view`.
fn scope(scope: Scope, target: Option<Target>, view: Option<&TurnView>) -> AuxResult<ScopeFilter> {
    if let Some(view) = view
        && view.public()
    {
        return Ok(ScopeFilter::Chat(view.chat));
    }
    let current = view.map(|v| v.chat);
    match (scope, target, current) {
        (Scope::Global, None, _) => Ok(ScopeFilter::Global),
        (Scope::Global, Some(_), _) => Err(Aux::Invalid("scope global takes no target".into())),
        (Scope::Chat, Some(Target::Chat(chat)), _) => Ok(ScopeFilter::Chat(chat)),
        (Scope::Folder, Some(Target::Folder(folder)), _) => Ok(ScopeFilter::Folder(folder)),
        (Scope::Surface, Some(Target::Surface(surface)), _) => Ok(ScopeFilter::Surface(surface)),
        (Scope::Chat, None, Some(chat)) => Ok(ScopeFilter::Chat(chat)),
        (Scope::Folder, None, Some(chat)) => Ok(ScopeFilter::FolderOf(chat)),
        (Scope::Surface, None, Some(chat)) => Ok(ScopeFilter::SurfaceOf(chat)),
        (scope, None, None) => Err(Aux::Invalid(format!("scope {scope:?} needs a target"))),
        (scope, Some(_), _) => Err(Aux::Invalid(format!(
            "target kind does not match scope {scope:?}"
        ))),
    }
}

#[eva::endpoint]
#[fastrace::trace]
pub async fn ToolsUsage<C: Components>(
    state: &mut State<C>,
    args: tools_usage::Args,
) -> Response<tools_usage::Ok, tools_usage::Err> {
    let State {
        shared,
        caller,
        db_conn,
        ..
    } = state;
    Policy::new(caller).stats().global()?;
    let tools_usage::Args {
        scope: kind,
        target,
        from,
        to,
        top,
        view,
    } = args;
    let period = period(from, to)?;
    let filter = scope(kind, target, view.as_ref())?;

    let db = db_conn.acquire(shared).await?;
    let (rows, turns) = db
        .run(move |c| -> Result<_, DbError> {
            let rows = c.stats().tools_usage(&filter, period, top.into_inner())?;
            let turns = c.stats().turns(&filter, period)?;
            Ok((rows, turns))
        })
        .await?;

    Ok(tools_usage::Ok {
        rows: rows.into_iter().map(convert::stats::tool_usage).collect(),
        turns,
    })
}

#[eva::endpoint]
#[fastrace::trace]
pub async fn Spend<C: Components>(
    state: &mut State<C>,
    args: spend::Args,
) -> Response<spend::Ok, spend::Err> {
    let State {
        shared,
        caller,
        db_conn,
        ..
    } = state;
    Policy::new(caller).stats().global()?;
    let spend::Args {
        scope: kind,
        target,
        from,
        to,
        group_by,
        view,
    } = args;
    let period = period(from, to)?;
    let filter = scope(kind, target, view.as_ref())?;
    let zone = shared.day_zone();

    let db = db_conn.acquire(shared).await?;
    let (rows, cache) = db
        .run(move |c| -> Result<_, DbError> {
            let rows = c.stats().spend(&filter, period, group_by, &zone)?;
            let cache = c.stats().cache(&filter, period, &zone)?;
            Ok((rows, cache))
        })
        .await?;

    Ok(spend::Ok {
        rows: rows.into_iter().map(convert::stats::spend).collect(),
        cache: convert::stats::cache(cache, &shared.models()),
    })
}

#[eva::endpoint]
#[fastrace::trace]
pub async fn SpendSeries<C: Components>(
    state: &mut State<C>,
    args: spend_series::Args,
) -> Response<spend_series::Ok, spend_series::Err> {
    let State {
        shared,
        caller,
        db_conn,
        ..
    } = state;
    Policy::new(caller).stats().global()?;
    let spend_series::Args {
        scope: kind,
        target,
        from,
        to,
        group_by,
    } = args;
    let period = period(from, to)?;
    let filter = scope(kind, target, None)?;
    let zone = shared.day_zone();

    let db = db_conn.acquire(shared).await?;
    let rows = db
        .run(move |c| c.stats().series(&filter, period, group_by, &zone))
        .await?;

    Ok(spend_series::Ok {
        series: rows.into_iter().map(convert::stats::series).collect(),
    })
}

#[eva::endpoint]
#[fastrace::trace]
pub async fn ChatSpend<C: Components>(
    state: &mut State<C>,
    args: chat_spend::Args,
) -> Response<chat_spend::Ok, chat_spend::Err> {
    let State {
        shared,
        caller,
        db_conn,
        ..
    } = state;
    let chat_spend::Args { chat } = args;
    let policy = Policy::new(caller);

    let db = db_conn.acquire(shared).await?;
    let chat_ref = db
        .run(move |c| c.chats().chat_ref(chat))
        .await?
        .ok_or_else(|| errors::chats::NotFound { chat: chat.into() }.spec())?;
    policy.chats().read(&chat_ref)?;

    let row = db.run(move |c| c.stats().chat_spend(chat)).await?;
    Ok(convert::stats::chat_spend(row))
}

#[eva::endpoint]
#[fastrace::trace]
pub async fn Health<C: Components>(
    state: &mut State<C>,
    args: health::Args,
) -> Response<health::Ok, health::Err> {
    let State {
        shared,
        world,
        caller,
        db_conn,
        ..
    } = state;
    Policy::new(caller).stats().global()?;
    let health::Args { from, to } = args;
    let period = period(from, to)?;
    let now = world.now();

    let db = db_conn.acquire(shared).await?;
    let (counters, modules, snapshot) = db
        .run(move |c| -> Result<_, DbError> {
            let snapshot = Snapshot {
                agents_running: c.agents().running()?,
                pending_open: c.agents().pending_open()?,
                db_bytes: c.stats().db_bytes()?,
                last_sleep_at: c.sleep().last_started()?,
                last_gc: c.sleep().last_gc()?.map(|gc| (gc.started, gc.finished)),
                index: c.stats().index_backlog()?,
                embed_pending: c.stats().embed_pending()?,
            };
            Ok((c.stats().counters(period)?, c.stats().modules()?, snapshot))
        })
        .await?;

    let tokens = &shared.cfg().modules.tokens;
    let modules = modules
        .into_iter()
        .map(|row| {
            let config_kind = tokens.get(&row.module).map(|t| t.kind);
            convert::stats::module(row, config_kind)
        })
        .filter_map(Result::transpose)
        .collect::<Result<_, DbError>>()?;

    let gauges = Gauges {
        modules,
        // Туры и hot-блок — состояние подсистем в памяти, в БД его нет.
        turns_in_flight: 0,
        agents_running: snapshot.agents_running,
        pending_open: snapshot.pending_open,
        db_bytes: snapshot.db_bytes,
        uptime: elapsed(shared.started_at(), now),
        last_sleep_at: snapshot.last_sleep_at,
        next_gc_at: None,
        gc_duration_last: snapshot
            .last_gc
            .map(|(started, finished)| elapsed(started, finished)),
        index_pending: snapshot.index.pending,
        index_lag: snapshot.index.oldest.map(|oldest| elapsed(oldest, now)),
        memory_hot_tokens: 0,
        memory_embed_pending: snapshot.embed_pending,
        cron_fire_lag: Vec::new(),
        latency: Vec::new(),
    };

    Ok(health::Ok {
        gauges,
        counters: counters.into_iter().map(convert::stats::counter).collect(),
        rate_limits: Vec::new(),
    })
}

/// Gauge'и из БД, собранные одним чтением.
struct Snapshot {
    agents_running: u32,
    pending_open: u32,
    db_bytes: u64,
    last_sleep_at: Option<Timestamp>,
    last_gc: Option<(Timestamp, Timestamp)>,
    index: crate::db::stats::IndexBacklog,
    embed_pending: u64,
}

/// Часы мира могут отстать от записанного — тогда ноль, не паника.
fn elapsed(from: Timestamp, to: Timestamp) -> HumanDuration {
    let nanos = to.as_nanos().get().saturating_sub(from.as_nanos().get());
    HumanDuration(Duration::from_nanos(nanos))
}
