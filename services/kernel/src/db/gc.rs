//! Таблица `gc_runs` и уборка `08 §3`: жертвы по порогам, удаление, каскад.
//! Пороги считает компонент: здесь только `Timestamp`'ы и `keep`.

use eva::{data, time::Timestamp};
use rusqlite::{Connection, OptionalExtension as _, named_params};

use eva_protocol::types::{BlobId, ChatId, ModuleId, RunId, SurfaceId};

use crate::db::{DbResult, chats::ChatRef, sql::Ts, surfaces::SurfaceRef};

/// Последний завершённый прогон для gauge'а `gc_duration_last`.
#[data(copy)]
pub struct GcRun {
    pub started: Timestamp,
    pub finished: Timestamp,
}

/// Границы «старше чем» на момент уборки.
#[data(copy)]
pub struct Cutoffs {
    pub now: Timestamp,
    pub ephemeral_idle: Timestamp,
    pub deleted: Timestamp,
    pub memory_archive: Timestamp,
    pub memory_history: Timestamp,
    pub embed_cache: Timestamp,
    pub pending: Timestamp,
}

pub struct Gc<'c>(pub &'c Connection);

impl Gc<'_> {
    /// Чаты-жертвы с каскадом подагентов (`08 §3.3` п.2); `undeletable` не
    /// попадают — триггер `chats_undeletable_purge` оборвал бы транзакцию.
    pub fn chat_victims(&self, cut: &Cutoffs) -> DbResult<Vec<ChatRef>> {
        let mut stmt = self.0.prepare(
            "WITH RECURSIVE v(id) AS (
               SELECT id FROM chats
                WHERE undeletable = 0
                  AND ((kind = 'ephemeral' AND pinned = 0 AND last_activity_at <= :idle)
                       OR deleted_at <= :deleted)
               UNION
               SELECT a.id FROM agents a JOIN v ON a.parent_chat_id = v.id
                 JOIN chats c ON c.id = a.id WHERE c.undeletable = 0)
             SELECT c.id, f.surface_id, s.module FROM v
               JOIN chats c ON c.id = v.id
               JOIN folders f ON f.id = c.folder_id
               JOIN surfaces s ON s.id = f.surface_id
              ORDER BY c.id",
        )?;
        let rows = stmt
            .query_map(
                named_params! {
                    ":idle": Ts(cut.ephemeral_idle),
                    ":deleted": Ts(cut.deleted),
                },
                |row| {
                    Ok(ChatRef {
                        id: ChatId(row.get(0)?),
                        surface: SurfaceRef {
                            id: SurfaceId(row.get(1)?),
                            module: row.get::<_, Option<String>>(2)?.map(ModuleId),
                        },
                    })
                },
            )?
            .collect::<Result<_, _>>()?;
        Ok(rows)
    }

    /// Каскад уносит таблицы первой строки `10 §6`; `blobs.chat_id → NULL`.
    pub fn delete_chats(&self, ids: &[ChatId]) -> DbResult<usize> {
        let mut stmt = self.0.prepare("DELETE FROM chats WHERE id = ?1")?;
        let mut deleted = 0;
        for id in ids {
            deleted += stmt.execute([id.get()])?;
        }
        Ok(deleted)
    }

    /// Просроченные blob'ы, кроме тех, чей id встречается в событиях открытого
    /// сегмента: модель ещё видит файл в контексте. Файл — каталог `<id>`.
    pub fn expired_blobs(&self, now: Timestamp) -> DbResult<Vec<BlobId>> {
        let mut stmt = self.0.prepare(
            "SELECT b.id FROM blobs b
              WHERE b.expires_at <= ?1
                AND NOT EXISTS (SELECT 1 FROM events e JOIN segments g ON g.id = e.segment_id
                                 WHERE g.ended_at IS NULL AND instr(e.payload, CAST(b.id AS BLOB)) > 0)
              ORDER BY b.id",
        )?;
        let rows = stmt
            .query_map([Ts(now)], |row| row.get(0).map(BlobId))?
            .collect::<Result<_, _>>()?;
        Ok(rows)
    }

    pub fn delete_blobs(&self, ids: &[BlobId]) -> DbResult<usize> {
        let mut stmt = self.0.prepare("DELETE FROM blobs WHERE id = ?1")?;
        let mut deleted = 0;
        for id in ids {
            deleted += stmt.execute([id.as_str()])?;
        }
        Ok(deleted)
    }

    /// Все id `blobs`: каталог без строки — сирота (`08 §3.2`).
    pub fn blob_ids(&self) -> DbResult<Vec<String>> {
        let mut stmt = self.0.prepare("SELECT id FROM blobs")?;
        let rows = stmt
            .query_map([], |row| row.get(0))?
            .collect::<Result<_, _>>()?;
        Ok(rows)
    }

    /// Забытые записи с архивом старше порога и без restore (`03 §9`).
    pub fn purge_memory_forgotten(&self, before: Timestamp) -> DbResult<usize> {
        Ok(self.0.execute(
            "DELETE FROM memory WHERE id IN
               (SELECT memory_id FROM memory_archive WHERE archived_at <= ?1 AND restored_at IS NULL)",
            [Ts(before)],
        )?)
    }

    /// Закрытые версии старше `memory.history_ttl`, у которых есть преемник.
    pub fn purge_memory_superseded(&self, before: Timestamp) -> DbResult<usize> {
        Ok(self.0.execute(
            "DELETE FROM memory WHERE valid_to <= ?1
                AND EXISTS (SELECT 1 FROM memory n WHERE n.supersedes = memory.id)",
            [Ts(before)],
        )?)
    }

    pub fn purge_notes(&self, before: Timestamp) -> DbResult<usize> {
        Ok(self.0.execute(
            "DELETE FROM chat_notes WHERE deleted_at <= ?1",
            [Ts(before)],
        )?)
    }

    pub fn purge_tool_pins(&self, now: Timestamp) -> DbResult<usize> {
        Ok(self
            .0
            .execute("DELETE FROM tool_pins WHERE until <= ?1", [Ts(now)])?)
    }

    pub fn purge_embed_cache(&self, before: Timestamp) -> DbResult<usize> {
        Ok(self.0.execute(
            "DELETE FROM embed_cache WHERE created_at <= ?1",
            [Ts(before)],
        )?)
    }

    pub fn purge_pending(&self, before: Timestamp) -> DbResult<usize> {
        Ok(self
            .0
            .execute("DELETE FROM pending WHERE resolved_at <= ?1", [Ts(before)])?)
    }

    /// Строка прогона; `deleted_json` — объект `сущность → строк`.
    pub fn insert_run(
        &self,
        run: &RunId,
        started: Timestamp,
        finished: Timestamp,
        ok: bool,
        deleted_json: &str,
    ) -> DbResult<()> {
        self.0.execute(
            "INSERT INTO gc_runs (run_id, started, finished, ok, deleted_json) VALUES (?1, ?2, ?3, ?4, ?5)",
            (run.as_str(), Ts(started), Ts(finished), ok, deleted_json),
        )?;
        Ok(())
    }

    pub fn last(&self) -> DbResult<Option<GcRun>> {
        Ok(self
            .0
            .query_row(
                "SELECT started, finished FROM gc_runs WHERE finished IS NOT NULL
                  ORDER BY started DESC, run_id DESC LIMIT 1",
                [],
                |row| {
                    Ok(GcRun {
                        started: row.get::<_, Ts>(0)?.0,
                        finished: row.get::<_, Ts>(1)?.0,
                    })
                },
            )
            .optional()?)
    }

    pub fn purge_runs(&self, keep: u32) -> DbResult<usize> {
        Ok(self.0.execute(
            "DELETE FROM gc_runs WHERE run_id NOT IN
               (SELECT run_id FROM gc_runs ORDER BY started DESC, run_id DESC LIMIT ?1)",
            [keep],
        )?)
    }
}

/// После коммита, вне транзакции (`08 §3.3` п.5): WAL в файл, свободные страницы — ОС.
pub fn vacuum(conn: &Connection) -> DbResult<()> {
    conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE); PRAGMA incremental_vacuum;")?;
    Ok(())
}
