//! Окно идемпотентности `(session_id, request.id)` мутирующих операций
//! (`06 §5`): повтор отдаёт сохранённый ответ, `hello.resume` переносит окно.

use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex, MutexGuard, PoisonError},
    time::Duration,
};

use tokio::{sync::watch, time::Instant};

use eva_protocol::{
    types::{ModuleId, SessionId},
    wire::{ErrorCode, RpcError, frame::Payload},
};

use crate::codec;

type Outcome = Result<Payload, RpcError>;

/// Итог крупнее не хранится: повтор получит `conflict`, но не второй эффект.
const RETAIN_MAX: usize = 64 * 1024;

/// Байт хранимых итогов в одном окне.
const WINDOW_BYTES: usize = 16 << 20;

/// Завершённых записей в одном окне; сверх старейшие забываются целиком.
const WINDOW_ENTRIES: usize = 16 * 1024;

/// Окна всех сессий сервера.
pub(crate) struct Windows {
    ttl: Duration,
    map: Mutex<HashMap<SessionId, Arc<Window>>>,
}

impl Windows {
    pub fn new(ttl: Duration) -> Self {
        Self {
            ttl,
            map: Mutex::default(),
        }
    }

    /// Окно новой сессии; `resume` того же модуля продолжает его окно.
    pub fn open(
        &self,
        session: &SessionId,
        module: &ModuleId,
        resume: Option<&SessionId>,
    ) -> Lease {
        let mut map = self.map.lock().unwrap_or_else(PoisonError::into_inner);
        let now = Instant::now();
        map.retain(|_, window| !window.expired(now));

        let window = resume
            .and_then(|old| map.get(old))
            .filter(|window| &window.module == module)
            .cloned()
            .unwrap_or_else(|| Arc::new(Window::new(module.clone(), self.ttl)));
        map.insert(session.clone(), window.clone());

        window.lock().live += 1;
        Lease(window)
    }
}

/// Окно, пока жива сессия; после её конца окно помнится ещё `ttl`.
pub(crate) struct Lease(Arc<Window>);

impl std::ops::Deref for Lease {
    type Target = Arc<Window>;

    fn deref(&self) -> &Arc<Window> {
        &self.0
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        let mut state = self.0.lock();
        state.live -= 1;
        state.touched = Instant::now();
    }
}

pub(crate) struct Window {
    module: ModuleId,
    ttl: Duration,
    state: Mutex<State>,
}

struct State {
    entries: HashMap<u64, Entry>,
    /// `(когда завершён, id)` по возрастанию: истёкшие снимаются с головы.
    order: VecDeque<(Instant, u64)>,
    /// Байт в хранимых итогах.
    bytes: usize,
    /// Последний запрос или конец сессии.
    touched: Instant,
    /// Сессий, которые сейчас пишут в окно.
    live: usize,
}

enum Entry {
    Running {
        method: String,
        done: watch::Sender<Option<Outcome>>,
    },
    Done {
        method: String,
        /// `None` — итог не хранится (крупный или вытеснен); повтор всё равно
        /// не исполняется второй раз.
        outcome: Option<Outcome>,
        at: Instant,
    },
}

pub(crate) enum Begin {
    /// Первый такой запрос: исполнить и отдать итог в `Slot`.
    Run(Slot),
    Cached(Outcome),
    /// Тот же запрос ещё исполняется (повтор после реконнекта).
    Wait(watch::Receiver<Option<Outcome>>),
    /// id уже занят другим методом.
    Reused,
}

