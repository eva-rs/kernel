//! Агрегаты статистики (`09 §7`) → тип протокола.

use eva_protocol::{
    requests::Stats::chat_spend,
    types::{
        ModuleId,
        module::{Dial, ModuleKind, State},
        stats::{
            BreakCount, CacheStats, Counter, ModelSpend, ModuleGauge, Money, SeriesPoint, SpendRow,
            ToolUsageRow, TurnSpend,
        },
    },
    wire::capabilities::Capabilities,
};

use crate::{
    db::{
        DbError,
        stats::{
            CacheTotals, ChatSpendRow, CounterRow, ModelTotals, ModuleRow, MoneyTotal, SeriesRow,
            SpendGroup, ToolUsage, TurnTotals,
        },
    },
    registry::Models,
};

pub fn tool_usage(row: ToolUsage) -> ToolUsageRow {
    let ToolUsage {
        tool_id,
        name,
        calls,
        turns_with_call,
        loads,
        errors,
        ends_turn,
    } = row;

    ToolUsageRow {
        tool_id,
        name,
        calls,
        turns_with_call,
        loads,
        errors,
        ends_turn,
    }
}

pub fn spend(row: SpendGroup) -> SpendRow {
    let SpendGroup {
        key,
        currency,
        input,
        cache_read,
        cache_write,
        cache_write_long,
        output,
        reasoning,
        cost,
    } = row;

    SpendRow {
        key,
        input,
        cache_read,
        cache_write,
        cache_write_long,
        output,
        reasoning,
        cost,
        currency,
    }
}

fn ratio(part: u64, whole: u64) -> f64 {
    if whole == 0 {
        0.0
    } else {
        part as f64 / whole as f64
    }
}

/// Холодные старты и экономия зависят от caps и цены модели в реестре (`09 §3`).
pub fn cache(totals: CacheTotals, models: &Models, currency: &str) -> CacheStats {
    let CacheTotals {
        step_cache_read,
        step_cache_write,
        step_prompt_total,
        context_edits,
        keep_alive,
        keep_alive_cost,
        breaks,
        cold_candidates,
        pricebook_cache_read,
    } = totals;

    let cold_starts = cold_candidates
        .iter()
        .filter(|(model, prompt_total)| {
            models.get(model).is_some_and(|m| {
                m.decl.caps.cache_breakpoints > 0
                    && *prompt_total >= u64::from(m.decl.caps.cache_min_prefix)
            })
        })
        .count() as u64;

    let estimated_cost_saved = pricebook_cache_read
        .iter()
        .filter_map(|(model, cache_read)| {
            let price = models.get(model)?.decl.price?;
            Some(*cache_read as f64 * (price.input - price.cache_read) / 1e6)
        })
        .reduce(|a, b| a + b)
        .map(|cost| Money {
            cost,
            currency: currency.to_owned(),
        });

    CacheStats {
        hit_ratio: ratio(step_cache_read, step_prompt_total),
        write_ratio: ratio(step_cache_write, step_prompt_total),
        cold_starts,
        breaks: breaks
            .into_iter()
            .map(|(reason, count)| BreakCount { reason, count })
            .collect(),
        context_edits,
        keep_alive,
        keep_alive_cost: keep_alive_cost.into_iter().map(money).collect(),
        estimated_cost_saved,
    }
}

pub fn series(row: SeriesRow) -> SeriesPoint {
    let SeriesRow {
        day,
        key,
        currency,
        cost,
        prompt_total,
        output,
    } = row;

    SeriesPoint {
        day,
        key,
        currency,
        cost,
        prompt_total,
        output,
    }
}

pub fn chat_spend(row: ChatSpendRow) -> chat_spend::Ok {
    let ChatSpendRow {
        total,
        by_model,
        last_turn,
        cache_read,
        prompt_total,
    } = row;
    chat_spend::Ok {
        total: total.into_iter().map(money).collect(),
        by_model: by_model
            .into_iter()
            .map(
                |ModelTotals {
                     model,
                     cost,
                     prompt_total,
                     output,
                 }| ModelSpend {
                    model,
                    cost: cost.into_iter().map(money).collect(),
                    prompt_total,
                    output,
                },
            )
            .collect(),
        last_turn: last_turn.map(
            |TurnTotals {
                 turn,
                 cost,
                 prompt_total,
                 cache_read,
             }| TurnSpend {
                turn_id: turn,
                cost: cost.into_iter().map(money).collect(),
                prompt_total,
                cache_read,
            },
        ),
        hit_ratio: ratio(cache_read, prompt_total),
    }
}

fn money(row: MoneyTotal) -> Money {
    Money {
        cost: row.cost,
        currency: row.currency,
    }
}

pub fn counter(row: CounterRow) -> Counter {
    let CounterRow { metric, key, count } = row;
    Counter { metric, key, count }
}

/// `kind` и `dial` считает вызывающий; без вида (ни реестра, ни токена) gauge не строится.
pub fn module(
    row: ModuleRow,
    kind: Option<ModuleKind>,
    dial: Option<Dial>,
) -> Result<Option<ModuleGauge>, DbError> {
    let ModuleRow {
        module,
        live_token: _,
        state,
        since,
        capabilities,
    } = row;
    let Some(kind) = kind else {
        return Ok(None);
    };
    let capabilities: Capabilities = rmp_serde::from_slice(&capabilities)
        .map_err(|e| DbError::corrupt(format_args!("modules.capabilities of {module}: {e}")))?;

    Ok(Some(ModuleGauge {
        module,
        kind,
        state,
        since,
        capabilities: capabilities.names(),
        dial,
    }))
}

/// Запись реестра без строки `modules`: ещё ни разу не дозвонились.
pub fn unseen_module(module: ModuleId, dial: Dial, since: eva::time::Timestamp) -> ModuleGauge {
    ModuleGauge {
        module,
        kind: ModuleKind::Service,
        state: State::Offline,
        since,
        capabilities: Vec::new(),
        dial: Some(dial),
    }
}
