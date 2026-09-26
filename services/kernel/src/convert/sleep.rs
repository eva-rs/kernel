//! Ночь сна и фаза (`08 §2`) → тип протокола.

use eva_protocol::types::sleep::{PhaseOp, PhaseRun, Run};

use crate::db::{
    DbError,
    sleep::{PhaseRow, RunRow},
};

pub fn run(row: RunRow) -> Run {
    let RunRow {
        run,
        started,
        finished,
        ok,
        dry_run,
        interrupted,
        report,
    } = row;

    Run {
        run_id: run,
        started,
        finished,
        ok,
        dry_run,
        interrupted,
        report,
    }
}

/// `ops_json` не разобрался — строка испорчена, не пустой список.
pub fn phase(row: PhaseRow) -> Result<PhaseRun, DbError> {
    let PhaseRow {
        run,
        phase,
        started,
        finished,
        model,
        ops_json,
        applied,
        discarded,
        report,
    } = row;
    let ops: Vec<PhaseOp> = serde_json::from_str(&ops_json).map_err(|e| {
        DbError::corrupt(format_args!(
            "sleep_phase_runs.ops_json of {run}/{phase:?}: {e}"
        ))
    })?;

    Ok(PhaseRun {
        run,
        phase,
        started,
        finished,
        model,
        ops,
        applied,
        discarded,
        report,
    })
}
