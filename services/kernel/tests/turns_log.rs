//! Транскрипт в БД (`01 §1`, `10 §2.1`): сегменты, append-only лог с `seq`
//! на сегмент, речь, индекс результатов с дедупом, калибровка и якорь.

mod common;

use eva::time::Timestamp;
use serde_json::json;

use eva_kernel::{
    db::{
        Connection, Db as _, DbError, DbResult, Queries as _,
        turns::{Anchor, Delivered, Gap, NewMessage, NewSegment, NewToolResult},
    },
    subsystems::turns::{
        log::{
            self, Append, Event, HygienePayload, MemoryUpdatedPayload, MessagePayload, Payload,
            Synthetic, ToolResultPayload, ToolUsePayload, TurnEndedPayload,
        },
        snapshot::{self, SegmentSnapshot},
    },
};
use eva_protocol::{
    types::turn::TurnOutcome,
    types::{
        ChatId, EventId, ModelId, PersonId, SegmentId, Tseq, TurnId,
        chat::{Role, SegmentReason},
        content::Block,
    },
};

fn at(secs: u64) -> Timestamp {
    Timestamp::from_secs(secs)
}

fn new_segment(chat: ChatId, reason: SegmentReason) -> NewSegment {
    let empty = SegmentSnapshot::default();
    NewSegment {
        chat,
        reason,
        summary: None,
        verbatim_tail: None,
        summary_model: None,
        summary_path: None,
        summary_attempts: 0,
        system_snapshot: snapshot::encode(&empty.system),
        tools_snapshot: snapshot::encode(&empty.tools),
        hot_memory_ids: snapshot::encode(&empty.hot_memory_ids),
        now: at(1),
    }
}

async fn write<T: Send + 'static>(
    h: &common::Harness,
    f: impl FnOnce(&Connection) -> DbResult<T> + Send + 'static,
) -> T {
    h.kernel
        .shared()
        .db()
        .write(move |tx| f(tx))
        .await
        .expect("write")
}

async fn chat(h: &common::Harness) -> ChatId {
    write(h, |tx| {
        Ok(ChatId(tx.query_row(
            "INSERT INTO chats (folder_id, kind, created_at, last_activity_at)
             SELECT id, 'persistent', 1, 1 FROM folders WHERE surface_id = 'local' AND name = 'Unsorted'
             RETURNING id",
            [],
            |r| r.get(0),
        )?))
    })
    .await
}

fn user(text: &str) -> Payload {
    Payload::Message(MessagePayload::kernel(vec![Block::text(text)]))
}

fn append(
    tx: &Connection,
    chat: ChatId,
    segment: SegmentId,
    turn: Option<&TurnId>,
    payload: &Payload,
) -> DbResult<EventId> {
    log::append(
        tx,
        Append {
            chat,
            segment,
            turn_id: turn,
            step: turn.map(|_| 1),
            now: at(10),
        },
        payload,
    )
}

