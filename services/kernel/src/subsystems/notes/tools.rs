//! Тулы группы `notes` (`04 §7`, `§9`): функции над соединением писателя —
//! движок туров зовёт их внутри транзакции шага. Отказ вместо обрезания,
//! всегда с причиной и числом; `Rejected.reason` — ключ `notes.rejected`.
//!
//! Гейты проверяет снимок шага, `turns::integration` публикует `NoteUpdated`.

use std::collections::BTreeSet;

use eva::time::Timestamp;

use eva_protocol::types::{
    ChatId,
    note::{
        Fill, Mode, NoteUpdated, Op,
        tools::{delete, read, save, update},
    },
};

use crate::{
    config::NotesConfig,
    db::{Connection, DbError, Queries as _, notes::NoteRow},
    subsystems::notes::{budget, render},
};

/// Контекст тура для записи в блокнот.
pub struct Ctx<'a> {
    pub chat: ChatId,
    /// Режим на входе в тур; внутри тура не меняется.
    pub mode: Mode,
    /// Имена, прочитанные `notes_read` или успешно записанные в этом туре: guard `index`.
    pub read_this_turn: &'a mut BTreeSet<String>,
    pub ratio: Option<f64>,
    pub budget: u32,
    pub now: Timestamp,
}

/// Отказ тула: `reason` — ключ метрики, `message` — текст модели.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rejected {
    pub reason: &'static str,
    pub message: String,
}

#[derive(Debug)]
pub enum Error {
    Rejected(Rejected),
    Db(DbError),
}

impl From<DbError> for Error {
    fn from(e: DbError) -> Self {
        Self::Db(e)
    }
}

impl From<Rejected> for Error {
    fn from(e: Rejected) -> Self {
        Self::Rejected(e)
    }
}

/// Успешная мутация и её событие гигиены.
#[derive(Debug, Clone, PartialEq)]
pub struct Applied<T> {
    pub ok: T,
    pub event: NoteUpdated,
}

fn reject(reason: &'static str, message: String) -> Rejected {
    Rejected { reason, message }
}

pub const NAME_MAX: usize = 48;
pub const DESCRIPTION_MAX: usize = 120;

fn normalize(name: &str) -> String {
    name.trim().to_lowercase()
}

fn check_name(name: &str) -> Result<String, Rejected> {
    let name = normalize(name);
    crate::subsystems::memory::text::is_slug(&name, NAME_MAX)
        .then_some(name)
        .ok_or_else(|| {
            reject(
                "name",
                format!("name must match [a-z0-9_-]{{1,{NAME_MAX}}}"),
            )
        })
}

fn check_description(description: &str) -> Result<(), Rejected> {
    let n = description.chars().count();
    (1..=DESCRIPTION_MAX)
        .contains(&n)
        .then_some(())
        .ok_or_else(|| {
            reject(
                "description",
                format!("description: 1..{DESCRIPTION_MAX} chars, got {n}"),
            )
        })
}

fn check_body(body: &str, cfg: &NotesConfig, ratio: Option<f64>) -> Result<(), Rejected> {
    if body.trim().is_empty() {
        return Err(reject(
            "empty_body",
            String::from("empty body; to remove a note use notes_delete"),
        ));
    }
    let tokens = budget::tokens(body, ratio);
    if tokens > cfg.max_note_tokens {
        return Err(reject(
            "body_tokens",
            format!(
                "body is {tokens} tokens, limit {}; split into two notes or cut",
                cfg.max_note_tokens
            ),
        ));
    }
    Ok(())
}

/// Порядок секции (`04 §1`).
fn sort(notes: &mut [NoteRow]) {
    notes.sort_by(|a, b| {
        b.pinned
            .cmp(&a.pinned)
            .then(a.created_at.cmp(&b.created_at))
            .then(a.name.cmp(&b.name))
    });
}

/// Блокнот после записи `note`: замена по имени или хвост.
fn with(mut notes: Vec<NoteRow>, note: NoteRow) -> Vec<NoteRow> {
    match notes.iter_mut().find(|n| n.name == note.name) {
        Some(slot) => *slot = note,
        None => notes.push(note),
    }
    sort(&mut notes);
    notes
}

