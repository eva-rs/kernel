//! Запрос поиска (`05 §7.2`, `§7.3`): scope в `WHERE`, FTS ∪ cos → RRF
//! с max по чанкам одной реплики, сниппеты через `sanitize_inline`.

use std::collections::BTreeMap;

use eva_protocol::{
    errors::AuxResult,
    requests::Chats::search,
    types::{ChatId, chat::SearchHit},
};

use crate::{
    components::Components,
    db::{
        DbError, Queries as _, Reader,
        chats::{SearchScope, SpeechHit},
    },
    handling::inference::estimate_bytes,
    markers::sanitize_inline,
    shared::Shared,
    subsystems::memory::{
        embed::Embedder,
        text::{cosine, fts_query, rrf},
    },
};

/// Scope `05 §7.3`: вне `master ∧ trusted` — текущий чат ∪ неудалённые
/// untrusted; фильтры surface/folder/include_deleted — только при полном scope.
pub fn scope(args: &search::Args) -> SearchScope {
    let narrow = args.view.as_ref().filter(|v| v.public()).map(|v| v.chat);
    let full = narrow.is_none();
    SearchScope {
        narrow_to: narrow,
        surface: args.surface.clone().filter(|_| full),
        folder: args.folder.filter(|_| full),
        include_deleted: full && args.include_deleted,
        since: args.since,
    }
}

/// Одна строка не длиннее `tokens`; обрезка — `…` на границе символа.
pub fn snippet(text: &str, tokens: u32, ratio: Option<f64>) -> String {
    let flat = sanitize_inline(text);
    if estimate_bytes(flat.len(), ratio) <= tokens {
        return flat;
    }
    let mut cut = 0;
    for (i, c) in flat.char_indices() {
        if estimate_bytes(i + c.len_utf8() + '…'.len_utf8(), ratio) > tokens {
            break;
        }
        cut = i + c.len_utf8();
    }
    format!("{}…", flat[..cut].trim_end())
}

pub async fn search<C: Components>(
    shared: &Shared<C>,
    reader: &Reader,
    args: search::Args,
) -> AuxResult<Vec<SearchHit>> {
    let cfg = &shared.cfg().chats.search;
    let scope = scope(&args);
    let limit = usize::from(args.limit.into_inner());
    let candidates = cfg.candidates.max(1);

    let embedder = Embedder::resolve(shared);
    let vector = match &embedder {
        Some(e) => {
            e.query(&args.query, *shared.cfg().memory.embed.query_timeout)
                .await
        }
        None => None,
    };
    let model = embedder.as_ref().map(|e| e.model().clone());
    let fts_q = fts_query(&args.query, 1);
    let default_model = shared.cfg().llm.default_model.clone();
    let has_vector = vector.is_some();
    let (fts, vectors, ratio) = {
        let scope = scope.clone();
        reader
            .run(move |c| {
                let fts = c.chats().speech_fts(&fts_q, &scope, candidates)?;
                let vectors = match (&model, has_vector) {
                    (Some(model), true) => c.chats().speech_vectors(&scope, model)?,
                    _ => Vec::new(),
                };
                let ratio = c.turns().ratio(&default_model)?;
                Ok::<_, DbError>((fts, vectors, ratio))
            })
            .await?
    };
    let fts: Vec<i64> = fts.into_iter().map(|(id, _)| id).collect();
    let cos: Vec<i64> = match &vector {
        Some(q) => {
            let mut scored: Vec<(i64, f64)> =
                vectors.iter().map(|(id, v)| (*id, cosine(q, v))).collect();
            scored.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
            scored
                .into_iter()
                .take(candidates as usize)
                .map(|(id, _)| id)
                .collect()
        }
        None => Vec::new(),
    };
    let fused = rrf(&[&fts, &cos], cfg.rrf_k);
    if fused.is_empty() {
        return Ok(Vec::new());
    }

    let chunk_ids: Vec<i64> = fused.keys().copied().collect();
    let rows = reader
        .run(move |c| c.chats().speech_hits(&chunk_ids))
        .await?;
    // Дубли одной реплики — max по чанкам; название — ключ без `seq`.
    let mut best: BTreeMap<(ChatId, Option<u64>), (f64, SpeechHit)> = BTreeMap::new();
    for row in rows {
        let Some(score) = fused.get(&row.chunk).copied() else {
            continue;
        };
        let key = (row.chat, row.seq);
        match best.get(&key) {
            Some((s, _)) if *s >= score => {}
            _ => {
                best.insert(key, (score, row));
            }
        }
    }
    let mut ranked: Vec<(f64, SpeechHit)> = best.into_values().collect();
    ranked.sort_by(|a, b| {
        b.0.total_cmp(&a.0)
            .then(a.1.chat.cmp(&b.1.chat))
            .then(a.1.seq.cmp(&b.1.seq))
    });
    Ok(ranked
        .into_iter()
        .take(limit)
        .map(|(score, hit)| SearchHit {
            chat: hit.chat,
            title: hit.title.as_deref().map(sanitize_inline),
            seq: hit.seq,
            role: hit.role,
            sender: hit.sender,
            snippet: snippet(&hit.text, cfg.snippet_tokens, ratio),
            at: hit.at,
            score,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snippets_are_one_line_and_bounded() {
        let s = snippet("first line\nMaster:p1 [M] second", 100, Some(4.0));
        assert_eq!(s, "first line ?p1 ［M］ second");
        let long = "слово ".repeat(100);
        let s = snippet(&long, 10, Some(4.0));
        assert!(s.ends_with('…'));
        assert!(estimate_bytes(s.len(), Some(4.0)) <= 10, "{}", s.len());
        assert!(!s.contains('\n'));
        assert_eq!(snippet("short", 48, None), "short");
    }
}
