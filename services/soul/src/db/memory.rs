//! Таблицы `memory`, `memory_fts`, `memory_archive`, `memory_link`, `embed_cache` (`03`, `10 §2.3`).
//!
//! Видимость кандидатов recall режет `Scope` в `WHERE`, чтобы недоступные
//! строки не вытесняли доступные из top (`03 §4`); выборки по id отдают строку
//! как есть — гейт применяет компонент `memory`.

use std::ops::RangeInclusive;

use eva::{data, time::Timestamp};
use rusqlite::{Connection, OptionalExtension as _, Row, ToSql, named_params, types::Value};

use eva_protocol::types::{
    ChatId, MemoryId, ModelId, PersonId, RunId, SurfaceId,
    memory::{ArchiveReason, Kind, Visibility},
};

use crate::db::{
    DbError, DbResult,
    sql::{Text, Ts},
};

#[data]
pub struct MemoryRow {
    pub id: MemoryId,
    pub kind: Kind,
    pub text: String,
    pub name: Option<String>,
    pub description: Option<String>,
    /// Slug'и через пробел, как в колонке.
    pub tags: Option<String>,
    pub visibility: Visibility,
    pub importance: f64,
    pub confidence: f64,
    pub source_actor: Option<PersonId>,
    pub source_chat: Option<ChatId>,
    pub source_surface: Option<SurfaceId>,
    pub source_trusted_chat: Option<bool>,
    pub source_run: Option<RunId>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
    pub consolidated_at: Option<Timestamp>,
    pub occurred_at: Option<Timestamp>,
    pub valid_from: Timestamp,
    pub valid_to: Option<Timestamp>,
    pub supersedes: Option<MemoryId>,
    pub version: u32,
    pub last_reinforced_at: Timestamp,
    pub access_count: f64,
    pub embed_model: Option<ModelId>,
    pub archive: Option<ArchiveRow>,
}

#[data]
pub struct ArchiveRow {
    pub reason: ArchiveReason,
    pub why: String,
    pub run: Option<RunId>,
    pub actor: Option<PersonId>,
    pub archived_at: Timestamp,
    pub restored_at: Option<Timestamp>,
}

impl ArchiveRow {
    /// Забыта: архив есть и restore его не снял.
    pub const fn is_open(&self) -> bool {
        self.restored_at.is_none()
    }
}

/// Фильтры `memory.list`; `None` — без условия.
#[data]
#[derive(Default)]
pub struct ListFilter {
    pub kind: Option<Kind>,
    pub visibility: Option<Visibility>,
    pub since: Option<Timestamp>,
}

/// Какие версии и какие записи считать кандидатами recall/дедупа (`03 §4` п.1).
#[derive(Debug, Clone, PartialEq)]
pub struct Scope {
    /// Версии, истинные в этот момент; `None` — текущие.
    pub at: Option<Timestamp>,
    pub kind: Option<Kind>,
    /// Только `event`: `occurred_at ≥ since`.
    pub since: Option<Timestamp>,
    pub until: Option<Timestamp>,
    /// Отрезок цепочки `public ⊂ trusted ⊂ private`: фильтр любого тура — её
    /// нижний отрезок (`03 §2`), отсечённое им — верхний.
    pub visibility: RangeInclusive<Visibility>,
}

impl Default for Scope {
    fn default() -> Self {
        Self {
            at: None,
            kind: None,
            since: None,
            until: None,
            visibility: Visibility::Public..=Visibility::Private,
        }
    }
}

