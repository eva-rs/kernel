//! Живые сессии модулей (`06 §2`): один id модуля — одна сессия. Новая
//! сессия того же id — повторный `hello` клиента или новый дозвон сервиса —
//! вытесняет прежнюю; между разными id вытеснения нет, поэтому `local`
//! держат сразу несколько клиентов. Кто обслуживает surface, выводится
//! отсюда же (`06 §3`): реестра нет.

use std::{
    collections::BTreeMap,
    sync::{Mutex, MutexGuard},
};

use eva::time::Timestamp;
use tokio::sync::broadcast;

use eva_core::Caller;
use eva_protocol::{
    types::{ModuleId, SessionId, module::ModuleKind},
    wire::{Capabilities, RpcError, capabilities::HooksCap, hooks::HookPoint},
};
use eva_rpc::{ModuleClient, Peer};

use crate::{
    bus::Published,
    db::surfaces::{LOCAL, SurfaceRef},
};

/// Откуда идентичность сессии: отзыв токена закрывает только сессии
/// выданного токена, `modules.remove` — только сессии записи из БД.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    ConfigToken,
    IssuedToken,
    ConfigService,
    RuntimeService,
}

#[derive(Debug)]
pub struct Entry {
    pub session: SessionId,
    pub caller: Caller,
    pub source: Source,
    /// Объявленные в `hello` возможности определяют доступные обратные вызовы.
    pub capabilities: Capabilities,
    pub since: Timestamp,
    /// Подписка capability `events`, взятая до `welcome`: первые вызовы
    /// модуля не обгоняют её своими событиями.
    events: Option<broadcast::Receiver<Published>>,
    /// `None` — handshake ещё не дошёл до `welcome`.
    peer: Option<Peer>,
    /// Сессию закрыли: новые запросы её модуля не исполняются.
    closing: Option<RpcError>,
}

impl Entry {
    pub fn new(
        session: SessionId,
        caller: Caller,
        source: Source,
        capabilities: Capabilities,
        since: Timestamp,
    ) -> Self {
        Self {
            session,
            caller,
            source,
            capabilities,
            since,
            events: None,
            peer: None,
            closing: None,
        }
    }

    #[must_use]
    pub fn with_events(mut self, events: broadcast::Receiver<Published>) -> Self {
        self.events = Some(events);
        self
    }

    /// Закрывает соединение; до `welcome` — как только пир появится.
    pub(crate) fn close(&mut self, reason: RpcError) {
        if let Some(peer) = &self.peer {
            peer.close(reason.clone());
        }
        self.closing = Some(reason);
    }
}

/// Итог `attach`: пир встал на место либо сессию надо закрыть сразу.
#[derive(Debug)]
pub enum Attach {
    /// Пир на месте; принятые capabilities и подписка на шину.
    Live(Capabilities, Option<broadcast::Receiver<Published>>),
    /// Вытеснена ещё до `welcome`: состояние модуля держит новая сессия.
    Superseded,
    /// Закрыта отзывом до `welcome`: закрыть и прибрать как обычный разрыв.
    Closing(RpcError),
}

/// `hello` опоздал к регистрации сессии.
#[derive(Debug, Clone)]
pub enum Refused {
    /// Отзыв токена или `modules.remove` застал `hello` между проверкой
    /// идентичности и регистрацией.
    Revoked,
    /// Soul останавливается: сессия, заявленная после `shut_down`, пережила бы его.
    Closing(RpcError),
}

#[derive(Debug, Default)]
struct State {
    sessions: BTreeMap<ModuleId, Entry>,
    /// Счётчик отзывов по имени: растёт и без живой сессии.
    epochs: BTreeMap<ModuleId, u64>,
    /// Причина `shut_down`: новых сессий больше нет.
    closing: Option<RpcError>,
}

#[derive(Debug, Default)]
pub struct Live(Mutex<State>);

impl Live {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Номер отзыва имени: `hello` читает его до проверки токена, дозвон — до connect.
    pub fn epoch(&self, module: &ModuleId) -> u64 {
        self.lock().epochs.get(module).copied().unwrap_or_default()
    }

    /// Новая сессия имени; прежняя возвращается, её закрывает вызывающий.
    /// `epoch` — номер, прочитанный до проверки идентичности: отзыв после
    /// него значит, что проверка устарела.
    pub fn claim(
        &self,
        module: ModuleId,
        entry: Entry,
        epoch: Option<u64>,
    ) -> Result<Option<Entry>, Refused> {
        let mut live = self.lock();
        if let Some(reason) = &live.closing {
            return Err(Refused::Closing(reason.clone()));
        }
        let current = live.epochs.get(&module).copied().unwrap_or_default();
        if epoch.is_some_and(|seen| seen != current) {
            return Err(Refused::Revoked);
        }
        Ok(live.sessions.insert(module, entry))
    }

    /// Пир принятой сессии.
    pub fn attach(&self, module: &ModuleId, session: &SessionId, peer: Peer) -> Attach {
        let mut live = self.lock();
        match live.sessions.get_mut(module) {
            Some(entry) if entry.session == *session => {
                entry.peer = Some(peer);
                match &entry.closing {
                    Some(reason) => Attach::Closing(reason.clone()),
                    None => Attach::Live(entry.capabilities.clone(), entry.events.take()),
                }
            }
            _ => Attach::Superseded,
        }
    }

