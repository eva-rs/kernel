//! Текстовые отчёты модели используют те же чтения и разрезы, что RPC.

use std::collections::BTreeMap;

use eva::{
    data,
    time::{Date, HumanDuration},
};

use eva_protocol::{
    errors::{Aux, Generic},
    requests::{
        Stats::{self, spend, tools_usage},
        Top,
    },
    types::{
        ChatId, FolderId, SurfaceId,
        stats::{GroupBy, Scope, Target},
        tool::{ResultForm, ToolContent, ToolDecl},
    },
    wire::tools::ToolResult,
};
use serde_json::Value;

use crate::{
    components::Components,
    db::{
        Db as _, Queries as _,
        stats::{Period as DateRange, day_str, parse_day},
    },
    shared::Shared,
    state::Policy,
    subsystems::{tools::schema::input_schema, turns::ports::ToolsetCall},
};

#[data(copy)]
pub enum Period {
    Day,
    Week,
    Month,
    Year,
}
fn week() -> Period {
    Period::Week
}
fn month() -> Period {
    Period::Month
}
fn day() -> Period {
    Period::Day
}

#[data]
#[serde(deny_unknown_fields)]
pub struct ToolsUsage {
    #[serde(default)]
    pub scope: Scope,
    pub target: Option<String>,
    #[serde(default = "week")]
    pub period: Period,
    pub ending: Option<Date>,
    #[serde(default)]
    pub top: Top,
}

#[data]
#[serde(deny_unknown_fields)]
pub struct Spend {
    #[serde(default)]
    pub scope: Scope,
    pub target: Option<String>,
    #[serde(default = "month")]
    pub period: Period,
    pub ending: Option<Date>,
    #[serde(default)]
    pub group_by: GroupBy,
}

#[data]
#[serde(deny_unknown_fields)]
pub struct Health {
    #[serde(default = "day")]
    pub period: Period,
    pub ending: Option<Date>,
}

pub fn decls() -> Vec<ToolDecl> {
    [
        (
            "stats.tools_usage",
            "Read tool calls, coverage and errors for this agent.",
            input_schema::<ToolsUsage>(),
        ),
        (
            "stats.spend",
            "Read inference usage, costs by currency and cache effectiveness.",
            input_schema::<Spend>(),
        ),
        (
            "stats.health",
            "Read live gauges, nonzero counters, rate limits and alerts.",
            input_schema::<Health>(),
        ),
    ]
    .into_iter()
    .map(|(path, description, input)| ToolDecl {
        path: path.into(),
        description: description.into(),
        input,
        master_only: true,
        trusted_only: false,
        needs: vec![],
        ends_turn: false,
        parallel_safe: true,
        waits_on_cancel: false,
        result: ResultForm::Windowed,
        timeout: None,
        idempotent: true,
    })
    .collect()
}

fn api_error<E: std::fmt::Display>(error: Generic<E>) -> Aux {
    match error {
        Generic::Aux(error) => error,
        Generic::Spec(error) => Aux::Invalid(error.to_string()),
    }
}

fn parse<T: serde::de::DeserializeOwned>(input: Value) -> Result<T, Aux> {
    serde_json::from_value(input).map_err(|e| Aux::Invalid(format!("invalid input: {e}")))
}

fn range<C: Components>(
    shared: &Shared<C>,
    period: Period,
    ending: Option<Date>,
) -> Result<DateRange, Aux> {
    let to = ending
        .map(Ok)
        .unwrap_or_else(|| parse_day(&shared.day_zone().day(shared.now())))?;
    interval(period, to)
}

fn interval(period: Period, to: Date) -> Result<DateRange, Aux> {
    let ending: jiff::civil::Date = day_str(to)
        .parse()
        .map_err(|e| Aux::Invalid(format!("invalid ending: {e}")))?;
    let span = match period {
        Period::Day => jiff::Span::new().days(1),
        Period::Week => jiff::Span::new().days(7),
        Period::Month => jiff::Span::new().months(1),
        Period::Year => jiff::Span::new().years(1),
    };
    let start = ending
        .checked_sub(span)
        .and_then(|d| d.tomorrow())
        .map_err(|e| Aux::Invalid(format!("period exceeds calendar range: {e}")))?;
    let from = parse_day(&start.to_string())
        .map_err(|_| Aux::Invalid("period precedes supported calendar range".into()))?;
    Ok(DateRange { from, to })
}

fn target(scope: Scope, target: Option<String>, public: bool) -> Result<Option<Target>, Aux> {
    if public {
        return Ok(None);
    }
    target
        .map(|target| match scope {
            Scope::Chat => serde_json::from_value::<ChatId>(Value::String(target))
                .ok()
                .filter(|id| id.get() > 0)
                .map(Target::Chat)
                .ok_or_else(|| Aux::Invalid("target must be a chat id".into())),
            Scope::Folder => serde_json::from_value::<FolderId>(Value::String(target))
                .ok()
                .filter(|id| id.get() > 0)
                .map(Target::Folder)
                .ok_or_else(|| Aux::Invalid("target must be a folder id".into())),
            Scope::Surface => Ok(Target::Surface(SurfaceId::from(target))),
            Scope::Global => Err(Aux::Invalid("scope global takes no target".into())),
        })
        .transpose()
}

