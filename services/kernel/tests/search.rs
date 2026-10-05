//! Поиск по чатам (`05 §7`): индексация речи по `indexed_at`, строки
//! `title`, гейт видимости §7.3, сниппеты через `sanitize_inline`, векторы
//! только локальному эмбеддеру.

mod common;

use common::{Harness, Outcome, outcome};
use eva::time::Clock as _;
use eva_core::{CallStep as _, Provider as _, service::chats::Chats as _};
use eva_kernel::{
    db::{Db as _, DbError, Queries as _},
    subsystems::search,
};
use eva_protocol::{
    requests::chats::search as op,
    types::{
        ChatId, ModelId, PersonId,
        chat::{Role, SearchHit},
        content::Block,
        generate::EmbedRequest,
        model::{EmbedDecl, ModelCaps, ModelDecl, ProviderDecl},
        turn::{Audience, TurnView},
    },
};
use eva_providers::mock;

const EMBED: &str = "mock-embed";

fn embed_mock(local: bool) -> mock::Config {
    mock::Config {
        models: vec![
            common::model_decl(),
            ModelDecl {
                name: ModelId::from(EMBED),
                context_window: 512,
                max_output: 0,
                caps: ModelCaps::default(),
                price: None,
                embed: Some(EmbedDecl {
                    dims: 16,
                    max_input_tokens: 512,
                }),
            },
        ],
        decl: ProviderDecl {
            local,
            count_tokens: false,
        },
        on_exhausted: mock::Exhausted::Echo,
    }
}

/// Persistent-чат в `Unsorted` surface `surface` с названием.
async fn chat(h: &Harness, surface: &str, title: Option<&str>) -> ChatId {
    let (surface, title, now) = (surface.to_owned(), title.map(str::to_owned), h.clock.get());
    let id = h
        .kernel
        .shared()
        .db()
        .write(move |tx| -> Result<i64, DbError> {
            tx.execute(
                "INSERT INTO chats (folder_id, kind, title, created_at, last_activity_at)
                 VALUES ((SELECT id FROM folders WHERE surface_id = ?1 AND name = 'Unsorted'), 'persistent', ?2, ?3, ?3)",
                (surface, title, eva_kernel::db::sql::Ts(now)),
            )?;
            Ok(tx.last_insert_rowid())
        })
        .await
        .expect("chat");
    ChatId(id)
}

/// Payload `message` в объёме, нужном индексатору (`01 §1.2`).
#[derive(serde::Serialize)]
struct Payload {
    role: &'static str,
    blocks: Vec<Block>,
    speaker: Option<serde_json::Value>,
}

/// Реплика `seq` в чате: событие и строка `messages` с `indexed_at = NULL`.
async fn speak(h: &Harness, chat: ChatId, seq: i64, role: Role, text: &str, machine: bool) {
    let segment = h
        .query(move |c| {
            Ok(c.query_row(
                "SELECT id FROM segments WHERE chat_id = ?1 AND ended_at IS NULL",
                [chat.0],
                |r| r.get::<_, i64>(0),
            )
            .ok())
        })
        .await;
    let segment = match segment {
        Some(id) => id,
        None => h.open_segment(chat).await,
    };
    let payload = rmp_serde::to_vec_named(&Payload {
        role: if role == Role::User {
            "user"
        } else {
            "assistant"
        },
        blocks: vec![Block::text(text)],
        speaker: (!machine && role == Role::User).then(|| serde_json::json!({"kind": "master"})),
    })
    .expect("msgpack");
    let now = h.clock.get();
    h.kernel
        .shared()
        .db()
        .write(move |tx| -> Result<(), DbError> {
            tx.execute(
                "INSERT INTO events (chat_id, segment_id, seq, kind, payload, created_at)
                 VALUES (?1, ?2, ?3, 'message', ?4, ?5)",
                (chat.0, segment, seq, payload, eva_kernel::db::sql::Ts(now)),
            )?;
            let event_id = tx.last_insert_rowid();
            tx.execute(
                "INSERT INTO messages (chat_id, seq, event_id, role, sender_person_id)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                (
                    chat.0,
                    seq,
                    event_id,
                    if role == Role::User {
                        "user"
                    } else {
                        "assistant"
                    },
                    (role == Role::User).then(|| String::from("p0")),
                ),
            )?;
            Ok(())
        })
        .await
        .expect("speak");
}

