mod common;
mod turns_support;

use common::Harness;
use eva::time::Timestamp;
use eva_kernel::{
    db::{Db as _, DbResult, Queries as _, notes::NoteRow},
    subsystems::{
        notes,
        turns::{
            compaction, ctx,
            ingest::{self, Message},
            integration,
        },
    },
};
use eva_protocol::types::{ChatId, MemoryId, ModelId, content::Block, note::Mode};
use turns_support::{chat, master};

fn note(bytes: usize, pinned: bool) -> NoteRow {
    NoteRow {
        name: "plan".into(),
        description: "Current work".into(),
        body: "p".repeat(bytes),
        prev_body: None,
        pinned,
        created_at: Timestamp::TEST_ORIGIN,
        updated_at: Timestamp::TEST_ORIGIN,
    }
}

async fn context(h: &Harness, chat: ChatId) -> ctx::Ctx {
    let entry = ctx::Entry::master(chat, master(h).await);
    let llm = h.kernel.shared().cfg().llm.clone();
    h.kernel
        .shared()
        .db()
        .reader()
        .await
        .unwrap()
        .run(move |c| ctx::resolve(c, entry, &llm))
        .await
        .unwrap()
}

async fn ratio(h: &Harness, model: &str, ratio: f64) {
    let model = ModelId::from(model);
    let now = h.kernel.shared().now();
    h.kernel
        .shared()
        .db()
        .write(move |tx| tx.turns().set_ratio(&model, ratio, now))
        .await
        .unwrap();
}

#[tokio::test]
async fn segment_memory_and_pinned_notes_use_the_calibrated_model_ratio() {
    let h = common::start_with(|cfg| {
        cfg.context.count_prefix = false;
        cfg.context.ratio_default = 4.0;
        cfg.memory.budget_tokens = 1000;
        cfg.memory.hot.core_share = 0.0;
        cfg.memory.hot.split.fact = 1.0;
        cfg.memory.hot.split.event = 0.0;
        cfg.memory.hot.split.procedure = 0.0;
        cfg.notes.pinned_budget_tokens = 100;
        cfg.notes.max_note_tokens = 100;
    })
    .await;
    let chat = chat(&h).await;
    let id = h.kernel.shared().db().write(move |tx| {
        let id = tx.query_row(
            "INSERT INTO memory (kind, text, visibility, created_at, updated_at, valid_from, last_reinforced_at) VALUES ('fact', ?1, 'public', 1, 1, 1, 1) RETURNING id",
            ["x".repeat(2500)], |r| r.get(0),
        )?;
        tx.notes().insert(chat, &note(300, true))?;
        let mut state = notes::state::initial(Timestamp::TEST_ORIGIN);
        state.mode = Mode::Index;
        tx.notes().save_state(chat, &state)?;
        DbResult::Ok(MemoryId(id))
    }).await.unwrap();
    let ctx = context(&h, chat).await;
    ratio(&h, common::MODEL, 2.0).await;
    let low = compaction::snapshot(h.kernel.shared(), &ctx).await.unwrap();
    assert!(
        !low.hot_memory_ids.contains(&id),
        "1250-token fact exceeds the 1000-token hot budget"
    );
    let low_notes = &low
        .system
        .iter()
        .find(|section| section.name == "chat")
        .unwrap()
        .text;
    assert!(
        low_notes.contains("body NOT quoted"),
        "150-token pin exceeds the 100-token pinned budget"
    );

    ratio(&h, common::MODEL, 8.0).await;
    let high = compaction::snapshot(h.kernel.shared(), &ctx).await.unwrap();
    assert!(high.hot_memory_ids.contains(&id));
    let high_notes = &high
        .system
        .iter()
        .find(|section| section.name == "chat")
        .unwrap()
        .text;
    assert!(high_notes.contains(&"p".repeat(300)));
    assert!(!high_notes.contains("body NOT quoted"));
    assert!(h.provider.requests().is_empty());
}

