//! Крон: только чтение; мутации — тулы модели.

use eva::data;

use crate::{
    requests::RunsLimit,
    types::{
        CronJobId, RunId,
        cron::{Job, Run},
    },
};

pub mod list {
    use super::*;

    #[data]
    #[derive(Default)]
    pub struct Args {}

    #[data]
    pub struct Ok {
        pub jobs: Vec<Job>,
    }

    #[data(error, display("_"))]
    pub enum Err {}

    code!(Err => []);
}

pub mod runs {
    use super::*;

    #[data]
    #[derive(Default)]
    pub struct Args {
        pub job: Option<CronJobId>,
        #[serde(default)]
        pub limit: RunsLimit,
        /// Keyset по `(started, run_id)`.
        pub after: Option<RunId>,
    }

    #[data]
    pub struct Ok {
        /// Новейшие первыми.
        pub found: Vec<Run>,
    }

    #[data(error, display("_"))]
    pub enum Err {}

    code!(Err => []);
}
