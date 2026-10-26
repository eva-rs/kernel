//! Автомат режимов (`04 §5`): чистая функция от состояния и наблюдения на
//! входе в тур; внутри тура режим не пересчитывается.

use eva::time::Timestamp;
use eva_protocol::types::note::{Fill, Mode, State};

use crate::config::NotesConfig;

/// `fill > 1 ∨ count > inline_max_notes`.
pub fn over(fill: Fill, cfg: &NotesConfig) -> bool {
    fill.tokens > fill.budget || fill.count > cfg.inline_max_notes
}

/// `fill < return_percent/100 ∧ count ≤ inline_max_notes − 2`.
pub fn under(fill: Fill, cfg: &NotesConfig) -> bool {
    let below = u64::from(fill.tokens) * 100
        < u64::from(fill.budget) * u64::from(cfg.return_percent.into_inner());
    below && fill.count <= cfg.inline_max_notes.saturating_sub(2)
}

pub fn initial(now: Timestamp) -> State {
    State {
        mode: Mode::Inline,
        since_at: now,
        squeeze_turns: 0,
        wrap_up_segment: None,
        nudge_segment: None,
        seen_at: None,
    }
}

/// Вход в тур: новое состояние и переход, если был.
#[derive(Debug, Clone, PartialEq)]
pub struct Entered {
    pub state: State,
    pub transition: Option<(Mode, Mode)>,
}

