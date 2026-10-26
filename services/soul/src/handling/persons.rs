//! Хендлеры `persons.*` (`05 §1.5`, `11 §3`): `resolve` — модулю по своему
//! surface, прочее — только хозяину.

use eva_protocol::{
    errors::{self, Aux, AuxResult, ext::*},
    requests::{
        Persons::{list, merge, rename, resolve, whitelist},
        Response,
    },
    types::PersonId,
};

use crate::{
    components::Components,
    convert,
    db::{
        Db as _, Queries as _, Tx,
        persons::{self, IdentityRow, PersonFilter, PersonRow},
        stats::Metric,
        surfaces::SurfaceFilter,
    },
    handling::text,
    state::{Policy, State},
};

fn check_name(name: &str) -> AuxResult<()> {
    let ok = !name.trim().is_empty() && !name.chars().any(char::is_control);
    ok.true_or(Aux::Invalid(
        "person name must be a non-empty single line".into(),
    ))?;
    text::chars("person name", name, text::NAME_MAX)
}

/// `p0` — актор soul: его не переименовывают, не сливают и не трогают whitelist.
fn check_not_system<E: From<errors::persons::System>>(person: &PersonId) -> Response<(), E> {
    if *person == PersonId::system() {
        Err(errors::persons::System {
            person: person.clone(),
        }
        .spec())
    } else {
        Ok(())
    }
}

type Loaded = (PersonRow, Vec<IdentityRow>);

/// Личность со всеми адресами после мутации в той же транзакции.
fn reload(tx: &Tx<'_>, person: &PersonId) -> AuxResult<Loaded> {
    let row = tx
        .persons()
        .get(person)?
        .ok_or_else(|| Aux::internal(format!("person {person} vanished inside its transaction")))?;
    let identities = tx.persons().identities(person, &SurfaceFilter::All)?;
    Ok((row, identities))
}

fn load<E: From<errors::persons::NotFound>>(
    tx: &Tx<'_>,
    person: &PersonId,
) -> Response<PersonRow, E> {
    tx.persons().get(person)?.ok_or_else(|| {
        errors::persons::NotFound {
            person: person.clone(),
        }
        .spec()
    })
}

#[eva::endpoint]
#[fastrace::trace]
pub async fn Resolve<C: Components>(
    state: &mut State<C>,
    args: resolve::Args,
) -> Response<resolve::Ok, resolve::Err> {
    resolve_person(state, args).await
}