#[tokio::test]
async fn segments_and_append_only_log() {
    let h = common::start().await;
    let chat = chat(&h).await;

    let (first, second, ids) = write(&h, move |tx| {
        assert!(tx.turns().open_segment(chat)?.is_none());
        let first = tx
            .turns()
            .insert_segment(&new_segment(chat, SegmentReason::Initial))?;
        let a = append(tx, chat, first, None, &user("a"))?;
        let b = append(tx, chat, first, None, &user("b"))?;
        tx.turns().close_segment(first, at(5))?;
        let second = tx
            .turns()
            .insert_segment(&new_segment(chat, SegmentReason::Compact))?;
        let c = append(tx, chat, second, None, &user("c"))?;
        Ok((first, second, [a, b, c]))
    })
    .await;

    let (open, rows_first, rows_second, upto, after, last) = h
        .query(move |c| {
            Ok((
                c.turns().open_segment(chat)?,
                c.turns().events_of_segment(first, None)?,
                c.turns().events_of_segment(second, None)?,
                c.turns().events_of_segment(first, Some(ids[0]))?,
                c.turns().events_after(chat, Some(ids[0]))?,
                c.turns().last_event_id(chat)?,
            ))
        })
        .await;
    let open = open.expect("open segment");
    assert_eq!(
        (open.id, open.seq, open.reason),
        (second, 2, SegmentReason::Compact)
    );
    assert_eq!(
        rows_first.iter().map(|r| r.seq).collect::<Vec<_>>(),
        [1, 2],
        "seq per segment"
    );
    assert_eq!(rows_second.iter().map(|r| r.seq).collect::<Vec<_>>(), [1]);
    assert_eq!(upto.len(), 1);
    assert_eq!(
        after.iter().map(|r| r.id).collect::<Vec<_>>(),
        [ids[1], ids[2]]
    );
    assert_eq!(last, Some(ids[2]));

    let loaded = h
        .query(move |c| Ok(log::load(c, first, None)))
        .await
        .expect("decodes")
        .expect("segment");
    assert_eq!(loaded.events.len(), 2);
    assert_eq!(loaded.row.ended_at, Some(at(5)));

    let edit = h
        .kernel
        .shared()
        .db()
        .write(move |tx| -> DbResult<usize> {
            Ok(tx.execute(
                "UPDATE events SET payload = x'00' WHERE id = ?1",
                [ids[0].0],
            )?)
        })
        .await;
    assert!(matches!(edit, Err(DbError::Constraint(_))), "{edit:?}");
}

#[tokio::test]
async fn open_turns_are_events_without_turn_ended() {
    let h = common::start().await;
    let chat = chat(&h).await;
    let open_turn = TurnId::from("t-open");
    let (a, b) = (open_turn.clone(), TurnId::from("t-closed"));
    let watermark = write(&h, move |tx| {
        let seg = tx
            .turns()
            .insert_segment(&new_segment(chat, SegmentReason::Initial))?;
        append(tx, chat, seg, Some(&b), &user("x"))?;
        append(
            tx,
            chat,
            seg,
            Some(&b),
            &Payload::TurnEnded(TurnEndedPayload {
                turn_id: b.clone(),
                outcome: TurnOutcome::Completed,
                by: None,
                error: None,
            }),
        )?;
        let w = append(tx, chat, seg, Some(&a), &user("y"))?;
        append(tx, chat, seg, Some(&a), &user("z"))?;
        Ok(w)
    })
    .await;
    let (open, after) = h
        .query(move |c| {
            Ok((
                c.turns().open_turns()?,
                c.turns().turn_events_after(&open_turn, watermark)?,
            ))
        })
        .await;
    assert_eq!(open.len(), 1);
    assert_eq!(open[0].turn, TurnId::from("t-open"));
    assert_eq!(open[0].chat, chat);
    assert_eq!(
        after.len(),
        1,
        "drain check sees only what came after the watermark"
    );
}

#[tokio::test]
async fn messages_idempotency_and_delivery() {
    let h = common::start().await;
    let chat = chat(&h).await;
    let turn = TurnId::from("t1");
    let (user_event, reply_event) = write(&h, move |tx| {
        tx.execute(
            "INSERT OR IGNORE INTO persons (id, name, created_at) VALUES ('p7', 'Ann', 1)",
            [],
        )?;
        let seg = tx
            .turns()
            .insert_segment(&new_segment(chat, SegmentReason::Initial))?;
        let u = append(tx, chat, seg, Some(&turn), &user("hi"))?;
        let person = PersonId::numbered(7);
        let seq = tx.turns().insert_message(&NewMessage {
            chat,
            event: u,
            role: Role::User,
            sender: Some(&person),
            external_id: Some("ext-1"),
            reply_to_external_id: None,
            turn_id: Some(&turn),
        })?;
        assert_eq!(seq, 1);
        let r = append(
            tx,
            chat,
            seg,
            Some(&turn),
            &Payload::Message(MessagePayload::assistant(
                ModelId::from("m"),
                vec![Block::text("yo")],
                None,
                false,
                u,
            )),
        )?;
        let seq = tx.turns().insert_message(&NewMessage {
            chat,
            event: r,
            role: Role::Assistant,
            sender: None,
            external_id: None,
            reply_to_external_id: Some("ext-1"),
            turn_id: Some(&turn),
        })?;
        assert_eq!(seq, 2);
        Ok((u, r))
    })
    .await;

    let known = h
        .query(move |c| c.turns().by_external_id(chat, "ext-1"))
        .await
        .expect("known");
    assert_eq!(known.event, user_event);
    assert_eq!(known.turn, Some(TurnId::from("t1")));
    assert!(
        h.query(move |c| c.turns().by_external_id(chat, "nope"))
            .await
            .is_none()
    );

    let outcomes = write(&h, move |tx| {
        Ok([
            tx.turns().set_delivered(chat, reply_event, "ext-2")?,
            tx.turns().set_delivered(chat, reply_event, "ext-3")?,
            tx.turns().set_delivered(chat, user_event, "ext-4")?,
        ])
    })
    .await;
    assert_eq!(
        outcomes,
        [
            Delivered::Ok,
            Delivered::AlreadyDelivered {
                external_id: "ext-2".into()
            },
            Delivered::NotFound
        ]
    );
}

