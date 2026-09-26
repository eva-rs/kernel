//! Хендлеры `surfaces.*` (`05 §1.1`, `06 §3`, `11 §4`) — эталон домена:
//! `Policy` без объекта первой строкой, объектная — сразу после загрузки,
//! чтение через `db_conn`, запись одной транзакцией писателя.

use eva_protocol::{
    errors::{self, Aux, AuxResult, ext::*},
    requests::{
        Patch, Response,
        surfaces::{create, get, list, update},
    },
    types::SurfaceId,
};

use crate::{
    components::Components,
    convert,
    db::{
        Db as _, Queries as _,
        surfaces::{LOCAL, SurfaceRow},
    },
    markers,
    state::{Policy, State},
};

/// Slug `[a-z][a-z0-9_]{0,31}` — тот же, что CHECK колонки `surfaces.id`.
fn check_id(id: &SurfaceId) -> AuxResult<()> {
    let mut chars = id.as_str().chars();
    let ok = matches!(chars.next(), Some('a'..='z'))
        && id.as_str().len() <= 32
        && chars.all(|c| matches!(c, 'a'..='z' | '0'..='9' | '_'));
    ok.true_or(Aux::Invalid(format!(
        "surface id {id:?} must match [a-z][a-z0-9_]{{0,31}}"
    )))
}

fn check_name(name: &str) -> AuxResult<()> {
    let ok = !name.trim().is_empty() && !name.chars().any(char::is_control);
    ok.true_or(Aux::Invalid(
        "surface name must be a non-empty single line".into(),
    ))
}

/// Строка `rules` с маркером реестра I13 → `BadRules` (`06 §3`).
fn check_rules<E: From<errors::surfaces::BadRules>>(rules: Option<&str>) -> Response<(), E> {
    match rules.map(markers::check_rules) {
        Some(Err(line)) => Err(errors::surfaces::BadRules { line }.spec()),
        _ => Ok(()),
    }
}

