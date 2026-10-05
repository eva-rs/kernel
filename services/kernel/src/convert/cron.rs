//! Джоба и запуск крона (`08 §1`) → тип протокола.

use eva_protocol::types::cron::{Job, Run};

use crate::db::cron::{JobRow, RunRow};

pub fn job(row: JobRow) -> Job {
    let JobRow {
        id,
        name,
        task,
        interval,
        next_fire_at,
        expires_at,
        model,
        effort,
        tools,
        chat,
        runs_total,
        spent_total,
        created_at,
    } = row;

    Job {
        id,
        name,
        task,
        interval,
        next_fire_at,
        expires_at,
        model,
        effort,
        tools,
        chat,
        runs_total,
        spent_total,
        created_at,
    }
}

pub fn run(row: RunRow) -> Run {
    let RunRow {
        run,
        job,
        started,
        finished,
        ok,
        output,
        cost,
        due,
    } = row;

    Run {
        run_id: run,
        job,
        started,
        finished,
        ok,
        output,
        cost,
        due,
    }
}
