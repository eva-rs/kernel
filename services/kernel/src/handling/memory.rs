//! Хендлеры `memory.*` (`03 §7`, `§9`, `11 §3`): только хозяину; `view`
//! сужает выдачу видимостью тура (`03 §2`), недоступная строка неотличима
//! от отсутствующей.

use eva_protocol::{
    errors::{self, Aux, ext::*},
    requests::{
        Response,
        memory::{get, history, list, restore},
    },
    types::{
        memory::{Kind, Visibility},
        turn::TurnView,
    },
    wire::events::{Event, MemoryChanged, MemoryOp},
};

use crate::{
    components::Components,
    convert,
    db::{
        Db as _, Queries as _,
        memory::{ArchiveRow, ListFilter, MemoryRow},
    },
    state::{Policy, State},
};

/// `03 §2`: `None` — взгляд хозяина, видно всё.
fn visible(visibility: Visibility, view: Option<&TurnView>) -> bool {
    view.is_none_or(|v| match visibility {
        Visibility::Public => true,
        Visibility::Trusted => v.trusted_chat,
        Visibility::Private => v.master_turn && v.trusted_chat,
    })
}

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
    Policy::new(caller).memory().read()?;
    let list::Args {
        kind,
        visibility,
        since,
        limit,
        after,
    } = args;
    let filter = ListFilter {
        kind,
        visibility,
        since,
    };

    let db = db_conn.acquire(shared).await?;
    let rows = db
        .run(move |c| c.memory().list(&filter, after, limit.into_inner()))
        .await?;

    Ok(list::Ok {
        found: rows
            .into_iter()
            .map(|row| convert::memory::record(row).0)
            .collect(),
    })
}

#[eva::endpoint]
#[fastrace::trace]
pub async fn Get<C: Components>(
    state: &mut State<C>,
    args: get::Args,
) -> Response<get::Ok, get::Err> {
    let State {
        shared,
        caller,
        db_conn,
        ..
    } = state;
    Policy::new(caller).memory().read()?;
    let get::Args { id, view } = args;

    let db = db_conn.acquire(shared).await?;
    let row = db
        .run(move |c| c.memory().get(id))
        .await?
        .filter(|row| visible(row.visibility, view.as_ref()))
        .ok_or_else(|| errors::memory::NotFound { id }.spec())?;

    let (record, archive) = convert::memory::record(row);
    Ok(get::Ok { record, archive })
}

#[eva::endpoint]
#[fastrace::trace]
pub async fn History<C: Components>(
    state: &mut State<C>,
    args: history::Args,
) -> Response<history::Ok, history::Err> {
    let State {
        shared,
        caller,
        db_conn,
        ..
    } = state;
    Policy::new(caller).memory().read()?;
    let history::Args { id, view } = args;

    let db = db_conn.acquire(shared).await?;
    let chain = db.run(move |c| c.memory().chain(id)).await?;
    // Запрошенная версия решает, есть ли ответ; соседние версии фильтр лишь прореживает.
    chain
        .iter()
        .any(|row| row.id == id && visible(row.visibility, view.as_ref()))
        .true_or(errors::memory::NotFound { id }.spec())?;

    Ok(history::Ok {
        versions: chain
            .into_iter()
            .filter(|row| visible(row.visibility, view.as_ref()))
            .map(convert::memory::version)
            .collect(),
    })
}

#[eva::endpoint]
#[fastrace::trace]
pub async fn Restore<C: Components>(
    state: &mut State<C>,
    args: restore::Args,
) -> Response<restore::Ok, restore::Err> {
    let State {
        shared,
        world,
        caller,
        ..
    } = state;
    Policy::new(caller).memory().read()?;
    let restore::Args { id } = args;

    let now = world.now();
    let row = shared
        .db()
        .write(move |tx| -> Response<MemoryRow, restore::Err> {
            let chain = tx.memory().chain(id)?;
            let row = chain
                .iter()
                .find(|row| row.id == id)
                .cloned()
                .ok_or_else(|| errors::memory::NotFound { id }.spec())?;
            let forgotten =
                row.valid_to.is_some() && row.archive.as_ref().is_some_and(|a| a.is_open());
            forgotten.true_or(errors::memory::NotArchived { id }.spec())?;
            if let Some(current) = chain.iter().find(|r| r.id != id && r.valid_to.is_none()) {
                return Err(errors::memory::Superseded {
                    id,
                    current: current.id,
                }
                .spec());
            }
            if let (Kind::Procedure, Some(name)) = (row.kind, row.name.as_deref())
                && let Some(holder) = tx.memory().procedure_named(name, id)?
            {
                return Err(Aux::Conflict(format!(
                    "procedure name {name:?} is taken by {holder}; forget or rename it first"
                ))
                .into());
            }

            tx.memory().restore(id, now)?;
            Ok(MemoryRow {
                valid_to: None,
                archive: row.archive.map(|a| ArchiveRow {
                    restored_at: Some(now),
                    ..a
                }),
                ..row
            })
        })
        .await?;

    shared.bus().publish(
        Event::MemoryChanged(MemoryChanged {
            ids: vec![row.id],
            op: MemoryOp::Restore,
        }),
        None,
    );
    log::info!(memory:% = row.id, kind:? = row.kind, caller = caller.label(); "memory restored");

    Ok(restore::Ok {
        record: convert::memory::record(row).0,
    })
}