#[eva::endpoint]
#[fastrace::trace]
pub async fn Create<C: Components>(
    state: &mut State<C>,
    args: create::Args,
) -> Response<create::Ok, create::Err> {
    let State {
        shared,
        world,
        caller,
        db_conn,
        ..
    } = state;
    let module = Policy::new(caller).surfaces().create(&args)?.clone();
    let create::Args {
        id,
        name,
        rules,
        ask,
        builtin,
    } = args;

    check_id(&id)?;
    let local = id.as_str() == LOCAL;
    (builtin == local).true_or(Aux::Invalid(
        "builtin: true registers surface local and only it".into(),
    ))?;

    if builtin {
        (name.is_none() && rules.is_none()).true_or(Aux::Invalid(
            "builtin registration takes no name or rules; change them with surfaces.update".into(),
        ))?;
        let db = db_conn.acquire(shared).await?;
        let row = db
            .run(move |c| c.surfaces().get(&id))
            .await?
            .ok_or_else(|| Aux::internal("builtin surface local is not seeded"))?;
        shared.serving().serve(&row.id, &module, ask);
        log::info!(surface:% = row.id, module:% = module, ask; "module serves builtin surface");
        return Ok(create::Ok {
            surface: convert::surface::from_db(row),
        });
    }

    let name = name.ok_or_else(|| Aux::Invalid("name is required".into()))?;
    check_name(&name)?;
    check_rules(rules.as_deref())?;

    let now = world.now();
    let persons = shared.cfg().chats.persons.clone();
    let owner = module.clone();
    let row = shared
        .db()
        .write(move |tx| -> Response<SurfaceRow, create::Err> {
            match tx.surfaces().get(&id)? {
                // Повторная регистрация своего surface: текст канала — модуля.
                Some(row) if row.module.as_ref() == Some(&owner) => {
                    let row = SurfaceRow { name, rules, ..row };
                    tx.surfaces().save(&row)?;
                    Ok(row)
                }
                Some(row) => Err(errors::surfaces::Taken {
                    surface: row.id,
                    owner: row.module,
                }
                .spec()),
                None => {
                    tx.modules()
                        .exists(&owner)?
                        .true_or(Aux::NotFound(format!("module {owner}")))?;
                    let row = SurfaceRow {
                        id,
                        name,
                        module: Some(owner),
                        rules,
                        trusted: None,
                        model: None,
                        effort: None,
                        thinking: None,
                        compact_at: None,
                        builtin: None,
                        created_at: now,
                    };
                    tx.surfaces().insert(&row)?;
                    tx.folders().insert_builtin(&row.id, false)?;
                    tx.persons()
                        .seed_masters(&persons.master, &persons.master_name, now)?;
                    Ok(row)
                }
            }
        })
        .await?;

    shared.serving().serve(&row.id, &module, ask);
    log::info!(surface:% = row.id, module:% = module, ask; "surface registered");

    Ok(create::Ok {
        surface: convert::surface::from_db(row),
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
    let get::Args { surface } = args;
    let policy = Policy::new(caller);

    let db = db_conn.acquire(shared).await?;
    let id = surface.clone();
    let row = db
        .run(move |c| c.surfaces().get(&id))
        .await?
        .ok_or_else(|| errors::surfaces::NotFound { surface }.spec())?;
    policy.surfaces().own(&row.surface_ref())?;

    Ok(get::Ok {
        surface: convert::surface::from_db(row),
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
    let list::Args { limit, after } = args;
    let filter = Policy::new(caller).surfaces().visible();

    let db = db_conn.acquire(shared).await?;
    let rows = db
        .run(move |c| {
            c.surfaces()
                .list(&filter, after.as_ref(), limit.into_inner())
        })
        .await?;

    Ok(list::Ok {
        found: rows.into_iter().map(convert::surface::from_db).collect(),
    })
}

#[eva::endpoint]
#[fastrace::trace]
pub async fn Update<C: Components>(
    state: &mut State<C>,
    args: update::Args,
) -> Response<update::Ok, update::Err> {
    let State { shared, caller, .. } = state;
    let update::Args { surface, update } = args;
    let ask = update.ask.clone();

    let who = caller.clone();
    let row = shared
        .db()
        .write(move |tx| -> Response<SurfaceRow, update::Err> {
            let row = tx
                .surfaces()
                .get(&surface)?
                .ok_or_else(|| errors::surfaces::NotFound { surface }.spec())?;
            let policy = Policy::new(&who);
            policy.surfaces().own(&row.surface_ref())?;
            let row = apply(policy, row, update)?;
            tx.surfaces().save(&row)?;
            Ok(row)
        })
        .await?;

    if let Some(module) = caller.module() {
        let serving = shared.serving();
        let creator = row.module.as_ref() == Some(module);
        // Создатель обслуживает свой surface в любой живой сессии: после
        // реконнекта SDK шлёт `update`, а не `create`.
        match ask {
            Patch::Change(ask) if creator => serving.serve(&row.id, module, ask),
            Patch::Keep if creator => serving.enlist(&row.id, module),
            Patch::Change(ask) => _ = serving.set_ask(&row.id, module, ask),
            Patch::Keep => {}
        }
    }
    log::info!(surface:% = row.id, caller = caller.label(); "surface updated");

    Ok(update::Ok {
        surface: convert::surface::from_db(row),
    })
}

/// Правка поверх загруженной строки; `ask` — состояние сессии, не строки.
fn apply(
    policy: Policy<'_>,
    mut row: SurfaceRow,
    update: update::Update,
) -> Response<SurfaceRow, update::Err> {
    let update::Update {
        name,
        rules,
        ask: _,
        trusted,
        model,
        effort,
        thinking,
        compact_at,
    } = update;

    let touches_trust = !(trusted.is_keep()
        && model.is_keep()
        && effort.is_keep()
        && thinking.is_keep()
        && compact_at.is_keep());
    if touches_trust {
        policy.set_trust()?;
    }

    if let Patch::Change(name) = name {
        check_name(&name)?;
        row.name = name;
    }
    if let Patch::Change(rules) = rules {
        check_rules(rules.as_deref())?;
        row.rules = rules;
    }
    if let Patch::Change(trusted) = trusted {
        if row.builtin.is_some() {
            return Err(errors::surfaces::Builtin {
                surface: row.id,
                field: String::from("trusted"),
            }
            .spec());
        }
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
        compact_at
            .is_none_or(|v| v > 0.0 && v <= 1.0)
            .true_or(Aux::Invalid("compact_at must be in (0, 1]".into()))?;
        row.compact_at = compact_at;
    }

    Ok(row)
}
