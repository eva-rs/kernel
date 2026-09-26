//! Живые сессии модулей (`06 §2`): одно имя — одна сессия. Повторный
//! `hello` того же имени вытесняет прежнюю; между разными именами
//! вытеснения нет, поэтому `local` держат сразу несколько клиентов.

use std::{
    collections::BTreeMap,
    sync::{Mutex, MutexGuard},
};

use eva::time::Timestamp;

use eva_core::Caller;
use eva_protocol::{
    types::{ModuleId, SessionId},
    wire::{Capabilities, RpcError},
};
use eva_rpc::Peer;

/// Откуда токен сессии: отзыв и ротация `module_tokens` не трогают сессию
/// с токеном из конфига.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenSource {
    Config,
    Issued,
}

#[derive(Debug)]
pub struct Entry {
    pub session: SessionId,
    pub caller: Caller,
    pub source: TokenSource,
    /// Объявленное в `hello`: `tools`/`hooks` исполняются с M2/M4, до тех
    /// пор только хранятся.
    pub capabilities: Capabilities,
    pub since: Timestamp,
    /// `None` — handshake ещё не дошёл до `welcome`.
    peer: Option<Peer>,
    /// Сессию закрыли, пока у неё не было пира.
    closing: Option<RpcError>,
}

impl Entry {
    pub fn new(
        session: SessionId,
        caller: Caller,
        source: TokenSource,
        capabilities: Capabilities,
        since: Timestamp,
    ) -> Self {
        Self {
            session,
            caller,
            source,
            capabilities,
            since,
            peer: None,
            closing: None,
        }
    }

    /// Закрывает соединение; до `welcome` — как только пир появится.
    pub(crate) fn close(&mut self, reason: RpcError) {
        match &self.peer {
            Some(peer) => peer.close(reason),
            None => self.closing = Some(reason),
        }
    }
}

/// Итог `attach`: пир встал на место либо сессию надо закрыть сразу.
#[derive(Debug)]
pub enum Attach {
    /// Пир на месте; принятые capabilities сессии.
    Live(Capabilities),
    /// Вытеснена ещё до `welcome`: состояние модуля держит новая сессия.
    Superseded,
    /// Закрыта отзывом до `welcome`: закрыть и прибрать как обычный разрыв.
    Closing(RpcError, Capabilities),
}

#[derive(Debug, Default)]
pub struct Live(Mutex<BTreeMap<ModuleId, Entry>>);

impl Live {
    fn lock(&self) -> MutexGuard<'_, BTreeMap<ModuleId, Entry>> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Новая сессия имени; прежняя возвращается, её закрывает вызывающий.
    pub fn claim(&self, module: ModuleId, entry: Entry) -> Option<Entry> {
        self.lock().insert(module, entry)
    }

    /// Пир принятой сессии.
    pub fn attach(&self, module: &ModuleId, session: &SessionId, peer: Peer) -> Attach {
        let mut live = self.lock();
        match live.get_mut(module) {
            Some(entry) if entry.session == *session => {
                entry.peer = Some(peer);
                let capabilities = entry.capabilities.clone();
                match entry.closing.take() {
                    Some(reason) => Attach::Closing(reason, capabilities),
                    None => Attach::Live(capabilities),
                }
            }
            _ => Attach::Superseded,
        }
    }

    /// Сессия закрылась; `Some` — она была текущей, и прибрать её состояние
    /// должен вызывающий.
    pub fn release(&self, module: &ModuleId, session: &SessionId) -> Option<Entry> {
        let mut live = self.lock();
        match live.get(module) {
            Some(entry) if entry.session == *session => live.remove(module),
            _ => None,
        }
    }

    /// Токен модуля отозван или заменён: сессия с выданным токеном
    /// закрывается (`06 §9`), с токеном из конфига — живёт.
    pub fn revoke(&self, module: &ModuleId, reason: RpcError) -> bool {
        match self.lock().get_mut(module) {
            Some(entry) if entry.source == TokenSource::Issued => {
                entry.close(reason);
                true
            }
            _ => false,
        }
    }

    /// Закрывает все сессии: остановка ядра.
    pub fn close_all(&self, reason: &RpcError) {
        for entry in self.lock().values_mut() {
            entry.close(reason.clone());
        }
    }

    pub fn is_empty(&self) -> bool {
        self.lock().is_empty()
    }

    /// Имена и caller'ы живых сессий.
    pub fn sessions(&self) -> Vec<(ModuleId, Caller)> {
        self.lock()
            .iter()
            .map(|(module, entry)| (module.clone(), entry.caller.clone()))
            .collect()
    }
}
