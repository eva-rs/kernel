//! Транскрипт: `segments`, `events`, `messages`, `tool_results`,
//! `token_ratios` и колонки состояния сегмента на `chats` (`01`, `10 §2.1`).
//! Строки → типы движка (`subsystems::turns::log::Event`) и протокола
//! (`convert::turn`) живут снаружи.

use eva::{data, time::Timestamp};
use rusqlite::{Connection, OptionalExtension as _, Row, named_params};

use eva_protocol::types::{
    ChatId, EventId, ModelId, PersonId, SegmentId, ToolId, Tseq, TurnId,
    chat::{Role, SegmentReason},
    turn::Audience,
};

use crate::db::{
    DbResult,
    sql::{Text, Ts},
};

/// Как компакция получила summary (`01 §6.2` п.6).
#[data(copy, ord)]
pub enum SummaryPath {
    Warm,
    Cold,
    Bisect,
    Fallback,
}

#[data]
pub struct SegmentRow {
    pub id: SegmentId,
    pub chat: ChatId,
    pub seq: u32,
    pub started_at: Timestamp,
    pub ended_at: Option<Timestamp>,
    pub reason: SegmentReason,
    pub summary: Option<String>,
    pub verbatim_tail: Option<String>,
    pub summary_model: Option<ModelId>,
    pub summary_path: Option<SummaryPath>,
    pub summary_attempts: u32,
    /// msgpack `Vec<SystemSection>`.
    pub system_snapshot: Vec<u8>,
    /// msgpack `Vec<ToolSpec>` — весь `tools[]` сегмента, deferred после visible.
    pub tools_snapshot: Vec<u8>,
    /// msgpack `Vec<MemoryId>`.
    pub hot_memory_ids: Vec<u8>,
}

/// Сегмент к открытию; `seq` — следующий по чату.
#[data]
pub struct NewSegment {
    pub chat: ChatId,
    pub reason: SegmentReason,
    pub summary: Option<String>,
    pub verbatim_tail: Option<String>,
    pub summary_model: Option<ModelId>,
    pub summary_path: Option<SummaryPath>,
    pub summary_attempts: u32,
    pub system_snapshot: Vec<u8>,
    pub tools_snapshot: Vec<u8>,
    pub hot_memory_ids: Vec<u8>,
    pub now: Timestamp,
}

/// Строка `events` как есть: `kind` и `payload` разбирает `log::Event`.
#[data]
pub struct EventRow {
    pub id: EventId,
    pub chat: ChatId,
    pub segment: SegmentId,
    pub seq: u32,
    pub kind: String,
    pub payload: Vec<u8>,
    pub turn_id: Option<TurnId>,
    /// Номер шага у событий движка; `None` у ingest и гигиены.
    pub step: Option<u32>,
    pub created_at: Timestamp,
}

/// Событие к записи; `seq` — следующий по сегменту.
pub struct NewEvent<'a> {
    pub chat: ChatId,
    pub segment: SegmentId,
    pub kind: &'a str,
    pub payload: &'a [u8],
    pub turn_id: Option<&'a TurnId>,
    pub step: Option<u32>,
    pub created_at: Timestamp,
}

/// Открытый тур: события с `turn_id` без `turn_ended`.
#[data]
pub struct OpenTurn {
    pub turn: TurnId,
    pub chat: ChatId,
    pub segment: SegmentId,
}

/// Строка `messages` к записи; `seq` — следующий по чату.
pub struct NewMessage<'a> {
    pub chat: ChatId,
    pub event: EventId,
    pub role: Role,
    pub sender: Option<&'a PersonId>,
    pub external_id: Option<&'a str>,
    pub reply_to_external_id: Option<&'a str>,
    pub turn_id: Option<&'a TurnId>,
}

/// Реплика по `external_id` поверхности: идемпотентность ingest.
#[data]
pub struct KnownMessage {
    pub event: EventId,
    pub turn: Option<TurnId>,
}

/// Исход `turns.delivered`.
#[data(ord)]
pub enum Delivered {
    Ok,
    NotFound,
    AlreadyDelivered { external_id: String },
}

