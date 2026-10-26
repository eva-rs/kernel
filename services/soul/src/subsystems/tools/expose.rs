//! Решётка Exposure (`02 §3.2`): одна функция для спек шага, каталога
//! резерва и отказа на исполнении (I3). Правила сверху вниз, первое
//! совпавшее решает.

use std::collections::{BTreeMap, BTreeSet};

use eva_protocol::types::{
    ToolId,
    tool::{Exposure, Forbid, Need, ToolDecl},
};

use crate::db::tools::{Attention, OverrideRow};

/// Список тулов тура (`05 §3`): крон, подагент, сон.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Whitelist {
    #[default]
    Unrestricted,
    /// `Restricted([])` — ни одного (I8).
    Restricted(BTreeSet<ToolId>),
}

/// Что гейты читают из контекста тура.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Gates {
    pub master_turn: bool,
    pub trusted_chat: bool,
    pub whitelist: Whitelist,
    pub available: BTreeSet<Need>,
}

/// Внимание чата на сутки (`02 §5`).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct DayState {
    /// `None` — пересчёта ещё не было: warm — все допустимые.
    pub warm: Option<BTreeSet<ToolId>>,
    pub pinned: BTreeSet<ToolId>,
    pub loaded: BTreeSet<ToolId>,
    pub overrides: BTreeMap<ToolId, OverrideRow>,
}

/// Тул реестра глазами решётки.
#[derive(Debug, Clone, Copy)]
pub struct Subject<'a> {
    pub id: ToolId,
    pub decl: &'a ToolDecl,
    pub online: bool,
    pub lost_collision: bool,
    /// Путь под `tools.core`.
    pub core: bool,
}

pub fn expose(tool: Subject<'_>, gates: &Gates, day: &DayState) -> Exposure {
    let over = day.overrides.get(&tool.id).copied().unwrap_or_default();
    if !tool.online {
        return Exposure::Forbidden(Forbid::ModuleOffline);
    }
    if tool.lost_collision {
        return Exposure::Forbidden(Forbid::NameCollision);
    }
    if let Whitelist::Restricted(allowed) = &gates.whitelist
        && !allowed.contains(&tool.id)
    {
        return Exposure::Forbidden(Forbid::NotInWhitelist);
    }
    if over.forbid {
        return Exposure::Forbidden(Forbid::Override);
    }
    let master_only = over.master_only.unwrap_or(tool.decl.master_only);
    let trusted_only = over.trusted_only.unwrap_or(tool.decl.trusted_only);
    if let Some(forbid) = gate(master_only, trusted_only, &tool.decl.needs, gates) {
        return Exposure::Forbidden(forbid);
    }
    let warm = match &day.warm {
        Some(warm) => warm.contains(&tool.id),
        None => over.attention != Some(Attention::Reserved),
    };
    let visible = matches!(gates.whitelist, Whitelist::Restricted(_))
        || tool.core
        || over.attention == Some(Attention::Always)
        || day.pinned.contains(&tool.id)
        || warm
        || day.loaded.contains(&tool.id);
    if visible {
        Exposure::Visible
    } else {
        Exposure::Reserved
    }
}

/// `extra_tools` тура (`02 §4.4`): правило 3 и гейты 5–7; прошедшее — `Visible`.
pub fn expose_extra(decl: &ToolDecl, gates: &Gates) -> Exposure {
    if matches!(gates.whitelist, Whitelist::Restricted(_)) {
        return Exposure::Forbidden(Forbid::NotInWhitelist);
    }
    match gate(decl.master_only, decl.trusted_only, &decl.needs, gates) {
        Some(forbid) => Exposure::Forbidden(forbid),
        None => Exposure::Visible,
    }
}

/// Гейты 5–7.
fn gate(master_only: bool, trusted_only: bool, needs: &[Need], gates: &Gates) -> Option<Forbid> {
    if master_only && !gates.master_turn {
        return Some(Forbid::MasterOnly);
    }
    if trusted_only && !gates.trusted_chat {
        return Some(Forbid::TrustedOnly);
    }
    needs
        .iter()
        .find(|n| !gates.available.contains(n))
        .map(|n| Forbid::Needs(*n))
}