pub(crate) async fn resolve_person<C: Components>(
    state: &mut State<C>,
    args: resolve::Args,
) -> Response<resolve::Ok, resolve::Err> {
    let State {
        shared,
        world,
        caller,
        ..
    } = state;
    let resolve::Args {
        surface,
        external_id,
        handle,
        name,
    } = args;
    (!external_id.is_empty()).true_or(Aux::Invalid("external_id must not be empty".into()))?;
    text::bytes("external_id", &external_id, text::EXTERNAL_ID_MAX)?;
    for (what, value) in [("handle", &handle), ("name", &name)] {
        if let Some(value) = value {
            text::chars(what, value, text::NAME_MAX)?;
        }
    }

    let (now, zone) = (world.now(), shared.day_zone());
    let who = caller.clone();
    let (row, identities, created) = shared
        .db()
        .write(move |tx| -> Response<_, resolve::Err> {
            let policy = Policy::new(&who);
            policy.surfaces().named(
                format_args!("surface {surface}"),
                tx.surfaces().get(&surface)?.map(|s| s.surface_ref()),
            )?;

            let (person, created) = match tx.persons().identity_owner(&surface, &external_id)? {
                Some(person) => (person, false),
                None => {
                    let display = name
                        .clone()
                        .or_else(|| handle.clone())
                        .filter(|n| !n.trim().is_empty())
                        .unwrap_or_else(|| format!("{surface}:{external_id}"));
                    let person = tx.persons().insert_numbered(&display, false, false, now)?;
                    tx.persons()
                        .insert_identity(&person, &surface, &external_id, now)?;
                    tx.stats()
                        .bump(Metric::PersonsCreated, surface.as_str(), now, &zone)?;
                    (person, true)
                }
            };
            tx.persons().touch_identity(
                &surface,
                &external_id,
                handle.as_deref(),
                name.as_deref(),
            )?;

            let row = tx.persons().get(&person)?.ok_or_else(|| {
                Aux::internal(format!("person {person} vanished inside its transaction"))
            })?;
            let identities = tx
                .persons()
                .identities(&person, &policy.surfaces().visible())?;
            Ok((row, identities, created))
        })
        .await?;

    if created {
        log::info!(person:% = row.id, caller = caller.label(); "person created");
    }
    Ok(resolve::Ok {
        person: convert::person::from_db(row, identities),
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
    Policy::new(caller).owner()?;
    let list::Args {
        surface,
        whitelisted,
        limit,
        after,
    } = args;
    let after = after
        .map(|p| persons::number(&p).ok_or_else(|| Aux::Invalid(format!("{p} is not a person id"))))
        .transpose()?;
    let filter = PersonFilter {
        surface,
        whitelisted,
    };

    let db = db_conn.acquire(shared).await?;
    let found = db
        .run(move |c| -> AuxResult<Vec<Loaded>> {
            let rows = c.persons().list(&filter, after, limit.into_inner())?;
            let mut found = Vec::with_capacity(rows.len());
            for row in rows {
                let identities = c.persons().identities(&row.id, &SurfaceFilter::All)?;
                found.push((row, identities));
            }
            Ok(found)
        })
        .await?;

    Ok(list::Ok {
        found: found
            .into_iter()
            .map(|(row, identities)| convert::person::from_db(row, identities))
            .collect(),
    })
}

#[eva::endpoint]
#[fastrace::trace]
pub async fn Rename<C: Components>(
    state: &mut State<C>,
    args: rename::Args,
) -> Response<rename::Ok, rename::Err> {
    let State { shared, caller, .. } = state;
    Policy::new(caller).owner()?;
    let rename::Args { person, name } = args;
    check_not_system(&person)?;
    check_name(&name)?;

    let (row, identities) = shared
        .db()
        .write(move |tx| -> Response<Loaded, rename::Err> {
            load(tx, &person)?;
            tx.persons().rename(&person, &name)?;
            Ok(reload(tx, &person)?)
        })
        .await?;

    log::info!(person:% = row.id, caller = caller.label(); "person renamed");
    Ok(rename::Ok {
        person: convert::person::from_db(row, identities),
    })
}

#[eva::endpoint]
#[fastrace::trace]
pub async fn Merge<C: Components>(
    state: &mut State<C>,
    args: merge::Args,
) -> Response<merge::Ok, merge::Err> {
    let State { shared, caller, .. } = state;
    Policy::new(caller).owner()?;
    let merge::Args {
        from,
        into,
        confirm,
    } = args;
    if from == into {
        return Err(errors::persons::SelfMerge { person: from }.spec());
    }
    check_not_system(&from)?;
    check_not_system(&into)?;

    let (from_id, into_id) = (from.clone(), into.clone());
    let (row, identities, master_changed) = shared
        .db()
        .write(move |tx| -> Response<_, merge::Err> {
            let source = load(tx, &from)?;
            let target = load(tx, &into)?;
            let master_changed = source.master != target.master;
            if master_changed && !confirm {
                return Err(errors::persons::MasterMerge { from, into }.spec());
            }
            tx.persons().merge(&from, &into)?;
            let (row, identities) = reload(tx, &into)?;
            Ok((row, identities, master_changed))
        })
        .await?;

    log::info!(
        from:% = from_id, into:% = into_id, master_changed, caller = caller.label();
        "persons merged"
    );
    Ok(merge::Ok {
        person: convert::person::from_db(row, identities),
    })
}

#[eva::endpoint]
#[fastrace::trace]
pub async fn Whitelist<C: Components>(
    state: &mut State<C>,
    args: whitelist::Args,
) -> Response<whitelist::Ok, whitelist::Err> {
    let State { shared, caller, .. } = state;
    Policy::new(caller).owner()?;
    let whitelist::Args { person, on } = args;
    check_not_system(&person)?;

    let (row, identities) = shared
        .db()
        .write(move |tx| -> Response<Loaded, whitelist::Err> {
            let row = load(tx, &person)?;
            if row.master && !on {
                return Err(errors::persons::MasterWhitelist { person }.spec());
            }
            tx.persons().set_whitelisted(&person, on)?;
            Ok(reload(tx, &person)?)
        })
        .await?;

    log::info!(person:% = row.id, on, caller = caller.label(); "person whitelist changed");
    Ok(whitelist::Ok {
        person: convert::person::from_db(row, identities),
    })
}