impl Scope {
    const SQL: &'static str = "((:at IS NULL AND m.valid_to IS NULL)
                OR (:at IS NOT NULL AND m.valid_from <= :at AND (m.valid_to IS NULL OR m.valid_to > :at)))
           AND (:kind IS NULL OR m.kind = :kind)
           AND (:since IS NULL OR (m.kind = 'event' AND m.occurred_at >= :since))
           AND (:until IS NULL OR (m.kind = 'event' AND m.occurred_at <= :until))
           AND (CASE m.visibility WHEN 'public' THEN 0 WHEN 'trusted' THEN 1 ELSE 2 END)
               BETWEEN :vis_lo AND :vis_hi";

    /// Только видимости не строже `ceiling`.
    pub fn up_to(self, ceiling: Visibility) -> Self {
        Self {
            visibility: Visibility::Public..=ceiling,
            ..self
        }
    }

    /// Видимости строже верхней границы — то, что фильтр отсёк; `None` — ничего.
    pub fn hidden(&self) -> Option<Self> {
        let stricter = match self.visibility.end() {
            Visibility::Public => Visibility::Trusted,
            Visibility::Trusted => Visibility::Private,
            Visibility::Private => return None,
        };
        Some(Self {
            visibility: stricter..=Visibility::Private,
            ..self.clone()
        })
    }

    fn params(&self) -> Vec<(&'static str, Value)> {
        let ts = |t: Option<Timestamp>| {
            t.and_then(|t| Ts(t).nanos().ok())
                .map_or(Value::Null, Value::Integer)
        };
        vec![
            (":at", ts(self.at)),
            (
                ":kind",
                self.kind
                    .map_or(Value::Null, |k| Value::Text(k.to_string())),
            ),
            (":since", ts(self.since)),
            (":until", ts(self.until)),
            (":vis_lo", Value::Integer(rank(*self.visibility.start()))),
            (":vis_hi", Value::Integer(rank(*self.visibility.end()))),
        ]
    }
}

const fn rank(visibility: Visibility) -> i64 {
    match visibility {
        Visibility::Public => 0,
        Visibility::Trusted => 1,
        Visibility::Private => 2,
    }
}

/// Новая строка: вставка `remember`/`supersede`; `embedding` считает воркер.
#[data]
pub struct NewMemory {
    pub kind: Kind,
    pub text: String,
    pub name: Option<String>,
    pub description: Option<String>,
    pub tags: Option<String>,
    pub visibility: Visibility,
    pub importance: f64,
    pub confidence: f64,
    pub source_actor: Option<PersonId>,
    pub source_chat: Option<ChatId>,
    pub source_surface: Option<SurfaceId>,
    pub source_trusted_chat: Option<bool>,
    pub source_run: Option<RunId>,
    pub occurred_at: Option<Timestamp>,
    pub valid_from: Timestamp,
    pub supersedes: Option<MemoryId>,
    pub version: u32,
    pub last_reinforced_at: Timestamp,
    pub access_count: f64,
    pub now: Timestamp,
}

/// Метаданные, которые слияние правит на месте (`03 §6`).
#[data]
pub struct Meta {
    pub kind: Kind,
    pub visibility: Visibility,
    pub importance: f64,
    pub version: u32,
    pub now: Timestamp,
}

const SELECT: &str = "SELECT m.id, m.kind, m.text, m.name, m.description, m.tags, m.visibility,
       m.importance, m.confidence, m.source_actor, m.source_chat, m.source_surface,
       m.source_trusted_chat, m.source_run_id, m.created_at, m.updated_at, m.consolidated_at,
       m.occurred_at, m.valid_from, m.valid_to, m.supersedes, m.version, m.last_reinforced_at,
       m.access_count, m.embed_model,
       a.reason AS a_reason, a.why AS a_why, a.run_id AS a_run_id, a.actor AS a_actor,
       a.archived_at AS a_archived_at, a.restored_at AS a_restored_at
  FROM memory m LEFT JOIN memory_archive a ON a.memory_id = m.id";

/// Вся цепочка версий `:id`: предки по `supersedes` и потомки.
const CHAIN: &str = "WITH RECURSIVE
  up(id, sup) AS (
    SELECT id, supersedes FROM memory WHERE id = :id
    UNION SELECT p.id, p.supersedes FROM memory p JOIN up ON p.id = up.sup),
  down(id) AS (
    SELECT id FROM memory WHERE id = :id
    UNION SELECT n.id FROM memory n JOIN down ON n.supersedes = down.id),
  chain(id) AS (SELECT id FROM up UNION SELECT id FROM down)";

pub struct Memory<'c>(pub &'c Connection);