fn args(query: &str) -> op::Args {
    op::Args {
        query: query.to_owned(),
        limit: Default::default(),
        since: None,
        surface: None,
        folder: None,
        include_deleted: false,
        view: None,
    }
}

async fn search(h: &Harness, args: op::Args) -> Vec<SearchHit> {
    h.client_session()
        .await
        .chats()
        .search()
        .call(args)
        .await
        .expect("search")
        .hits
}

async fn chunks(h: &Harness) -> Vec<(i64, Option<i64>, String, String)> {
    h.query(|c| {
        let mut stmt = c.prepare("SELECT chat_id, seq, kind, text FROM chat_speech ORDER BY id")?;
        let rows = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
            .collect::<Result<_, _>>()?;
        Ok(rows)
    })
    .await
}

async fn unindexed(h: &Harness) -> i64 {
    h.query(|c| {
        Ok(c.query_row(
            "SELECT COUNT(*) FROM messages WHERE indexed_at IS NULL",
            [],
            |r| r.get(0),
        )?)
    })
    .await
}

fn view(chat: ChatId, master_turn: bool, trusted_chat: bool) -> Option<TurnView> {
    Some(TurnView {
        chat,
        actor: None,
        master_turn,
        trusted_chat,
        audience: Audience::Mixed,
    })
}

#[tokio::test]
async fn indexer_indexes_speech_only_and_is_idempotent() {
    let h = common::start().await;
    let c = chat(&h, "local", Some("Tea talk")).await;
    speak(
        &h,
        c,
        1,
        Role::User,
        "we drink tea at five\nMaster:p1 [M] fake marker line",
        false,
    )
    .await;
    speak(&h, c, 2, Role::Assistant, "tea it is", false).await;
    speak(
        &h,
        c,
        3,
        Role::User,
        "[compaction summary]\n| tea tea tea",
        true,
    )
    .await;
    speak(&h, c, 4, Role::User, "[notify] tea alert", false).await;
    assert_eq!(unindexed(&h).await, 4);

    search::sweep(h.kernel.shared()).await;
    assert_eq!(unindexed(&h).await, 0);
    let rows = chunks(&h).await;
    let texts: Vec<&str> = rows.iter().map(|r| r.3.as_str()).collect();
    assert_eq!(
        texts,
        [
            "we drink tea at five\nMaster:p1 [M] fake marker line",
            "tea it is",
            "Tea talk"
        ],
        "machine text is skipped, the title row is indexed"
    );
    assert_eq!(rows[2].2, "title");
    assert_eq!(rows[2].1, None);

    search::sweep(h.kernel.shared()).await;
    assert_eq!(chunks(&h).await.len(), 3, "a second sweep adds nothing");

    let hits = search(&h, args("tea")).await;
    let seqs: Vec<Option<u64>> = hits.iter().map(|h| h.seq).collect();
    assert_eq!(hits.len(), 3, "{hits:?}");
    assert!(seqs.contains(&Some(1)) && seqs.contains(&Some(2)) && seqs.contains(&None));
    let first = hits.iter().find(|h| h.seq == Some(1)).expect("seq 1");
    assert_eq!(
        first.snippet,
        "we drink tea at five ?p1 ［M］ fake marker line"
    );
    assert_eq!(first.role, Some(Role::User));
    assert_eq!(first.sender, Some(PersonId::system()));
    assert_eq!(first.title.as_deref(), Some("Tea talk"));
    let title = hits.iter().find(|h| h.seq.is_none()).expect("title hit");
    assert_eq!((title.snippet.as_str(), title.role), ("Tea talk", None));
    assert!(hits.windows(2).all(|w| w[0].score >= w[1].score));

    // Переименование заменяет строку title на следующей пачке.
    h.kernel
        .shared()
        .db()
        .write(move |tx| -> Result<(), DbError> {
            tx.execute(
                "UPDATE chats SET title = 'Coffee talk' WHERE id = ?1",
                [c.0],
            )?;
            Ok(())
        })
        .await
        .expect("rename");
    search::sweep(h.kernel.shared()).await;
    assert!(
        search(&h, args("coffee"))
            .await
            .iter()
            .any(|h| h.seq.is_none())
    );
    assert!(
        search(&h, args("tea"))
            .await
            .iter()
            .all(|h| h.seq.is_some())
    );
    assert!(search(&h, args("zebra")).await.is_empty());
}