fn text(body: String) -> ToolResult {
    ToolResult::ok(vec![ToolContent::text(body)])
}

pub async fn call<C: Components>(
    shared: &Shared<C>,
    call: ToolsetCall<'_>,
) -> Result<ToolResult, Aux> {
    let result = async {
        Policy::stats_tool(call.view)?;
        let mut session = crate::session::internal(shared);
        match call.canonical {
            "stats_tools_usage" => {
                let args: ToolsUsage = parse(call.input)?;
                let period = range(shared, args.period, args.ending)?;
                let target = target(args.scope, args.target, call.view.public())?;
                let filter =
                    crate::handling::stats::scope(args.scope, target.clone(), Some(call.view))?;
                let result = session
                    .stats()
                    .tools_usage(
                        args.scope,
                        target,
                        period.from,
                        period.to,
                        args.top,
                        Some(call.view.clone()),
                    )
                    .await
                    .map_err(api_error)?;
                let (calls, distinct) = shared
                    .db()
                    .reader()
                    .await?
                    .run(move |c| c.stats().tools_totals(&filter, period))
                    .await?;
                Ok(text(usage_text(result, calls, distinct)))
            }
            "stats_spend" => {
                let args: Spend = parse(call.input)?;
                let period = range(shared, args.period, args.ending)?;
                let target = target(args.scope, args.target, call.view.public())?;
                let filter =
                    crate::handling::stats::scope(args.scope, target.clone(), Some(call.view))?;
                let result = session
                    .stats()
                    .spend(
                        args.scope,
                        target,
                        period.from,
                        period.to,
                        args.group_by,
                        Some(call.view.clone()),
                    )
                    .await
                    .map_err(api_error)?;
                let zone = shared.day_zone();
                let unpriced = shared
                    .db()
                    .reader()
                    .await?
                    .run(move |c| c.stats().unpriced(&filter, period, &zone))
                    .await?;
                Ok(text(spend_text(result, unpriced)))
            }
            "stats_health" => {
                let args: Health = parse(call.input)?;
                let period = range(shared, args.period, args.ending)?;
                let result = session
                    .stats()
                    .health(period.from, period.to)
                    .await
                    .map_err(api_error)?;
                Ok(text(health_text(result, call.view.public())))
            }
            _ => Err(Aux::Internal(format!(
                "unknown stats tool {}",
                call.canonical
            ))),
        }
    }
    .await;
    match result {
        Err(error @ (Aux::Db(_) | Aux::Internal(_))) => Err(error),
        Err(error) => Ok(ToolResult::error(error.to_string())),
        Ok(result) => Ok(result),
    }
}

fn usage_text(result: tools_usage::Ok, calls: u64, distinct: u64) -> String {
    let mut text = String::from("tool | calls | coverage % | loads | errors | ends_turn\n");
    for row in result.rows {
        let name = row.name.unwrap_or_else(|| format!("#{}", row.tool_id));
        let coverage = if result.turns == 0 {
            0.0
        } else {
            100.0 * row.turns_with_call as f64 / result.turns as f64
        };
        text.push_str(&format!(
            "{name} | {} | {coverage:.1} | {} | {} | {}\n",
            row.calls, row.loads, row.errors, row.ends_turn
        ));
    }
    text.push_str(&format!(
        "turns: {}; tools used: {calls}; distinct: {distinct}",
        result.turns
    ));
    text
}

fn spend_text(result: spend::Ok, unpriced: u64) -> String {
    let mut text = String::from(
        "group | input | cache_read | cache_write | output | reasoning | cost currency\n",
    );
    let mut totals = BTreeMap::<String, f64>::new();
    for row in result.rows {
        let cost = match (row.cost, row.currency) {
            (Some(cost), Some(currency)) => {
                *totals.entry(currency.clone()).or_default() += cost;
                format!("{cost:.6} {currency}")
            }
            _ => "unpriced".into(),
        };
        text.push_str(&format!(
            "{} | {} | {} | {} | {} | {} | {cost}\n",
            row.key, row.input, row.cache_read, row.cache_write, row.output, row.reasoning
        ));
    }
    for (currency, cost) in totals {
        text.push_str(&format!("total: {cost:.6} {currency}\n"));
    }
    let cache = result.cache;
    text.push_str(&format!("cache: hit {:.1}%; write {:.1}%; cold starts {}; context edits {}; keep-alive {}; keep-alive cost {}; cost saved {}\n",
        100.0 * cache.hit_ratio, 100.0 * cache.write_ratio, cache.cold_starts, cache.context_edits, cache.keep_alive,
        optional(cache.keep_alive_cost), optional(cache.cost_saved)));
    for count in cache.breaks {
        text.push_str(&format!("cache break {}: {}\n", count.reason, count.count));
    }
    if unpriced != 0 {
        text.push_str(&format!("unpriced: {unpriced} rows\n"));
    }
    text
}

