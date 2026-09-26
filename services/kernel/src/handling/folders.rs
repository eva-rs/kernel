//! Хендлеры `folders.*` (`05 §1.2`, `11 §3`): операции только хозяину.

use eva_protocol::{
    errors::{self, Aux, AuxResult, ext::*},
    requests::{
        Patch, Response,
        folders::{create, delete, list, update},
    },
};

use crate::{
    components::Components,
    convert,
    db::{
        Db as _, Queries as _,
        folders::{FolderRow, UNSORTED},
    },
    state::{Policy, State},
};

fn check_name(name: &str) -> AuxResult<()> {
    let ok = !name.trim().is_empty() && !name.chars().any(char::is_control);
    ok.true_or(Aux::Invalid(
        "folder name must be a non-empty single line".into(),
    ))
}

pub(crate) fn check_compact_at(compact_at: Option<f32>) -> AuxResult<()> {
    compact_at
        .is_none_or(|v| v > 0.0 && v <= 1.0)
        .true_or(Aux::Invalid("compact_at must be in (0, 1]".into()))
}

#[eva::endpoint]
#[fastrace::trace]
pub async fn Create<C: Components>(
    state: &mut State<C>,
    args: create::Args,
) -> Response<create::Ok, create::Err> {
    let State { shared, caller, .. } = state;
    Policy::new(caller).owner()?;
    let create::Args { surface, name } = args;
    check_name(&name)?;

    let row = shared
        .db()
        .write(move |tx| -> Response<FolderRow, create::Err> {
            tx.surfaces()
                .get(&surface)?
                .ok_or_else(|| Aux::NotFound(format!("surface {surface}")))?;
            if tx.folders().by_name(&surface, &name)?.is_some() {
                return Err(errors::folders::NameTaken { surface, name }.spec());
            }
            let id = tx.folders().insert(&surface, &name)?;
            Ok(tx
                .folders()
                .get(id)?
                .ok_or_else(|| Aux::internal("folder vanished right after insert"))?)
        })
        .await?;

    log::info!(folder:% = row.id, surface:% = row.surface.id, caller = caller.label(); "folder created");
    Ok(create::Ok {
        folder: convert::folder::from_db(row),
    })
}

#[eva::endpoint]
#[fastrace::trace]
pub async fn Update<C: Components>(
    state: &mut State<C>,
    args: update::Args,
) -> Response<update::Ok, update::Err> {
    let State { shared, caller, .. } = state;
    Policy::new(caller).owner()?;
    let update::Args { folder, update } = args;

    let row = shared
        .db()
        .write(move |tx| -> Response<FolderRow, update::Err> {
            let mut row = tx
                .folders()
                .get(folder)?
                .ok_or_else(|| errors::folders::NotFound { folder }.spec())?;
            let update::Update {
                name,
                trusted,
                model,
                effort,
                thinking,
                compact_at,
            } = update;

            if let Patch::Change(name) = name
                && name != row.name
            {
                if row.builtin {
                    return Err(errors::folders::Builtin { folder }.spec());
                }
                check_name(&name)?;
                if tx.folders().by_name(&row.surface.id, &name)?.is_some() {
                    return Err(errors::folders::NameTaken {
                        surface: row.surface.id,
                        name,
                    }
                    .spec());
                }
                row.name = name;
            }
            if let Patch::Change(trusted) = trusted {
                row.trusted = trusted;
            }
            if let Patch::Change(model) = model {
                row.model = model;
            }
            if let Patch::Change(effort) = effort {
                row.effort = effort;
            }
            if let Patch::Change(thinking) = thinking {
                row.thinking = thinking;
            }
            if let Patch::Change(compact_at) = compact_at {
                check_compact_at(compact_at)?;
                row.compact_at = compact_at;
            }

            tx.folders().save(&row)?;
            Ok(row)
        })
        .await?;

    log::info!(folder:% = row.id, caller = caller.label(); "folder updated");
    Ok(update::Ok {
        folder: convert::folder::from_db(row),
    })
}

#[eva::endpoint]
#[fastrace::trace]
pub async fn Delete<C: Components>(
    state: &mut State<C>,
    args: delete::Args,
) -> Response<delete::Ok, delete::Err> {
    let State { shared, caller, .. } = state;
    Policy::new(caller).owner()?;
    let delete::Args { folder } = args;

    let moved = shared
        .db()
        .write(move |tx| -> Response<u64, delete::Err> {
            let row = tx
                .folders()
                .get(folder)?
                .ok_or_else(|| errors::folders::NotFound { folder }.spec())?;
            if row.builtin {
                return Err(errors::folders::Builtin { folder }.spec());
            }
            let unsorted = tx
                .folders()
                .by_name(&row.surface.id, UNSORTED)?
                .ok_or_else(|| {
                    Aux::internal(format!("surface {} has no Unsorted", row.surface.id))
                })?;
            let moved = tx.folders().move_chats(folder, unsorted.id)?;
            tx.folders().delete(folder)?;
            Ok(moved)
        })
        .await?;

    log::info!(folder:% = folder, moved, caller = caller.label(); "folder deleted");
    Ok(delete::Ok { moved })
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
    Policy::new(caller).owner()?;
    let list::Args {
        surface,
        limit,
        after,
    } = args;

    let db = db_conn.acquire(shared).await?;
    let rows = db
        .run(move |c| {
            c.folders()
                .list(surface.as_ref(), after, limit.into_inner())
        })
        .await?;

    Ok(list::Ok {
        found: rows.into_iter().map(convert::folder::from_db).collect(),
    })
}