#[tokio::test]
async fn notes_entry_and_pressure_follow_the_current_model_calibration() {
    let mut mock = common::mock_config();
    mock.models[0].context_window = 20_000;
    let mut alternate = mock.models[0].clone();
    alternate.name = ModelId::from("alternate");
    mock.models.push(alternate);
    let h = common::start_with_mock(
        |cfg| {
            cfg.context.count_prefix = false;
            cfg.context.ratio_default = 4.0;
            cfg.notes.budget_max_tokens = 150;
            cfg.notes.wrap_up_reserve_tokens = 0;
        },
        mock,
    )
    .await;
    let chat = chat(&h).await;
    h.kernel
        .shared()
        .db()
        .write(move |tx| tx.notes().insert(chat, &note(400, false)))
        .await
        .unwrap();
    ratio(&h, common::MODEL, 2.0).await;
    ratio(&h, "alternate", 8.0).await;
    let ctx = context(&h, chat).await;
    h.kernel.shared().turns().slot(chat).set_ctx(ctx.clone());
    integration::enter(h.kernel.shared(), &ctx).await.unwrap();
    let state = h
        .kernel
        .shared()
        .db()
        .reader()
        .await
        .unwrap()
        .run(move |c| c.notes().state(chat))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        state.mode,
        Mode::Squeeze,
        "calibrated fill exceeds 150 tokens; default ratio would fit"
    );

    ingest::append(
        h.kernel.shared(),
        ctx::Entry::master(chat, master(&h).await),
        Message {
            content: vec![Block::text("x".repeat(30_000))],
            external_id: None,
            reply_to_external_id: None,
            silent: true,
            machine: false,
        },
    )
    .await
    .unwrap();
    let notes = &h.kernel.shared().ports().notes;
    let low = notes
        .pressure_line(h.kernel.shared(), chat, ctx.model.clone())
        .await
        .unwrap();
    let high = notes
        .pressure_line(h.kernel.shared(), chat, ModelId::from("alternate"))
        .await
        .unwrap();
    let low_pct = notes::budget::fill(&[note(400, false)], 150, Some(2.0)).percent();
    let high_pct = notes::budget::fill(&[note(400, false)], 150, Some(8.0)).percent();
    assert!(low_pct > 100 && high_pct < 100);
    assert!(low.contains(&format!("{low_pct}% of budget")), "{low}");
    assert!(high.contains(&format!("{high_pct}% of budget")), "{high}");
    assert!(low.contains("[window]"), "{low}");
    assert!(!high.contains("[window]"), "{high}");
    let state = h
        .kernel
        .shared()
        .db()
        .reader()
        .await
        .unwrap()
        .run(move |c| c.notes().state(chat))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        state.mode,
        Mode::Squeeze,
        "pressure never changes the mode inside a turn"
    );
    assert!(h.provider.requests().is_empty());
}

