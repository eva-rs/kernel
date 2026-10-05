//! Статистика (`09`): единственная дверь записи — `Recorder` (`spend` на
//! вызов провайдера с прайс-буком и метриками кэша, `tally` тура, `bump`) и
//! живые gauge'и в `Shared`; чтение — операции `stats.*` в `handling/stats.rs`.
//! Фоновой работы нет: счётчики пишутся сразу, ретеншн исполняет GC.

pub mod tools;

use eva::{data, time::Timestamp};

use eva_protocol::types::{
    ChatId, ModelId, ModuleId, RunId, TurnId,
    generate::Usage,
    model::{ModelDecl, Price},
    stats::BreakReason,
};

use crate::{
    components::Components,
    db::{
        Db as _, DbResult, Queries as _,
        modules::{Cost, CostSource},
        stats::{Metric, Op, Source, SpendRow},
    },
    shared::Shared,
};

pub use self::{
    gauges::{Gauges, InFlight, Live},
    turn::TurnTally,
};

mod gauges;
mod turn;

/// Кто заказал инференс (`09 §2`): модуль несёт своё имя, остальные — нет.
#[data]
pub enum Origin {
    Surface,
    Cron,
    Sleep,
    Agent,
    System,
    Module(ModuleId),
}

/// Контекст одного вызова провайдера: общий для всех его usage-чанков.
#[data]
pub struct Call {
    pub model: ModelId,
    pub origin: Origin,
    pub op: Op,
    pub chat: Option<ChatId>,
    pub turn: Option<TurnId>,
    /// Запуск крона или ночь сна (`08`).
    pub run: Option<RunId>,
    /// Разрыв кэша этого запроса (`01 §11`); ложится на первый чанк.
    pub cache_break_reason: Option<BreakReason>,
}

/// Прайс-бук `09 §2.1`: цена провайдера, иначе `ModelDecl.price`, иначе нет.
pub fn cost(price: Option<Price>, usage: &Usage, currency: &str) -> Option<Cost> {
    let (amount, source) = match (usage.cost, price) {
        (Some(cost), _) => (cost, CostSource::Provider),
        (None, Some(p)) => {
            let tokens = |n: u64, price: f64| n as f64 * price;
            let total = tokens(usage.input, p.input)
                + tokens(usage.cache_read, p.cache_read)
                + tokens(usage.cache_write_short, p.cache_write_short)
                + tokens(usage.cache_write_long, p.cache_write_long)
                + tokens(usage.output, p.output);
            (total / 1e6, CostSource::Pricebook)
        }
        (None, None) => return None,
    };
    Some(Cost {
        amount,
        currency: currency.to_owned(),
        source,
    })
}

/// `cache_read = 0` при префиксе не короче `caps.cache_min_prefix`; модель без
/// точек кэша холодной не бывает (`09 §3`).
pub fn cold_start(decl: Option<&ModelDecl>, chunks: &[Usage]) -> bool {
    let Some(decl) = decl else {
        return false;
    };
    let read: u64 = chunks.iter().map(|u| u.cache_read).sum();
    let prompt: u64 = chunks
        .iter()
        .map(|u| u.input + u.cache_read + u.cache_write_short + u.cache_write_long)
        .sum();
    decl.caps.cache_breakpoints > 0 && read == 0 && prompt >= u64::from(decl.caps.cache_min_prefix)
}

/// Разрыв, который считается в `cache.break`: первый запрос и сброс сегмента — нет.
pub const fn is_break(reason: BreakReason) -> bool {
    !matches!(
        reason,
        BreakReason::FirstRequest | BreakReason::SegmentReset
    )
}

pub struct Recorder<'a, C: Components> {
    shared: &'a Shared<C>,
}

impl<'a, C: Components> Recorder<'a, C> {
    pub fn new(shared: &'a Shared<C>) -> Self {
        Self { shared }
    }

    /// `+1` к счётчику здоровья (`09 §5`).
    pub async fn bump(&self, metric: Metric, key: &str) {
        self.shared.bump(metric, key).await;
    }

    pub fn gauges(&self) -> &Gauges {
        self.shared.gauges()
    }

    /// Учёт тура (`09 §1.3`): сутки — старт тура.
    pub fn tally(&self, chat: ChatId, started_at: Timestamp) -> TurnTally<C> {
        TurnTally::new(self.shared.clone(), chat, started_at)
    }