/// Стены блокнота после записи (`04 §5`, `§9`): штук, булавки, индекс.
fn check_limits(
    after: &[NoteRow],
    note: &NoteRow,
    ctx: &Ctx<'_>,
    cfg: &NotesConfig,
) -> Result<(), Rejected> {
    if after.len() > cfg.max_notes as usize {
        return Err(reject(
            "max_notes",
            format!(
                "notebook holds {} notes; delete or merge first",
                cfg.max_notes
            ),
        ));
    }
    if note.pinned {
        let others: Vec<&NoteRow> = after
            .iter()
            .filter(|n| n.pinned && n.name != note.name)
            .collect();
        if others.len() >= cfg.pinned_max as usize {
            let names: Vec<&str> = others.iter().map(|n| n.name.as_str()).collect();
            return Err(reject(
                "pinned_max",
                format!(
                    "{} notes already pinned: {}",
                    cfg.pinned_max,
                    names.join(", ")
                ),
            ));
        }
        let own = budget::tokens(&note.body, ctx.ratio);
        if own > cfg.pinned_budget_tokens {
            return Err(reject(
                "pinned_body",
                format!(
                    "note is {own} tokens, pinned budget {}; cut it",
                    cfg.pinned_budget_tokens
                ),
            ));
        }
        let taken: u32 = others
            .iter()
            .map(|n| budget::tokens(&n.body, ctx.ratio))
            .sum();
        if taken.saturating_add(own) > cfg.pinned_budget_tokens {
            let names: Vec<&str> = others.iter().map(|n| n.name.as_str()).collect();
            return Err(reject(
                "pinned_sum",
                format!(
                    "pinned budget {} tokens: {} already take {taken}; unpin one",
                    cfg.pinned_budget_tokens,
                    names.join(" + ")
                ),
            ));
        }
    }
    if ctx.mode == Mode::Index {
        let tokens = budget::tokens(&render::index(after, cfg, ctx.ratio), ctx.ratio);
        if tokens > cfg.index_max_tokens {
            return Err(reject(
                "index_tokens",
                format!(
                    "index would be {tokens} tokens, limit {}; merge or delete first",
                    cfg.index_max_tokens
                ),
            ));
        }
    }
    Ok(())
}

/// Guard «прочти перед перезаписью» (`04 §7`): только в `index`.
fn check_guard(ctx: &Ctx<'_>, name: &str) -> Result<(), Rejected> {
    if ctx.mode == Mode::Index && !ctx.read_this_turn.contains(name) {
        return Err(reject(
            "guard",
            format!("read {name} with notes_read first, then rewrite it whole"),
        ));
    }
    Ok(())
}

fn fill(after: &[NoteRow], ctx: &Ctx<'_>) -> Fill {
    budget::fill(after, ctx.budget, ctx.ratio)
}

/// `notes_save`: создать или переписать целиком; существующее имя хранит
/// `created_at` и `pinned` (если не передан), старое тело — в `prev_body`.
pub fn save(
    conn: &Connection,
    cfg: &NotesConfig,
    ctx: &mut Ctx<'_>,
    args: save::Args,
) -> Result<Applied<save::Ok>, Error> {
    let name = check_name(&args.name)?;
    check_description(&args.description)?;
    check_body(&args.body, cfg, ctx.ratio)?;
    let notes = conn.notes().list(ctx.chat)?;
    let existing = notes.iter().find(|n| n.name == name).cloned();
    if existing.is_some() {
        check_guard(ctx, &name)?;
    }
    let note = match &existing {
        Some(old) => NoteRow {
            name: name.clone(),
            description: args.description,
            body: args.body,
            prev_body: Some(old.body.clone()),
            pinned: args.pinned.unwrap_or(old.pinned),
            created_at: old.created_at,
            updated_at: ctx.now,
        },
        None => NoteRow {
            name: name.clone(),
            description: args.description,
            body: args.body,
            prev_body: None,
            pinned: args.pinned.unwrap_or(false),
            created_at: ctx.now,
            updated_at: ctx.now,
        },
    };
    let after = with(notes, note.clone());
    check_limits(&after, &note, ctx, cfg)?;
    if existing.is_some() {
        conn.notes().save(ctx.chat, &note)?;
    } else {
        conn.notes().insert(ctx.chat, &note)?;
    }
    ctx.read_this_turn.insert(name.clone());
    let entry = render::entry(&after, &name, ctx.mode, cfg, ctx.ratio);
    Ok(Applied {
        ok: save::Ok {
            name: name.clone(),
            replaced: existing.is_some(),
            fill: fill(&after, ctx),
        },
        event: NoteUpdated {
            name,
            op: Op::Saved,
            entry,
        },
    })
}

