//! Хендлеры `sleep.*` (`08 §2.8`): журнал ночей, только хозяину.

use eva_protocol::{
    errors::{self, Aux, ext::*},
    requests::{
        Response,
        Sleep::{phase_runs, runs},
    },
};

use crate::{
    components::Components,
    convert,
    db::{DbError, Queries as _},
    state::{Policy, State},
};

#[eva::endpoint]
#[fastrace::trace]
pub async fn Runs<C: Components>(
    state: &mut State<C>,
    args: runs::Args,
) -> Response<runs::Ok, runs::Err> {
    let State {
        shared,
        caller,
        db_conn,
        ..
    } = state;
    Policy::new(caller).owner()?;
    let runs::Args { limit, after } = args;

    let db = db_conn.acquire(shared).await?;
    let rows = db
        .run(move |c| -> Response<_, runs::Err> {
            if let Some(after) = &after {
                c.sleep()
                    .exists(after)?
                    .true_or(Aux::NotFound(format!("sleep run {after}")))?;
            }
            Ok(c.sleep().runs(after.as_ref(), limit.into_inner())?)
        })
        .await?;

    Ok(runs::Ok {
        found: rows.into_iter().map(convert::sleep::run).collect(),
    })
}

#[eva::endpoint]
#[fastrace::trace]
pub async fn PhaseRuns<C: Components>(
    state: &mut State<C>,
    args: phase_runs::Args,
) -> Response<phase_runs::Ok, phase_runs::Err> {
    let State {
        shared,
        caller,
        db_conn,
        ..
    } = state;
    Policy::new(caller).owner()?;
    let phase_runs::Args { run } = args;

    let db = db_conn.acquire(shared).await?;
    let rows = db
        .run(move |c| -> Response<_, phase_runs::Err> {
            c.sleep()
                .exists(&run)?
                .true_or(errors::sleep::RunNotFound { run: run.clone() }.spec())?;
            Ok(c.sleep().phases(&run)?)
        })
        .await?;

    let phases = rows
        .into_iter()
        .map(convert::sleep::phase)
        .collect::<Result<_, DbError>>()?;
    Ok(phase_runs::Ok { phases })
}
