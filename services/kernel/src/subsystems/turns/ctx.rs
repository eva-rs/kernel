//! Контекст тура (`05 §3`): разрешается один раз на входе в тур; на провод
//! уходит проекция `TurnCtx` (`06 §4`).

use eva_protocol::{
    errors::Aux,
    types::{
        ChatId, Effort, FolderId, ModelId, ModuleId, PersonId, RunId, SessionId, Thinking, TurnId,
        tool::ToolDecl,
        turn::{Actor, ActorKind, Audience, TurnCtx, TurnView},
    },
};
use serde_json::Value;

use crate::{
    config::LlmConfig,
    db::{Connection, Queries as _, surfaces::SurfaceRef},
    subsystems::{stats::Origin, tools::expose::Whitelist},
};

/// Разрешённый контекст тура.
#[derive(Debug, Clone)]
pub struct Ctx {
    pub chat: ChatId,
    pub surface: SurfaceRef,
    pub folder: FolderId,
    pub actor: Actor,
    pub master_turn: bool,
    pub trusted_chat: bool,
    pub audience: Audience,
    pub whitelist: Whitelist,
    pub model: ModelId,
    pub effort: Effort,
    pub thinking: Thinking,
    pub run_id: Option<RunId>,
    pub sleep_run_id: Option<RunId>,
    pub anchor: Option<Value>,
    /// Тулы на один тур (`02 §4.4`), исполняет `origin`.
    pub extra_tools: Vec<ToolDecl>,
    /// Сессия `turns.start`: исполнитель `extra_tools`.
    pub origin: Option<(ModuleId, SessionId)>,
    /// Источник `spend` (`09 §2`).
    pub source: Origin,
}

impl Ctx {
    /// `¬master_turn ∨ ¬trusted_chat`.
    pub const fn public(&self) -> bool {
        !(self.master_turn && self.trusted_chat)
    }

    pub fn wire(&self, turn: &TurnId) -> TurnCtx {
        TurnCtx {
            turn_id: turn.clone(),
            chat: self.chat,
            actor: self.actor.clone(),
            master_turn: self.master_turn,
            trusted_chat: self.trusted_chat,
            public: self.public(),
            surface: self.surface.id.clone(),
            anchor: self.anchor.clone(),
            run_id: self.run_id.clone(),
        }
    }

    pub fn view(&self) -> TurnView {
        TurnView {
            chat: self.chat,
            actor: self.actor.person_id.clone(),
            master_turn: self.master_turn,
            trusted_chat: self.trusted_chat,
            audience: self.audience,
        }
    }
}

/// Перекрытие уровня сообщения (`05 §2.1`): учитывается только у master-актора.
#[derive(Debug, Clone, Default)]
pub struct Overrides {
    pub model: Option<ModelId>,
    pub effort: Option<Effort>,
    pub thinking: Option<Thinking>,
}

/// Вход в тур без транспорта: актор уже разрешён (`persons.resolve`).
#[derive(Debug, Clone)]
pub struct Entry {
    pub chat: ChatId,
    pub actor: Actor,
    pub overrides: Overrides,
    pub whitelist: Whitelist,
    pub run_id: Option<RunId>,
    pub sleep_run_id: Option<RunId>,
    pub anchor: Option<Value>,
    pub extra_tools: Vec<ToolDecl>,
    pub origin: Option<(ModuleId, SessionId)>,
    pub source: Origin,
}

impl Entry {
    /// Тур хозяина без вложений — крон, сон, тесты.
    pub fn master(chat: ChatId, person: PersonId) -> Self {
        Self {
            chat,
            actor: Actor {
                person_id: Some(person),
                kind: ActorKind::Master,
                name: None,
                handle: None,
            },
            overrides: Overrides::default(),
            whitelist: Whitelist::Unrestricted,
            run_id: None,
            sleep_run_id: None,
            anchor: None,
            extra_tools: Vec::new(),
            origin: None,
            source: Origin::Surface,
        }
    }
}

/// Цепочка `message > chat > folder > surface > global` и trust чата.
pub fn resolve(conn: &Connection, mut entry: Entry, llm: &LlmConfig) -> Result<Ctx, Aux> {
    let chat = conn
        .chats()
        .get(entry.chat)?
        .ok_or_else(|| Aux::NotFound(format!("chat {} does not exist", entry.chat)))?;
    let folder = conn
        .folders()
        .get(chat.folder_id)?
        .ok_or_else(|| Aux::Internal(format!("chat {} has no folder", chat.id)))?;
    let surface = conn
        .surfaces()
        .get(&chat.surface.id)?
        .ok_or_else(|| Aux::Internal(format!("chat {} has no surface", chat.id)))?;
    if chat.agent {
        let agent = conn
            .agents()
            .get(chat.id)?
            .ok_or_else(|| Aux::Internal(format!("agent chat {} has no agent", chat.id)))?;
        if !matches!(
            agent.status,
            eva_protocol::types::agent::Status::Running | eva_protocol::types::agent::Status::Idle
        ) {
            return Err(Aux::Invalid(String::from("agent finished")));
        }
        entry.whitelist = Whitelist::Restricted(agent.tools.into_iter().collect());
        entry.source = Origin::Agent;
        entry.actor.kind = if agent.master_turn {
            ActorKind::Master
        } else {
            ActorKind::Whitelisted
        };
        entry.extra_tools.clear();
    }
    let master_turn = entry.actor.kind == ActorKind::Master;
    let message = if master_turn {
        entry.overrides
    } else {
        Overrides::default()
    };
    let audience = conn.turns().audience(chat.id)?;
    Ok(Ctx {
        chat: chat.id,
        surface: chat.surface.clone(),
        folder: folder.id,
        master_turn,
        trusted_chat: chat.trusted_chat,
        audience,
        whitelist: entry.whitelist,
        model: message
            .model
            .or(chat.model)
            .or(folder.model)
            .or(surface.model)
            .unwrap_or_else(|| llm.default_model.clone()),
        effort: message
            .effort
            .or(chat.effort)
            .or(folder.effort)
            .or(surface.effort)
            .unwrap_or(llm.default_effort),
        thinking: message
            .thinking
            .or(chat.thinking)
            .or(folder.thinking)
            .or(surface.thinking)
            .unwrap_or(llm.default_thinking),
        actor: entry.actor,
        run_id: entry.run_id,
        sleep_run_id: entry.sleep_run_id,
        anchor: entry.anchor,
        extra_tools: entry.extra_tools,
        origin: entry.origin,
        source: entry.source,
    })
}