#[tokio::test]
async fn tool_results_dedup_within_segment_and_recall() {
    let h = common::start().await;
    let chat = chat(&h).await;
    let key = blake3::hash(b"k").as_bytes().to_vec();
    let found = write(&h, move |tx| {
        let seg = tx
            .turns()
            .insert_segment(&new_segment(chat, SegmentReason::Initial))?;
        let write_result = |dedup_of: Option<Tseq>,
                            synthetic: Option<Synthetic>,
                            content: Option<&[u8]>|
         -> DbResult<Tseq> {
            let tseq = tx.turns().next_tseq(chat)?;
            append(
                tx,
                chat,
                seg,
                None,
                &Payload::ToolUse(ToolUsePayload {
                    call_id: format!("c{}", tseq.0),
                    tool_id: None,
                    name: "fs_read".into(),
                    input: json!({}),
                }),
            )?;
            let payload = match synthetic {
                Some(kind) => ToolResultPayload::synthetic(format!("c{}", tseq.0), tseq, kind),
                None => ToolResultPayload {
                    call_id: format!("c{}", tseq.0),
                    tseq,
                    is_error: false,
                    content: Vec::new(),
                    dedup_of,
                    synthetic: None,
                },
            };
            let event = append(tx, chat, seg, None, &Payload::ToolResult(payload))?;
            tx.turns().insert_result(&NewToolResult {
                chat,
                tseq,
                event,
                segment: seg,
                tool_id: None,
                dedup_key: &key,
                dedup_of,
                content,
                synthetic: synthetic.map(Synthetic::as_str),
                created_at: at(10),
            })?;
            Ok(tseq)
        };
        let synthetic = write_result(
            None,
            Some(Synthetic::Interrupted),
            Some(b"[kernel] tool call interrupted"),
        )?;
        let none_yet = tx.turns().find_dedup(chat, seg, &key)?;
        let verbatim = write_result(None, None, Some(b"line1\nline2"))?;
        let reference = write_result(Some(verbatim), None, None)?;
        let found = tx.turns().find_dedup(chat, seg, &key)?;
        tx.turns().close_segment(seg, at(20))?;
        let next = tx
            .turns()
            .insert_segment(&new_segment(chat, SegmentReason::Compact))?;
        let elsewhere = tx.turns().find_dedup(chat, next, &key)?;
        Ok((synthetic, none_yet, verbatim, reference, found, elsewhere))
    })
    .await;
    let (synthetic, none_yet, verbatim, reference, found, elsewhere) = found;
    assert_eq!(
        (synthetic, verbatim, reference),
        (Tseq(1), Tseq(2), Tseq(3)),
        "tseq is per chat"
    );
    assert_eq!(none_yet, None, "synthetic results are not dedup targets");
    assert_eq!(found, Some(verbatim));
    assert_eq!(elsewhere, None, "dedup never crosses a segment");

    let (full, through_ref, missing) = h
        .query(move |c| {
            Ok((
                c.turns().result_content(chat, verbatim)?,
                c.turns().result_content(chat, reference)?,
                c.turns().result_content(chat, Tseq(99))?,
            ))
        })
        .await;
    assert_eq!(full.as_deref(), Some(&b"line1\nline2"[..]));
    assert_eq!(through_ref, full, "recall resolves dedup_of");
    assert_eq!(missing, None);
}

