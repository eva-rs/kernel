//! Лестница давления и строка режима в reminder (`04 §5`, `§6`): только в
//! master-туре, только при `notes_save` Visible в снимке первого шага.

use eva_protocol::types::{
    SegmentId,
    note::{Fill, Mode, State},
};

use crate::config::NotesConfig;

/// Токены в области компакции и порог компакции сегмента (`01 §6.1`, `§9`).
#[derive(Debug, Clone, Copy)]
pub struct Window {
    pub used: u32,
    pub fold_at: u32,
}

#[derive(Debug, Clone, Copy)]
pub struct Turn {
    pub master_turn: bool,
    /// `notes_save` Visible в снимке первого шага: иначе звать некуда (I3).
    pub save_visible: bool,
    pub segment: SegmentId,
    /// Сегмент рождён компакцией (`reason <> initial ∧ summary IS NOT NULL`).
    pub born_of_compaction: bool,
}

/// Строки reminder; `None` — строки нет.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Reminder {
    pub window: Option<String>,
    pub mode: Option<String>,
}

/// Ступени и строка режима; `state` получает `wrap_up_segment`/`nudge_segment`
/// (писать в транзакции старта тура).
pub fn reminder(
    window: Window,
    turn: Turn,
    fill: Fill,
    state: &mut State,
    cfg: &NotesConfig,
) -> Reminder {
    if !cfg.enabled || !turn.master_turn || !turn.save_visible {
        return Reminder::default();
    }
    let left = window.fold_at.saturating_sub(window.used);
    let notice_from =
        u64::from(window.fold_at) * u64::from(cfg.pressure_notice_percent.into_inner()) / 100;
    let step = if turn.born_of_compaction && state.nudge_segment != Some(turn.segment) {
        state.nudge_segment = Some(turn.segment);
        Some(String::from(
            "[window] history was just compacted; anything still needed lives only in notes — check them",
        ))
    } else if left <= cfg.wrap_up_reserve_tokens && state.wrap_up_segment != Some(turn.segment) {
        state.wrap_up_segment = Some(turn.segment);
        Some(format!(
            "[window] wrap up: {left} tokens to compaction — write notes now"
        ))
    } else if u64::from(window.used) >= notice_from {
        Some(format!(
            "[window] {} of {} tokens before compaction; {left} left — notes survive it, history does not",
            window.used, window.fold_at
        ))
    } else {
        None
    };
    Reminder {
        window: step,
        mode: Some(mode_line(state.mode, fill, state.squeeze_turns, cfg)),
    }
}

/// Строка режима (`04 §5`): заполненность видна на каждом туре.
pub fn mode_line(mode: Mode, fill: Fill, squeeze_turns: u32, cfg: &NotesConfig) -> String {
    let (pct, k) = (fill.percent(), fill.count);
    match mode {
        Mode::Inline => format!("notes: {pct}% of budget, {k} notes"),
        Mode::Squeeze => format!(
            "notes: {pct}% of budget, {k} notes — over, cut or merge (turn {squeeze_turns} of {} before index)",
            cfg.squeeze_turns
        ),
        Mode::Index => format!("notes: index, {k} notes, {pct}% of budget"),
    }
}

#[cfg(test)]
mod tests {
    use eva::time::Timestamp;

    use super::*;
    use crate::subsystems::notes::state::initial;

    fn turn(segment: i64, born: bool) -> Turn {
        Turn {
            master_turn: true,
            save_visible: true,
            segment: SegmentId(segment),
            born_of_compaction: born,
        }
    }

    const FILL: Fill = Fill {
        tokens: 300,
        budget: 1000,
        count: 2,
    };

