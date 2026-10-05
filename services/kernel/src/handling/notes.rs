//! Хендлеры `notes.*` (`04 §5`): чтение блокнота хозяином; режим не
//! пересчитывается и не пишется (п. 5).

use eva_protocol::{
    errors::{self, Aux, ext::*},
    requests::{Response, notes::list},
};

use crate::{
    components::Components,
    convert,
    db::Queries as _,
    state::{Policy, State},
    subsystems::{
        notes::{budget, state},
        turns::estimate::Estimator,
    },
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
    let list::Args { chat } = args;
    let cfg = &shared.cfg().notes;
    cfg.enabled.true_or(Aux::Unavailable(
        "notes are disabled by notes.enabled".into(),
    ))?;

    let default_model = shared.cfg().llm.default_model.clone();
    let db = db_conn.acquire(shared).await?;
    let (model, notes, notes_state, ratio) = db
        .run(move |c| -> Response<_, list::Err> {
            let model = c
                .notes()
                .chat_model(chat)?
                .ok_or_else(|| errors::chats::NotFound { chat: chat.into() }.spec())?
                .unwrap_or(default_model);
            let notes = c.notes().list(chat)?;
            let state = c.notes().state(chat)?;
            let ratio = c.turns().ratio(&model)?;
            Ok((model, notes, state, ratio))
        })
        .await?;

    let window = shared.models().get(&model).map(|m| m.decl.context_window);
    let budget = budget::budget(window, cfg);
    let ratio = Some(Estimator::new(ratio, &shared.cfg().context).ratio);
    let fill = budget::fill(&notes, budget, ratio);
    let notes_state = notes_state.unwrap_or_else(|| state::initial(shared.now()));

    Ok(list::Ok {
        notes: notes.into_iter().map(convert::note::from_db).collect(),
        mode: notes_state.mode,
        squeeze_turns: notes_state.squeeze_turns,
        tokens: fill.tokens,
        budget,
    })
}