/// Байт-стабильная причина отказа (I13): логи и метрики.
pub const fn forbid_str(forbid: Forbid) -> &'static str {
    match forbid {
        Forbid::ModuleOffline => "module_offline",
        Forbid::NameCollision => "name_collision",
        Forbid::NotInWhitelist => "not_in_whitelist",
        Forbid::Override => "override",
        Forbid::MasterOnly => "master_only",
        Forbid::TrustedOnly => "trusted_only",
        Forbid::Needs(_) => "needs",
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;
    use serde_json::json;

    use super::*;

    const NEEDS: [Need; 11] = [
        Need::Hands,
        Need::Lead,
        Need::Team,
        Need::Agent,
        Need::Work,
        Need::Private,
        Need::AskUi,
        Need::IndexNotes,
        Need::Sleep,
        Need::Embedder,
        Need::Unknown,
    ];

    fn decl(master_only: bool, trusted_only: bool, needs: Vec<Need>) -> ToolDecl {
        ToolDecl {
            path: "x.tool".into(),
            description: "A tool.".into(),
            input: json!({"type": "object", "properties": {}}),
            master_only,
            trusted_only,
            needs,
            ends_turn: false,
            parallel_safe: false,
            waits_on_cancel: false,
            result: Default::default(),
            timeout: None,
            idempotent: false,
        }
    }

    /// Эталон §3.2 таблицей, независимо от `expose`.
    #[allow(clippy::too_many_arguments, clippy::fn_params_excessive_bools)]
    fn reference(
        online: bool,
        lost: bool,
        whitelisted: Option<bool>,
        over: OverrideRow,
        decl: &ToolDecl,
        gates: &Gates,
        core: bool,
        warm: Option<bool>,
        pinned: bool,
        loaded: bool,
    ) -> Exposure {
        let rows: [(bool, Exposure); 7] = [
            (!online, Exposure::Forbidden(Forbid::ModuleOffline)),
            (lost, Exposure::Forbidden(Forbid::NameCollision)),
            (
                whitelisted == Some(false),
                Exposure::Forbidden(Forbid::NotInWhitelist),
            ),
            (over.forbid, Exposure::Forbidden(Forbid::Override)),
            (
                over.master_only.unwrap_or(decl.master_only) && !gates.master_turn,
                Exposure::Forbidden(Forbid::MasterOnly),
            ),
            (
                over.trusted_only.unwrap_or(decl.trusted_only) && !gates.trusted_chat,
                Exposure::Forbidden(Forbid::TrustedOnly),
            ),
            (false, Exposure::Visible),
        ];
        for (hit, exposure) in rows {
            if hit {
                return exposure;
            }
        }
        if let Some(n) = decl.needs.iter().find(|n| !gates.available.contains(n)) {
            return Exposure::Forbidden(Forbid::Needs(*n));
        }
        let warm = warm.unwrap_or(over.attention != Some(Attention::Reserved));
        if whitelisted.is_some()
            || core
            || over.attention == Some(Attention::Always)
            || pinned
            || warm
            || loaded
        {
            Exposure::Visible
        } else {
            Exposure::Reserved
        }
    }

    fn need() -> impl Strategy<Value = Need> {
        prop::sample::select(NEEDS.to_vec())
    }

    fn over() -> impl Strategy<Value = OverrideRow> {
        (
            prop::option::of(any::<bool>()),
            prop::option::of(any::<bool>()),
            any::<bool>(),
            prop::option::of(prop::sample::select(vec![
                Attention::Always,
                Attention::Reserved,
            ])),
        )
            .prop_map(
                |(master_only, trusted_only, forbid, attention)| OverrideRow {
                    master_only,
                    trusted_only,
                    forbid,
                    attention,
                },
            )
    }

    proptest! {
        #[test]
        fn exposure_matches_the_table(
            online in any::<bool>(),
            lost in any::<bool>(),
            whitelisted in prop::option::of(any::<bool>()),
            over in over(),
            master_only in any::<bool>(),
            trusted_only in any::<bool>(),
            needs in prop::collection::vec(need(), 0..3),
            master_turn in any::<bool>(),
            trusted_chat in any::<bool>(),
            available in prop::collection::btree_set(need(), 0..11),
            core in any::<bool>(),
            warm in prop::option::of(any::<bool>()),
            pinned in any::<bool>(),
            loaded in any::<bool>(),
        ) {
            let id = ToolId(7);
            let decl = decl(master_only, trusted_only, needs);
            let gates = Gates {
                master_turn,
                trusted_chat,
                whitelist: match whitelisted {
                    None => Whitelist::Unrestricted,
                    Some(true) => Whitelist::Restricted([id].into()),
                    Some(false) => Whitelist::Restricted([ToolId(8)].into()),
                },
                available,
            };
            let set = |on: bool| if on { [id].into() } else { BTreeSet::new() };
            let day = DayState {
                warm: warm.map(set),
                pinned: set(pinned),
                loaded: set(loaded),
                overrides: [(id, over)].into(),
            };
            let subject = Subject { id, decl: &decl, online, lost_collision: lost, core };
            prop_assert_eq!(
                expose(subject, &gates, &day),
                reference(online, lost, whitelisted, over, &decl, &gates, core, warm, pinned, loaded)
            );
        }
    }

    #[test]
    fn whitelist_empty_forbids_even_core_and_restricted_has_no_reserve() {
        let decl = decl(false, false, vec![]);
        let subject = |id| Subject {
            id: ToolId(id),
            decl: &decl,
            online: true,
            lost_collision: false,
            core: true,
        };
        let empty = Gates {
            whitelist: Whitelist::Restricted(BTreeSet::new()),
            ..Gates::default()
        };
        let day = DayState {
            warm: Some(BTreeSet::new()),
            ..DayState::default()
        };
        assert_eq!(
            expose(subject(1), &empty, &day),
            Exposure::Forbidden(Forbid::NotInWhitelist)
        );
        let one = Gates {
            whitelist: Whitelist::Restricted([ToolId(1)].into()),
            ..Gates::default()
        };
        let cold = Subject {
            core: false,
            ..subject(1)
        };
        assert_eq!(expose(cold, &one, &day), Exposure::Visible);
        assert_eq!(expose(cold, &Gates::default(), &day), Exposure::Reserved);
        // без пересчёта warm-набора ничего не прячется
        assert_eq!(
            expose(cold, &Gates::default(), &DayState::default()),
            Exposure::Visible
        );
    }

    #[test]
    fn private_tool_is_closed_in_public_chats_and_open_for_system_turns() {
        let decl = decl(false, false, vec![Need::Private]);
        let subject = Subject {
            id: ToolId(1),
            decl: &decl,
            online: true,
            lost_collision: false,
            core: false,
        };
        let public = Gates::default();
        assert_eq!(
            expose(subject, &public, &DayState::default()),
            Exposure::Forbidden(Forbid::Needs(Need::Private))
        );
        let system = Gates {
            master_turn: true,
            trusted_chat: true,
            whitelist: Whitelist::Unrestricted,
            available: [Need::Private].into(),
        };
        assert_eq!(
            expose(subject, &system, &DayState::default()),
            Exposure::Visible
        );
    }

    #[test]
    fn extra_tools_follow_rule_three_and_gates() {
        let open = decl(false, false, vec![]);
        assert_eq!(expose_extra(&open, &Gates::default()), Exposure::Visible);
        let restricted = Gates {
            whitelist: Whitelist::Restricted(BTreeSet::new()),
            ..Gates::default()
        };
        assert_eq!(
            expose_extra(&open, &restricted),
            Exposure::Forbidden(Forbid::NotInWhitelist)
        );
        assert_eq!(
            expose_extra(&decl(true, false, vec![]), &Gates::default()),
            Exposure::Forbidden(Forbid::MasterOnly)
        );
    }
}