/// Строка `tool_results` к записи; `content: None` — ссылка `dedup_of`.
pub struct NewToolResult<'a> {
    pub chat: ChatId,
    pub tseq: Tseq,
    pub event: EventId,
    pub segment: SegmentId,
    pub tool_id: Option<ToolId>,
    pub dedup_key: &'a [u8],
    pub dedup_of: Option<Tseq>,
    pub content: Option<&'a [u8]>,
    /// Имя варианта `Synthetic` под CHECK.
    pub synthetic: Option<&'a str>,
    pub created_at: Timestamp,
}

/// Якорь usage (`01 §9`).
#[data(copy)]
pub struct Anchor {
    pub event: EventId,
    pub tokens: u32,
}

/// Gap запросов чата для TTL кэша (`01 §7.2`).
#[data(copy)]
pub struct Gap {
    /// EMA start-to-start в наносекундах.
    pub ema_ns: Option<f64>,
    pub last_request_at: Option<Timestamp>,
}

pub struct Turns<'c>(pub &'c Connection);

pub struct FeedSnapshot {
    pub events: Vec<EventRow>,
    pub pending: Vec<super::agents::PendingRow>,
    pub plan: Vec<super::agents::PlanRow>,
}

impl Turns<'_> {
    pub fn open_segments(&self) -> DbResult<Vec<SegmentRow>> {
        let mut query = self.0.prepare(&format!(
            "{SEGMENT} WHERE ended_at IS NULL ORDER BY chat_id"
        ))?;
        Ok(query
            .query_map([], segment_from_row)?
            .collect::<Result<_, _>>()?)
    }
    pub fn notify_chat(&self, now: Timestamp) -> DbResult<ChatId> {
        use super::Queries as _;
        const KEY: &str = "notify_chat_id";
        if let Some(chat) = self.0.meta().chat(KEY)?
            && self.0.chats().get(chat)?.is_some()
        {
            return Ok(chat);
        }
        let folder = self
            .0
            .folders()
            .by_name(
                &eva_protocol::types::SurfaceId::from(super::surfaces::SYSTEM),
                "notify",
            )?
            .ok_or_else(|| super::DbError::corrupt("system has no notify folder"))?;
        let chat = self.0.chats().insert(&super::chats::NewChat {
            folder_id: folder.id,
            kind: eva_protocol::types::chat::Kind::Persistent,
            title: Some("Notifications".into()),
            project: None,
            workdir: None,
            workdir_on: None,
            now,
        })?;
        self.0
            .execute("UPDATE chats SET undeletable = 1 WHERE id = ?1", [chat.0])?;
        self.0.meta().set_chat(KEY, chat)?;
        Ok(chat)
    }
    pub fn touch_chat(&self, chat: ChatId, now: Timestamp) -> DbResult<()> {
        self.0.execute(
            "UPDATE chats SET last_activity_at = ?2 WHERE id = ?1",
            (chat.0, Ts(now)),
        )?;
        Ok(())
    }
    pub fn feed_snapshot(&self, chat: ChatId) -> DbResult<FeedSnapshot> {
        use super::Queries as _;
        let tx = self.0.unchecked_transaction()?;
        let snapshot = FeedSnapshot {
            events: tx.turns().events_after(chat, None)?,
            pending: tx.agents().open_pending(chat)?,
            plan: tx.agents().plan(chat)?,
        };
        tx.commit()?;
        Ok(snapshot)
    }
    // ---- segments ----

    pub fn open_segment(&self, chat: ChatId) -> DbResult<Option<SegmentRow>> {
        Ok(self
            .0
            .query_row(
                &format!("{SEGMENT} WHERE chat_id = ?1 AND ended_at IS NULL"),
                [chat.0],
                segment_from_row,
            )
            .optional()?)
    }

    pub fn segment(&self, id: SegmentId) -> DbResult<Option<SegmentRow>> {
        Ok(self
            .0
            .query_row(
                &format!("{SEGMENT} WHERE id = ?1"),
                [id.0],
                segment_from_row,
            )
            .optional()?)
    }

    pub fn insert_segment(&self, seg: &NewSegment) -> DbResult<SegmentId> {
        let id = self.0.query_row(
            "INSERT INTO segments (chat_id, seq, started_at, reason, summary, verbatim_tail,
                                   summary_model, summary_path, summary_attempts,
                                   system_snapshot, tools_snapshot, hot_memory_ids)
             VALUES (:chat, (SELECT COALESCE(MAX(seq), 0) + 1 FROM segments WHERE chat_id = :chat),
                     :now, :reason, :summary, :tail, :model, :path, :attempts, :system, :tools, :hot)
             RETURNING id",
            named_params! {
                ":chat": seg.chat.0,
                ":now": Ts(seg.now),
                ":reason": Text(seg.reason),
                ":summary": seg.summary,
                ":tail": seg.verbatim_tail,
                ":model": seg.summary_model.as_ref().map(ModelId::as_str),
                ":path": seg.summary_path.map(Text),
                ":attempts": seg.summary_attempts,
                ":system": seg.system_snapshot,
                ":tools": seg.tools_snapshot,
                ":hot": seg.hot_memory_ids,
            },
            |row| row.get(0),
        )?;
        Ok(SegmentId(id))
    }

    pub fn close_segment(&self, id: SegmentId, at: Timestamp) -> DbResult<()> {
        self.0.execute(
            "UPDATE segments SET ended_at = ?2 WHERE id = ?1 AND ended_at IS NULL",
            (id.0, Ts(at)),
        )?;
        Ok(())
    }

    // ---- events ----

    /// Append в лог; `seq` берётся тем же стейтментом — писатель один.
    pub fn append(&self, ev: &NewEvent<'_>) -> DbResult<EventId> {
        let id = self.0.query_row(
            "INSERT INTO events (chat_id, segment_id, seq, kind, payload, turn_id, step, created_at)
             VALUES (:chat, :segment, (SELECT COALESCE(MAX(seq), 0) + 1 FROM events WHERE segment_id = :segment),
                     :kind, :payload, :turn, :step, :now)
             RETURNING id",
            named_params! {
                ":chat": ev.chat.0,
                ":segment": ev.segment.0,
                ":kind": ev.kind,
                ":payload": ev.payload,
                ":turn": ev.turn_id.map(TurnId::as_str),
                ":step": ev.step,
                ":now": Ts(ev.created_at),
            },
            |row| row.get(0),
        )?;
        Ok(EventId(id))
    }

    /// Лог сегмента по `seq`; `upto` — включительно.
    pub fn events_of_segment(
        &self,
        segment: SegmentId,
        upto: Option<EventId>,
    ) -> DbResult<Vec<EventRow>> {
        let mut stmt = self.0.prepare(&format!(
            "{EVENT} WHERE segment_id = ?1 AND (?2 IS NULL OR id <= ?2) ORDER BY seq"
        ))?;
        let rows = stmt
            .query_map((segment.0, upto.map(|e| e.0)), event_from_row)?
            .collect::<Result<_, _>>()?;
        Ok(rows)
    }

    /// События чата с `id > since` по возрастанию: история ленты (`07 §7`).
    pub fn event(&self, chat: ChatId, id: EventId) -> DbResult<Option<EventRow>> {
        Ok(self
            .0
            .query_row(
                &format!("{EVENT} WHERE chat_id = ?1 AND id = ?2"),
                (chat.0, id.0),
                event_from_row,
            )
            .optional()?)
    }

    /// События чата с `id > since` по возрастанию: история ленты (`07 §7`).
    pub fn events_after(&self, chat: ChatId, since: Option<EventId>) -> DbResult<Vec<EventRow>> {
        let mut stmt = self.0.prepare(&format!(
            "{EVENT} WHERE chat_id = ?1 AND (?2 IS NULL OR id > ?2) ORDER BY id"
        ))?;
        let rows = stmt
            .query_map((chat.0, since.map(|e| e.0)), event_from_row)?
            .collect::<Result<_, _>>()?;
        Ok(rows)
    }

    /// События тура с `id > after`: drain-проверка читает среди них `message{user}`.
    pub fn turn_events_after(&self, turn: &TurnId, after: EventId) -> DbResult<Vec<EventRow>> {
        let mut stmt = self.0.prepare(&format!(
            "{EVENT} WHERE turn_id = ?1 AND id > ?2 ORDER BY id"
        ))?;
        let rows = stmt
            .query_map((turn.as_str(), after.0), event_from_row)?
            .collect::<Result<_, _>>()?;
        Ok(rows)
    }

    /// Туры без `turn_ended` (recovery, `01 §1.2`).
    pub fn open_turns(&self) -> DbResult<Vec<OpenTurn>> {
        let mut stmt = self.0.prepare(
            "SELECT e.turn_id, e.chat_id, MAX(e.segment_id) FROM events e
              WHERE e.turn_id IS NOT NULL
                AND NOT EXISTS (SELECT 1 FROM events x WHERE x.turn_id = e.turn_id AND x.kind = 'turn_ended')
              GROUP BY e.turn_id ORDER BY MIN(e.id)",
        )?;
        let rows = stmt
            .query_map([], |row| {
                Ok(OpenTurn {
                    turn: TurnId(row.get(0)?),
                    chat: ChatId(row.get(1)?),
                    segment: SegmentId(row.get(2)?),
                })
            })?
            .collect::<Result<_, _>>()?;
        Ok(rows)
    }

    pub fn last_event_id(&self, chat: ChatId) -> DbResult<Option<EventId>> {
        let id: Option<i64> = self.0.query_row(
            "SELECT MAX(id) FROM events WHERE chat_id = ?1",
            [chat.0],
            |row| row.get(0),
        )?;
        Ok(id.map(EventId))
    }

    // ---- messages ----

    /// Строка речи к событию; возвращает `seq` реплики в чате.
    pub fn insert_message(&self, m: &NewMessage<'_>) -> DbResult<u64> {
        let seq: i64 = self.0.query_row(
            "INSERT INTO messages (chat_id, seq, event_id, role, sender_person_id, external_id, reply_to_external_id, turn_id)
             VALUES (:chat, (SELECT COALESCE(MAX(seq), 0) + 1 FROM messages WHERE chat_id = :chat),
                     :event, :role, :sender, :ext, :reply, :turn)
             RETURNING seq",
            named_params! {
                ":chat": m.chat.0,
                ":event": m.event.0,
                ":role": Text(m.role),
                ":sender": m.sender.map(PersonId::as_str),
                ":ext": m.external_id,
                ":reply": m.reply_to_external_id,
                ":turn": m.turn_id.map(TurnId::as_str),
            },
            |row| row.get(0),
        )?;
        Ok(u64::try_from(seq).unwrap_or(0))
    }

    pub fn by_external_id(
        &self,
        chat: ChatId,
        external_id: &str,
    ) -> DbResult<Option<KnownMessage>> {
        Ok(self
            .0
            .query_row(
                "SELECT event_id, turn_id FROM messages WHERE chat_id = ?1 AND external_id = ?2 LIMIT 1",
                (chat.0, external_id),
                |row| {
                    Ok(KnownMessage {
                        event: EventId(row.get(0)?),
                        turn: row.get::<_, Option<String>>(1)?.map(TurnId),
                    })
                },
            )
            .optional()?)
    }

    pub fn actor_of_turn(&self, chat: ChatId, turn: &TurnId) -> DbResult<Option<PersonId>> {
        Ok(self.0.query_row(
            "SELECT sender_person_id FROM messages WHERE chat_id = ?1 AND turn_id = ?2 AND role = 'user' ORDER BY seq LIMIT 1",
            (chat.0, turn.as_str()),
            |row| row.get::<_, Option<String>>(0),
        ).optional()?.flatten().map(PersonId))
    }

    /// `turns.delivered`: `external_id` реплики Евы ставится один раз.
    pub fn set_delivered(
        &self,
        chat: ChatId,
        message: EventId,
        external_id: &str,
    ) -> DbResult<Delivered> {
        let current: Option<Option<String>> = self
            .0
            .query_row(
                "SELECT external_id FROM messages WHERE chat_id = ?1 AND event_id = ?2 AND role = 'assistant'",
                (chat.0, message.0),
                |row| row.get(0),
            )
            .optional()?;
        match current {
            None => Ok(Delivered::NotFound),
            Some(Some(external_id)) => Ok(Delivered::AlreadyDelivered { external_id }),
            Some(None) => {
                self.0.execute(
                    "UPDATE messages SET external_id = ?3 WHERE chat_id = ?1 AND event_id = ?2",
                    (chat.0, message.0, external_id),
                )?;
                Ok(Delivered::Ok)
            }
        }
    }

    // ---- tool_results ----

    /// `chats.tool_seq++`: `t<seq>` переживает компакцию.
    pub fn next_tseq(&self, chat: ChatId) -> DbResult<Tseq> {
        let seq: i64 = self.0.query_row(
            "UPDATE chats SET tool_seq = tool_seq + 1 WHERE id = ?1 RETURNING tool_seq",
            [chat.0],
            |row| row.get(0),
        )?;
        Ok(Tseq(seq))
    }

    pub fn insert_result(&self, r: &NewToolResult<'_>) -> DbResult<()> {
        self.0.execute(
            "INSERT INTO tool_results (chat_id, tseq, event_id, segment_id, tool_id, dedup_key, dedup_of, content, synthetic, created_at)
             VALUES (:chat, :tseq, :event, :segment, :tool, :key, :dedup_of, :content, :synthetic, :now)",
            named_params! {
                ":chat": r.chat.0,
                ":tseq": r.tseq.0,
                ":event": r.event.0,
                ":segment": r.segment.0,
                ":tool": r.tool_id.map(|t| t.0),
                ":key": r.dedup_key,
                ":dedup_of": r.dedup_of.map(|t| t.0),
                ":content": r.content,
                ":synthetic": r.synthetic,
                ":now": Ts(r.created_at),
            },
        )?;
        Ok(())
    }

    /// Verbatim-запись того же сегмента с тем же ключом (`01 §4`).
    pub fn find_dedup(
        &self,
        chat: ChatId,
        segment: SegmentId,
        key: &[u8],
    ) -> DbResult<Option<Tseq>> {
        Ok(self
            .0
            .query_row(
                "SELECT tseq FROM tool_results
                  WHERE chat_id = ?1 AND segment_id = ?2 AND dedup_key = ?3 AND dedup_of IS NULL AND synthetic IS NULL
                  ORDER BY tseq LIMIT 1",
                (chat.0, segment.0, key),
                |row| row.get(0).map(Tseq),
            )
            .optional()?)
    }

    /// Полные байты результата (`context_recall`); ссылка `dedup_of` разворачивается.
    pub fn result_content(&self, chat: ChatId, tseq: Tseq) -> DbResult<Option<Vec<u8>>> {
        Ok(self
            .0
            .query_row(
                "SELECT COALESCE(r.content, o.content) FROM tool_results r
                   LEFT JOIN tool_results o ON o.chat_id = r.chat_id AND o.tseq = r.dedup_of
                  WHERE r.chat_id = ?1 AND r.tseq = ?2",
                (chat.0, tseq.0),
                |row| row.get::<_, Option<Vec<u8>>>(0),
            )
            .optional()?
            .flatten())
    }

    // ---- token_ratios ----

    /// Калиброванный `bytes_per_token` модели (`01 §9`); `None` — модель ещё не калибрована.
    pub fn ratio(&self, model: &ModelId) -> DbResult<Option<f64>> {
        Ok(self
            .0
            .query_row(
                "SELECT bytes_per_token FROM token_ratios WHERE model = ?1",
                [model.as_str()],
                |row| row.get(0),
            )
            .optional()?)
    }

    pub fn set_ratio(&self, model: &ModelId, ratio: f64, now: Timestamp) -> DbResult<()> {
        self.0.execute(
            "INSERT INTO token_ratios (model, bytes_per_token, samples, updated_at) VALUES (?1, ?2, 1, ?3)
             ON CONFLICT (model) DO UPDATE SET bytes_per_token = excluded.bytes_per_token,
                                               samples = samples + 1, updated_at = excluded.updated_at",
            (model.as_str(), ratio, Ts(now)),
        )?;
        Ok(())
    }

    // ---- chats: segment state ----

    pub fn anchor(&self, chat: ChatId) -> DbResult<Option<Anchor>> {
        let (event, tokens): (Option<i64>, Option<i64>) = self.0.query_row(
            "SELECT token_anchor_event_id, token_anchor_tokens FROM chats WHERE id = ?1",
            [chat.0],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        Ok(match (event, tokens) {
            (Some(event), Some(tokens)) => Some(Anchor {
                event: EventId(event),
                tokens: u32::try_from(tokens).unwrap_or(u32::MAX),
            }),
            _ => None,
        })
    }

    pub fn set_anchor(&self, chat: ChatId, anchor: Anchor) -> DbResult<()> {
        self.0.execute(
            "UPDATE chats SET token_anchor_event_id = ?2, token_anchor_tokens = ?3 WHERE id = ?1",
            (chat.0, anchor.event.0, anchor.tokens),
        )?;
        Ok(())
    }

    pub fn clear_anchor(&self, chat: ChatId) -> DbResult<()> {
        self.0.execute(
            "UPDATE chats SET token_anchor_event_id = NULL, token_anchor_tokens = NULL WHERE id = ?1",
            [chat.0],
        )?;
        Ok(())
    }

    pub fn gap(&self, chat: ChatId) -> DbResult<Gap> {
        Ok(self.0.query_row(
            "SELECT request_gap_ema, last_request_at FROM chats WHERE id = ?1",
            [chat.0],
            |row| {
                Ok(Gap {
                    ema_ns: row.get(0)?,
                    last_request_at: row.get::<_, Option<Ts>>(1)?.map(|t| t.0),
                })
            },
        )?)
    }

    pub fn set_gap(&self, chat: ChatId, gap: Gap) -> DbResult<()> {
        self.0.execute(
            "UPDATE chats SET request_gap_ema = ?2, last_request_at = ?3 WHERE id = ?1",
            (chat.0, gap.ema_ns, gap.last_request_at.map(Ts)),
        )?;
        Ok(())
    }

    /// Отложенный `SegmentReset` чата (`05 §3` п.5).
    pub fn reset_pending(&self, chat: ChatId) -> DbResult<bool> {
        Ok(self.0.query_row(
            "SELECT reset_pending IS NOT NULL FROM chats WHERE id = ?1",
            [chat.0],
            |row| row.get(0),
        )?)
    }

    pub fn clear_reset_pending(&self, chat: ChatId) -> DbResult<()> {
        self.0.execute(
            "UPDATE chats SET reset_pending = NULL WHERE id = ?1",
            [chat.0],
        )?;
        Ok(())
    }

    /// `compact_at` чата: первое `Some` снизу вверх по цепочке chat → folder → surface (`01 §6.1`).
    pub fn compact_at(&self, chat: ChatId) -> DbResult<Option<f32>> {
        Ok(self.0.query_row(
            "SELECT COALESCE(c.compact_at, f.compact_at, s.compact_at) FROM chats c
               JOIN folders f ON f.id = c.folder_id JOIN surfaces s ON s.id = f.surface_id
              WHERE c.id = ?1",
            [chat.0],
            |row| row.get(0),
        )?)
    }

    /// `master_only` ⇔ ни одной `user`-реплики от не-master (`05 §2.4`).
    pub fn audience(&self, chat: ChatId) -> DbResult<Audience> {
        let mixed: bool = self.0.query_row(
            "SELECT EXISTS (SELECT 1 FROM messages m LEFT JOIN persons p ON p.id = m.sender_person_id
                             WHERE m.chat_id = ?1 AND m.role = 'user' AND COALESCE(p.master, 0) = 0)",
            [chat.0],
            |row| row.get(0),
        )?;
        Ok(if mixed {
            Audience::Mixed
        } else {
            Audience::MasterOnly
        })
    }

    /// `chats.alerted = 1`: тур упал или модель отказала (`05 §6.1`).
    pub fn set_alerted(&self, chat: ChatId) -> DbResult<()> {
        self.0
            .execute("UPDATE chats SET alerted = 1 WHERE id = ?1", [chat.0])?;
        Ok(())
    }

    /// Говорившие в чате люди для секции `chat` (`01 §2.2`): ядро (`p0`) не участник.
    pub fn participants(&self, chat: ChatId) -> DbResult<Vec<Participant>> {
        let mut stmt = self.0.prepare(
            "SELECT p.id, p.name, p.master FROM persons p
              WHERE p.id <> 'p0' AND p.id IN (SELECT sender_person_id FROM messages WHERE chat_id = ?1)
              ORDER BY p.master DESC, CAST(substr(p.id, 2) AS INTEGER)",
        )?;
        let rows = stmt
            .query_map([chat.0], |row| {
                Ok(Participant {
                    person: PersonId(row.get(0)?),
                    name: row.get(1)?,
                    master: row.get(2)?,
                })
            })?
            .collect::<Result<_, _>>()?;
        Ok(rows)
    }

    /// Время последней человеческой реплики чата раньше `before`: gap reminder.
    pub fn last_human_before(&self, chat: ChatId, before: EventId) -> DbResult<Option<Timestamp>> {
        Ok(self
            .0
            .query_row(
                "SELECT e.created_at FROM messages m JOIN events e ON e.id = m.event_id
                  WHERE m.chat_id = ?1 AND m.role = 'user' AND m.event_id < ?2
                    AND COALESCE(m.sender_person_id, '') <> 'p0'
                  ORDER BY m.event_id DESC LIMIT 1",
                (chat.0, before.0),
                |row| row.get::<_, Ts>(0),
            )
            .optional()?
            .map(|t| t.0))
    }

    /// Компакций сегмента, упавших подряд (`01 §6.2` п.4).
    pub fn failed_compactions(&self, segment: SegmentId) -> DbResult<u32> {
        let n: i64 = self.0.query_row(
            "SELECT failed_compactions FROM segments WHERE id = ?1",
            [segment.0],
            |row| row.get(0),
        )?;
        Ok(u32::try_from(n).unwrap_or(u32::MAX))
    }

    pub fn note_failed_compaction(&self, segment: SegmentId) -> DbResult<()> {
        self.0.execute(
            "UPDATE segments SET failed_compactions = failed_compactions + 1 WHERE id = ?1",
            [segment.0],
        )?;
        Ok(())
    }

    /// Строка `messages` по событию: `delivered` и атрибуция.
    pub fn message_of(&self, event: EventId) -> DbResult<Option<(ChatId, Role)>> {
        Ok(self
            .0
            .query_row(
                "SELECT chat_id, role FROM messages WHERE event_id = ?1",
                [event.0],
                |row| Ok((ChatId(row.get(0)?), row.get::<_, Text<Role>>(1)?.0)),
            )
            .optional()?)
    }
}