/// `notes_update`: частичная правка; `body` заменяется целиком.
pub fn update(
    conn: &Connection,
    cfg: &NotesConfig,
    ctx: &mut Ctx<'_>,
    args: update::Args,
) -> Result<Applied<update::Ok>, Error> {
    let name = check_name(&args.name)?;
    if args.description.is_none() && args.body.is_none() && args.pinned.is_none() {
        return Err(reject(
            "empty_update",
            String::from("nothing to update: pass description, body or pinned"),
        )
        .into());
    }
    let notes = conn.notes().list(ctx.chat)?;
    let old = notes
        .iter()
        .find(|n| n.name == name)
        .cloned()
        .ok_or_else(|| reject("not_found", format!("note {name} not found")))?;
    let mut fields = Vec::new();
    let mut note = NoteRow {
        updated_at: ctx.now,
        ..old.clone()
    };
    if let Some(description) = args.description {
        check_description(&description)?;
        note.description = description;
        fields.push(String::from("description"));
    }
    if let Some(body) = args.body {
        check_body(&body, cfg, ctx.ratio)?;
        check_guard(ctx, &name)?;
        if body != old.body {
            note.prev_body = Some(old.body.clone());
        }
        note.body = body;
        fields.push(String::from("body"));
    }
    if let Some(pinned) = args.pinned {
        note.pinned = pinned;
        fields.push(String::from("pinned"));
    }
    let after = with(notes, note.clone());
    check_limits(&after, &note, ctx, cfg)?;
    conn.notes().save(ctx.chat, &note)?;
    ctx.read_this_turn.insert(name.clone());
    let entry = render::entry(&after, &name, ctx.mode, cfg, ctx.ratio);
    Ok(Applied {
        ok: update::Ok {
            name: name.clone(),
            fields,
            fill: fill(&after, ctx),
        },
        event: NoteUpdated {
            name,
            op: Op::Updated,
            entry,
        },
    })
}

/// `notes_delete`: soft-delete; отсутствие имени — ошибка, не тихий успех.
pub fn delete(
    conn: &Connection,
    _cfg: &NotesConfig,
    ctx: &mut Ctx<'_>,
    args: delete::Args,
) -> Result<Applied<delete::Ok>, Error> {
    let name = check_name(&args.name)?;
    if !conn.notes().delete(ctx.chat, &name, ctx.now)? {
        return Err(reject("not_found", format!("note {name} not found")).into());
    }
    let after = conn.notes().list(ctx.chat)?;
    ctx.read_this_turn.remove(&name);
    Ok(Applied {
        ok: delete::Ok {
            name: name.clone(),
            fill: fill(&after, ctx),
        },
        event: NoteUpdated {
            name,
            op: Op::Deleted,
            entry: String::new(),
        },
    })
}

/// `notes_read`: тела в порядке секции; имена попадают в `read_this_turn`.
pub fn read(
    conn: &Connection,
    cfg: &NotesConfig,
    ctx: &mut Ctx<'_>,
    args: read::Args,
) -> Result<read::Ok, Error> {
    if args.names.is_empty() || args.names.len() > cfg.max_notes as usize {
        return Err(reject(
            "names",
            format!(
                "names: 1..{} names, got {}",
                cfg.max_notes,
                args.names.len()
            ),
        )
        .into());
    }
    let wanted: BTreeSet<String> = args.names.iter().map(|n| normalize(n)).collect();
    let notes = conn.notes().list(ctx.chat)?;
    let found: Vec<NoteRow> = notes
        .into_iter()
        .filter(|n| wanted.contains(&n.name))
        .collect();
    let missing: Vec<String> = wanted
        .iter()
        .filter(|w| !found.iter().any(|n| &n.name == *w))
        .cloned()
        .collect();
    for n in &found {
        ctx.read_this_turn.insert(n.name.clone());
    }
    Ok(read::Ok {
        text: render::read(&found, &missing),
    })
}