/// Переходы `04 §5`: `inline → squeeze` при `over`; `squeeze → inline` при
/// `¬over`; `squeeze → index` при `over` после ≥ `squeeze_turns`
/// master-туров в `squeeze`; `index → inline` только при `under`.
pub fn enter(
    state: Option<State>,
    fill: Fill,
    master_turn: bool,
    cfg: &NotesConfig,
    now: Timestamp,
) -> Entered {
    let mut state = state.unwrap_or_else(|| initial(now));
    let from = state.mode;
    let over = over(fill, cfg);
    let to = match from {
        Mode::Inline if over => Mode::Squeeze,
        Mode::Inline => Mode::Inline,
        Mode::Squeeze if !over => Mode::Inline,
        Mode::Squeeze => {
            if master_turn {
                state.squeeze_turns = state.squeeze_turns.saturating_add(1);
            }
            if state.squeeze_turns >= cfg.squeeze_turns {
                Mode::Index
            } else {
                Mode::Squeeze
            }
        }
        Mode::Index if under(fill, cfg) => Mode::Inline,
        Mode::Index => Mode::Index,
    };
    if to != Mode::Squeeze {
        state.squeeze_turns = 0;
    }
    let transition = (from != to).then_some((from, to));
    if transition.is_some() {
        state.mode = to;
        state.since_at = now;
    }
    Entered { state, transition }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn cfg() -> NotesConfig {
        NotesConfig::default()
    }

    fn fill(tokens: u32, count: u32) -> Fill {
        Fill {
            tokens,
            budget: 1000,
            count,
        }
    }

    const NOW: Timestamp = Timestamp::TEST_ORIGIN;

    #[test]
    fn inline_to_index_takes_squeeze_turns_master_turns() {
        let c = cfg();
        let e = enter(None, fill(1200, 3), true, &c, NOW);
        assert_eq!(e.transition, Some((Mode::Inline, Mode::Squeeze)));
        assert_eq!(e.state.squeeze_turns, 0);
        // Чужой тур в squeeze не считается.
        let e = enter(Some(e.state), fill(1200, 3), false, &c, NOW);
        assert_eq!(e.transition, None);
        assert_eq!(e.state.squeeze_turns, 0);
        let e = enter(Some(e.state), fill(1200, 3), true, &c, NOW);
        assert_eq!((e.transition, e.state.squeeze_turns), (None, 1));
        let e = enter(Some(e.state), fill(1200, 3), true, &c, NOW);
        assert_eq!(e.transition, Some((Mode::Squeeze, Mode::Index)));
        assert_eq!(e.state.squeeze_turns, 0, "outside squeeze it is 0");
        // Из index только при under: 70% — нет, 50% и мало штук — да.
        let e = enter(Some(e.state), fill(700, 3), true, &c, NOW);
        assert_eq!((e.transition, e.state.mode), (None, Mode::Index));
        let e = enter(Some(e.state), fill(500, 7), true, &c, NOW);
        assert_eq!(e.transition, None, "count 7 > inline_max − 2");
        let e = enter(Some(e.state), fill(500, 6), true, &c, NOW);
        assert_eq!(e.transition, Some((Mode::Index, Mode::Inline)));
    }

    #[test]
    fn squeeze_returns_to_inline_when_no_longer_over() {
        let c = cfg();
        let e = enter(None, fill(0, 9), true, &c, NOW);
        assert_eq!(e.state.mode, Mode::Squeeze, "count over inline_max");
        let e = enter(Some(e.state), fill(999, 8), true, &c, NOW);
        assert_eq!(e.transition, Some((Mode::Squeeze, Mode::Inline)));
    }

    proptest! {
        /// Гистерезис `04 §12`: переходы совпадают с таблицей; из `index` выход
        /// только при `under`; в `index` только через ≥ `squeeze_turns` master-туров.
        #[test]
        fn transitions_match_the_table(
            steps in proptest::collection::vec((0_u32..2000, 0_u32..12, any::<bool>()), 1..60),
        ) {
            let c = cfg();
            let mut state: Option<State> = None;
            let mut masters_in_squeeze = 0_u32;
            for (tokens, count, master) in steps {
                let f = fill(tokens, count);
                let before = state.clone().map_or(Mode::Inline, |s| s.mode);
                let e = enter(state.take(), f, master, &c, NOW);
                let (is_over, is_under) = (over(f, &c), under(f, &c));
                match (before, e.state.mode) {
                    (Mode::Inline, Mode::Squeeze) => prop_assert!(is_over),
                    (Mode::Inline, Mode::Inline) => prop_assert!(!is_over),
                    (Mode::Squeeze, Mode::Inline) => prop_assert!(!is_over),
                    (Mode::Squeeze, Mode::Squeeze) => prop_assert!(is_over),
                    (Mode::Squeeze, Mode::Index) => {
                        prop_assert!(is_over);
                        prop_assert!(masters_in_squeeze + u32::from(master) >= c.squeeze_turns);
                    }
                    (Mode::Index, Mode::Inline) => prop_assert!(is_under),
                    (Mode::Index, Mode::Index) => prop_assert!(!is_under),
                    (from, to) => prop_assert!(false, "impossible transition {from:?} → {to:?}"),
                }
                masters_in_squeeze = if e.state.mode == Mode::Squeeze {
                    masters_in_squeeze + u32::from(master && before == Mode::Squeeze)
                } else {
                    0
                };
                prop_assert_eq!(e.state.squeeze_turns, masters_in_squeeze);
                state = Some(e.state);
            }
        }

        /// Режим не мигает: `fill ∈ [return, 100]%` в `index` — ни одного перехода;
        /// `fill > 100%` — ни одного выхода из `index`.
        #[test]
        fn index_does_not_flicker(
            fills in proptest::collection::vec(600_u32..=1000, 1..40),
            highs in proptest::collection::vec(1001_u32..5000, 1..40),
            counts in proptest::collection::vec(0_u32..=6, 40),
        ) {
            let c = cfg();
            let mut state = initial(NOW);
            state.mode = Mode::Index;
            let mut state = Some(state);
            for (tokens, count) in fills.iter().chain(&highs).zip(counts) {
                let e = enter(state.take(), fill(*tokens, count), true, &c, NOW);
                prop_assert_eq!(e.transition, None, "tokens={} count={}", tokens, count);
                state = Some(e.state);
            }
        }
    }
}