#[tokio::test]
async fn downshift_compaction_uses_target_and_summary_model_calibrations() {
    use eva_kernel::subsystems::turns::{
        engine::Turn,
        log::{self, Append, Payload, ResultContent, ToolResultPayload, ToolUsePayload},
    };
    use eva_protocol::types::{
        Tseq, TurnId,
        chat::SegmentReason,
        generate::{Block as GenBlock, CacheMode, Message as GenMessage, Purpose},
    };
    use eva_providers::mock::{Exhausted, Script};

    let mut mock = common::mock_config();
    mock.on_exhausted = Exhausted::Fail;
    let mut target = mock.models[0].clone();
    target.name = ModelId::from("small-target");
    target.context_window = 2000;
    target.max_output = 1000;
    let mut summary = target.clone();
    summary.name = ModelId::from("summary-model");
    summary.context_window = 900;
    summary.max_output = 900;
    mock.models.extend([target.clone(), summary]);
    let h = common::start_with_mock(
        |cfg| {
            cfg.context.count_prefix = false;
            cfg.context.ratio_default = 4.0;
            cfg.context.summary_tokens = 400;
            cfg.context.summary_result_tokens = 10;
            cfg.context.verbatim_share = 0.75;
            cfg.llm.roles.summary = Some(ModelId::from("summary-model"));
        },
        mock,
    )
    .await;
    let chat = chat(&h).await;
    ratio(&h, common::MODEL, 8.0).await;
    ratio(&h, "small-target", 2.0).await;
    ratio(&h, "summary-model", 8.0).await;
    for (body, machine) in [
        ("history ".repeat(500), true),
        (format!("older {}", "a".repeat(200)), false),
        (format!("latest {}", "b".repeat(200)), false),
    ] {
        ingest::append(
            h.kernel.shared(),
            ctx::Entry::master(chat, master(&h).await),
            Message {
                content: vec![Block::text(body)],
                external_id: None,
                reply_to_external_id: None,
                silent: true,
                machine,
            },
        )
        .await
        .unwrap();
    }
    let ctx = context(&h, chat).await;
    let segment = compaction::ensure_segment(h.kernel.shared(), &ctx)
        .await
        .unwrap();
    let now = h.kernel.shared().now();
    h.kernel
        .shared()
        .db()
        .write(move |tx| {
            let at = Append {
                chat,
                segment,
                turn_id: None,
                step: None,
                now,
            };
            log::append(
                tx,
                at,
                &Payload::ToolUse(ToolUsePayload {
                    call_id: "sample".into(),
                    tool_id: None,
                    name: "sample".into(),
                    input: serde_json::json!({}),
                }),
            )?;
            log::append(
                tx,
                at,
                &Payload::ToolResult(ToolResultPayload {
                    call_id: "sample".into(),
                    tseq: Tseq(1),
                    is_error: false,
                    content: vec![ResultContent::text("z".repeat(200))],
                    dedup_of: None,
                    synthetic: None,
                }),
            )?;
            DbResult::Ok(())
        })
        .await
        .unwrap();
    h.provider.push(Script::reply("s".repeat(600)));
    h.provider.push(Script::reply("s".repeat(200)));
    let shared = h.kernel.shared();
    let slot = shared.turns().slot(chat);
    let live = {
        let _gate = slot.gate.lock().await;
        shared.turns().claim(&slot, TurnId::from("ratio-downshift"))
    };
    let mut turn = Turn::new(shared.clone(), slot.clone(), live.clone(), ctx, segment);
    let result = turn.compact(SegmentReason::Downshift, &target).await;
    {
        let _gate = slot.gate.lock().await;
        slot.set_live(None);
        live.closed.cancel();
    }
    result.unwrap();

    let requests = h.provider.requests();
    assert_eq!(
        requests.len(),
        2,
        "600 bytes fit the source and default ratios but need squeezing for the target"
    );
    for request in &requests {
        assert_eq!(request.model.as_str(), "summary-model");
        assert_eq!(request.purpose, Purpose::Compaction);
        assert_eq!(request.cache, CacheMode::Off);
    }
    let first = &requests[0];
    let input = first
        .messages
        .iter()
        .flat_map(|message| match message {
            GenMessage::User { blocks }
            | GenMessage::Assistant { blocks }
            | GenMessage::System { blocks, .. } => blocks,
        })
        .filter_map(|block| match block {
            GenBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        input.contains(&"z".repeat(80)),
        "ten summary-model tokens keep eighty bytes"
    );
    assert!(!input.contains(&"z".repeat(81)));
    let bytes = input.len()
        + first
            .system
            .iter()
            .map(|section| section.text.len())
            .sum::<usize>();
    assert!(
        bytes > 900 * 4,
        "the default ratio would wrongly exhaust the summary window"
    );
    assert!(first.max_tokens > 0);
    assert!(bytes.div_ceil(8) as u32 + first.max_tokens <= 900);
    assert!(format!("{:?}", requests[1].messages).contains("Squeeze the summary"));

    let new = turn.segment;
    let row = shared
        .db()
        .reader()
        .await
        .unwrap()
        .run(move |c| c.turns().segment(new))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.reason, SegmentReason::Downshift);
    assert_eq!(row.summary.as_deref(), Some("s".repeat(200).as_str()));
    assert_eq!(row.summary_attempts, 2);
    assert_eq!(
        row.summary_model.as_ref().map(ModelId::as_str),
        Some("summary-model")
    );
    let tail = row.verbatim_tail.unwrap();
    assert!(tail.contains("latest"));
    assert!(
        !tail.contains("older"),
        "both replies fit only when the target calibration is ignored"
    );
}
