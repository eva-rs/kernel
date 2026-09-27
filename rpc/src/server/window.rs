//! Окно идемпотентности `(session_id, request.id)` мутирующих операций
//! (`06 §5`): повтор отдаёт сохранённый ответ, `hello.resume` переносит окно.

use std::{
    collections::{HashMap, VecDeque},
    sync::{
        Arc, Mutex, MutexGuard, PoisonError, Weak,
        atomic::{AtomicUsize, Ordering},
    },
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

/// Байт хранимых итогов во всех окнах сервера.
const TOTAL_BYTES: usize = 64 << 20;

/// Окна всех сессий сервера.
pub(crate) struct Windows(Arc<Shared>);

struct Shared {
    ttl: Duration,
    /// Потолок `retained`.
    budget: usize,
    /// Сессия → окно; у модуля в карте одно окно: `resume` ведёт лишь к
    /// сессии прямо перед новой.
    map: Mutex<HashMap<SessionId, Arc<Window>>>,
    /// Байт хранимых итогов во всех окнах карты.
    retained: Arc<AtomicUsize>,
}

impl Windows {
    pub fn new(ttl: Duration) -> Self {
        Self::with_budget(ttl, TOTAL_BYTES)
    }

    fn with_budget(ttl: Duration, budget: usize) -> Self {
        Self(Arc::new(Shared {
            ttl,
            budget,
            map: Mutex::default(),
            retained: Arc::default(),
        }))
    }

    /// Окно новой сессии; `resume` того же модуля продолжает его окно.
    /// Прочие окна модуля отвязываются: иначе переподключения без `resume`
    /// копили бы окна без предела.
    pub fn open(
        &self,
        session: &SessionId,
        module: &ModuleId,
        resume: Option<&SessionId>,
    ) -> Lease {
        let shared = &self.0;
        let mut map = shared.lock();
        let now = Instant::now();

        let continued = resume
            .and_then(|old| map.get(old))
            .filter(|window| &window.module == module)
            .cloned();
        let resumed = continued.is_some();
        let window = continued.unwrap_or_else(|| Arc::new(Window::new(module.clone(), shared)));

        let mut gone = Vec::new();
        map.retain(|key, other| {
            let keep = if &other.module == module {
                Arc::ptr_eq(other, &window) && resume == Some(key)
            } else {
                !other.expired(now)
            };
            if !keep && !Arc::ptr_eq(other, &window) {
                gone.push(other.clone());
            }
            keep
        });
        map.insert(session.clone(), window.clone());
        window.lock().live += 1;
        drop(map);

        for window in gone {
            window.detach();
        }
        Lease { window, resumed }
    }
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, HashMap<SessionId, Arc<Window>>> {
        self.map.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Держит итоги всех окон в `budget`: худеет самое толстое окно, чтобы
    /// шумный модуль вытеснял свои итоги, а не чужие.
    fn shed(&self) {
        while self.retained.load(Ordering::Relaxed) > self.budget {
            let fattest = {
                let map = self.lock();
                map.values()
                    .max_by_key(|window| window.lock().bytes)
                    .cloned()
            };
            let Some(window) = fattest else {
                return;
            };
            let excess = self
                .retained
                .load(Ordering::Relaxed)
                .saturating_sub(self.budget);
            let mut state = window.lock();
            let keep = state.bytes.saturating_sub(excess);
            if !state.forget_outcomes(keep) {
                return;
            }
        }
    }
}

/// Окно, пока жива сессия; после её конца окно помнится ещё `ttl`.
pub(crate) struct Lease {
    window: Arc<Window>,
    resumed: bool,
}

impl Lease {
    /// Окно прошлой сессии продолжено: это `welcome.resumed`.
    pub fn resumed(&self) -> bool {
        self.resumed
    }
}

impl std::ops::Deref for Lease {
    type Target = Arc<Window>;

    fn deref(&self) -> &Arc<Window> {
        &self.window
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        let mut state = self.window.lock();
        state.live -= 1;
        state.touched = Instant::now();
    }
}

pub(crate) struct Window {
    module: ModuleId,
    ttl: Duration,
    shared: Weak<Shared>,
    state: Mutex<State>,
}

struct State {
    entries: HashMap<u64, Entry>,
    /// `(когда завершён, id)` по возрастанию: истёкшие снимаются с головы.
    order: VecDeque<(Instant, u64)>,
    /// Байт в хранимых итогах; каждое изменение зеркалится в `retained`.
    bytes: usize,
    retained: Arc<AtomicUsize>,
    /// Последний запрос или конец сессии.
    touched: Instant,
    /// Сессий, которые сейчас пишут в окно.
    live: usize,
    /// Окно вне карты: `resume` к нему не ведёт, итоги не хранятся, но
    /// записи держат повтор от второго исполнения.
    detached: bool,
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
    fn new(module: ModuleId, shared: &Arc<Shared>) -> Self {
        Self {
            module,
            ttl: shared.ttl,
            shared: Arc::downgrade(shared),
            state: Mutex::new(State {
                entries: HashMap::new(),
                order: VecDeque::new(),
                bytes: 0,
                retained: shared.retained.clone(),
                touched: Instant::now(),
                live: 0,
                detached: false,
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

    fn detach(&self) {
        let mut state = self.lock();
        state.detached = true;
        state.forget_outcomes(0);
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
            self.release(size(&outcome));
        }
    }

    fn retain(&mut self, bytes: usize) {
        self.bytes += bytes;
        self.retained.fetch_add(bytes, Ordering::Relaxed);
    }

    fn release(&mut self, bytes: usize) {
        let bytes = bytes.min(self.bytes);
        self.bytes -= bytes;
        self.retained.fetch_sub(bytes, Ordering::Relaxed);
    }

    /// Держит окно в `WINDOW_ENTRIES` и `WINDOW_BYTES`: у старейших сперва
    /// забываются итоги, записи — лишь сверх потолка числа.
    fn shed(&mut self) {
        while self.order.len() > WINDOW_ENTRIES {
            self.pop_oldest();
        }
        self.forget_outcomes(WINDOW_BYTES);
    }

    /// Забывает итоги старейших записей, пока их байт больше `keep`;
    /// `false` — забывать было нечего.
    fn forget_outcomes(&mut self, keep: usize) -> bool {
        let mut forgot = false;
        let Self {
            entries,
            order,
            bytes,
            retained,
            ..
        } = self;
        for &(at, id) in order.iter() {
            if *bytes <= keep {
                break;
            }
            if let Some(Entry::Done {
                outcome, at: done, ..
            }) = entries.get_mut(&id)
                && *done == at
                && let Some(dropped) = outcome.take()
            {
                let size = size(&dropped).min(*bytes);
                *bytes -= size;
                retained.fetch_sub(size, Ordering::Relaxed);
                forgot = true;
            }
        }
        forgot
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
        {
            let mut state = self.window.lock();
            let Some(Entry::Running { method, done }) = state.entries.remove(&self.id) else {
                return;
            };
            done.send_replace(Some(outcome.clone()));
            if !cacheable(outcome) {
                return;
            }

            let at = Instant::now();
            let size = size(outcome);
            let retained = (size <= RETAIN_MAX && !state.detached).then(|| outcome.clone());
            if retained.is_some() {
                state.retain(size);
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
        if let Some(shared) = self.window.shared.upgrade() {
            shared.shed();
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

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use eva_protocol::types::Bytes;

    use super::*;

    fn sid(n: usize) -> SessionId {
        SessionId(format!("s{n}"))
    }

    fn payload(bytes: usize) -> Outcome {
        Ok(Bytes(vec![7; bytes]))
    }

    /// Исполнить `id` в окне и сохранить итог.
    fn run(lease: &Lease, id: u64, outcome: &Outcome) {
        match lease.begin(id, "surfaces.create") {
            Begin::Run(slot) => slot.fill(outcome),
            _ => panic!("request {id} was not fresh"),
        }
    }

    fn windows_of(windows: &Windows, module: &str) -> usize {
        let map = windows.0.lock();
        map.values()
            .filter(|window| window.module.as_str() == module)
            .map(Arc::as_ptr)
            .collect::<HashSet<_>>()
            .len()
    }

    fn retained(windows: &Windows) -> usize {
        windows.0.retained.load(Ordering::Relaxed)
    }

    #[tokio::test]
    async fn reconnects_without_resume_do_not_pile_up_windows() {
        let windows = Windows::new(Duration::from_secs(600));
        let module = ModuleId::from("m");
        for n in 0..10 {
            let lease = windows.open(&sid(n), &module, None);
            for id in 0..50 {
                run(&lease, id, &payload(RETAIN_MAX));
            }
        }
        assert_eq!(windows_of(&windows, "m"), 1);
        assert_eq!(retained(&windows), 50 * RETAIN_MAX);
    }

    #[tokio::test]
    async fn resume_keeps_the_window_and_fresh_start_drops_it() {
        let windows = Windows::new(Duration::from_secs(600));
        let module = ModuleId::from("m");
        let first = windows.open(&sid(1), &module, None);
        assert!(!first.resumed());
        run(&first, 7, &payload(10));
        drop(first);

        // Повтор `resume` той же сессии: прошлый `welcome` мог не дойти.
        for n in [2, 3] {
            let resumed = windows.open(&sid(n), &module, Some(&sid(1)));
            assert!(resumed.resumed());
            assert!(matches!(
                resumed.begin(7, "surfaces.create"),
                Begin::Cached(Ok(_))
            ));
        }
        assert!(windows.0.lock().len() <= 2);

        let fresh = windows.open(&sid(4), &module, None);
        assert!(!fresh.resumed());
        assert!(matches!(fresh.begin(7, "surfaces.create"), Begin::Run(_)));
        assert_eq!(windows_of(&windows, "m"), 1);
        assert_eq!(retained(&windows), 0);
    }

    /// Окна живут в памяти: после рестарта ядра `resume` ведёт в никуда.
    #[tokio::test]
    async fn resume_of_an_unknown_session_is_not_resumed() {
        let windows = Windows::new(Duration::from_secs(600));
        let lease = windows.open(&sid(2), &ModuleId::from("m"), Some(&sid(1)));
        assert!(!lease.resumed());
        assert!(matches!(lease.begin(7, "surfaces.create"), Begin::Run(_)));
    }

    #[tokio::test]
    async fn detached_live_window_still_blocks_a_second_run() {
        let windows = Windows::new(Duration::from_secs(600));
        let module = ModuleId::from("m");
        let old = windows.open(&sid(1), &module, None);
        let _new = windows.open(&sid(2), &module, None);

        run(&old, 1, &payload(10));
        assert!(matches!(
            old.begin(1, "surfaces.create"),
            Begin::Cached(Err(RpcError {
                code: ErrorCode::Conflict,
                ..
            }))
        ));
        assert_eq!(retained(&windows), 0);
    }

    #[tokio::test]
    async fn total_budget_thins_the_fattest_window() {
        let budget = 1 << 20;
        let windows = Windows::with_budget(Duration::from_secs(600), budget);
        let quiet = windows.open(&sid(1), &ModuleId::from("quiet"), None);
        let noisy = windows.open(&sid(2), &ModuleId::from("noisy"), None);

        for id in 0..4 {
            run(&quiet, id, &payload(RETAIN_MAX));
        }
        for id in 0..40 {
            run(&noisy, id, &payload(RETAIN_MAX));
        }

        assert!(retained(&windows) <= budget, "{}", retained(&windows));
        for id in 0..4 {
            assert!(matches!(
                quiet.begin(id, "surfaces.create"),
                Begin::Cached(Ok(_))
            ));
        }
        assert!(matches!(
            noisy.begin(39, "surfaces.create"),
            Begin::Cached(Ok(_))
        ));
        assert!(matches!(
            noisy.begin(0, "surfaces.create"),
            Begin::Cached(Err(RpcError {
                code: ErrorCode::Conflict,
                ..
            }))
        ));
    }
}
