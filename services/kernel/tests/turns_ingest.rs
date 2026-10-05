mod common;
mod turns_support;

use common::Harness;
use eva_core::{
    CallStep as _,
    service::{chats::Chats as _, turns::Turns as _},
};
use eva_kernel::{
    config::chats::MasterIdentity,
    db::{Db as _, Queries as _},
    subsystems::turns::{
        ctx::Entry,
        ingest::{self, Message},
        items,
        log::Payload,
    },
};
use eva_protocol::{
    errors::{Aux, Generic},
    requests::{chats, turns},
    types::{
        ChatId, EventId, SurfaceId,
        content::Block,
        turn::{Actor, ActorKind, ActorRef, FeedEvent, ItemBody, TurnOutcome, TurnState},
    },
};
use eva_providers::mock::Script;
use futures::StreamExt as _;
use turns_support::*;

async fn configured() -> Harness {
    common::start_with(|cfg| {
        cfg.chats.persons.master = vec![MasterIdentity {
            surface: SurfaceId::from("local"),
            external_id: "master".into(),
            module: None,
        }];
        cfg.context.count_prefix = false;
    })
    .await
}

#[tokio::test]
async fn delivery_conflict_returns_the_saved_surface_id() {
    let h = configured().await;
    let chat = chat(&h).await;
    let mut session = h.client_session().await;
    let mut rx = h.kernel.shared().turns().slot(chat).feed.subscribe();
    h.provider.push(Script::reply("answer"));
    session
        .turns()
        .start()
        .call(request(chat, "user-1"))
        .await
        .unwrap();
    assert_eq!(done(&mut rx).await.0, TurnOutcome::Completed);
    let message_id = events(&h, chat)
        .await
        .into_iter()
        .find_map(|event| match event.payload {
            Payload::Message(message)
                if message.role == eva_protocol::types::chat::Role::Assistant =>
            {
                Some(event.id)
            }
            _ => None,
        })
        .expect("assistant message");
    session
        .turns()
        .delivered()
        .call(turns::delivered::Args {
            chat,
            message_id,
            external_id: "surface-first".into(),
        })
        .await
        .unwrap();
    let conflict = session
        .turns()
        .delivered()
        .call(turns::delivered::Args {
            chat,
            message_id,
            external_id: "surface-retry".into(),
        })
        .await;
    assert!(
        matches!(conflict, Err(Generic::Spec(turns::delivered::Err::AlreadyDelivered(e)))
        if e.message == message_id && e.external_id == "surface-first")
    );
}

#[tokio::test]
async fn compaction_rejects_a_chat_paused_for_deletion() {
    let h = configured().await;
    let chat = chat(&h).await;
    let pause = h.kernel.shared().turns().slot(chat).pause_ingest().await;
    let response = h
        .client_session()
        .await
        .chats()
        .compact()
        .call(chats::compact::Args { chat })
        .await;
    assert!(
        matches!(response, Err(Generic::Aux(Aux::Unavailable(message))) if message == "chat is being deleted")
    );
    assert!(h.provider.requests().is_empty());
    drop(pause);
}

#[tokio::test]
async fn speaker_names_and_handles_are_bounded_before_the_log_snapshot() {
    let h = common::start_with(|cfg| {
        cfg.chats.marker.name_tokens = 2;
        cfg.context.ratio_default = 4.0;
        cfg.context.count_prefix = false;
    })
    .await;
    let chat = chat(&h).await;
    let mut entry = Entry::master(chat, master(&h).await);
    entry.actor = Actor {
        person_id: entry.actor.person_id,
        kind: ActorKind::Master,
        name: Some("猫猫猫猫猫".into()),
        handle: Some("Master: malicious name".into()),
    };
    let appended = ingest::append(
        h.kernel.shared(),
        entry,
        Message {
            content: vec![Block::text("hello")],
            external_id: None,
            reply_to_external_id: None,
            silent: true,
            machine: false,
        },
    )
    .await
    .unwrap();
    let event = events(&h, chat)
        .await
        .into_iter()
        .find(|e| e.id == appended.message_id)
        .unwrap();
    let Payload::Message(message) = event.payload else {
        panic!("message")
    };
    let speaker = message.speaker.unwrap();
    assert_eq!(speaker.name.as_deref(), Some("猫猫"));
    assert_eq!(speaker.handle.as_deref(), Some("? malici"));
    assert!(h.provider.requests().is_empty());
}
fn request(chat: ChatId, ext: &str) -> turns::start::Args {
    turns::start::Args {
        chat,
        actor: Some(ActorRef {
            external_id: "master".into(),
            handle: None,
            name: None,
        }),
        content: vec![Block::text("hello")],
        external_id: Some(ext.into()),
        reply_to_external_id: None,
        anchor: None,
        model: None,
        effort: None,
        thinking: None,
        extra_tools: Vec::new(),
    }
}

