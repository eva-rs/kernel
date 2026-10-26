//! Чаты: права решает surface чата; `Service` правит только поля §3.

use eva_core::Caller;
use eva_protocol::{
    errors::{Aux, AuxResult},
    requests::Chats::update::Update,
    types::SurfaceId,
};

use crate::{
    db::{chats::ChatRef, folders::FolderRef, surfaces::SurfaceRef},
    state::policy::Policy,
};

#[derive(Debug, Clone, Copy)]
pub struct ChatsPolicy<'a>(Policy<'a>);

impl<'a> Policy<'a> {
    pub const fn chats(self) -> ChatsPolicy<'a> {
        ChatsPolicy(self)
    }
}

impl ChatsPolicy<'_> {
    /// `chats.get/history_read`, `turns.*`, `blobs.*`, `stats.chat_spend`.
    pub fn read(self, chat: &ChatRef) -> AuxResult<()> {
        self.0.surfaces().own(&chat.surface)
    }

    /// `chats.get` по alias — до поиска чата: `Service` иначе узнавал бы,
    /// есть ли у хозяина чат с этим внешним id на чужом surface.
    pub fn alias(self, surface: &SurfaceId, owner: Option<&SurfaceRef>) -> AuxResult<()> {
        match self.0.caller() {
            Caller::Service { .. } => self
                .0
                .surfaces()
                .named(format_args!("surface {surface}"), owner)
                .map(drop),
            Caller::Soul | Caller::Client { .. } => Ok(()),
        }
    }

    /// `chats.update`: `Service` — `title, pinned, alerted, kind, project,
    /// workdir, workdir_on`; прочие поля — `set_trust()`.
    pub fn mutate(self, chat: &ChatRef, update: &Update) -> AuxResult<()> {
        self.0.surfaces().own(&chat.surface)?;
        let touches_trust = !(update.trusted.is_keep()
            && update.model.is_keep()
            && update.effort.is_keep()
            && update.thinking.is_keep()
            && update.compact_at.is_keep());
        if touches_trust {
            self.0.set_trust()?;
        }
        Ok(())
    }

    /// `chats.create`: `Service` — свой surface и папка с trust `Unsorted`,
    /// иначе модуль получал бы trusted-чат без действия хозяина.
    pub fn create(self, surface: &SurfaceRef, folder: &FolderRef) -> AuxResult<()> {
        self.0.surfaces().own(surface)?;
        match self.0.caller() {
            Caller::Service { .. } if folder.trusted != folder.unsorted_trusted => Err(
                Aux::forbidden("folder trust differs from Unsorted of this surface"),
            ),
            _ => Ok(()),
        }
    }
}
