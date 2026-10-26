//! Хендлеры `modules.*` (`06 §2`, `§9`, `10 §2.6`): список модулей, реестр
//! сервисов `modules.add/remove` и токены клиентов, выданные хозяином.
//! Токен хранится как `blake3(token)`, ключ сервиса — как есть: soul сам им
//! дозванивается.

use std::collections::BTreeMap;

use eva::rand::Rng as _;
use eva_protocol::{
    errors::{self, Aux, AuxResult, ext::*},
    requests::{
        Modules::{add, list, remove, token_issue, token_revoke},
        Response,
    },
    types::{
        ModuleId,
        module::{ModuleKind, ServiceKey},
    },
    wire::{ErrorCode, RpcError},
};
use eva_rpc::Psk;

use crate::{
    components::Components,
    convert,
    db::{
        Db as _, DbResult, Queries as _,
        modules::{ServiceRow, TokenRow},
        surfaces::{LOCAL, SYSTEM},
    },
    shared::Shared,
    state::{Policy, State},
    subsystems::modules::{
        live::Source,
        services::{Listed, Loop, Service, WEAK_KEY, trimmed},
    },
};

/// Id, которые не бывают id модуля (`06 §2`): их занимают soul и builtin surface.
pub const RESERVED: &[&str] = &["soul", SYSTEM, LOCAL];

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

fn check_id(module: &ModuleId) -> AuxResult<()> {
    (!RESERVED.contains(&module.as_str()))
        .true_or(Aux::Invalid(format!("module id {module} is reserved")))?;
    check_name(module)
}

/// Адрес уже набирает другая запись реестра.
fn dials_already(twin: &ModuleId, address: &impl std::fmt::Display) -> Aux {
    Aux::Conflict(format!("module {twin} already dials {address}"))
}

/// Реестр сервисов для списков: конфиг и незатенённые строки `module_services`.
pub fn registry<C: Components>(
    shared: &Shared<C>,
    rows: Vec<ServiceRow>,
) -> BTreeMap<ModuleId, Listed> {
    let rows = rows
        .into_iter()
        .filter_map(|row| match convert::module::service_address(&row) {
            Ok(address) => Some((row.module, address, row.added_at)),
            Err(e) => {
                log::error!(error:% = e; "module_services row is not listable");
                None
            }
        })
        .collect();
    shared
        .services()
        .listed(rows, shared.tokens(), shared.started_at())
}

/// Запись реестра — сервис; токен конфига или живой выданный — клиент.
pub fn kind<C: Components>(
    shared: &Shared<C>,
    registry: &BTreeMap<ModuleId, Listed>,
    module: &ModuleId,
    live_token: bool,
) -> Option<ModuleKind> {
    if registry.contains_key(module) {
        Some(ModuleKind::Service)
    } else if live_token || shared.tokens().configures(module) {
        Some(ModuleKind::Client)
    } else {
        None
    }
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
    let (rows, services) = db
        .run(|c| DbResult::Ok((c.modules().list()?, c.modules().services()?)))
        .await?;
    let mut registry = registry(shared, services);

    let mut modules: Vec<_> = rows
        .into_iter()
        .map(|row| {
            let kind = kind(shared, &registry, &row.name, row.live_token);
            let dial = registry.remove(&row.name).map(|listed| listed.dial);
            convert::module::from_db(row, kind, dial)
        })
        .collect();
    modules.extend(
        registry
            .into_iter()
            .map(|(name, listed)| convert::module::unseen(name, listed.dial, listed.since)),
    );
    modules.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(list::Ok { modules })
}