impl Memory<'_> {
    pub fn get(&self, id: MemoryId) -> DbResult<Option<MemoryRow>> {
        let sql = format!("{SELECT} WHERE m.id = ?1");
        Ok(self.0.query_row(&sql, [id.get()], from_row).optional()?)
    }

    /// Строки по id, в порядке id.
    pub fn rows(&self, ids: &[MemoryId]) -> DbResult<Vec<MemoryRow>> {
        let sql = format!("{SELECT} WHERE m.id = ?1");
        let mut stmt = self.0.prepare(&sql)?;
        let mut rows = Vec::with_capacity(ids.len());
        let mut sorted: Vec<i64> = ids.iter().map(|id| id.get()).collect();
        sorted.sort_unstable();
        sorted.dedup();
        for id in sorted {
            if let Some(row) = stmt.query_row([id], from_row).optional()? {
                rows.push(row);
            }
        }
        Ok(rows)
    }

    /// Текущие версии, keyset по `id DESC`; `since` — по `created_at`.
    pub fn list(
        &self,
        filter: &ListFilter,
        after: Option<MemoryId>,
        limit: u16,
    ) -> DbResult<Vec<MemoryRow>> {
        let sql = format!(
            "{SELECT}
              WHERE m.valid_to IS NULL
                AND (:kind IS NULL OR m.kind = :kind)
                AND (:visibility IS NULL OR m.visibility = :visibility)
                AND (:since IS NULL OR m.created_at >= :since)
                AND (:after IS NULL OR m.id < :after)
              ORDER BY m.id DESC LIMIT :limit"
        );
        let mut stmt = self.0.prepare(&sql)?;
        let rows = stmt
            .query_map(
                named_params! {
                    ":kind": filter.kind.map(Text),
                    ":visibility": filter.visibility.map(Text),
                    ":since": filter.since.map(Ts),
                    ":after": after.map(MemoryId::get),
                    ":limit": limit,
                },
                from_row,
            )?
            .collect::<Result<_, _>>()?;
        Ok(rows)
    }

    /// Все текущие версии: hot-блок и дедуп режут их в Rust.
    pub fn current(&self) -> DbResult<Vec<MemoryRow>> {
        let sql = format!("{SELECT} WHERE m.valid_to IS NULL ORDER BY m.id");
        let mut stmt = self.0.prepare(&sql)?;
        let rows = stmt.query_map([], from_row)?.collect::<Result<_, _>>()?;
        Ok(rows)
    }

    /// Цепочка версий записи от корня к новейшей.
    pub fn chain(&self, id: MemoryId) -> DbResult<Vec<MemoryRow>> {
        let sql = format!(
            "{CHAIN} {SELECT} WHERE m.id IN (SELECT id FROM chain) ORDER BY m.version, m.valid_from, m.id"
        );
        let mut stmt = self.0.prepare(&sql)?;
        let rows = stmt
            .query_map(named_params! { ":id": id.get() }, from_row)?
            .collect::<Result<_, _>>()?;
        Ok(rows)
    }

    /// Текущая procedure с этим именем, кроме `except`: уникальный индекс `memory_procedure_name`.
    pub fn procedure_named(
        &self,
        name: &str,
        except: Option<MemoryId>,
    ) -> DbResult<Option<MemoryId>> {
        Ok(self
            .0
            .query_row(
                "SELECT id FROM memory
                  WHERE kind = 'procedure' AND valid_to IS NULL AND name = ?1 AND id IS NOT ?2",
                (name, except.map(MemoryId::get)),
                |row| row.get(0).map(MemoryId),
            )
            .optional()?)
    }

    pub fn insert(&self, new: &NewMemory) -> DbResult<MemoryId> {
        let id = self.0.query_row(
            "INSERT INTO memory (kind, text, name, description, tags, visibility, importance, confidence,
                source_actor, source_chat, source_surface, source_trusted_chat, source_run_id,
                created_at, updated_at, occurred_at, valid_from, supersedes, version,
                last_reinforced_at, access_count)
             VALUES (:kind, :text, :name, :description, :tags, :visibility, :importance, :confidence,
                :actor, :chat, :surface, :trusted_chat, :run,
                :now, :now, :occurred_at, :valid_from, :supersedes, :version,
                :reinforced, :access)
             RETURNING id",
            named_params! {
                ":kind": Text(new.kind),
                ":text": new.text,
                ":name": new.name,
                ":description": new.description,
                ":tags": new.tags,
                ":visibility": Text(new.visibility),
                ":importance": new.importance,
                ":confidence": new.confidence,
                ":actor": new.source_actor.as_ref().map(PersonId::as_str),
                ":chat": new.source_chat.map(ChatId::get),
                ":surface": new.source_surface.as_ref().map(SurfaceId::as_str),
                ":trusted_chat": new.source_trusted_chat,
                ":run": new.source_run.as_ref().map(RunId::as_str),
                ":now": Ts(new.now),
                ":occurred_at": new.occurred_at.map(Ts),
                ":valid_from": Ts(new.valid_from),
                ":supersedes": new.supersedes.map(MemoryId::get),
                ":version": new.version,
                ":reinforced": Ts(new.last_reinforced_at),
                ":access": new.access_count,
            },
            |row| row.get(0),
        )?;
        Ok(MemoryId(id))
    }

    /// Версия закрыта: `valid_to = at`.
    pub fn close(&self, id: MemoryId, at: Timestamp) -> DbResult<()> {
        let changed = self.0.execute(
            "UPDATE memory SET valid_to = ?2, updated_at = ?2 WHERE id = ?1 AND valid_to IS NULL",
            (id.get(), Ts(at)),
        )?;
        (changed == 1)
            .then_some(())
            .ok_or_else(|| DbError::corrupt(format_args!("close of {id} found no current row")))
    }

    /// Слияние только метаданных: строка та же, `version + 1`.
    pub fn set_meta(&self, id: MemoryId, meta: &Meta) -> DbResult<()> {
        self.0.execute(
            "UPDATE memory SET kind = :kind, visibility = :visibility, importance = :importance,
                    version = :version, updated_at = :now
              WHERE id = :id",
            named_params! {
                ":id": id.get(),
                ":kind": Text(meta.kind),
                ":visibility": Text(meta.visibility),
                ":importance": meta.importance,
                ":version": meta.version,
                ":now": Ts(meta.now),
            },
        )?;
        Ok(())
    }

    /// Теги слияния на месте: FTS перестраивает триггер.
    pub fn set_tags(&self, id: MemoryId, tags: Option<&str>) -> DbResult<()> {
        self.0.execute(
            "UPDATE memory SET tags = ?2 WHERE id = ?1",
            (id.get(), tags),
        )?;
        Ok(())
    }

    /// `archive` (`03 §9`): `valid_to = now` и строка архива; повторный forget
    /// после restore — та же строка с `restored_at := NULL`.
    pub fn archive(
        &self,
        id: MemoryId,
        reason: ArchiveReason,
        why: &str,
        run: Option<&RunId>,
        actor: Option<&PersonId>,
        now: Timestamp,
    ) -> DbResult<()> {
        self.close(id, now)?;
        self.0.execute(
            "INSERT INTO memory_archive (memory_id, reason, why, run_id, actor, archived_at, restored_at)
             VALUES (:id, :reason, :why, :run, :actor, :now, NULL)
             ON CONFLICT (memory_id) DO UPDATE SET reason = excluded.reason, why = excluded.why,
                run_id = excluded.run_id, actor = excluded.actor, archived_at = excluded.archived_at,
                restored_at = NULL",
            named_params! {
                ":id": id.get(),
                ":reason": Text(reason),
                ":why": why,
                ":run": run.map(RunId::as_str),
                ":actor": actor.map(PersonId::as_str),
                ":now": Ts(now),
            },
        )?;
        Ok(())
    }

    /// `restore` (`03 §9`): версия снова текущая, архив помечен; `access_count` не трогается.
    pub fn restore(&self, id: MemoryId, now: Timestamp) -> DbResult<()> {
        let reopened = self.0.execute(
            "UPDATE memory SET valid_to = NULL WHERE id = ?1",
            [id.get()],
        )?;
        let unarchived = self.0.execute(
            "UPDATE memory_archive SET restored_at = ?2 WHERE memory_id = ?1 AND restored_at IS NULL",
            (id.get(), Ts(now)),
        )?;
        if reopened == 1 && unarchived == 1 {
            Ok(())
        } else {
            Err(DbError::corrupt(format_args!(
                "restore of {id} touched {reopened} memory and {unarchived} archive rows"
            )))
        }
    }

    /// Подкрепление (`03 §3`): `access_count += delta`; `touch` сдвигает возраст.
    pub fn reinforce(
        &self,
        ids: &[MemoryId],
        delta: f64,
        touch: Option<Timestamp>,
    ) -> DbResult<()> {
        let mut stmt = self.0.prepare(
            "UPDATE memory SET access_count = access_count + :delta,
                    last_reinforced_at = COALESCE(:touch, last_reinforced_at)
              WHERE id = :id",
        )?;
        for id in ids {
            stmt.execute(named_params! {
                ":id": id.get(),
                ":delta": delta,
                ":touch": touch.map(Ts),
            })?;
        }
        Ok(())
    }

    /// Лексические кандидаты: `(id, bm25)` по возрастанию (меньше — лучше).
    pub fn fts(&self, query: &str, scope: &Scope, limit: u32) -> DbResult<Vec<(MemoryId, f64)>> {
        if query.is_empty() {
            return Ok(Vec::new());
        }
        let sql = format!(
            "SELECT m.id, bm25(memory_fts) AS rank
               FROM memory_fts JOIN memory m ON m.id = memory_fts.rowid
              WHERE memory_fts MATCH :query AND {}
              ORDER BY rank, m.id LIMIT :limit",
            Scope::SQL
        );
        let mut params = scope.params();
        params.push((":query", Value::Text(query.to_owned())));
        params.push((":limit", Value::Integer(i64::from(limit))));
        let mut stmt = self.0.prepare(&sql)?;
        let rows = stmt
            .query_map(bind(&params).as_slice(), |row| {
                Ok((MemoryId(row.get(0)?), row.get(1)?))
            })?
            .collect::<Result<_, _>>()?;
        Ok(rows)
    }

    /// Векторы кандидатов под активной моделью: cos считает вызывающий.
    pub fn vectors(&self, scope: &Scope, model: &ModelId) -> DbResult<Vec<(MemoryId, Vec<f32>)>> {
        let sql = format!(
            "SELECT m.id, m.embedding FROM memory m
              WHERE m.embedding IS NOT NULL AND m.embed_model = :model AND {}
              ORDER BY m.id",
            Scope::SQL
        );
        let mut params = scope.params();
        params.push((":model", Value::Text(model.as_str().to_owned())));
        let mut stmt = self.0.prepare(&sql)?;
        let rows = stmt
            .query_map(bind(&params).as_slice(), |row| {
                let blob: Vec<u8> = row.get(1)?;
                Ok((MemoryId(row.get(0)?), decode(&blob)))
            })?
            .collect::<Result<_, _>>()?;
        Ok(rows)
    }

    /// Очередь эмбеддера (`03 §8`): текущие строки без вектора активной модели;
    /// private — только если эмбеддер локальный.
    pub fn embed_queue(
        &self,
        model: &ModelId,
        include_private: bool,
        limit: u32,
    ) -> DbResult<Vec<(MemoryId, String)>> {
        let mut stmt = self.0.prepare(
            "SELECT id, text FROM memory
              WHERE valid_to IS NULL AND (embedding IS NULL OR embed_model IS NOT :model)
                AND (:private OR visibility <> 'private')
              ORDER BY id LIMIT :limit",
        )?;
        let rows = stmt
            .query_map(
                named_params! { ":model": model.as_str(), ":private": include_private, ":limit": limit },
                |row| Ok((MemoryId(row.get(0)?), row.get(1)?)),
            )?
            .collect::<Result<_, _>>()?;
        Ok(rows)
    }

    /// Gauge `memory.embed_pending`: размер очереди воркера.
    pub fn embed_pending(&self, model: &ModelId, include_private: bool) -> DbResult<u32> {
        Ok(self.0.query_row(
            "SELECT COUNT(*) FROM memory
              WHERE valid_to IS NULL AND (embedding IS NULL OR embed_model IS NOT :model)
                AND (:private OR visibility <> 'private')",
            named_params! { ":model": model.as_str(), ":private": include_private },
            |row| row.get(0),
        )?)
    }

    pub fn clear_embedding(&self, id: MemoryId) -> DbResult<()> {
        self.0.execute(
            "UPDATE memory SET embedding = NULL, embed_model = NULL WHERE id = ?1",
            [id.get()],
        )?;
        Ok(())
    }

    pub fn set_embedding(&self, id: MemoryId, model: &ModelId, vector: &[f32]) -> DbResult<()> {
        self.0.execute(
            "UPDATE memory SET embedding = ?2, embed_model = ?3 WHERE id = ?1",
            (id.get(), encode(vector), model.as_str()),
        )?;
        Ok(())
    }

    /// Вектор строки под `model`, если уже посчитан.
    pub fn embedding(&self, id: MemoryId, model: &ModelId) -> DbResult<Option<Vec<f32>>> {
        Ok(self
            .0
            .query_row(
                "SELECT embedding FROM memory WHERE id = ?1 AND embed_model = ?2 AND embedding IS NOT NULL",
                (id.get(), model.as_str()),
                |row| row.get::<_, Vec<u8>>(0).map(|blob| decode(&blob)),
            )
            .optional()?)
    }

    /// Кэш `blake3(text) → вектор` (`03 §8`).
    pub fn cached_vector(&self, hash: &[u8; 32], model: &ModelId) -> DbResult<Option<Vec<f32>>> {
        Ok(self
            .0
            .query_row(
                "SELECT embedding FROM embed_cache WHERE text_hash = ?1 AND embed_model = ?2",
                (hash.as_slice(), model.as_str()),
                |row| row.get::<_, Vec<u8>>(0).map(|blob| decode(&blob)),
            )
            .optional()?)
    }

    pub fn cache_vector(
        &self,
        hash: &[u8; 32],
        model: &ModelId,
        vector: &[f32],
        now: Timestamp,
    ) -> DbResult<()> {
        self.0.execute(
            "INSERT OR REPLACE INTO embed_cache (text_hash, embed_model, embedding, created_at)
             VALUES (?1, ?2, ?3, ?4)",
            (hash.as_slice(), model.as_str(), encode(vector), Ts(now)),
        )?;
        Ok(())
    }
}

