//! Хендлеры `modules.*` (`06 §2`, `§9`, `10 §2.6`): список модулей и
//! токены, выданные хозяином. Хранится только `blake3(token)`.

use eva::rand::Rng as _;
use eva_protocol::{
    errors::{self, Aux, AuxResult, ext::*},
    requests::{
        Response,
        modules::{list, token_issue, token_revoke},
    },
    types::ModuleId,
    wire::{ErrorCode, RpcError},
};

use crate::{
    components::Components,
    convert,
    db::{
        Db as _, Queries as _,
        modules::TokenRow,
        surfaces::{LOCAL, SYSTEM},
    },
    state::{Policy, State},
};

/// Id, которые не бывают именем токена (`06 §2`): их занимают ядро и builtin surface.
pub const RESERVED: &[&str] = &["kernel", SYSTEM, LOCAL];

/// Имя модуля — ещё и каталог его `fs`/`sqlite`: `[a-z][a-z0-9_-]{0,63}`.
pub fn check_name(module: &ModuleId) -> AuxResult<()> {
    let mut chars = module.as_str().chars();
    let ok = matches!(chars.next(), Some('a'..='z'))
        && module.as_str().len() <= 64
        && chars.all(|c| matches!(c, 'a'..='z' | '0'..='9' | '_' | '-'));
    ok.true_or(Aux::Invalid(format!(
        "module name {module:?} must match [a-z][a-z0-9_-]{{0,63}}"
    )))
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
    let list::Args {} = args;
    Policy::new(caller).owner()?;

    let db = db_conn.acquire(shared).await?;
    let rows = db.run(|c| c.modules().list()).await?;
    let tokens = &shared.cfg().modules.tokens;

    Ok(list::Ok {
        modules: rows
            .into_iter()
            .map(|row| {
                let config = tokens.get(&row.name).map(|t| t.kind);
                convert::module::from_db(row, config)
            })
            .collect(),
    })
}

#[eva::endpoint]
#[fastrace::trace]
pub async fn TokenIssue<C: Components>(
    state: &mut State<C>,
    args: token_issue::Args,
) -> Response<token_issue::Ok, token_issue::Err> {
    let State {
        shared,
        world,
        caller,
        ..
    } = state;
    Policy::new(caller)
        .modules()
        .issue(&args.module, args.issued_by.as_ref())?;
    let token_issue::Args {
        module,
        kind,
        issued_by,
    } = args;

    (!RESERVED.contains(&module.as_str()))
        .true_or(Aux::Invalid(format!("module id {module} is reserved")))?;
    check_name(&module)?;
    let issued_via = caller.module().cloned();
    (issued_by.is_some() || issued_via.is_some()).true_or(Aux::Invalid(
        "issued_by is required when the kernel issues a token".into(),
    ))?;
    // Handshake такой токен не примет: конфиг сильнее (`06 §9`).
    (!shared.tokens().configures(&module)).true_or(Aux::Conflict(format!(
        "modules.tokens configures module {module}: change its token in the config"
    )))?;

    let secret: [u8; 32] = world.rng().random();
    let token = convert::blob::hex(&secret);
    let row = TokenRow {
        hash: blake3::hash(token.as_bytes()).as_bytes().to_vec(),
        module: module.clone(),
        kind,
        issued_by,
        issued_via,
        issued_at: world.now(),
    };

    let revoked = shared
        .db()
        .write(move |tx| -> Response<usize, token_issue::Err> {
            if let Some(person) = &row.issued_by {
                tx.modules()
                    .person_exists(person)?
                    .true_or(Aux::NotFound(format!("person {person}")))?;
            }
            // Один живой токен на модуль: выдача нового — ротация.
            let revoked = tx.modules().revoke_tokens(&row.module, row.issued_at)?;
            tx.modules().insert_token(&row)?;
            Ok(revoked)
        })
        .await?;

    if revoked > 0 {
        shared.live().revoke(
            &module,
            RpcError::new(ErrorCode::Unauthorized, "module token rotated"),
        );
    }
    log::info!(module:% = module, kind:% = kind, revoked, caller = caller.label(); "module token issued");
    Ok(token_issue::Ok { token })
}

#[eva::endpoint]
#[fastrace::trace]
pub async fn TokenRevoke<C: Components>(
    state: &mut State<C>,
    args: token_revoke::Args,
) -> Response<token_revoke::Ok, token_revoke::Err> {
    let State {
        shared,
        world,
        caller,
        ..
    } = state;
    Policy::new(caller).owner()?;
    let token_revoke::Args { module } = args;

    let now = world.now();
    let name = module.clone();
    let revoked = shared
        .db()
        .write(move |tx| tx.modules().revoke_tokens(&name, now))
        .await
        .aux_err()?;
    if revoked == 0 {
        return Err(errors::modules::NotFound { module }.spec());
    }

    shared.live().revoke(
        &module,
        RpcError::new(ErrorCode::Unauthorized, "module token revoked"),
    );
    log::info!(module:% = module, revoked, caller = caller.label(); "module token revoked");
    Ok(token_revoke::Ok {})
}
