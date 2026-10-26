//! Компонент `notes` (`04`): бюджет, автомат режимов, рендер секции,
//! лестница давления и тулы блокнота. Все функции чистые или над
//! соединением БД; движок туров складывает их в транзакцию старта тура и
//! шага.
//!
//! `turns::integration` связывает вход в режим, reminder и метрики с туром.

use eva_protocol::types::{ChatId, note::Fill};

use crate::db::{Connection, DbResult, Queries as _, notes::NoteRow};

pub mod budget;
pub mod pressure;
pub mod render;
pub mod state;
pub mod tools;

/// Живые заметки чата и их заполненность при бюджете `budget`.
pub fn observe(
    conn: &Connection,
    chat: ChatId,
    budget: u32,
    ratio: Option<f64>,
) -> DbResult<(Vec<NoteRow>, Fill)> {
    let notes = conn.notes().list(chat)?;
    let fill = budget::fill(&notes, budget, ratio);
    Ok((notes, fill))
}

/// Ключ метрики `notes.mode_transition`.
pub fn transition_key(
    from: eva_protocol::types::note::Mode,
    to: eva_protocol::types::note::Mode,
) -> String {
    format!("{from}→{to}")
}
