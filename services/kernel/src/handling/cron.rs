//! Хендлеры `cron.*` (`08 §1.7`): только чтение и только хозяину;
//! мутации джоб — тулы модели.

use eva_protocol::{
    errors::{Aux, ext::*},
    requests::{
        Response,
        cron::{list, runs},
    },
};

use crate::{
    components::Components,
    convert,
    db::Queries as _,
    state::{Policy, State},
};

#[eva::endpoint]
#[fastrace::trace]
pub async fn List<C: Components>(
    state: &mut State<C>,
    args: list::Args,
) -> Response<list::Ok, list::Err> {
    let State {
        shared,
        caller,
        db_conn,
        ..
    } = state;
    Policy::new(caller).owner()?;
    let list::Args {} = args;

    let db = db_conn.acquire(shared).await?;
    let rows = db.run(|c| c.cron().jobs()).await?;

    Ok(list::Ok {
        jobs: rows.into_iter().map(convert::cron::job).collect(),
    })
}

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
    let runs::Args { job, limit, after } = args;

    let db = db_conn.acquire(shared).await?;
    let rows = db
        .run(move |c| -> Response<_, runs::Err> {
            if let Some(after) = &after {
                c.cron()
                    .run_exists(after)?
                    .true_or(Aux::NotFound(format!("cron run {after}")))?;
            }
            Ok(c.cron()
                .runs(job.as_ref(), after.as_ref(), limit.into_inner())?)
        })
        .await?;

    Ok(runs::Ok {
        found: rows.into_iter().map(convert::cron::run).collect(),
    })
}