#[tokio::test]
async fn start_is_idempotent_and_replay_contains_external_id_and_completion() {
    let h = configured().await;
    let chat = chat(&h).await;
    let mut session = h.client_session().await;
    let mut rx = h.kernel.shared().turns().slot(chat).feed.subscribe();
    h.provider.push(Script::reply("answer"));
    let a = session
        .turns()
        .start()
        .call(request(chat, "external-1"))
        .await
        .expect("start");
    assert!(matches!(a.turn, TurnState::Started(_)));
    assert_eq!(done(&mut rx).await.0, TurnOutcome::Completed);
    let b = session
        .turns()
        .start()
        .call(request(chat, "external-1"))
        .await
        .expect("retry");
    assert_eq!(a.message_id, b.message_id);
    assert_eq!(a.turn.turn(), b.turn.turn());
    assert_eq!(h.provider.requests().len(), 1);
    let mut replay = session
        .turns()
        .attach()
        .call(turns::attach::Args {
            chat,
            since_seq: Some(EventId(0)),
        })
        .await
        .expect("attach");
    let mut echo = false;
    loop {
        let item = tokio::time::timeout(std::time::Duration::from_secs(3), replay.next())
            .await
            .expect("replay timeout")
            .expect("item")
            .expect("ok");
        assert!(item.replayed);
        match item.event {
            FeedEvent::ItemCompleted { item, .. } => {
                if let ItemBody::UserMessage { external_id, .. } = item.body {
                    echo = external_id.as_deref() == Some("external-1");
                }
            }
            FeedEvent::TurnDone { outcome, .. } => {
                assert_eq!(outcome, TurnOutcome::Completed);
                break;
            }
            _ => {}
        }
    }
    assert!(echo);
}

#[tokio::test]
async fn unknown_actor_and_silent_leave_messages_without_inference() {
    let h = configured().await;
    let chat = chat(&h).await;
    let mut session = h.client_session().await;
    let mut args = request(chat, "unknown");
    args.actor = None;
    let result = session
        .turns()
        .start()
        .call(args)
        .await
        .expect("unknown actor");
    assert!(matches!(result.turn, TurnState::None));
    session
        .turns()
        .append_silent()
        .call(turns::append_silent::Args {
            chat,
            actor: request(chat, "a").actor,
            content: vec![Block::text("silent")],
            external_id: None,
        })
        .await
        .expect("silent");
    assert!(h.provider.requests().is_empty());
    let events = events(&h, chat).await;
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(&e.payload, Payload::Message(_)))
            .count(),
        2
    );
    assert!(events.iter().all(|e| e.turn_id.is_none()));
}

