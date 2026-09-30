//! Петли дозвона сервисов (`06 §2.1`): одна на запись реестра. Попытка —
//! connect, Noise, `hello`, `welcome`; провал — `modules.dial_failed` и
//! причина в `modules.list`, повтор по паузе с джиттером. Петля не сдаётся:
//! чужой ключ и версию лечат на стороне модуля без рестарта ядра.

use std::{collections::BTreeMap, time::Duration};

use eva::rand::Rng as _;
use tokio::{sync::mpsc, task::JoinSet, time::Instant};
use tokio_util::{sync::CancellationToken, task::TaskTracker};

use eva_protocol::types::ModuleId;
use eva_rpc::Server;

use crate::{
    components::Components,
    db::stats::Metric,
    kernel::Kernel,
    shared::Shared,
    subsystems::modules::{
        DRAIN, KernelAuth,
        live::Source,
        on_session,
        services::{Control, Loop},
    },
};

const BACKOFF_MIN: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(60);

/// Попытка дозвона: id записи и номер отзыва, с которым запущена петля.
#[derive(Debug, Clone)]
pub struct Attempt {
    pub module: ModuleId,
    pub epoch: Option<u64>,
    pub source: Source,
}

/// Пауза перед повтором: равномерно в `[d/2, d]`, затем `d ← min(2d, 60s)`.
#[derive(Debug)]
pub struct Backoff {
    next: Duration,
}

impl Default for Backoff {
    fn default() -> Self {
        Self { next: BACKOFF_MIN }
    }
}

impl Backoff {
    pub fn reset(&mut self) {
        self.next = BACKOFF_MIN;
    }

    pub fn pause(&mut self) -> Duration {
        let d = self.next;
        self.next = (d * 2).min(BACKOFF_MAX);
        eva::rand::rng().random_range(d / 2..=d)
    }
}

/// Паузы петли: у провалов попытки и у коротких сессий своё `d`. Простой
/// модуля до `welcome` не удлиняет паузу после его первой сессии, а
/// модуль, падающий сразу после `welcome`, всё равно получает растущую.
#[derive(Debug, Default)]
pub struct Pacing {
    failures: Backoff,
    sessions: Backoff,
}

impl Pacing {
    /// Пауза после провала попытки.
    pub fn failed(&mut self) -> Duration {
        self.failures.pause()
    }

    /// Попытка дошла до `welcome`: прежние провалы — прошлый простой модуля.
    pub fn welcomed(&mut self) {
        self.failures.reset();
    }

    /// Пауза после сессии, прожившей `lived`: дольше `ping_timeout` — модуль
    /// здоров, и счёт коротких сессий начинается заново.
    pub fn ended(&mut self, lived: Duration, ping_timeout: Duration) -> Duration {
        if lived > ping_timeout {
            self.sessions.reset();
        }
        self.sessions.pause()
    }
}

pub(super) type KernelServer<C> = Server<Kernel<C>, KernelAuth<C>>;

/// Петли дозвона компонента `modules`; команды — от `Services`.
pub(super) struct Dialer<C: Components> {
    pub server: KernelServer<C>,
    pub shared: Shared<C>,
    pub sessions: TaskTracker,
    /// Остановка компонента: гасит все петли.
    pub stop: CancellationToken,
}

impl<C: Components> Dialer<C> {
    /// Исполняет команды до `stop`, затем ждёт петли: попытка в полёте
    /// кончается не позже `handshake_timeout`, прерывать её нельзя —
    /// `authenticate` мог завести сессию.
    pub(super) async fn run(self, mut control: mpsc::UnboundedReceiver<Control>) {
        let mut loops: BTreeMap<ModuleId, CancellationToken> = BTreeMap::new();
        let mut tasks = JoinSet::new();
        loop {
            tokio::select! {
                () = self.stop.cancelled() => break,
                command = control.recv() => match command {
                    Some(Control::Start(run)) => {
                        let stop = self.stop.child_token();
                        if let Some(old) = loops.insert(run.module.clone(), stop.clone()) {
                            old.cancel();
                        }
                        tasks.spawn(dial_loop(
                            self.server.clone(),
                            self.shared.clone(),
                            self.sessions.clone(),
                            run,
                            stop,
                        ));
                    }
                    Some(Control::Stop(module)) => {
                        if let Some(stop) = loops.remove(&module) {
                            stop.cancel();
                        }
                    }
                    None => break,
                },
                Some(_) = tasks.join_next(), if !tasks.is_empty() => {}
            }
        }

        self.stop.cancel();
        let limit = *self.shared.cfg().modules.transport.handshake_timeout + DRAIN;
        let joined =
            tokio::time::timeout(limit, async { while tasks.join_next().await.is_some() {} }).await;
        if joined.is_err() {
            log::warn!(dials = tasks.len(); "service dials did not stop in time");
            tasks.abort_all();
        }
    }
}

async fn dial_loop<C: Components>(
    server: KernelServer<C>,
    shared: Shared<C>,
    sessions: TaskTracker,
    run: Loop,
    stop: CancellationToken,
) {
    let Loop {
        module,
        service,
        epoch,
    } = run;
    let attempt = Attempt {
        module: module.clone(),
        epoch,
        source: service.source,
    };
    // `modules.remove` отзывает имя раньше, чем команда дойдёт до петли.
    let removed = || epoch.is_some_and(|epoch| shared.live().epoch(&module) != epoch);
    let ping_timeout = *shared.cfg().modules.transport.ping_timeout;
    let mut pacing = Pacing::default();
    // `warn` — первый провал после старта петли или после сессии, дальше `debug`.
    let mut loud = true;
    while !stop.is_cancelled() && !removed() {
        let pause = match server.dial(&service.address, &service.psk, &attempt).await {
            Ok(accepted) => {
                pacing.welcomed();
                shared.services().welcomed(&module);
                let peer = accepted.peer.clone();
                let started = Instant::now();
                log::info!(module:% = module, address:% = service.address; "service module dialed");
                on_session(&shared, &sessions, accepted);
                tokio::select! {
                    _ = peer.closed() => {}
                    () = stop.cancelled() => return,
                }
                loud = true;
                pacing.ended(started.elapsed(), ping_timeout)
            }
            // Запись удалена или ядро встаёт: провал попытки ничего не говорит о модуле.
            Err(_) if stop.is_cancelled() || removed() => return,
            Err(e) => {
                let error = e.to_string();
                if loud {
                    log::warn!(module:% = module, address:% = service.address, error:% = error; "service module dial failed");
                } else {
                    log::debug!(module:% = module, address:% = service.address, error:% = error; "service module dial failed");
                }
                loud = false;
                shared.services().failed(&module, error);
                shared
                    .bump(Metric::ModulesDialFailed, module.as_str())
                    .await;
                pacing.failed()
            }
        };

        tokio::select! {
            () = tokio::time::sleep(pause) => {}
            () = stop.cancelled() => return,
        }
    }
}
