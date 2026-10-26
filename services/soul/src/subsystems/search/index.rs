//! Индексатор поиска (`05 §7.1`): речь → чанки `chat_speech` по
//! `indexed_at`, строки `title`, векторы чанков тем же эмбеддером, что у
//! памяти. Machine text и вложения не индексируются.

use eva_protocol::types::{ChatId, chat::Role, content::Block};

use crate::{
    components::Components,
    db::{Db as _, DbError, Queries as _, chats::Unindexed},
    handling::inference::estimate_bytes,
    markers::is_machine_text,
    shared::Shared,
    subsystems::memory::embed::{self, Embedder},
};

/// Чанки по границам строк: строка длиннее порога остаётся целой.
pub fn chunk(text: &str, chunk_tokens: u32, ratio: Option<f64>) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut current = String::new();
    for line in text.lines() {
        let candidate = if current.is_empty() {
            line.to_owned()
        } else {
            format!("{current}\n{line}")
        };
        if !current.is_empty() && estimate_bytes(candidate.len(), ratio) > chunk_tokens {
            chunks.push(std::mem::take(&mut current));
            current = line.to_owned();
        } else {
            current = candidate;
        }
    }
    if !current.trim().is_empty() {
        chunks.push(current);
    }
    chunks.retain(|c| !c.trim().is_empty());
    chunks
}

/// Текст речи реплики: `user` — тело без маркера, `assistant` — текстовые
/// блоки; machine text (I13) — `None`.
pub fn speech(row: &Unindexed) -> Option<String> {
    let text = row
        .blocks
        .iter()
        .filter_map(|b| match b {
            Block::Text { text } => Some(text.as_str()),
            Block::Image { .. } | Block::File { .. } => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    if text.trim().is_empty() {
        return None;
    }
    match row.role {
        Role::User if row.machine || is_machine_text(&text) => None,
        Role::User | Role::Assistant => Some(text),
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Indexed {
    pub messages: usize,
    pub chunks: usize,
    /// Строк `title`, созданных или заменённых.
    pub titles: usize,
}

/// Пачка `indexed_at IS NULL` (до `limit` реплик) и синхронизация названий.
pub async fn index_batch<C: Components>(
    shared: &Shared<C>,
    limit: u32,
) -> Result<Indexed, DbError> {
    let chunk_tokens = shared.cfg().chats.search.chunk_tokens;
    let model = shared.cfg().llm.default_model.clone();
    let (pending, ratio) = shared
        .db()
        .reader()
        .await?
        .run(move |c| Ok::<_, DbError>((c.chats().unindexed(limit)?, c.turns().ratio(&model)?)))
        .await?;
    let now = shared.now();
    let plan: Vec<(ChatId, i64, Vec<String>)> = pending
        .iter()
        .map(|row| {
            let chunks = speech(row).map_or_else(Vec::new, |t| chunk(&t, chunk_tokens, ratio));
            (row.chat, row.seq, chunks)
        })
        .collect();
    let messages = plan.len();
    let (chunks, titles) = shared
        .db()
        .write(move |tx| {
            let mut chunks = 0_usize;
            for (chat, seq, texts) in &plan {
                tx.chats().clear_speech(*chat, *seq)?;
                for (i, text) in texts.iter().enumerate() {
                    tx.chats().insert_speech(
                        *chat,
                        *seq,
                        u32::try_from(i).unwrap_or(u32::MAX),
                        text,
                    )?;
                    chunks += 1;
                }
                tx.chats().mark_indexed(*chat, *seq, now)?;
            }
            let titles = tx.chats().sync_titles()?;
            Ok::<_, DbError>((chunks, titles))
        })
        .await?;
    Ok(Indexed {
        messages,
        chunks,
        titles,
    })
}

/// Векторы чанков без вектора активной модели; речь trusted-чатов — только
/// локальному эмбеддеру (`05 §7.1` п.3).
pub async fn embed_chunks<C: Components>(
    shared: &Shared<C>,
    embedder: &Embedder<C>,
) -> Result<usize, embed::Error> {
    let batch = shared.cfg().memory.embed.batch.max(1);
    let mut done = 0;
    loop {
        let (model, local) = (embedder.model().clone(), embedder.local());
        let queue = shared
            .db()
            .reader()
            .await?
            .run(move |c| c.chats().speech_embed_queue(&model, local, batch))
            .await?;
        if queue.is_empty() {
            return Ok(done);
        }
        let texts: Vec<String> = queue.iter().map(|(_, text)| text.clone()).collect();
        let vectors = embedder
            .embed(texts)
            .await
            .map_err(embed::Error::Provider)?;
        let model = embedder.model().clone();
        let count = queue.len();
        shared
            .db()
            .write(move |tx| {
                for ((chunk, _), vector) in queue.iter().zip(&vectors) {
                    tx.chats().set_speech_vector(*chunk, &model, vector)?;
                }
                Ok::<(), DbError>(())
            })
            .await?;
        done += count;
        if count < batch as usize {
            return Ok(done);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunks_split_on_line_boundaries() {
        let text = "aaaa\nbbbb\ncccc\ndddd";
        // 3 токена по ratio 4 = 12 байт: по два слова на чанк ("aaaa\nbbbb" = 9 байт).
        assert_eq!(chunk(text, 3, Some(4.0)), ["aaaa\nbbbb", "cccc\ndddd"]);
        assert_eq!(
            chunk("one long line that never splits", 1, Some(4.0)),
            ["one long line that never splits"]
        );
        assert!(chunk("\n\n", 10, None).is_empty());
        assert_eq!(chunk("x", 512, None), ["x"]);
    }

    #[test]
    fn speech_skips_machine_text_and_attachments() {
        let row = |role, machine, blocks: Vec<Block>| Unindexed {
            chat: ChatId(1),
            seq: 1,
            role,
            trusted_chat: false,
            machine,
            blocks,
        };
        assert_eq!(
            speech(&row(
                Role::User,
                false,
                vec![Block::text("hello"), Block::File { blob: "b1".into() }]
            )),
            Some(String::from("hello"))
        );
        assert_eq!(
            speech(&row(
                Role::User,
                true,
                vec![Block::text("[compaction summary]\n| x")]
            )),
            None
        );
        assert_eq!(
            speech(&row(
                Role::User,
                false,
                vec![Block::text("[notify] subject")]
            )),
            None
        );
        assert_eq!(
            speech(&row(Role::Assistant, true, vec![Block::text("reply")])),
            Some(String::from("reply"))
        );
        assert_eq!(
            speech(&row(
                Role::Assistant,
                false,
                vec![Block::File { blob: "b1".into() }]
            )),
            None
        );
    }
}