    /// Вызов провайдера (`09 §2`): строка `spend` на каждый usage-чанк, без
    /// чанков — `inference.usage_missing`; цена, `spend.unpriced`,
    /// `cache.{cold_start,break,keep_alive}` и снимок rate limits — здесь же.
    pub async fn spend(&self, call: Call, chunks: &[Usage]) -> DbResult<()> {
        let shared = self.shared;
        let model = call.model.clone();
        if chunks.is_empty() {
            shared
                .bump(Metric::InferenceUsageMissing, model.as_str())
                .await;
            return Ok(());
        }
        let now = shared.now();
        if let Some(limits) = chunks.iter().rev().find_map(|u| u.rate_limits) {
            shared.gauges().rate_limit(&model, limits, now);
        }
        let entry = shared.models().get(&model).cloned();
        let decl = entry.as_ref().map(|e| &e.decl);
        let currency = &shared.cfg().llm.currency;
        let (source, module) = match call.origin {
            Origin::Surface => (Source::Surface, None),
            Origin::Cron => (Source::Cron, None),
            Origin::Sleep => (Source::Sleep, None),
            Origin::Agent => (Source::Agent, None),
            Origin::System => (Source::System, None),
            Origin::Module(module) => (Source::Module, Some(module)),
        };
        let rows: Vec<SpendRow> = chunks
            .iter()
            .enumerate()
            .map(|(i, u)| SpendRow {
                ts: now,
                model: model.clone(),
                source,
                op: call.op,
                module: module.clone(),
                chat: call.chat,
                turn: call.turn.clone(),
                run: call.run.clone(),
                input: u.input,
                cache_read: u.cache_read,
                cache_write: u.cache_write_short.saturating_add(u.cache_write_long),
                cache_write_long: u.cache_write_long,
                output: u.output,
                reasoning: u.reasoning,
                context_edits_applied: u.context_edits.applied,
                cache_break_reason: call.cache_break_reason.filter(|_| i == 0),
                cost: cost(decl.and_then(|d| d.price), u, currency),
            })
            .collect();
        let mut metrics: Vec<(Metric, String)> = rows
            .iter()
            .filter(|r| r.cost.is_none())
            .map(|_| (Metric::SpendUnpriced, model.to_string()))
            .collect();
        if call.op == Op::Step && cold_start(decl, chunks) {
            metrics.push((Metric::CacheColdStart, model.to_string()));
        }
        if let Some(reason) = call.cache_break_reason.filter(|r| is_break(*r)) {
            metrics.push((Metric::CacheBreak, reason.to_string()));
        }
        if call.op == Op::KeepAlive {
            metrics.push((Metric::CacheKeepAlive, model.to_string()));
        }
        let zone = shared.day_zone();
        shared
            .db()
            .write(move |tx| {
                let stats = tx.stats();
                for row in &rows {
                    stats.insert_spend(row)?;
                }
                for (metric, key) in &metrics {
                    stats.bump(*metric, key, now, &zone)?;
                }
                Ok(())
            })
            .await
    }
}

#[cfg(test)]
mod tests {
    use eva_protocol::types::model::ModelCaps;

    use super::*;

    fn usage(input: u64, cache_read: u64, output: u64) -> Usage {
        Usage {
            input,
            cache_read,
            output,
            ..Usage::default()
        }
    }

    #[test]
    fn pricebook_prefers_provider_cost() {
        let price = Price {
            input: 3.0,
            output: 15.0,
            cache_read: 0.3,
            cache_write_short: 3.75,
            cache_write_long: 6.0,
        };
        let usage = Usage {
            input: 1_000_000,
            cache_read: 1_000_000,
            cache_write_long: 1_000_000,
            output: 1_000_000,
            ..Usage::default()
        };
        let c = cost(Some(price), &usage, "USD").expect("priced");
        assert!((c.amount - (3.0 + 0.3 + 6.0 + 15.0)).abs() < 1e-9);
        assert!(matches!(c.source, CostSource::Pricebook));

        let provided = Usage {
            cost: Some(0.5),
            ..usage.clone()
        };
        let c = cost(Some(price), &provided, "USD").expect("priced");
        assert!(matches!(c.source, CostSource::Provider) && (c.amount - 0.5).abs() < 1e-12);
        assert!(cost(None, &usage, "USD").is_none());
    }

    #[test]
    fn cold_start_needs_breakpoints_and_a_long_prefix() {
        let decl = |breakpoints, min_prefix| ModelDecl {
            name: ModelId::from("m"),
            context_window: 200_000,
            max_output: 8_192,
            caps: ModelCaps {
                cache_breakpoints: breakpoints,
                cache_min_prefix: min_prefix,
                ..ModelCaps::default()
            },
            price: None,
            embed: None,
        };
        let cached = decl(4, 1024);
        assert!(cold_start(Some(&cached), &[usage(2000, 0, 10)]));
        assert!(
            !cold_start(Some(&cached), &[usage(500, 0, 10)]),
            "short prefix"
        );
        assert!(
            !cold_start(Some(&cached), &[usage(2000, 0, 0), usage(0, 1, 5)]),
            "read in a later chunk"
        );
        assert!(
            !cold_start(Some(&decl(0, 0)), &[usage(5000, 0, 10)]),
            "no breakpoints"
        );
        assert!(!cold_start(None, &[usage(5000, 0, 10)]), "unknown model");
        assert!(!is_break(BreakReason::FirstRequest) && !is_break(BreakReason::SegmentReset));
        assert!(is_break(BreakReason::ToolsChange) && is_break(BreakReason::Unknown));
    }
}