impl Window {
    fn new(module: ModuleId, ttl: Duration) -> Self {
        Self {
            module,
            ttl,
            state: Mutex::new(State {
                entries: HashMap::new(),
                order: VecDeque::new(),
                bytes: 0,
                touched: Instant::now(),
                live: 0,
            }),
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn expired(&self, now: Instant) -> bool {
        let state = self.lock();
        state.live == 0 && (state.entries.is_empty() || state.touched + self.ttl < now)
    }

    pub fn begin(self: &Arc<Self>, id: u64, method: &str) -> Begin {
        let mut state = self.lock();
        let now = Instant::now();
        state.touched = now;
        state.prune(now, self.ttl);

        match state.entries.get(&id) {
            Some(Entry::Done {
                method: done,
                outcome,
                ..
            }) => {
                if done != method {
                    Begin::Reused
                } else {
                    Begin::Cached(outcome.clone().unwrap_or_else(|| {
                        Err(RpcError::new(
                            ErrorCode::Conflict,
                            "request already executed; its response is not retained",
                        ))
                    }))
                }
            }
            Some(Entry::Running {
                method: running,
                done,
            }) => {
                if running == method {
                    Begin::Wait(done.subscribe())
                } else {
                    Begin::Reused
                }
            }
            None => {
                let (done, _) = watch::channel(None);
                state.entries.insert(
                    id,
                    Entry::Running {
                        method: method.to_owned(),
                        done,
                    },
                );
                Begin::Run(Slot {
                    window: self.clone(),
                    id,
                    filled: false,
                })
            }
        }
    }
}

impl State {
    fn prune(&mut self, now: Instant, ttl: Duration) {
        while let Some(&(at, _)) = self.order.front() {
            if at + ttl > now {
                break;
            }
            self.pop_oldest();
        }
    }

    fn pop_oldest(&mut self) {
        let Some((at, id)) = self.order.pop_front() else {
            return;
        };
        if matches!(self.entries.get(&id), Some(Entry::Done { at: done, .. }) if *done == at)
            && let Some(Entry::Done {
                outcome: Some(outcome),
                ..
            }) = self.entries.remove(&id)
        {
            self.bytes = self.bytes.saturating_sub(size(&outcome));
        }
    }

    /// Держит окно в `WINDOW_ENTRIES` и `WINDOW_BYTES`: у старейших сперва
    /// забываются итоги, записи — лишь сверх потолка числа.
    fn shed(&mut self) {
        while self.order.len() > WINDOW_ENTRIES {
            self.pop_oldest();
        }
        let Self {
            entries,
            order,
            bytes,
            ..
        } = self;
        for &(at, id) in order.iter() {
            if *bytes <= WINDOW_BYTES {
                break;
            }
            if let Some(Entry::Done {
                outcome, at: done, ..
            }) = entries.get_mut(&id)
                && *done == at
                && let Some(dropped) = outcome.take()
            {
                *bytes = bytes.saturating_sub(size(&dropped));
            }
        }
    }
}

/// Итог исполняемого запроса; drop без итога снимает запись, повтор исполнится заново.
pub(crate) struct Slot {
    window: Arc<Window>,
    id: u64,
    filled: bool,
}

impl Slot {
    pub fn fill(mut self, outcome: &Outcome) {
        self.filled = true;
        let mut state = self.window.lock();
        let Some(Entry::Running { method, done }) = state.entries.remove(&self.id) else {
            return;
        };
        done.send_replace(Some(outcome.clone()));

        if cacheable(outcome) {
            let at = Instant::now();
            let size = size(outcome);
            let retained = (size <= RETAIN_MAX).then(|| outcome.clone());
            if retained.is_some() {
                state.bytes += size;
            }
            state.entries.insert(
                self.id,
                Entry::Done {
                    method,
                    outcome: retained,
                    at,
                },
            );
            state.order.push_back((at, self.id));
            state.shed();
        }
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        if !self.filled {
            self.window.lock().entries.remove(&self.id);
        }
    }
}

/// Сколько итог держит памяти в окне.
fn size(outcome: &Outcome) -> usize {
    match outcome {
        Ok(payload) => payload.len(),
        Err(e) => e.message.len() + e.data.as_ref().map_or(0, |data| data.len()),
    }
}

/// Сбой системы не запоминается: повтор имеет шанс пройти.
fn cacheable(outcome: &Outcome) -> bool {
    match outcome {
        Ok(_) => true,
        Err(e) => !matches!(
            e.code,
            ErrorCode::Internal
                | ErrorCode::Unavailable
                | ErrorCode::Timeout
                | ErrorCode::Cancelled
        ),
    }
}

/// Итог запроса, который исполняет другое соединение той же сессии.
pub(crate) async fn wait(mut rx: watch::Receiver<Option<Outcome>>) -> Outcome {
    match rx.wait_for(Option::is_some).await {
        Ok(outcome) => outcome
            .clone()
            .unwrap_or_else(|| Err(codec::unavailable("original request was interrupted"))),
        Err(_) => Err(codec::unavailable("original request was interrupted")),
    }
}
