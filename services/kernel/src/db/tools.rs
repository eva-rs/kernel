//! Таблицы `tools`, `tool_groups`, `tool_overrides`, `tool_pins`,
//! `chat_warm_sets`, `chat_loaded_tools` (`02`, `10 §2.2`).

use std::collections::{BTreeMap, BTreeSet};

use eva::{data, time::Timestamp};
use rusqlite::{Connection, OptionalExtension as _, Row, named_params};

use eva_protocol::types::{ChatId, FolderId, ModuleId, ToolId, tool::ToolDecl};

use crate::db::{
    DbError, DbResult,
    sql::{Text, Ts},
};

/// Модуль тулов ядра (`06 §2`).
pub const KERNEL: &str = "kernel";

/// Строка `tools` без `retired_at`.
#[data]
pub struct ToolRow {
    pub id: ToolId,
    pub module: ModuleId,
    pub path: String,
    pub canonical: String,
    pub decl: ToolDecl,
    pub online: bool,
}

/// Декларация к регистрации: `canonical` и `spec_hash` считает реестр.
pub struct NewTool<'a> {
    pub module: &'a ModuleId,
    pub decl: &'a ToolDecl,
    pub canonical: &'a str,
    pub spec_hash: &'a [u8],
}

/// Оверрайд хозяина (`02 §3.3`); `None` — не трогать.
#[data(copy)]
#[derive(Default)]
pub struct OverrideRow {
    pub master_only: Option<bool>,
    pub trusted_only: Option<bool>,
    pub forbid: bool,
    pub attention: Option<Attention>,
}

#[data(copy, ord)]
pub enum Attention {
    Always,
    Reserved,
}

pub struct Tools<'c>(pub &'c Connection);

#[data]
pub struct UsageRow {
    pub day: String,
    pub chat: ChatId,
    pub folder: Option<FolderId>,
    pub tool: ToolId,
    pub turns_with_call: u32,
    pub loads: u32,
    pub calls: u32,
}

#[data]
pub struct ActivityRow {
    pub day: String,
    pub chat: ChatId,
    pub folder: Option<FolderId>,
}

