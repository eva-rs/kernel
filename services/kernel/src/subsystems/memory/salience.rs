//! Salience и предикат забывания (`03 §3`): считается в Rust над
//! кандидатами, не материализуется.

use std::time::Duration;

use eva::time::Timestamp;
use eva_protocol::types::memory::Kind;

use crate::{config::memory::SalienceConfig, db::memory::MemoryRow};

/// `T½(kind)`; `core` не распадается.
pub fn half_life(kind: Kind, cfg: &SalienceConfig) -> Option<Duration> {
    match kind {
        Kind::Core => None,
        Kind::Fact => Some(*cfg.half_life.fact),
        Kind::Event => Some(*cfg.half_life.event),
        Kind::Procedure => Some(*cfg.half_life.procedure),
    }
}

/// `max(floor, 2^(−age / T½))`; `core` — 1.
pub fn decay(kind: Kind, age: Duration, cfg: &SalienceConfig) -> f64 {
    let Some(half_life) = half_life(kind, cfg) else {
        return 1.0;
    };
    let half_life = half_life.as_secs_f64();
    if half_life <= 0.0 {
        return cfg.floor.clamp(0.0, 1.0);
    }
    let raw = (-(age.as_secs_f64() / half_life)).exp2();
    raw.max(cfg.floor).clamp(0.0, 1.0)
}

/// `importance · confidence · decay · (1 + access_bonus · ln(1 + access_count))`.
pub fn salience(
    kind: Kind,
    importance: f64,
    confidence: f64,
    access_count: f64,
    age: Duration,
    cfg: &SalienceConfig,
) -> f64 {
    let bonus = 1.0 + cfg.access_bonus * (1.0 + access_count.max(0.0)).ln();
    importance * confidence * decay(kind, age, cfg) * bonus
}

/// Возраст от `then` до `now`; будущее — 0.
pub fn age(now: Timestamp, then: Timestamp) -> Duration {
    Duration::from_nanos(now.as_nanos().get().saturating_sub(then.as_nanos().get()))
}

pub fn of_row(row: &MemoryRow, now: Timestamp, cfg: &SalienceConfig) -> f64 {
    salience(
        row.kind,
        row.importance,
        row.confidence,
        row.access_count,
        age(now, row.last_reinforced_at),
        cfg,
    )
}

/// Пороги сна (`08 §2.7`): сон приносит свои, память даёт предикат.
#[derive(Debug, Clone, Copy)]
pub struct ForgetThresholds {
    pub importance_max: f64,
    pub keep_recent: Duration,
}

/// `03 §3`: `kind ≠ core ∧ importance ≤ max ∧ access_count < 1 ∧ age > 2·T½ ∧ created_at < now − keep_recent`.
pub fn forget_candidate(
    row: &MemoryRow,
    now: Timestamp,
    cfg: &SalienceConfig,
    thresholds: ForgetThresholds,
) -> bool {
    let Some(half_life) = half_life(row.kind, cfg) else {
        return false;
    };
    row.importance <= thresholds.importance_max
        && row.access_count < 1.0
        && age(now, row.last_reinforced_at) > half_life * 2
        && age(now, row.created_at) > thresholds.keep_recent
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn cfg() -> SalienceConfig {
        SalienceConfig::default()
    }

    const DAY: u64 = 86_400;

    #[test]
    fn decay_halves_at_half_life_and_floors() {
        let c = cfg();
        assert_eq!(
            decay(Kind::Core, Duration::from_secs(10_000 * DAY), &c),
            1.0
        );
        assert!((decay(Kind::Fact, Duration::from_secs(180 * DAY), &c) - 0.5).abs() < 1e-9);
        assert!((decay(Kind::Event, Duration::from_secs(30 * DAY), &c) - 0.5).abs() < 1e-9);
        assert_eq!(decay(Kind::Fact, Duration::ZERO, &c), 1.0);
        assert_eq!(
            decay(Kind::Event, Duration::from_secs(10_000 * DAY), &c),
            c.floor
        );
    }

    #[test]
    fn forget_candidate_follows_the_predicate() {
        let now = Timestamp::TEST_ORIGIN + Duration::from_secs(1000 * DAY);
        let old = now.as_nanos().get() - 500 * DAY * 1_000_000_000;
        let row = |kind, importance, access_count| MemoryRow {
            id: eva_protocol::types::MemoryId(1),
            kind,
            text: String::new(),
            name: None,
            description: None,
            tags: None,
            visibility: eva_protocol::types::memory::Visibility::Public,
            importance,
            confidence: 1.0,
            source_actor: None,
            source_chat: None,
            source_surface: None,
            source_trusted_chat: None,
            source_run: None,
            created_at: Timestamp::from_nanos(old.try_into().expect("nonzero")),
            updated_at: now,
            consolidated_at: None,
            occurred_at: None,
            valid_from: now,
            valid_to: None,
            supersedes: None,
            version: 1,
            last_reinforced_at: Timestamp::from_nanos(old.try_into().expect("nonzero")),
            access_count,
            embed_model: None,
            archive: None,
        };
        let thr = ForgetThresholds {
            importance_max: 0.3,
            keep_recent: Duration::from_secs(2 * DAY),
        };
        let c = cfg();
        assert!(forget_candidate(&row(Kind::Fact, 0.2, 0.3), now, &c, thr));
        assert!(!forget_candidate(&row(Kind::Core, 0.2, 0.0), now, &c, thr));
        assert!(!forget_candidate(&row(Kind::Fact, 0.5, 0.0), now, &c, thr));
        assert!(!forget_candidate(&row(Kind::Fact, 0.2, 1.0), now, &c, thr));
        // 500 дней < 2·365 у procedure.
        assert!(!forget_candidate(
            &row(Kind::Procedure, 0.2, 0.0),
            now,
            &c,
            thr
        ));
    }

    proptest! {
        #[test]
        fn salience_is_monotone(
            kind in prop_oneof![Just(Kind::Fact), Just(Kind::Event), Just(Kind::Core), Just(Kind::Procedure)],
            importance in 0.0_f64..=1.0,
            confidence in 0.0_f64..=1.0,
            access in 0.0_f64..1000.0,
            age_a in 0_u64..(5 * 365 * DAY),
            age_b in 0_u64..(5 * 365 * DAY),
        ) {
            let c = cfg();
            let (young, old) = (age_a.min(age_b), age_a.max(age_b));
            let s_young = salience(kind, importance, confidence, access, Duration::from_secs(young), &c);
            let s_old = salience(kind, importance, confidence, access, Duration::from_secs(old), &c);
            prop_assert!(s_old <= s_young + 1e-12, "∂s/∂age ≤ 0");
            let s_more = salience(kind, importance, confidence, access + 1.0, Duration::from_secs(old), &c);
            prop_assert!(s_more + 1e-12 >= s_old, "∂s/∂access ≥ 0");
            let d = decay(kind, Duration::from_secs(old), &c);
            prop_assert!((c.floor..=1.0).contains(&d), "decay ∈ [floor, 1]: {d}");
            if kind == Kind::Core {
                prop_assert!((s_old - s_young).abs() < 1e-12, "core does not age");
            }
        }
    }
}
