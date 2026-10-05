//! Рабочие операции и состояние разрешений рук.

use crate::{components::Components, shared::Shared};
use eva_protocol::types::{ChatId, ModuleId, SegmentId, SessionId, hands::GrantKind};
use std::{collections::BTreeSet, sync::Mutex};

pub mod agents;
pub mod files;
pub mod hands;
pub mod patch;

#[derive(Default)]
pub struct Access {
    pub agents_changed: tokio::sync::Notify,
    reads: Mutex<BTreeSet<(ChatId, SegmentId, String, String)>>,
    sessions: Mutex<Vec<SessionGrant>>,
}

struct SessionGrant {
    module: ModuleId,
    session: SessionId,
    endpoint: String,
    chat: ChatId,
    kind: GrantKind,
    prefix: String,
}

impl Access {
    pub fn forget(&self, chat: ChatId) {
        self.reads
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .retain(|(id, _, _, _)| *id != chat);
        self.sessions
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .retain(|grant| grant.chat != chat);
    }
    pub fn allow_session(
        &self,
        module: ModuleId,
        session: SessionId,
        endpoint: String,
        chat: ChatId,
        kind: GrantKind,
        prefix: String,
    ) {
        self.sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(SessionGrant {
                module,
                session,
                endpoint,
                chat,
                kind,
                prefix,
            });
    }
    pub fn session_allows<C: Components>(
        &self,
        shared: &Shared<C>,
        endpoint: &str,
        chat: ChatId,
        kind: GrantKind,
        value: &str,
    ) -> bool {
        let mut grants = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        grants.retain(|grant| {
            shared
                .live()
                .client(&grant.module)
                .is_some_and(|(id, _)| id == grant.session)
        });
        grants.iter().any(|g| {
            g.endpoint == endpoint
                && g.chat == chat
                && g.kind == kind
                && matches_prefix(kind, &g.prefix, value)
        })
    }
    pub fn record_read(&self, chat: ChatId, segment: SegmentId, endpoint: &str, path: &str) {
        let mut reads = self.reads.lock().unwrap_or_else(|e| e.into_inner());
        reads.retain(|(c, s, _, _)| *c != chat || *s == segment);
        reads.insert((chat, segment, endpoint.to_owned(), path.to_owned()));
    }
    pub fn was_read(&self, chat: ChatId, segment: SegmentId, endpoint: &str, path: &str) -> bool {
        self.reads
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(&(chat, segment, endpoint.to_owned(), path.to_owned()))
    }
}

pub fn matches_prefix(kind: GrantKind, prefix: &str, value: &str) -> bool {
    if kind == GrantKind::Shell {
        value.starts_with(prefix)
    } else {
        std::path::Path::new(value).starts_with(prefix)
    }
}