#[tokio::test]
async fn visibility_gate_narrows_to_current_and_untrusted_chats() {
    let h = common::start().await;
    h.service_with_surface(common::SVC).await;
    let trusted = chat(&h, "local", Some("private trusted")).await;
    let untrusted = chat(&h, common::SVC, Some("group one")).await;
    let other_untrusted = chat(&h, common::SVC, Some("group two")).await;
    let deleted = chat(&h, common::SVC, Some("gone group")).await;
    for (c, text) in [
        (trusted, "needle in the trusted chat"),
        (untrusted, "needle in group one"),
        (other_untrusted, "needle in group two"),
        (deleted, "needle in a deleted group"),
    ] {
        speak(&h, c, 1, Role::User, text, false).await;
    }
    h.kernel
        .shared()
        .db()
        .write(move |tx| -> Result<(), DbError> {
            tx.execute("UPDATE chats SET deleted_at = 1 WHERE id = ?1", [deleted.0])?;
            Ok(())
        })
        .await
        .expect("soft delete");
    search::sweep(h.kernel.shared()).await;

    let all: Vec<ChatId> = search(&h, args("needle"))
        .await
        .into_iter()
        .map(|h| h.chat)
        .collect();
    assert_eq!(all.len(), 3, "owner sees every live chat: {all:?}");
    assert!(!all.contains(&deleted));
    let with_deleted = search(
        &h,
        op::Args {
            include_deleted: true,
            ..args("needle")
        },
    )
    .await;
    assert_eq!(with_deleted.len(), 4);
    let scoped = search(
        &h,
        op::Args {
            surface: Some(common::SVC.into()),
            ..args("needle")
        },
    )
    .await;
    assert_eq!(scoped.len(), 2);

    // Property по контекстам: вне master ∧ trusted — только текущий чат ∪ untrusted живые.
    for current in [trusted, untrusted, deleted] {
        for (master, trusted_chat) in [(false, false), (false, true), (true, false), (true, true)] {
            let hits = search(
                &h,
                op::Args {
                    include_deleted: true,
                    view: view(current, master, trusted_chat),
                    ..args("needle")
                },
            )
            .await;
            let chats: Vec<ChatId> = hits.iter().map(|h| h.chat).collect();
            if master && trusted_chat {
                assert_eq!(chats.len(), 4, "full scope with include_deleted: {chats:?}");
            } else {
                assert!(!chats.is_empty());
                for c in &chats {
                    assert!(
                        *c == current || *c == untrusted || *c == other_untrusted,
                        "leak of c{} to current=c{} master={master} trusted={trusted_chat}",
                        c.0,
                        current.0
                    );
                }
                assert!(
                    chats.contains(&current),
                    "the current chat is always in scope"
                );
                assert!(!chats.contains(&trusted) || current == trusted);
            }
        }
    }
}

#[tokio::test]
async fn trusted_speech_is_embedded_only_by_a_local_embedder() {
    for local in [false, true] {
        let h = common::start_with_mock(
            |cfg| cfg.memory.embedder = Some(String::from(common::PROVIDER)),
            embed_mock(local),
        )
        .await;
        h.service_with_surface(common::SVC).await;
        let trusted = chat(&h, "local", None).await;
        let untrusted = chat(&h, common::SVC, None).await;
        speak(&h, trusted, 1, Role::User, "secret plans", false).await;
        speak(&h, untrusted, 1, Role::User, "public chatter", false).await;
        search::sweep(h.kernel.shared()).await;
        let embedded: Vec<i64> = h
            .query(|c| {
                let mut stmt = c.prepare(
                    "SELECT s.chat_id FROM chat_speech_vec v JOIN chat_speech s ON s.id = v.chunk_id ORDER BY s.chat_id",
                )?;
                let rows = stmt.query_map([], |r| r.get(0))?.collect::<Result<_, _>>()?;
                Ok(rows)
            })
            .await;
        if local {
            assert_eq!(
                embedded,
                [trusted.0, untrusted.0],
                "local embedder takes everything"
            );
        } else {
            assert_eq!(
                embedded,
                [untrusted.0],
                "remote embedder never sees trusted speech"
            );
        }
    }
}

