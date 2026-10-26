//! Входы движка из соседних подсистем: память, заметки, работа и тулы soul
//! соседних подсистем. Реализации подключены в `Ports::default`.

use std::{pin::Pin, sync::Arc};

use eva_protocol::{
    errors::Aux,
    types::{
        ChatId, MemoryId, ModelId, TurnId,
        tool::ToolDecl,
        turn::{Audience, TurnCtx, TurnOutcome, TurnView},
    },
    wire::tools::ToolResult,
};
use serde_json::Value;

use crate::{components::Components, config::Config, shared::Shared};

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

fn ready<'a, T: Send + 'a>(value: T) -> BoxFuture<'a, T> {
    Box::pin(std::future::ready(value))
}

/// Для чего собирается hot-блок сегмента (`03 §5`).
#[derive(Debug, Clone, Copy)]
pub struct HotCtx {
    pub chat: ChatId,
    pub trusted_chat: bool,
    pub audience: Audience,
    pub master_turn: bool,
    pub ratio: Option<f64>,
}

/// Секция 2 system: core и hot-блок с шапкой; `ids` — `segments.hot_memory_ids`.
#[derive(Debug, Clone, Default)]
pub struct HotBlock {
    pub text: Option<String>,
    pub ids: Vec<MemoryId>,
}

/// Hot-блок памяти сегмента.
pub trait MemoryPort<C: Components>: Send + Sync {
    fn hot_block<'a>(&'a self, shared: &'a Shared<C>, ctx: HotCtx) -> BoxFuture<'a, HotBlock> {
        let _ = (shared, ctx);
        ready(HotBlock::default())
    }
}

/// Секция заметок, давление и нуджи.
pub trait NotesPort<C: Components>: Send + Sync {
    /// `[notes rules]` и `[notes]` секции 5 system на старте сегмента.
    fn section<'a>(
        &'a self,
        shared: &'a Shared<C>,
        chat: ChatId,
        ratio: Option<f64>,
    ) -> BoxFuture<'a, Option<String>> {
        let _ = (shared, chat, ratio);
        ready(None)
    }

    /// Строка давления заметок в reminder (`04 §6`).
    fn pressure_line<'a>(
        &'a self,
        shared: &'a Shared<C>,
        chat: ChatId,
        model: ModelId,
    ) -> BoxFuture<'a, Option<String>> {
        let _ = (shared, chat, model);
        ready(None)
    }

    /// Заметок, перенесённых через границу сегмента (`04 §10`).
    fn carried_over<'a>(&'a self, shared: &'a Shared<C>, chat: ChatId) -> BoxFuture<'a, u32> {
        let _ = (shared, chat);
        ready(0)
    }

    /// Нудж reminder первого шага после компакции (`04 §6`).
    fn after_compaction<'a>(
        &'a self,
        shared: &'a Shared<C>,
        chat: ChatId,
    ) -> BoxFuture<'a, Option<String>> {
        let _ = (shared, chat);
        ready(None)
    }

    /// Блокнот чата в режиме `index` — предикат `Need::IndexNotes`.
    fn index_mode<'a>(&'a self, shared: &'a Shared<C>, chat: ChatId) -> BoxFuture<'a, bool> {
        let _ = (shared, chat);
        ready(false)
    }
}

/// Роль подагента, почта тимлида, живые подагенты.
pub trait WorkPort<C: Components>: Send + Sync {
    fn role_line<'a>(
        &'a self,
        shared: &'a Shared<C>,
        chat: ChatId,
    ) -> BoxFuture<'a, Option<String>> {
        let _ = (shared, chat);
        ready(None)
    }

    /// Письма `to_parent` в reminder тимлида, не больше `budget` токенов.
    fn inbox_lines<'a>(
        &'a self,
        shared: &'a Shared<C>,
        chat: ChatId,
        budget: u32,
    ) -> BoxFuture<'a, Vec<String>> {
        let _ = (shared, chat, budget);
        ready(Vec::new())
    }

    /// Живые подагенты: граница активных задач плана (`07 §5`).
    fn live_subagents<'a>(&'a self, shared: &'a Shared<C>, chat: ChatId) -> BoxFuture<'a, u32> {
        let _ = (shared, chat);
        ready(0)
    }

    /// Тур закрыт: статус подагента и stop-notice тимлиду (`07 §2.2`).
    fn on_turn_closed<'a>(
        &'a self,
        shared: &'a Shared<C>,
        chat: ChatId,
        turn: &'a TurnId,
        outcome: TurnOutcome,
    ) -> BoxFuture<'a, ()> {
        let _ = (shared, chat, turn, outcome);
        ready(())
    }
}

/// Вызов тула soul чужой главы.
pub struct ToolsetCall<'a> {
    pub canonical: &'a str,
    pub call_id: &'a str,
    pub input: Value,
    pub ctx: &'a TurnCtx,
    pub view: &'a TurnView,
    /// `hot_memory_ids` сегмента.
    pub hot_ids: &'a [MemoryId],
    /// Калиброванный ratio модели чата.
    pub ratio: Option<f64>,
}

/// Тулы соседних подсистем — декларации от
/// идентичности `soul` и исполнение их функциями-контрактами.
pub trait Toolset<C: Components>: Send + Sync {
    fn decls(&self, cfg: &Config) -> Vec<ToolDecl> {
        let _ = cfg;
        Vec::new()
    }

    /// `Err` — беда soul (`Fatal`); отказ модели — `ToolResult{is_error}`.
    fn call<'a>(
        &'a self,
        shared: &'a Shared<C>,
        call: ToolsetCall<'a>,
    ) -> BoxFuture<'a, Result<ToolResult, Aux>> {
        let _ = shared;
        let name = call.canonical.to_owned();
        ready(Err(Aux::Internal(format!(
            "soul tool {name} has no handler"
        ))))
    }
}

/// Порты, которыми владеет `Shared`.
pub struct Ports<C: Components> {
    pub memory: Arc<dyn MemoryPort<C>>,
    pub notes: Arc<dyn NotesPort<C>>,
    pub work: Arc<dyn WorkPort<C>>,
    pub toolset: Arc<dyn Toolset<C>>,
}

/// Порт без реализации.
pub struct Inert;

impl<C: Components> MemoryPort<C> for Inert {}
impl<C: Components> NotesPort<C> for Inert {}
impl<C: Components> WorkPort<C> for Inert {}
impl<C: Components> Toolset<C> for Inert {}

impl<C: Components> Default for Ports<C> {
    fn default() -> Self {
        let integrated = Arc::new(super::integration::Integrated::default());
        Self {
            memory: integrated.clone(),
            notes: integrated.clone(),
            work: Arc::new(crate::subsystems::work::agents::WorkPort),
            toolset: integrated,
        }
    }
}
