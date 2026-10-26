//! Сон: журнал ночей и их фаз.

use crate::requests::Response;
use eva::{service, time::Timestamp};

use crate::{
    errors,
    requests::RunsLimit,
    types::{
        RunId,
        sleep::{Active, PhaseRun, Run},
    },
};

#[service(result = Response)]
pub trait Sleep {
    #[args(derive(Default))]
    fn status() -> ok! {
        /// Включено автоматическое расписание; ручной старт доступен и при false.
        enabled: bool,
        active: Option<Active>,
        last_run_at: Option<Timestamp>,
    };
    embed! {
        code!(Err => []);
    }

    #[flags(mutating)]
    #[args(derive(Default))]
    #[errors(errors::sleep::AlreadyRunning, errors::sleep::NoProvider)]
    fn start(
        /// None — sleep.dry_run; переопределение действует только на этот запуск.
        dry_run: Option<bool>,
    ) -> ok! {
        /// Запуск записан в журнал и продолжится асинхронно.
        run: RunId,
    };
    embed! {
        code!(Err => [AlreadyRunning, NoProvider]);
    }

    #[flags(mutating)]
    #[errors(errors::sleep::NotRunning)]
    fn cancel(
        /// Отмена адресована запуску, поэтому не заденет следующую ночь.
        run: RunId,
    ) -> ok! {
        run: RunId,
    };
    embed! {
        code!(Err => [NotRunning]);
    }

    #[args(derive(Default))]
    fn runs(
        #[serde(default)] limit: RunsLimit,
        /// Keyset по `(started, run_id)`.
        after: Option<RunId>,
    ) -> ok! {
        found: Vec<Run>,
    };
    embed! {
        code!(Err => []);
    }

    #[errors(errors::sleep::RunNotFound)]
    fn phase_runs(
        run: RunId,
    ) -> ok! {
        /// В порядке фаз.
        phases: Vec<PhaseRun>,
    };
    embed! {
        code!(Err => [RunNotFound]);
    }
}