fn optional<T: std::fmt::Display>(value: Option<T>) -> String {
    value.map(|v| v.to_string()).unwrap_or_else(|| "n/a".into())
}

fn duration(value: Option<HumanDuration>) -> String {
    value
        .map(|v| format!("{:?}", v.0))
        .unwrap_or_else(|| "n/a".into())
}

fn health_text(result: Stats::health::Ok, public: bool) -> String {
    let g = result.gauges;
    let mut text = format!(
        "turns_in_flight: {}; agents_running: {}; pending_open: {}; db_bytes: {}; uptime: {:?}\nlast_sleep_at: {}; next_gc_at: {}; gc_duration_last: {}\nindex_pending: {}; index_lag: {}; memory_hot_tokens: {}; memory_embed_pending: {}\n",
        g.turns_in_flight,
        g.agents_running,
        g.pending_open,
        g.db_bytes,
        g.uptime.0,
        optional(g.last_sleep_at),
        optional(g.next_gc_at),
        duration(g.gc_duration_last),
        g.index_pending,
        duration(g.index_lag),
        g.memory_hot_tokens,
        g.memory_embed_pending
    );
    let mut alerts = Vec::new();
    for module in g.modules {
        text.push_str(&format!(
            "module {}: {:?}, {:?}, since {}; capabilities {}\n",
            module.module,
            module.kind,
            module.state,
            module.since,
            module.capabilities.join(", ")
        ));
        if let Some(dial) = module.dial {
            text.push_str(&format!(
                "dial {}: {:?}; error {}\n",
                module.module,
                dial.address,
                optional(dial.error)
            ));
        }
        if matches!(module.state, eva_protocol::types::module::State::Offline) {
            alerts.push(format!("module {} offline", module.module));
        }
    }
    if public {
        if let Some(max) = g.cron_fire_lag.iter().map(|lag| lag.lag.0).max() {
            text.push_str(&format!("cron_fire_lag max: {max:?}\n"));
        }
    } else {
        for lag in g.cron_fire_lag {
            text.push_str(&format!(
                "cron_fire_lag {}: {:?}\n",
                crate::markers::sanitize_inline(&lag.job),
                lag.lag.0
            ));
        }
    }
    for latency in g.latency {
        text.push_str(&format!(
            "latency {}: p50 {:?}, p95 {:?}\n",
            latency.method, latency.p50.0, latency.p95.0
        ));
    }
    text.push_str("metric | key | count\n");
    let mut counters = BTreeMap::<(String, String), u64>::new();
    for counter in result.counters {
        let key = if public && counter.metric.starts_with("cron.") {
            String::new()
        } else {
            counter.key
        };
        let count = counters.entry((counter.metric, key)).or_default();
        *count = count.saturating_add(counter.count);
    }
    for ((metric, key), count) in counters {
        text.push_str(&format!(
            "{} | {} | {}\n",
            metric,
            crate::markers::sanitize_inline(&key),
            count
        ));
        if matches!(
            metric.as_str(),
            "agents.silent" | "cron.failed" | "hooks.timeout"
        ) && count > 0
        {
            alerts.push(format!("{metric}={count}"));
        }
    }
    text.push_str("rate_limits:\n");
    for limit in result.rate_limits {
        text.push_str(&format!(
            "{}: requests {}/{}; tokens {}/{}; reset {}; seen {}\n",
            limit.model,
            optional(limit.requests_remaining),
            optional(limit.requests_limit),
            optional(limit.tokens_remaining),
            optional(limit.tokens_limit),
            optional(limit.reset_at),
            limit.seen_at
        ));
    }
    text.push_str(&format!(
        "alerts: {}",
        if alerts.is_empty() {
            "none".into()
        } else {
            alerts.join("; ")
        }
    ));
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets_use_wire_ids_and_api_errors_keep_their_kind() {
        assert_eq!(
            target(Scope::Chat, Some("c42".into()), false).unwrap(),
            Some(Target::Chat(ChatId(42)))
        );
        assert_eq!(
            target(Scope::Folder, Some("f7".into()), false).unwrap(),
            Some(Target::Folder(FolderId(7)))
        );
        assert!(matches!(
            api_error::<eva_protocol::errors::Impossible>(Generic::Aux(Aux::Unavailable(
                "offline".into()
            ))),
            Aux::Unavailable(_)
        ));
        assert!(
            serde_json::from_value::<ToolsUsage>(serde_json::json!({"view":{"chat":42}})).is_err()
        );
    }

    #[test]
    fn calendar_periods_are_inclusive_and_handle_short_months() {
        let ending = parse_day("2024-03-31").expect("date");
        for (period, expected) in [
            (Period::Day, "2024-03-31"),
            (Period::Week, "2024-03-25"),
            (Period::Month, "2024-03-01"),
            (Period::Year, "2023-04-01"),
        ] {
            let got = interval(period, ending).expect("range");
            assert_eq!(day_str(got.from), expected);
            assert_eq!(got.to, ending);
        }
    }
}
