//! Хендлеры `hands_grants.*` (`07 §3.1`): постоянные разрешения рук,
//! только хозяину. Выдаёт их резолв `permission` с `allow_always`.

use eva_protocol::{
    errors::{self, ext::*},
    requests::{
        Response,
        hands_grants::{list, revoke},
    },
};

use crate::{
    components::Components,
    convert,
    db::{Db as _, Queries as _, hands_grants::GrantFilter},
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
    let list::Args {
        endpoint,
        surface,
        chat,
    } = args;
    let filter = GrantFilter {
        endpoint,
        surface,
        chat,
    };

    let db = db_conn.acquire(shared).await?;
    let rows = db.run(move |c| c.hands_grants().list(&filter)).await?;

    Ok(list::Ok {
        grants: rows
            .into_iter()
            .map(convert::hands_grant::from_db)
            .collect(),
    })
}

#[eva::endpoint]
#[fastrace::trace]
pub async fn Revoke<C: Components>(
    state: &mut State<C>,
    args: revoke::Args,
) -> Response<revoke::Ok, revoke::Err> {
    let State { shared, caller, .. } = state;
    Policy::new(caller).owner()?;
    let revoke::Args { id } = args;

    let deleted = shared
        .db()
        .write(move |tx| tx.hands_grants().delete(id))
        .await
        .aux_err()?;
    if !deleted {
        return Err(errors::hands_grants::NotFound { id }.spec());
    }

    log::info!(grant:% = id, caller = caller.label(); "hands grant revoked");
    Ok(revoke::Ok {})
}