fn bind<'p>(params: &'p [(&'static str, Value)]) -> Vec<(&'static str, &'p dyn ToSql)> {
    params.iter().map(|(k, v)| (*k, v as &dyn ToSql)).collect()
}

/// f32 little-endian подряд (`03 §1`).
pub fn encode(vector: &[f32]) -> Vec<u8> {
    vector.iter().flat_map(|x| x.to_le_bytes()).collect()
}

pub fn decode(blob: &[u8]) -> Vec<f32> {
    blob.chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

fn from_row(row: &Row<'_>) -> rusqlite::Result<MemoryRow> {
    let archive = match row.get::<_, Option<Text<ArchiveReason>>>("a_reason")? {
        Some(Text(reason)) => Some(ArchiveRow {
            reason,
            why: row.get("a_why")?,
            run: row.get::<_, Option<String>>("a_run_id")?.map(RunId),
            actor: row.get::<_, Option<String>>("a_actor")?.map(PersonId),
            archived_at: row.get::<_, Ts>("a_archived_at")?.0,
            restored_at: row.get::<_, Option<Ts>>("a_restored_at")?.map(|t| t.0),
        }),
        None => None,
    };

    Ok(MemoryRow {
        id: MemoryId(row.get("id")?),
        kind: row.get::<_, Text<Kind>>("kind")?.0,
        text: row.get("text")?,
        name: row.get("name")?,
        description: row.get("description")?,
        tags: row.get("tags")?,
        visibility: row.get::<_, Text<Visibility>>("visibility")?.0,
        importance: row.get("importance")?,
        confidence: row.get("confidence")?,
        source_actor: row.get::<_, Option<String>>("source_actor")?.map(PersonId),
        source_chat: row.get::<_, Option<i64>>("source_chat")?.map(ChatId),
        source_surface: row
            .get::<_, Option<String>>("source_surface")?
            .map(SurfaceId),
        source_trusted_chat: row.get("source_trusted_chat")?,
        source_run: row.get::<_, Option<String>>("source_run_id")?.map(RunId),
        created_at: row.get::<_, Ts>("created_at")?.0,
        updated_at: row.get::<_, Ts>("updated_at")?.0,
        consolidated_at: row.get::<_, Option<Ts>>("consolidated_at")?.map(|t| t.0),
        occurred_at: row.get::<_, Option<Ts>>("occurred_at")?.map(|t| t.0),
        valid_from: row.get::<_, Ts>("valid_from")?.0,
        valid_to: row.get::<_, Option<Ts>>("valid_to")?.map(|t| t.0),
        supersedes: row.get::<_, Option<i64>>("supersedes")?.map(MemoryId),
        version: row.get("version")?,
        last_reinforced_at: row.get::<_, Ts>("last_reinforced_at")?.0,
        access_count: row.get("access_count")?,
        embed_model: row.get::<_, Option<String>>("embed_model")?.map(ModelId),
        archive,
    })
}