#[tokio::test]
async fn ratio_anchor_gap_and_segment_state() {
    let h = common::start().await;
    let chat = chat(&h).await;
    let model = ModelId::from("m");
    let m = model.clone();
    write(&h, move |tx| {
        assert_eq!(tx.turns().ratio(&m)?, None);
        tx.turns().set_ratio(&m, 3.5, at(1))?;
        tx.turns().set_ratio(&m, 3.75, at(2))?;
        tx.turns().set_anchor(
            chat,
            Anchor {
                event: EventId(42),
                tokens: 1234,
            },
        )?;
        tx.turns().set_gap(
            chat,
            Gap {
                ema_ns: Some(6e10),
                last_request_at: Some(at(9)),
            },
        )?;
        tx.execute(
            "UPDATE chats SET reset_pending = 'reset' WHERE id = ?1",
            [chat.0],
        )?;
        tx.execute(
            "UPDATE surfaces SET compact_at = 0.6 WHERE id = 'local'",
            [],
        )?;
        Ok(())
    })
    .await;
    let (ratio, samples, anchor, gap, pending, compact_at) = h
        .query(move |c| {
            Ok((
                c.turns().ratio(&model)?,
                c.query_row(
                    "SELECT samples FROM token_ratios WHERE model = 'm'",
                    [],
                    |r| r.get::<_, i64>(0),
                )?,
                c.turns().anchor(chat)?,
                c.turns().gap(chat)?,
                c.turns().reset_pending(chat)?,
                c.turns().compact_at(chat)?,
            ))
        })
        .await;
    assert_eq!((ratio, samples), (Some(3.75), 2));
    assert_eq!(
        anchor,
        Some(Anchor {
            event: EventId(42),
            tokens: 1234
        })
    );
    assert_eq!(
        gap,
        Gap {
            ema_ns: Some(6e10),
            last_request_at: Some(at(9))
        }
    );
    assert!(pending);
    assert_eq!(compact_at, Some(0.6));

    write(&h, move |tx| {
        tx.turns().clear_anchor(chat)?;
        tx.turns().clear_reset_pending(chat)?;
        tx.execute("UPDATE chats SET compact_at = 0.5 WHERE id = ?1", [chat.0])?;
        Ok(())
    })
    .await;
    let (anchor, pending, compact_at) = h
        .query(move |c| {
            Ok((
                c.turns().anchor(chat)?,
                c.turns().reset_pending(chat)?,
                c.turns().compact_at(chat)?,
            ))
        })
        .await;
    assert_eq!(anchor, None);
    assert!(!pending);
    assert_eq!(
        compact_at,
        Some(0.5),
        "the chat level wins over the surface"
    );
}

#[tokio::test]
async fn hygiene_lands_in_the_open_segment_only() {
    let h = common::start().await;
    let chat = chat(&h).await;
    let payload = || {
        HygienePayload::MemoryUpdated(MemoryUpdatedPayload {
            added: vec!["m1 · core · 1d · i0.5 · tea".into()],
            removed: Vec::new(),
        })
    };
    let shared = h.kernel.shared().clone();
    let none = log::append_hygiene(&shared, chat, payload())
        .await
        .expect("append");
    assert_eq!(
        none, None,
        "no open segment — the next snapshot rebuilds the section"
    );

    let seg = write(&h, move |tx| {
        tx.turns()
            .insert_segment(&new_segment(chat, SegmentReason::Initial))
    })
    .await;
    let id = log::append_hygiene(&shared, chat, payload())
        .await
        .expect("append")
        .expect("event");
    let rows = h
        .query(move |c| c.turns().events_of_segment(seg, None))
        .await;
    let event = Event::try_from(rows.into_iter().next().expect("row")).expect("decodes");
    assert_eq!(event.id, id);
    assert_eq!((event.step, event.turn_id), (None, None));
    assert!(matches!(event.payload, Payload::MemoryUpdated(_)));
}
