//! Один захват на ручную и плановую ночь, с ожиданием завершения при shutdown.

use std::sync::{Arc, Mutex};

use eva::time::Timestamp;
use eva_protocol::{
    errors::{
        Aux,
        sleep::{AlreadyRunning, NotRunning},
    },
    types::{
        RunId,
        sleep::{Active, Phase},
    },
};
use tokio::sync::{Mutex as AsyncMutex, OnceCell, OwnedMutexGuard};
use tokio_util::{
    sync::CancellationToken,
    task::{TaskTracker, task_tracker::TaskTrackerToken},
};

#[derive(Default)]
struct Inner {
    admission: Arc<AsyncMutex<()>>,
    active: Mutex<Option<(Active, CancellationToken)>>,
    stopping: CancellationToken,
    tasks: TaskTracker,
    recovered: OnceCell<()>,
}

#[derive(Clone, Default)]
pub struct Control(Arc<Inner>);

pub(super) enum ClaimError {
    Busy(AlreadyRunning),
    Stopping(Aux),
}

pub(super) struct Claim {
    control: Control,
    pub run: RunId,
    pub started: Timestamp,
    pub dry_run: bool,
    pub cancel: CancellationToken,
    _tracked: TaskTrackerToken,
}

/// Подготовка старта не видна как активная ночь до commit журнала.
pub(super) struct Admission {
    control: Control,
    _gate: OwnedMutexGuard<()>,
    tracked: TaskTrackerToken,
}

impl Admission {
    pub(super) fn commit(self, run: RunId, started: Timestamp, dry_run: bool) -> Claim {
        let control = self.control;
        let cancel = control.0.stopping.child_token();
        *control.0.active.lock().unwrap_or_else(|e| e.into_inner()) = Some((
            Active {
                run: run.clone(),
                started,
                phase: None,
                dry_run,
                cancelling: cancel.is_cancelled(),
            },
            cancel.clone(),
        ));
        Claim {
            control,
            run,
            started,
            dry_run,
            cancel,
            _tracked: self.tracked,
        }
    }
}

impl Drop for Claim {
    fn drop(&mut self) {
        let mut active = self
            .control
            .0
            .active
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if active.as_ref().is_some_and(|(run, _)| run.run == self.run) {
            *active = None;
        }
    }
}

impl Control {
    pub fn current(&self) -> Option<Active> {
        self.0
            .active
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|(active, cancel)| {
                let mut active = active.clone();
                active.cancelling = cancel.is_cancelled();
                active
            })
    }

    pub fn cancel(&self, run: &RunId) -> Result<(), NotRunning> {
        let active = self.0.active.lock().unwrap_or_else(|e| e.into_inner());
        match active.as_ref() {
            Some((active, cancel)) if &active.run == run => {
                cancel.cancel();
                Ok(())
            }
            _ => Err(NotRunning { run: run.clone() }),
        }
    }

    pub(super) async fn admit(&self) -> Result<Admission, ClaimError> {
        let gate = self.0.admission.clone().lock_owned().await;
        let active = self.0.active.lock().unwrap_or_else(|e| e.into_inner());
        if self.0.stopping.is_cancelled() {
            return Err(ClaimError::Stopping(Aux::Unavailable(String::from(
                "sleep is shutting down",
            ))));
        }
        if let Some((active, _)) = active.as_ref() {
            return Err(ClaimError::Busy(AlreadyRunning {
                run: active.run.clone(),
            }));
        }
        Ok(Admission {
            control: self.clone(),
            _gate: gate,
            tracked: self.0.tasks.token(),
        })
    }

    pub(super) fn phase(&self, run: &RunId, phase: Option<Phase>) {
        let mut active = self.0.active.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((active, _)) = active.as_mut()
            && &active.run == run
        {
            active.phase = phase;
        }
    }

    pub(super) fn recovered(&self) -> &OnceCell<()> {
        &self.0.recovered
    }

    pub async fn shutdown(&self) {
        {
            let _active = self.0.active.lock().unwrap_or_else(|e| e.into_inner());
            self.0.stopping.cancel();
            self.0.tasks.close();
        }
        self.0.tasks.wait().await;
    }
}