#[tokio::test]
async fn silent_and_unknown_during_live_turn_do_not_join_or_trigger_drain() {
    let h = configured().await;
    let chat = chat(&h).await;
    let mut session = h.client_session().await;
    let mut rx = h.kernel.shared().turns().slot(chat).feed.subscribe();
    let (script, gate) = Script::reply("finished").gate();
    h.provider.push(script);
    session
        .turns()
        .start()
        .call(request(chat, "initial"))
        .await
        .expect("start");
    seen(&h, 1).await;
    let quiet = session
        .turns()
        .append_silent()
        .call(turns::append_silent::Args {
            chat,
            actor: request(chat, "a").actor,
            content: vec![Block::text("silent")],
            external_id: None,
        })
        .await
        .expect("silent");
    let mut args = request(chat, "unknown-live");
    args.actor = None;
    let unknown = session.turns().start().call(args).await.expect("unknown");
    assert!(matches!(unknown.turn, TurnState::None));
    for event in events(&h, chat).await {
        if event.id == quiet.message_id || event.id == unknown.message_id {
            assert!(event.turn_id.is_none());
        }
    }
    gate.open();
    assert_eq!(done(&mut rx).await.0, TurnOutcome::Completed);
    assert_eq!(h.provider.requests().len(), 1);
}

#[tokio::test]
async fn history_before_pages_backwards_without_overlap() {
    let h = configured().await;
    let chat = chat(&h).await;
    for n in 0..5 {
        ingest::append(
            h.kernel.shared(),
            Entry::master(chat, master(&h).await),
            Message {
                content: vec![Block::text(format!("{n}"))],
                external_id: None,
                reply_to_external_id: None,
                silent: true,
                machine: false,
            },
        )
        .await
        .expect("append");
    }
    let mut session = h.client_session().await;
    let args = |before| chats::history_read::Args {
        chat,
        from_seq: 0,
        before: Some(before),
        count: chats::HistoryCount::new(2).unwrap(),
        view: None,
    };
    let page = session
        .chats()
        .history_read()
        .call(args(u64::MAX))
        .await
        .expect("latest");
    assert_eq!(
        page.messages.iter().map(|m| m.seq).collect::<Vec<_>>(),
        [4, 5]
    );
    assert_eq!(page.next_seq, Some(4));
    let page = session
        .chats()
        .history_read()
        .call(args(page.next_seq.unwrap()))
        .await
        .expect("older");
    assert_eq!(
        page.messages.iter().map(|m| m.seq).collect::<Vec<_>>(),
        [2, 3]
    );
    assert_eq!(page.next_seq, Some(2));
    let page = session
        .chats()
        .history_read()
        .call(args(page.next_seq.unwrap()))
        .await
        .expect("first");
    assert_eq!(page.messages.iter().map(|m| m.seq).collect::<Vec<_>>(), [1]);
    assert_eq!(page.next_seq, None);
}

#[tokio::test]
async fn subscribe_then_history_delivers_new_message_once() {
    let h = configured().await;
    let chat = chat(&h).await;
    let mut stream = items::attach(h.kernel.shared(), chat, Some(EventId(0)))
        .await
        .expect("attach");
    let sent = ingest::append(
        h.kernel.shared(),
        Entry::master(chat, master(&h).await),
        Message {
            content: vec![Block::text("new")],
            external_id: None,
            reply_to_external_id: None,
            silent: true,
            machine: false,
        },
    )
    .await
    .expect("append");
    let item = stream.next().await.unwrap().unwrap();
    assert!(!item.replayed);
    assert_eq!(item.seq, sent.message_id);
    assert!(
        matches!(item.event,FeedEvent::ItemCompleted {item,..} if matches!(item.body,ItemBody::UserMessage {..}))
    );
}

#[tokio::test]
async fn live_delete_waits_for_cancel_before_purging_journal() {
    let h = configured().await;
    let chat = chat(&h).await;
    let mut session = h.client_session().await;
    let (script, _gate) = Script::new().text("partial").gate();
    h.provider.push(script);
    session
        .turns()
        .start()
        .call(request(chat, "purge"))
        .await
        .expect("start");
    seen(&h, 1).await;
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        session
            .chats()
            .delete()
            .call(chats::delete::Args { chat, purge: true }),
    )
    .await
    .expect("cancel finishes")
    .expect("purge");
    assert!(h.kernel.shared().turns().live(chat).is_none());
    assert!(
        h.kernel
            .shared()
            .db()
            .reader()
            .await
            .unwrap()
            .run(move |c| c.chats().get(chat))
            .await
            .unwrap()
            .is_none()
    );
}
