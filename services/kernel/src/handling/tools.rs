//! Хендлеры `tools.*` (`11 §3`).

use eva_protocol::{
    errors::{Aux, ext::*, tools::PathNotFound},
    requests::{Response, tools::catalog},
    types::tool::Exposure,
};

use crate::{
    components::Components,
    db::Queries as _,
    state::{Policy, State},
    subsystems::{
        tools::{
            canonical, catalog as tree,
            expose::{self, Subject},
        },
        turns::gates,
    },
};

#[eva::endpoint]
#[fastrace::trace]
pub async fn Catalog<C: Components>(
    state: &mut State<C>,
    args: catalog::Args,
) -> Response<catalog::Ok, catalog::Err> {
    Policy::new(&state.caller).owner()?;
    let catalog = state.shared.tools().get();
    let exposure = if let Some(chat) = args.chat {
        let db = state.db_conn.acquire(&state.shared).await?;
        if db.run(move |c| c.chats().get(chat)).await?.is_none() {
            return Err(Aux::NotFound(format!("chat {chat} does not exist")).into());
        }
        if let Some(ctx) = state
            .shared
            .turns()
            .get(chat)
            .and_then(|slot| slot.last_ctx())
        {
            Some((
                gates::gates(&state.shared, &ctx).await?,
                gates::day_state(&state.shared, &ctx).await?,
            ))
        } else {
            None
        }
    } else {
        None
    };
    let cfg = state.shared.cfg();
    let listed = tree::subtree(
        &catalog,
        args.path.as_deref(),
        args.depth.into(),
        &|entry| {
            exposure.as_ref().map_or(Exposure::Visible, |(gates, day)| {
                expose::expose(
                    Subject {
                        id: entry.id,
                        decl: &entry.decl,
                        online: entry.online,
                        lost_collision: entry.lost_collision,
                        core: cfg
                            .tools
                            .core
                            .iter()
                            .any(|path| canonical::under(entry.path(), path)),
                    },
                    gates,
                    day,
                )
            })
        },
    )
    .ok_or_else(|| {
        PathNotFound {
            path: args.path.unwrap_or_default(),
        }
        .spec()
    })?;
    let entries = listed
        .iter()
        .map(|listed| tree::entry(&catalog, listed, exposure.is_some()))
        .collect();
    Ok(catalog::Ok { entries })
}