    /// Сессия закрылась; `Some` — она была текущей, и прибрать её состояние
    /// должен вызывающий.
    pub fn release(&self, module: &ModuleId, session: &SessionId) -> Option<Entry> {
        let mut live = self.lock();
        match live.sessions.get(module) {
            Some(entry) if entry.session == *session => live.sessions.remove(module),
            _ => None,
        }
    }

    /// Токен модуля отозван или заменён: сессия с выданным токеном
    /// закрывается (`06 §9`), с токеном из конфига — живёт; `hello`,
    /// проверивший токен до отзыва, сессию уже не получит.
    pub fn revoke(&self, module: &ModuleId, reason: RpcError) -> bool {
        self.invalidate(module, Source::IssuedToken, reason)
    }

    /// `modules.remove`: сессия записи из БД закрывается, попытка дозвона,
    /// начатая до удаления, сессию уже не получит.
    pub fn remove(&self, module: &ModuleId, reason: RpcError) -> bool {
        self.invalidate(module, Source::RuntimeService, reason)
    }

    fn invalidate(&self, module: &ModuleId, source: Source, reason: RpcError) -> bool {
        let mut live = self.lock();
        *live.epochs.entry(module.clone()).or_default() += 1;
        match live.sessions.get_mut(module) {
            Some(entry) if entry.source == source => {
                entry.close(reason);
                true
            }
            _ => false,
        }
    }

    /// Причина закрытия текущей сессии модуля: запрос, который пир успел
    /// прочесть до закрытия, исполнять уже нельзя.
    pub fn closed(&self, module: &ModuleId) -> Option<RpcError> {
        self.lock()
            .sessions
            .get(module)
            .and_then(|entry| entry.closing.clone())
    }

    /// Закрывает все сессии; модули вправе вернуться.
    pub fn close_all(&self, reason: &RpcError) {
        for entry in self.lock().sessions.values_mut() {
            entry.close(reason.clone());
        }
    }

    /// Остановка soul: закрывает все сессии и отказывает новым.
    pub fn shut_down(&self, reason: &RpcError) {
        let mut live = self.lock();
        live.closing = Some(reason.clone());
        for entry in live.sessions.values_mut() {
            entry.close(reason.clone());
        }
    }

    pub fn is_empty(&self) -> bool {
        self.lock().sessions.is_empty()
    }

    pub fn has_client(&self) -> bool {
        self.lock()
            .sessions
            .values()
            .any(|entry| entry.closing.is_none() && entry.caller.kind() == Some(ModuleKind::Client))
    }

    /// Имена и caller'ы живых сессий.
    pub fn sessions(&self) -> Vec<(ModuleId, Caller)> {
        self.lock()
            .sessions
            .iter()
            .map(|(module, entry)| (module.clone(), entry.caller.clone()))
            .collect()
    }

    /// Модули, чьи живые сессии обслуживают surface (`06 §3`): сессия
    /// модуля-создателя, у builtin `local` — каждая `client`-сессия.
    pub fn serving(&self, surface: &SurfaceRef) -> Vec<ModuleId> {
        self.lock()
            .sessions
            .iter()
            .filter(|(module, entry)| serves(module, entry, surface))
            .map(|(module, _)| module.clone())
            .collect()
    }

    /// Клиент живой сессии модуля для `tools.call`; закрываемая — `None`.
    pub fn client(&self, module: &ModuleId) -> Option<(SessionId, ModuleClient)> {
        let live = self.lock();
        let entry = live.sessions.get(module)?;
        if entry.closing.is_some() {
            return None;
        }
        let peer = entry.peer.clone()?;
        Some((entry.session.clone(), ModuleClient::new(peer)))
    }

    /// Сессии, объявившие точку хука, по имени модуля (`06 §7`); модуль
    /// запасного входа `provider` хуков не получает.
    pub fn hooked(&self, point: HookPoint) -> Vec<Hooked> {
        self.lock()
            .sessions
            .iter()
            .filter(|(_, e)| e.closing.is_none() && e.capabilities.provider.is_none())
            .filter_map(|(module, e)| {
                let cap = e.capabilities.hooks.as_ref()?;
                if !cap.points.contains(&point) {
                    return None;
                }
                Some(Hooked {
                    module: module.clone(),
                    caller: e.caller.clone(),
                    cap: cap.clone(),
                    client: e.peer.clone().map(ModuleClient::new),
                })
            })
            .collect()
    }

    /// `ask_ui` чатов surface (`07 §1`): хоть одна обслуживающая его живая
    /// сессия объявила capability `ask`.
    pub fn ask_ui(&self, surface: &SurfaceRef) -> bool {
        self.lock()
            .sessions
            .iter()
            .any(|(module, entry)| entry.capabilities.ask && serves(module, entry, surface))
    }
}

/// Цель хука; `client: None` — сессия ещё до `welcome`, хук недоступен.
#[derive(Debug, Clone)]
pub struct Hooked {
    pub module: ModuleId,
    pub caller: Caller,
    pub cap: HooksCap,
    pub client: Option<ModuleClient>,
}

/// Закрываемая сессия уже никого не обслуживает; `system` не обслуживает никто.
fn serves(module: &ModuleId, entry: &Entry, surface: &SurfaceRef) -> bool {
    entry.closing.is_none()
        && match &surface.module {
            Some(creator) => creator == module,
            None => surface.id.as_str() == LOCAL && entry.caller.kind() == Some(ModuleKind::Client),
        }
}
