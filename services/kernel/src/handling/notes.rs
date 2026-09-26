//! Хендлеры `notes.*` (`04 §5`): чтение блокнота хозяином; режим не
//! пересчитывается и не пишется (п. 5).

use eva_protocol::{
    errors::{self, Aux, ext::*},
    requests::{Response, notes::list},
    types::note::Mode,
};

use crate::{
    components::Components,
    convert,
    db::{Queries as _, notes::NoteRow},
    state::{Policy, State},
};

/// `context.ratio_default` (`01 §9`): модель ещё не калибрована.
const RATIO_DEFAULT: f64 = 4.0;

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
    let (percent, ceiling) = (
        u64::from(cfg.budget_percent.into_inner()),
        cfg.budget_max_tokens,
    );

    let default_model = shared.cfg().llm.default_model.clone();
    let db = db_conn.acquire(shared).await?;
    let (model, notes, mode, ratio) = db
        .run(move |c| -> Response<_, list::Err> {
            let model = c
                .notes()
                .chat_model(chat)?
                .ok_or_else(|| errors::chats::NotFound { chat: chat.into() }.spec())?
                .unwrap_or(default_model);
            let notes = c.notes().list(chat)?;
            let mode = c.notes().mode(chat)?;
            let ratio = c.notes().ratio(&model)?;
            Ok((model, notes, mode, ratio))
        })
        .await?;

    // Модели нет в реестре (провайдер не подключён) — потолок бюджета, не отказ чтения.
    let budget = shared.models().get(&model).map_or(ceiling, |m| {
        let share = u64::from(m.decl.context_window) * percent / 100;
        u32::try_from(share).map_or(ceiling, |s| s.min(ceiling))
    });
    let tokens = estimate(&render_inline(&notes), ratio.unwrap_or(RATIO_DEFAULT));

    Ok(list::Ok {
        notes: notes.into_iter().map(convert::note::from_db).collect(),
        mode: mode.unwrap_or(Mode::Inline),
        tokens,
        budget,
    })
}

/// Inline-рендер секции `[notes]` (`04 §3`): `fill` меряется по нему в любом режиме.
fn render_inline(notes: &[NoteRow]) -> String {
    if notes.is_empty() {
        return String::from("[notes] empty");
    }
    let blocks: Vec<String> = notes
        .iter()
        .map(|n| format!("## {} | {}\n{}", n.name, n.description, n.body))
        .collect();
    format!("[notes]\n{}", blocks.join("\n\n"))
}

/// `est(Text) = bytes / ratio` (`01 §9`), вверх.
fn estimate(text: &str, ratio: f64) -> u32 {
    let tokens = (text.len() as f64 / ratio.clamp(1.0, 10.0)).ceil();
    if tokens >= f64::from(u32::MAX) {
        u32::MAX
    } else {
        tokens as u32
    }
}
