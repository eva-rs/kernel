//! Сон: журнал ночей и их фаз.

use eva::data;

use crate::{
    errors,
    requests::RunsLimit,
    types::{
        RunId,
        sleep::{PhaseRun, Run},
    },
};

pub mod runs {
    use super::*;

    #[data]
    #[derive(Default)]
    pub struct Args {
        #[serde(default)]
        pub limit: RunsLimit,
        /// Keyset по `(started, run_id)`.
        pub after: Option<RunId>,
    }

    #[data]
    pub struct Ok {
        pub found: Vec<Run>,
    }

    #[data(error, display("_"))]
    pub enum Err {}

    code!(Err => []);
}

pub mod phase_runs {
    use super::*;

    #[data]
    pub struct Args {
        pub run: RunId,
    }

    #[data]
    pub struct Ok {
        /// В порядке фаз.
        pub phases: Vec<PhaseRun>,
    }

    #[data(error)]
    pub enum Err {
        #[display("{_0}")]
        RunNotFound(#[from] errors::sleep::RunNotFound),
    }

    code!(Err => [RunNotFound]);
}