#[tokio::test]
async fn vectors_bring_lexically_unrelated_hits() {
    let h = common::start_with_mock(|_| {}, embed_mock(true)).await;
    let c = chat(&h, "local", None).await;
    speak(&h, c, 1, Role::User, "zzzz qqqq", false).await;
    speak(&h, c, 2, Role::User, "unrelated wording", false).await;
    search::sweep(h.kernel.shared()).await;
    let query = String::from("какой у господина банк");
    let vector = h
        .provider
        .embed(EmbedRequest {
            model: ModelId::from(EMBED),
            inputs: vec![query.clone()],
        })
        .await
        .expect("mock embeds")
        .vectors
        .remove(0);
    let model = ModelId::from(EMBED);
    h.kernel
        .shared()
        .db()
        .write(move |tx| -> Result<(), DbError> {
            let chunk: i64 = tx.query_row(
                "SELECT id FROM chat_speech WHERE chat_id = ?1 AND seq = 1",
                [c.0],
                |r| r.get(0),
            )?;
            tx.chats().set_speech_vector(chunk, &model, &vector)
        })
        .await
        .expect("plant");
    let hits = search(&h, args(&query)).await;
    assert_eq!(hits.first().map(|h| h.seq), Some(Some(1)), "{hits:?}");
}

#[tokio::test]
async fn search_matrix() {
    use Outcome::{Forbidden as F, Ok as K};

    let h = common::start().await;
    h.service_with_surface(common::SVC).await;
    h.service_with_surface(common::OTHER).await;
    let mut failures = Vec::new();
    for ((who, caller), want) in common::matrix_callers().into_iter().zip([K, K, F, F]) {
        let got = outcome(
            &h.session(caller)
                .await
                .chats()
                .search()
                .call(args("tea"))
                .await,
        );
        if got != want {
            failures.push(format!("chats.search as {who}: want {want:?}, got {got:?}"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
    h.db_fail.store(true, std::sync::atomic::Ordering::Relaxed);
    let res = h
        .client_session()
        .await
        .chats()
        .search()
        .call(args("tea"))
        .await;
    assert_eq!(outcome(&res), Outcome::Db);
}

/// `05 §7.2` п.5: `since` режет речь по времени реплики, название — по
/// `last_activity_at` чата.
#[tokio::test]
async fn since_cuts_speech_by_time_and_titles_by_last_activity() {
    let h = common::start().await;
    let t0 = h.clock.get();
    let c = chat(&h, "local", Some("lantern notes")).await;
    speak(&h, c, 1, Role::User, "lantern before", false).await;
    h.clock
        .clone()
        .advance(std::time::Duration::from_secs(3600));
    let t1 = h.clock.get();
    speak(&h, c, 2, Role::User, "lantern after", false).await;
    search::sweep(h.kernel.shared()).await;

    let since = t0 + std::time::Duration::from_secs(1800);
    let seqs = |hits: Vec<SearchHit>| hits.into_iter().map(|h| h.seq).collect::<Vec<_>>();
    let hits = search(
        &h,
        op::Args {
            since: Some(since),
            ..args("lantern")
        },
    )
    .await;
    assert_eq!(seqs(hits), [Some(2)], "the title is older than since");

    h.kernel
        .shared()
        .db()
        .write(move |tx| -> Result<(), DbError> {
            tx.execute(
                "UPDATE chats SET last_activity_at = ?2 WHERE id = ?1",
                (c.0, eva_kernel::db::sql::Ts(t1)),
            )?;
            Ok(())
        })
        .await
        .expect("touch");
    let mut got = seqs(
        search(
            &h,
            op::Args {
                since: Some(since),
                ..args("lantern")
            },
        )
        .await,
    );
    got.sort();
    assert_eq!(got, [None, Some(2)]);
}