impl Tools<'_> {
    /// Канон с текущим размещением: сохранённые whitelist продолжают ссылаться на id.
    pub fn canonical_name(&self, id: ToolId) -> DbResult<Option<String>> {
        let row: Option<(String, String, Option<String>)> = self
            .0
            .query_row(
                "SELECT t.canonical, t.path, p.path FROM tools t
               LEFT JOIN tool_placements p ON p.tool_id = t.tool_id WHERE t.tool_id = ?1",
                [id.get()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        Ok(row.map(|(canonical, path, group)| {
            group.map_or(canonical, |group| {
                format!("{group}.{}", path.rsplit('.').next().unwrap_or(&path)).replace('.', "_")
            })
        }))
    }

    /// Действующие строки реестра по `tool_id`.
    pub fn live(&self) -> DbResult<Vec<ToolRow>> {
        let mut stmt = self.0.prepare(
            "SELECT tool_id, module, path, canonical, decl, online FROM tools
              WHERE retired_at IS NULL ORDER BY tool_id",
        )?;
        let rows = stmt
            .query_map([], tool_from_row)?
            .collect::<Result<Vec<_>, _>>()?;
        rows.into_iter().collect()
    }

    pub fn placements(&self) -> DbResult<BTreeMap<ToolId, String>> {
        let mut stmt = self
            .0
            .prepare("SELECT tool_id, path FROM tool_placements")?;
        Ok(stmt
            .query_map([], |r| Ok((ToolId(r.get(0)?), r.get(1)?)))?
            .collect::<Result<_, _>>()?)
    }

    pub fn put_group(&self, path: &str, about: &str, source: &str, now: Timestamp) -> DbResult<()> {
        self.0.execute("INSERT INTO tool_groups (path, about, source, updated_at) VALUES (?1, ?2, ?3, ?4)
            ON CONFLICT(path) DO UPDATE SET about = excluded.about, source = excluded.source, updated_at = excluded.updated_at",
            (path, about, source, Ts(now)))?;
        Ok(())
    }

    pub fn delete_group(&self, path: &str) -> DbResult<()> {
        self.0
            .execute("DELETE FROM tool_groups WHERE path = ?1", [path])?;
        Ok(())
    }

    pub fn place(&self, tool: ToolId, group: &str, source: &str, now: Timestamp) -> DbResult<()> {
        self.0.execute("INSERT INTO tool_placements (tool_id, path, source, updated_at) VALUES (?1, ?2, ?3, ?4)
            ON CONFLICT(tool_id) DO UPDATE SET path = excluded.path, source = excluded.source, updated_at = excluded.updated_at",
            (tool.0, group, source, Ts(now)))?;
        Ok(())
    }

    pub fn set_override(
        &self,
        tool: ToolId,
        value: OverrideRow,
        note: &str,
        now: Timestamp,
    ) -> DbResult<()> {
        self.0.execute("INSERT INTO tool_overrides (tool_id, master_only, trusted_only, gate, attention, note, updated_at)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) ON CONFLICT(tool_id) DO UPDATE SET
            master_only = excluded.master_only, trusted_only = excluded.trusted_only, gate = excluded.gate,
            attention = excluded.attention, note = excluded.note, updated_at = excluded.updated_at",
            (tool.0, value.master_only, value.trusted_only, value.forbid.then_some("forbid"), value.attention.map(Text), note, Ts(now)))?;
        Ok(())
    }

    pub fn usage(&self, since: &str) -> DbResult<Vec<UsageRow>> {
        let mut stmt = self.0.prepare("SELECT t.day, t.chat_id, c.folder_id, t.tool_id, t.turns_with_call, t.loads, t.calls
            FROM tool_stats t LEFT JOIN chats c ON c.id = t.chat_id WHERE t.day >= ?1 ORDER BY t.day, t.chat_id, t.tool_id")?;
        Ok(stmt
            .query_map([since], |r| {
                Ok(UsageRow {
                    day: r.get(0)?,
                    chat: ChatId(r.get(1)?),
                    folder: r.get::<_, Option<i64>>(2)?.map(FolderId),
                    tool: ToolId(r.get(3)?),
                    turns_with_call: r.get(4)?,
                    loads: r.get(5)?,
                    calls: r.get(6)?,
                })
            })?
            .collect::<Result<_, _>>()?)
    }

    pub fn activity(&self, since: &str) -> DbResult<Vec<ActivityRow>> {
        let mut stmt = self.0.prepare("SELECT s.day, s.chat_id, c.folder_id FROM chat_stats s LEFT JOIN chats c ON c.id = s.chat_id
            WHERE s.day >= ?1 AND s.turns > 0 ORDER BY s.day, s.chat_id")?;
        Ok(stmt
            .query_map([since], |r| {
                Ok(ActivityRow {
                    day: r.get(0)?,
                    chat: ChatId(r.get(1)?),
                    folder: r.get::<_, Option<i64>>(2)?.map(FolderId),
                })
            })?
            .collect::<Result<_, _>>()?)
    }

    pub fn active_chats(&self, since: Timestamp) -> DbResult<Vec<ChatId>> {
        let mut stmt = self.0.prepare(
            "SELECT id FROM chats WHERE deleted_at IS NULL AND last_activity_at >= ?1 ORDER BY id",
        )?;
        Ok(stmt
            .query_map([Ts(since)], |r| Ok(ChatId(r.get(0)?)))?
            .collect::<Result<_, _>>()?)
    }

    pub fn chat_turns(&self, chat: ChatId, since: &str) -> DbResult<u32> {
        Ok(self.0.query_row(
            "SELECT COALESCE(SUM(turns), 0) FROM chat_stats WHERE chat_id = ?1 AND day >= ?2",
            (chat.0, since),
            |r| r.get(0),
        )?)
    }

    pub fn registered(&self) -> DbResult<BTreeMap<ToolId, Timestamp>> {
        let mut stmt = self
            .0
            .prepare("SELECT tool_id, registered_at FROM tools WHERE retired_at IS NULL")?;
        Ok(stmt
            .query_map([], |r| Ok((ToolId(r.get(0)?), r.get::<_, Ts>(1)?.0)))?
            .collect::<Result<_, _>>()?)
    }

    pub fn recently_seen(&self, since: Timestamp) -> DbResult<BTreeSet<ToolId>> {
        ids(
            self.0,
            "SELECT tool_id FROM tools WHERE retired_at IS NULL AND last_seen_at >= ?1",
            [Ts(since)],
        )
    }

    pub fn latest_warm(&self, chat: ChatId) -> DbResult<Option<BTreeSet<ToolId>>> {
        let day: Option<String> = self
            .0
            .query_row(
                "SELECT day FROM chat_warm_sets WHERE chat_id = ?1 ORDER BY day DESC LIMIT 1",
                [chat.0],
                |r| r.get(0),
            )
            .optional()?;
        day.map(|day| self.warm(chat, &day))
            .transpose()
            .map(Option::flatten)
    }

    pub fn set_warm(
        &self,
        chat: ChatId,
        day: &str,
        tools: &BTreeSet<ToolId>,
        hidden: u32,
        over: bool,
        now: Timestamp,
    ) -> DbResult<()> {
        let json = serde_json::to_string(&tools.iter().map(|id| id.0).collect::<Vec<_>>())
            .map_err(DbError::corrupt)?;
        self.0.execute("INSERT INTO chat_warm_sets (chat_id, day, tool_ids, hidden_count, over_budget, computed_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
            ON CONFLICT(chat_id, day) DO UPDATE SET tool_ids = excluded.tool_ids, hidden_count = excluded.hidden_count, over_budget = excluded.over_budget, computed_at = excluded.computed_at",
            (chat.0, day, json, hidden, over, Ts(now)))?;
        self.0
            .execute("DELETE FROM chat_loaded_tools WHERE chat_id = ?1", [chat.0])?;
        Ok(())
    }

    pub fn pin(&self, chat: ChatId, tool: ToolId, until: Timestamp) -> DbResult<()> {
        self.0.execute(
            "INSERT INTO tool_pins (chat_id, tool_id, until) VALUES (?1, ?2, ?3)
            ON CONFLICT(chat_id, tool_id) DO UPDATE SET until = MAX(until, excluded.until)",
            (chat.0, tool.0, Ts(until)),
        )?;
        Ok(())
    }

    pub fn purge_warm(&self, before: &str) -> DbResult<usize> {
        Ok(self
            .0
            .execute("DELETE FROM chat_warm_sets WHERE day < ?1", [before])?)
    }

    /// Регистрация `(module, path)`: повторная сохраняет `tool_id`, обновляет
    /// декларацию и снимает `retired_at`.
    pub fn upsert(&self, tool: &NewTool<'_>, now: Timestamp) -> DbResult<ToolId> {
        let decl = serde_json::to_string(tool.decl).map_err(|e| DbError::Sqlite(e.to_string()))?;
        let id = self.0.query_row(
            "INSERT INTO tools (module, path, canonical, decl, spec_hash, registered_at, last_seen_at, online)
             VALUES (:module, :path, :canonical, :decl, :hash, :now, :now, 1)
             ON CONFLICT (module, path) DO UPDATE SET canonical = excluded.canonical, decl = excluded.decl,
                spec_hash = excluded.spec_hash, last_seen_at = excluded.last_seen_at, retired_at = NULL, online = 1
             RETURNING tool_id",
            named_params! {
                ":module": tool.module.as_str(),
                ":path": tool.decl.path,
                ":canonical": tool.canonical,
                ":decl": decl,
                ":hash": tool.spec_hash,
                ":now": Ts(now),
            },
            |row| row.get(0),
        )?;
        Ok(ToolId(id))
    }

    /// Пути, которых модуль больше не объявляет, уходят в `retired_at` (`02 §1.2`).
    pub fn retire_missing(
        &self,
        module: &ModuleId,
        keep: &[ToolId],
        now: Timestamp,
    ) -> DbResult<u64> {
        let keep = serde_json::to_string(&keep.iter().map(|t| t.0).collect::<Vec<_>>())
            .map_err(|e| DbError::Sqlite(e.to_string()))?;
        let n = self.0.execute(
            "UPDATE tools SET retired_at = ?3, online = 0
              WHERE module = ?1 AND retired_at IS NULL
                AND tool_id NOT IN (SELECT value FROM json_each(?2))",
            (module.as_str(), keep, Ts(now)),
        )?;
        Ok(n as u64)
    }

    /// Модуль отключился: его тулы `Forbidden(module_offline)`.
    pub fn set_offline(&self, module: &ModuleId) -> DbResult<u64> {
        let n = self.0.execute(
            "UPDATE tools SET online = 0 WHERE module = ?1 AND online = 1",
            [module.as_str()],
        )?;
        Ok(n as u64)
    }

    /// Старт ядра: сессии прошлого процесса мертвы (`01 §1.2`).
    pub fn all_offline(&self) -> DbResult<u64> {
        let n = self.0.execute(
            "UPDATE tools SET online = 0 WHERE module <> ?1 AND online = 1",
            [KERNEL],
        )?;
        Ok(n as u64)
    }

    /// `about` групп; декларация модуля ложится под overlay, а не поверх.
    pub fn groups(&self) -> DbResult<BTreeMap<String, String>> {
        let mut stmt = self.0.prepare("SELECT path, about FROM tool_groups")?;
        let rows = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<Result<_, _>>()?;
        Ok(rows)
    }

    pub fn put_module_group(&self, path: &str, about: &str, now: Timestamp) -> DbResult<()> {
        self.0.execute(
            "INSERT INTO tool_groups (path, about, source, updated_at) VALUES (?1, ?2, 'module', ?3)
             ON CONFLICT (path) DO UPDATE SET about = excluded.about, updated_at = excluded.updated_at
              WHERE tool_groups.source = 'module'",
            (path, about, Ts(now)),
        )?;
        Ok(())
    }

    pub fn overrides(&self) -> DbResult<BTreeMap<ToolId, OverrideRow>> {
        let mut stmt = self.0.prepare(
            "SELECT tool_id, master_only, trusted_only, gate, attention FROM tool_overrides",
        )?;
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    ToolId(row.get(0)?),
                    OverrideRow {
                        master_only: row.get(1)?,
                        trusted_only: row.get(2)?,
                        forbid: row.get::<_, Option<String>>(3)?.is_some(),
                        attention: row.get::<_, Option<Text<Attention>>>(4)?.map(|a| a.0),
                    },
                ))
            })?
            .collect::<Result<_, _>>()?;
        Ok(rows)
    }

    /// Пины чата, ещё не истёкшие.
    pub fn pins(&self, chat: ChatId, now: Timestamp) -> DbResult<BTreeSet<ToolId>> {
        ids(
            self.0,
            "SELECT tool_id FROM tool_pins WHERE chat_id = ?1 AND until > ?2",
            (chat.0, Ts(now)),
        )
    }

    /// Warm-набор чата на сутки `day`; `None` — пересчёта ещё не было.
    pub fn warm(&self, chat: ChatId, day: &str) -> DbResult<Option<BTreeSet<ToolId>>> {
        let raw: Option<String> = self
            .0
            .query_row(
                "SELECT tool_ids FROM chat_warm_sets WHERE chat_id = ?1 AND day = ?2",
                (chat.0, day),
                |row| row.get(0),
            )
            .optional()?;
        raw.map(|json| {
            serde_json::from_str::<Vec<i64>>(&json)
                .map(|ids| ids.into_iter().map(ToolId).collect())
                .map_err(|e| DbError::Corrupt(format!("chat_warm_sets.tool_ids: {e}")))
        })
        .transpose()
    }

    /// `loaded` чата (`02 §5.2`).
    pub fn loaded(&self, chat: ChatId) -> DbResult<BTreeSet<ToolId>> {
        ids(
            self.0,
            "SELECT tool_id FROM chat_loaded_tools WHERE chat_id = ?1",
            [chat.0],
        )
    }

    /// `true` — тул загружен впервые.
    pub fn load(&self, chat: ChatId, tool: ToolId, now: Timestamp) -> DbResult<bool> {
        let n = self.0.execute(
            "INSERT OR IGNORE INTO chat_loaded_tools (chat_id, tool_id, loaded_at) VALUES (?1, ?2, ?3)",
            (chat.0, tool.0, Ts(now)),
        )?;
        Ok(n > 0)
    }
}

fn ids(conn: &Connection, sql: &str, params: impl rusqlite::Params) -> DbResult<BTreeSet<ToolId>> {
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt
        .query_map(params, |row| row.get(0).map(ToolId))?
        .collect::<Result<_, _>>()?;
    Ok(rows)
}

fn tool_from_row(row: &Row<'_>) -> rusqlite::Result<DbResult<ToolRow>> {
    let id = ToolId(row.get(0)?);
    let decl: String = row.get(4)?;
    let online: i64 = row.get(5)?;
    let module = ModuleId(row.get(1)?);
    let path: String = row.get(2)?;
    let canonical: String = row.get(3)?;
    Ok(serde_json::from_str(&decl)
        .map(|decl| ToolRow {
            id,
            module,
            path,
            canonical,
            decl,
            online: online == 1,
        })
        .map_err(|e| DbError::Corrupt(format!("tools.decl of {id}: {e}"))))
}