#[eva::endpoint]
#[fastrace::trace]
pub async fn Add<C: Components>(
    state: &mut State<C>,
    args: add::Args,
) -> Response<add::Ok, add::Err> {
    let State {
        shared,
        world,
        caller,
        ..
    } = state;
    let added_via = Policy::new(caller).client()?.clone();
    let add::Args {
        module,
        address,
        key,
    } = args;
    check_id(&module)?;

    let (key, generated) = match key {
        Some(key) => {
            let len = trimmed(key.expose()).chars().count();
            (len >= WEAK_KEY).true_or(Aux::Invalid(format!(
                "key must be a high-entropy secret of at least {WEAK_KEY} characters: a guessable key is brute-forced offline from one captured handshake"
            )))?;
            (key, false)
        }
        None => {
            let secret: [u8; 32] = world.rng().random();
            (ServiceKey(convert::blob::hex(&secret)), true)
        }
    };
    let services = shared.services();
    // Конфиг сильнее: его id заняты без обращения к БД.
    if services.shadowed(&module, shared.tokens()) {
        return Err(errors::modules::Taken { module }.spec());
    }
    // Две петли на один модуль вытесняли бы друг друга без конца.
    if let Some(twin) = services.dialing(&address) {
        return Err(dials_already(twin, &address).into());
    }

    let _ops = services.ops().await;
    let row = ServiceRow {
        module: module.clone(),
        address: address.to_string(),
        added_via,
        added_at: world.now(),
    };
    let stored = key.clone();
    shared
        .db()
        .write(move |tx| -> Response<(), add::Err> {
            let modules = tx.modules();
            if modules.service_exists(&row.module)? || modules.has_live_token(&row.module)? {
                return Err(errors::modules::Taken {
                    module: row.module.clone(),
                }
                .spec());
            }
            if let Some(twin) = modules.service_at(&row.address)? {
                return Err(dials_already(&twin, &row.address).into());
            }
            modules.insert_service(&row, &stored)?;
            Ok(())
        })
        .await?;

    log::info!(module:% = module, address:% = address, generated, caller = caller.label(); "service module added");
    // Под `ops`: `modules.remove` того же id отзовёт уже этот номер.
    let epoch = Some(shared.live().epoch(&module));
    services.start(Loop {
        module,
        service: Service {
            address,
            psk: Psk::derive(trimmed(key.expose())),
            source: Source::RuntimeService,
        },
        epoch,
    });
    Ok(add::Ok {
        key: generated.then_some(key),
    })
}

#[eva::endpoint]
#[fastrace::trace]
pub async fn Remove<C: Components>(
    state: &mut State<C>,
    args: remove::Args,
) -> Response<remove::Ok, remove::Err> {
    let State { shared, caller, .. } = state;
    Policy::new(caller).client()?;
    let remove::Args { module } = args;

    let services = shared.services();
    (!services.configures(&module)).true_or(Aux::Conflict(format!(
        "modules.services configures module {module}: remove it from the config"
    )))?;

    let _ops = services.ops().await;
    let name = module.clone();
    let deleted = shared
        .db()
        .write(move |tx| tx.modules().delete_service(&name))
        .await
        .aux_err()?;
    if !deleted {
        return Err(errors::modules::ServiceNotFound { module }.spec());
    }

    // Номер отзыва — раньше остановки петли: попытка в полёте сессией не станет.
    shared.live().remove(
        &module,
        RpcError::new(ErrorCode::Unauthorized, "module removed"),
    );
    services.stop(&module);
    log::info!(module:% = module, caller = caller.label(); "service module removed");
    Ok(remove::Ok {})
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
    let token_issue::Args { module, issued_by } = args;

    check_id(&module)?;
    let issued_via = caller.module().cloned();
    (issued_by.is_some() || issued_via.is_some()).true_or(Aux::Invalid(
        "issued_by is required when the soul issues a token".into(),
    ))?;
    // Handshake такой токен не примет: конфиг сильнее (`06 §9`).
    (!shared.tokens().configures(&module)).true_or(Aux::Conflict(format!(
        "modules.tokens configures module {module}: change its token in the config"
    )))?;
    (!shared.services().configures(&module)).true_or(Aux::Conflict(format!(
        "modules.services configures module {module}: a service authenticates by its key"
    )))?;

    let secret: [u8; 32] = world.rng().random();
    let token = convert::blob::hex(&secret);
    let row = TokenRow {
        hash: blake3::hash(token.as_bytes()).as_bytes().to_vec(),
        module: module.clone(),
        issued_by,
        issued_via,
        issued_at: world.now(),
    };

    let revoked = shared
        .db()
        .write(move |tx| -> Response<usize, token_issue::Err> {
            let modules = tx.modules();
            (!modules.service_exists(&row.module)?).true_or(Aux::Conflict(format!(
                "module {} is a service added by modules.add: it authenticates by its key",
                row.module
            )))?;
            if let Some(person) = &row.issued_by {
                modules
                    .person_exists(person)?
                    .true_or(Aux::NotFound(format!("person {person}")))?;
            }
            // Один живой токен на модуль: выдача нового — ротация.
            let revoked = modules.revoke_tokens(&row.module, row.issued_at)?;
            modules.insert_token(&row)?;
            Ok(revoked)
        })
        .await?;

    if revoked > 0 {
        shared.live().revoke(
            &module,
            RpcError::new(ErrorCode::Unauthorized, "module token rotated"),
        );
    }
    log::info!(module:% = module, revoked, caller = caller.label(); "client token issued");
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