/// Участник чата.
#[data]
pub struct Participant {
    pub person: PersonId,
    pub name: String,
    pub master: bool,
}

const SEGMENT: &str = "SELECT id, chat_id, seq, started_at, ended_at, reason, summary, verbatim_tail,
       summary_model, summary_path, summary_attempts, system_snapshot, tools_snapshot, hot_memory_ids
  FROM segments";

const EVENT: &str =
    "SELECT id, chat_id, segment_id, seq, kind, payload, turn_id, step, created_at FROM events";

fn segment_from_row(row: &Row<'_>) -> rusqlite::Result<SegmentRow> {
    let seq: i64 = row.get("seq")?;
    let attempts: i64 = row.get("summary_attempts")?;
    Ok(SegmentRow {
        id: SegmentId(row.get("id")?),
        chat: ChatId(row.get("chat_id")?),
        seq: u32::try_from(seq).map_err(|_| rusqlite::Error::IntegralValueOutOfRange(2, seq))?,
        started_at: row.get::<_, Ts>("started_at")?.0,
        ended_at: row.get::<_, Option<Ts>>("ended_at")?.map(|t| t.0),
        reason: row.get::<_, Text<SegmentReason>>("reason")?.0,
        summary: row.get("summary")?,
        verbatim_tail: row.get("verbatim_tail")?,
        summary_model: row.get::<_, Option<String>>("summary_model")?.map(ModelId),
        summary_path: row
            .get::<_, Option<Text<SummaryPath>>>("summary_path")?
            .map(|p| p.0),
        summary_attempts: u32::try_from(attempts).unwrap_or(u32::MAX),
        system_snapshot: row.get("system_snapshot")?,
        tools_snapshot: row.get("tools_snapshot")?,
        hot_memory_ids: row.get("hot_memory_ids")?,
    })
}

fn event_from_row(row: &Row<'_>) -> rusqlite::Result<EventRow> {
    let seq: i64 = row.get("seq")?;
    let step: Option<i64> = row.get("step")?;
    Ok(EventRow {
        id: EventId(row.get("id")?),
        chat: ChatId(row.get("chat_id")?),
        segment: SegmentId(row.get("segment_id")?),
        seq: u32::try_from(seq).map_err(|_| rusqlite::Error::IntegralValueOutOfRange(3, seq))?,
        kind: row.get("kind")?,
        payload: row.get("payload")?,
        turn_id: row.get::<_, Option<String>>("turn_id")?.map(TurnId),
        step: step.map(|s| u32::try_from(s).unwrap_or(u32::MAX)),
        created_at: row.get::<_, Ts>("created_at")?.0,
    })
}
