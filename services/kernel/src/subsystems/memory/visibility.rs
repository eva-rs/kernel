//! Видимость записей (`03 §2`): фильтр до скоринга, одинаковый для FTS и cos.

use eva_protocol::types::{
    memory::Visibility,
    turn::{Audience, TurnView},
};

/// Маркер к recall и hot-блоку, если фильтр что-то отсёк; счётчика нет.
pub const WITHHELD: &str = "[memory: some entries withheld in this chat]";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gate {
    /// Рука хозяина (`view = None`): видно всё.
    Owner,
    /// recall / read / history — по туру.
    Turn {
        master_turn: bool,
        trusted_chat: bool,
    },
    /// hot-блок живёт сегмент: private только пока в чате говорил один хозяин.
    Hot {
        trusted_chat: bool,
        audience: Audience,
    },
}

impl Gate {
    pub const fn turn(view: &TurnView) -> Self {
        Self::Turn {
            master_turn: view.master_turn,
            trusted_chat: view.trusted_chat,
        }
    }

    /// `None` — взгляд хозяина.
    pub const fn view(view: Option<&TurnView>) -> Self {
        match view {
            Some(view) => Self::turn(view),
            None => Self::Owner,
        }
    }

    pub const fn hot(view: &TurnView) -> Self {
        Self::Hot {
            trusted_chat: view.trusted_chat,
            audience: view.audience,
        }
    }

    /// Самая строгая видимая видимость: любой гейт пропускает нижний отрезок
    /// цепочки `public ⊂ trusted ⊂ private`, поэтому фильтр — одно сравнение.
    pub const fn ceiling(self) -> Visibility {
        let (trusted, private) = match self {
            Self::Owner => (true, true),
            Self::Turn {
                master_turn,
                trusted_chat,
            } => (trusted_chat, master_turn && trusted_chat),
            Self::Hot {
                trusted_chat,
                audience,
            } => (
                trusted_chat,
                trusted_chat && matches!(audience, Audience::MasterOnly),
            ),
        };
        match (trusted, private) {
            (_, true) => Visibility::Private,
            (true, false) => Visibility::Trusted,
            (false, false) => Visibility::Public,
        }
    }

    pub fn allows(self, visibility: Visibility) -> bool {
        visibility <= self.ceiling()
    }
}

/// Дефолт `remember` (`03 §2`): `master_only ∧ trusted → private`, `trusted → trusted`, иначе `public`.
pub const fn default_visibility(trusted_chat: bool, audience: Audience) -> Visibility {
    match (trusted_chat, audience) {
        (true, Audience::MasterOnly) => Visibility::Private,
        (true, Audience::Mixed) => Visibility::Trusted,
        (false, _) => Visibility::Public,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [Visibility; 3] = [Visibility::Public, Visibility::Trusted, Visibility::Private];

    #[test]
    fn turn_gate_matches_the_table() {
        for (master, trusted, want) in [
            (false, false, [true, false, false]),
            (false, true, [true, true, false]),
            (true, false, [true, false, false]),
            (true, true, [true, true, true]),
        ] {
            let gate = Gate::Turn {
                master_turn: master,
                trusted_chat: trusted,
            };
            let got: Vec<bool> = ALL.iter().map(|v| gate.allows(*v)).collect();
            assert_eq!(got, want, "master={master} trusted={trusted}");
        }
        assert!(ALL.iter().all(|v| Gate::Owner.allows(*v)));
    }

    #[test]
    fn hot_gate_needs_a_master_only_audience_for_private() {
        let hot = |trusted, audience| Gate::Hot {
            trusted_chat: trusted,
            audience,
        };
        assert!(hot(true, Audience::MasterOnly).allows(Visibility::Private));
        assert!(!hot(true, Audience::Mixed).allows(Visibility::Private));
        assert!(hot(true, Audience::Mixed).allows(Visibility::Trusted));
        assert!(!hot(false, Audience::MasterOnly).allows(Visibility::Private));
        assert!(!hot(false, Audience::MasterOnly).allows(Visibility::Trusted));
    }

    #[test]
    fn remember_default_visibility() {
        assert_eq!(
            default_visibility(true, Audience::MasterOnly),
            Visibility::Private
        );
        assert_eq!(
            default_visibility(true, Audience::Mixed),
            Visibility::Trusted
        );
        assert_eq!(
            default_visibility(false, Audience::MasterOnly),
            Visibility::Public
        );
    }
}
