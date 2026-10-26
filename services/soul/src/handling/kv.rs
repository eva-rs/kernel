//! Хендлеры `kv.*` (`06 §5`, I12): KV модуля, last-write-wins. Скоуп —
//! имя модуля сессии, поэтому чужого ключа не адресовать вовсе. Значение —
//! до `modules.kv_value_max`, всё KV модуля — до `modules.kv_quota`.

use eva_protocol::{
    errors::{Aux, AuxResult, ext::*},
    requests::{
        Kv::{del, get, list, set},
        Response,
    },
    types::Value,
};

use crate::{
    components::Components,
    db::{Db as _, DbError, Queries as _, kv::row_cost},
    state::{Policy, State},
};

/// Ключ — строка индекса, не документ.
const KEY_MAX: usize = 1024;

fn check_key(key: &str) -> AuxResult<()> {
    (!key.is_empty() && key.len() <= KEY_MAX)
        .true_or(Aux::Invalid(format!("kv key must be 1..={KEY_MAX} bytes")))
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
    let module = Policy::new(caller).module()?.clone();
    let get::Args { key } = args;
    check_key(&key)?;

    let db = db_conn.acquire(shared).await?;
    let raw = db.run(move |c| c.kv().get(&module, &key)).await?;
    let value = raw
        .map(|raw| {
            rmp_serde::from_slice::<Value>(&raw)
                .map_err(|e| Aux::from(DbError::corrupt(format_args!("module_kv value: {e}"))))
        })
        .transpose()?;

    Ok(get::Ok { value })
}

#[eva::endpoint]
#[fastrace::trace]
pub async fn Set<C: Components>(
    state: &mut State<C>,
    args: set::Args,
) -> Response<set::Ok, set::Err> {
    let State {
        shared,
        world,
        caller,
        ..
    } = state;
    let module = Policy::new(caller).module()?.clone();
    let set::Args { key, value } = args;
    check_key(&key)?;

    let raw = rmp_serde::to_vec_named(&value).map_err(Aux::internal)?;
    let modules = &shared.cfg().modules;
    let (value_max, quota) = (modules.kv_value_max.as_u64(), modules.kv_quota.as_u64());
    (raw.len() as u64 <= value_max).true_or(Aux::Quota(format!(
        "kv value is {} bytes, modules.kv_value_max is {value_max}",
        raw.len()
    )))?;

    let now = world.now();
    // Проверка в той же транзакции писателя: параллельные `set` её не обойдут.
    shared
        .db()
        .write(move |tx| -> AuxResult<()> {
            let used = tx.kv().usage(&module, &key)?;
            (used.saturating_add(row_cost(&key, &raw)) <= quota).true_or(Aux::Quota(format!(
                "module kv takes {used} of modules.kv_quota {quota} bytes"
            )))?;
            Ok(tx.kv().set(&module, &key, &raw, now)?)
        })
        .await?;

    Ok(set::Ok {})
}

#[eva::endpoint]
#[fastrace::trace]
pub async fn Del<C: Components>(
    state: &mut State<C>,
    args: del::Args,
) -> Response<del::Ok, del::Err> {
    let State { shared, caller, .. } = state;
    let module = Policy::new(caller).module()?.clone();
    let del::Args { key } = args;
    check_key(&key)?;

    shared
        .db()
        .write(move |tx| tx.kv().del(&module, &key))
        .await
        .aux_err()?;

    Ok(del::Ok {})
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
    let module = Policy::new(caller).module()?.clone();
    let list::Args {
        prefix,
        limit,
        after,
    } = args;

    let db = db_conn.acquire(shared).await?;
    let keys = db
        .run(move |c| {
            c.kv()
                .list(&module, &prefix, after.as_deref(), limit.into_inner())
        })
        .await?;

    Ok(list::Ok { keys })
}