    #[test]
    fn ladder_steps() {
        let cfg = NotesConfig::default();
        let mut state = initial(Timestamp::TEST_ORIGIN);
        let fold = Window {
            used: 0,
            fold_at: 100_000,
        };
        // Тихо до 60%.
        let r = reminder(
            Window {
                used: 59_999,
                ..fold
            },
            turn(1, false),
            FILL,
            &mut state,
            &cfg,
        );
        assert_eq!(r.window, None);
        assert_eq!(r.mode.as_deref(), Some("notes: 30% of budget, 2 notes"));
        // Notice ровно с 60%.
        let r = reminder(
            Window {
                used: 60_000,
                ..fold
            },
            turn(1, false),
            FILL,
            &mut state,
            &cfg,
        );
        assert_eq!(
            r.window.as_deref(),
            Some(
                "[window] 60000 of 100000 tokens before compaction; 40000 left — notes survive it, history does not"
            )
        );
        // Wrap-up один раз на сегмент, затем снова notice.
        let r = reminder(
            Window {
                used: 95_000,
                ..fold
            },
            turn(1, false),
            FILL,
            &mut state,
            &cfg,
        );
        assert_eq!(
            r.window.as_deref(),
            Some("[window] wrap up: 5000 tokens to compaction — write notes now")
        );
        assert_eq!(state.wrap_up_segment, Some(SegmentId(1)));
        let r = reminder(
            Window {
                used: 96_000,
                ..fold
            },
            turn(1, false),
            FILL,
            &mut state,
            &cfg,
        );
        assert!(
            r.window
                .as_deref()
                .is_some_and(|w| w.starts_with("[window] 96000 of"))
        );
        // Новый сегмент, рождённый компакцией: nudge заменяет всё и один раз.
        let r = reminder(
            Window {
                used: 99_000,
                ..fold
            },
            turn(2, true),
            FILL,
            &mut state,
            &cfg,
        );
        assert!(
            r.window
                .as_deref()
                .is_some_and(|w| w.starts_with("[window] history was just compacted"))
        );
        assert_eq!(state.nudge_segment, Some(SegmentId(2)));
        let r = reminder(
            Window {
                used: 99_000,
                ..fold
            },
            turn(2, true),
            FILL,
            &mut state,
            &cfg,
        );
        assert!(
            r.window
                .as_deref()
                .is_some_and(|w| w.starts_with("[window] wrap up"))
        );
        // Первый сегмент чата nudge не получает.
        let mut fresh = initial(Timestamp::TEST_ORIGIN);
        let r = reminder(
            Window { used: 0, ..fold },
            turn(3, false),
            FILL,
            &mut fresh,
            &cfg,
        );
        assert_eq!(r.window, None);
    }

    #[test]
    fn silent_without_master_or_save_tool() {
        let cfg = NotesConfig::default();
        let mut state = initial(Timestamp::TEST_ORIGIN);
        let w = Window {
            used: 99_000,
            fold_at: 100_000,
        };
        let not_master = Turn {
            master_turn: false,
            ..turn(1, true)
        };
        assert_eq!(
            reminder(w, not_master, FILL, &mut state, &cfg),
            Reminder::default()
        );
        let restricted = Turn {
            save_visible: false,
            ..turn(1, true)
        };
        assert_eq!(
            reminder(w, restricted, FILL, &mut state, &cfg),
            Reminder::default()
        );
        assert_eq!(state.nudge_segment, None, "a silent turn spends no step");
        let disabled = NotesConfig {
            enabled: false,
            ..cfg
        };
        assert_eq!(
            reminder(w, turn(1, true), FILL, &mut state, &disabled),
            Reminder::default()
        );
    }

    #[test]
    fn mode_lines() {
        let cfg = NotesConfig::default();
        assert_eq!(
            mode_line(
                Mode::Squeeze,
                Fill {
                    tokens: 1300,
                    budget: 1000,
                    count: 9
                },
                1,
                &cfg
            ),
            "notes: 130% of budget, 9 notes — over, cut or merge (turn 1 of 2 before index)"
        );
        assert_eq!(
            mode_line(
                Mode::Index,
                Fill {
                    tokens: 900,
                    budget: 1000,
                    count: 12
                },
                0,
                &cfg
            ),
            "notes: index, 12 notes, 90% of budget"
        );
    }
}
