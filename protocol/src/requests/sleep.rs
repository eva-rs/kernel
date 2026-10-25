//! Сон: журнал ночей и их фаз.

use crate::requests::Response;
use eva::service;

use crate::{
    errors,
    requests::RunsLimit,
    types::{
        RunId,
        sleep::{PhaseRun, Run},
    },
};

#[service(export, result = Response)]
pub trait Sleep {
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
