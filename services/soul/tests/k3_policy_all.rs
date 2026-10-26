//! Полная матрица `Policy` (`11 §8`): каждая операция `NAMESPACES` × caller'ы
//! `Soul`, `Client`, `Service` своего surface A, `Service` чужого (B про
//! объект A), `Service` против builtin `local`/`system`. Операция без строки
//! здесь роняет `matrix_is_total` — новая операция без решения о правах не
//! проходит.

mod common;

use eva::handling::CallStep as _;

use std::sync::atomic::{AtomicUsize, Ordering};

use common::{Harness, OTHER, Outcome, SVC, TestSession, client, outcome, service};
use eva::{bytes::Bytes, time::Clock as _};
use eva_core::{NAMESPACES, prelude::*};
use eva_protocol::{
    errors::Aux,
    requests::{
        Agents, Blobs, ByteStream, Chats, Cron, Folders, Fs, HandsGrants, Kv, Memory, Models,
        Modules, Notes, Patch, Persons, Selector, Sleep, Statement, Surfaces, Tools,
    },
    types::{
        BlobId, ChatId, EventId, FolderId, HandsGrantId, MemoryId, ModuleId, PendingId, PersonId,
        RunId, SurfaceId, blob,
        chat::ExternalKind,
        content::Block,
        folder,
        hands::GrantKind,
        module::{ServiceAddress, ServiceKey},
        stats::{Scope, SpendWindow},
        turn::Answer,
    },
};
use eva_soul::db::{Db as _, DbError, Queries as _, hands_grants::NewGrant};

use Outcome::{Conflict as C, Forbidden as F, Ok as K, Spec as S, Unavailable as U};

/// Колонки: soul, client, service-own (A), service-foreign (B на объекте A),
/// service против `local` и `system` (оба прогона обязаны совпасть).
const MATRIX: &[(&str, [Outcome; 5])] = &[
    // Занятый чужим модулем id — `Taken` (`Spec`); `local`/`system` сервису — нет.
    ("surfaces.create", [F, S, K, S, F]),
    ("surfaces.get", [K, K, K, F, F]),
    ("surfaces.list", [K, K, K, K, K]),
    ("surfaces.update", [K, K, K, F, F]),
    ("folders.create", [K, K, F, F, F]),
    ("folders.update", [K, K, F, F, F]),
    ("folders.delete", [K, K, F, F, F]),
    ("folders.list", [K, K, F, F, F]),
    ("chats.ingest", [K, K, K, F, F]),
    ("chats.create", [K, K, K, F, F]),
    ("chats.get", [K, K, K, F, F]),
    ("chats.get#alias", [K, K, K, F, F]),
    ("chats.list", [K, K, K, K, K]),
    ("chats.list#surface", [K, K, K, F, F]),
    ("chats.list#folder", [K, K, K, F, F]),
    // Невидимый якорь неотличим от исчезнувшего.
    ("chats.list#after", [K, K, K, C, C]),
    ("chats.update", [K, K, K, F, F]),
    ("chats.update#trust", [K, K, F, F, F]),
    ("chats.move", [K, K, F, F, F]),
    ("chats.delete", [K, K, F, F, F]),
    ("chats.restore", [K, K, F, F, F]),
    ("chats.search", [K, K, F, F, F]),
    ("chats.history_read", [K, K, K, F, F]),
    ("chats.compact", [U, U, F, F, F]),
    ("chats.notify", [K, F, F, F, F]),
    ("turns.start", [K, K, K, F, F]),
    ("turns.attach", [K, K, K, F, F]),
    ("turns.append_silent", [K, K, K, F, F]),
    // Отсутствует исходящее сообщение, но чат доступен.
    ("turns.delivered", [S, S, S, F, F]),
    ("turns.cancel", [K, K, K, F, F]),
    // Подпись клиента позволяет ответить без actor; для soul и сервиса нужен актор.
    ("turns.respond", [F, K, F, F, F]),
    ("persons.resolve", [K, K, K, F, F]),
    ("persons.list", [K, K, F, F, F]),
    ("persons.rename", [K, K, F, F, F]),
    ("persons.merge", [K, K, F, F, F]),
    ("persons.whitelist", [K, K, F, F, F]),
    ("memory.list", [K, K, F, F, F]),
    ("memory.get", [K, K, F, F, F]),
    ("memory.history", [K, K, F, F, F]),
    ("memory.restore", [K, K, F, F, F]),
    ("notes.list", [K, K, F, F, F]),
    ("notes.put", [K, K, F, F, F]),
    ("notes.delete", [K, K, F, F, F]),
    ("notes.history", [K, K, F, F, F]),
    ("notes.snapshot", [K, K, F, F, F]),
    ("notes.restore", [K, K, F, F, F]),
    ("tools.catalog", [K, K, F, F, F]),
    ("models.list", [K, K, F, F, F]),
    ("cron.list", [K, K, F, F, F]),
    ("cron.runs", [K, K, F, F, F]),
    ("sleep.runs", [K, K, F, F, F]),
    ("sleep.phase_runs", [K, K, F, F, F]),
    ("sleep.status", [K, K, F, F, F]),
    // Модель сна отсутствует; права проверяются до её доступности.
    ("sleep.start", [S, S, F, F, F]),
    ("sleep.cancel", [S, S, F, F, F]),
    ("agents.list", [K, K, F, F, F]),
    ("stats.tools_usage", [K, K, F, F, F]),
    ("stats.spend", [K, K, F, F, F]),
    ("stats.spend_series", [K, K, F, F, F]),
    ("stats.chat_spend", [K, K, K, F, F]),
    ("stats.health", [K, K, F, F, F]),
    ("modules.list", [K, K, F, F, F]),
    // Дозвон на произвольный адрес наводит только клиент хозяина.
    ("modules.add", [F, K, F, F, F]),
    ("modules.remove", [F, K, F, F, F]),
    ("modules.token_issue", [K, K, F, F, F]),
    ("modules.token_revoke", [K, K, F, F, F]),
    ("hands_grants.list", [K, K, F, F, F]),
    ("hands_grants.revoke", [K, K, F, F, F]),
    ("blobs.put", [K, K, K, F, F]),
    ("blobs.get", [K, K, K, F, F]),
    ("blobs.stat", [K, K, K, F, F]),
    ("inference.complete", [K, K, K, K, K]),
    // Своё хранилище модуля: у soul его нет, чужого не адресовать.
    ("fs.read", [F, K, K, K, K]),
    ("fs.write", [F, K, K, K, K]),
    ("fs.list", [F, K, K, K, K]),
    ("fs.stat", [F, K, K, K, K]),
    ("fs.remove", [F, K, K, K, K]),
    ("sqlite.exec", [F, K, K, K, K]),
    ("sqlite.query", [F, K, K, K, K]),
    ("sqlite.tx", [F, K, K, K, K]),
    ("kv.get", [F, K, K, K, K]),
    ("kv.set", [F, K, K, K, K]),
    ("kv.del", [F, K, K, K, K]),
    ("kv.list", [F, K, K, K, K]),
];

/// `ns.op` строки без варианта `#…`.
fn op_of(key: &str) -> &str {
    key.split_once('#').map_or(key, |(op, _)| op)
}

#[test]
fn matrix_is_total() {
    let known: Vec<String> = NAMESPACES
        .iter()
        .flat_map(|(ns, ops)| ops.iter().map(move |op| format!("{ns}.{}", op.name)))
        .collect();
    let missing: Vec<&String> = known
        .iter()
        .filter(|op| !MATRIX.iter().any(|(key, _)| op_of(key) == op.as_str()))
        .collect();
    assert!(
        missing.is_empty(),
        "operations without a Policy row: {missing:?}"
    );
    let stale: Vec<&str> = MATRIX
        .iter()
        .map(|(key, _)| *key)
        .filter(|key| !known.iter().any(|op| op == op_of(key)))
        .collect();
    assert!(stale.is_empty(), "rows of unknown operations: {stale:?}");
}

/// Один прогон строки: caller и surface его объекта.
struct Ctx<'a> {
    h: &'a Harness,
    caller: Caller,
    target: SurfaceId,
    n: usize,
}

static SEQ: AtomicUsize = AtomicUsize::new(0);

fn unique() -> usize {
    SEQ.fetch_add(1, Ordering::Relaxed)
}

fn body(bytes: &'static [u8]) -> ByteStream {
    Box::pin(futures::stream::iter([Ok::<_, Aux>(Bytes::from_static(
        bytes,
    ))]))
}

fn statement(sql: &str) -> Statement {
    Statement {
        sql: sql.to_owned(),
        params: Vec::new(),
    }
}

impl Ctx<'_> {
    async fn s(&self) -> TestSession {
        self.h.session(self.caller.clone()).await
    }

    async fn owner(&self) -> TestSession {
        self.h.soul_session().await
    }

    fn new_chat(&self) -> Chats::create::Args {
        Chats::create::Args {
            surface: self.target.clone(),
            folder: None,
            external_id: None,
            external_kind: None,
            kind: None,
            title: None,
            project: None,
            workdir: None,
            workdir_on: None,
        }
    }

    async fn chat_with(&self, args: Chats::create::Args) -> ChatId {
        self.owner()
            .await
            .chats()
            .inner_mut()
            .create()
            .call(args)
            .await
            .expect("target chat")
            .chat
            .id
    }

    async fn chat(&self) -> ChatId {
        self.chat_with(self.new_chat()).await
    }

    async fn folder(&self) -> FolderId {
        self.owner()
            .await
            .folders()
            .create(self.target.clone(), format!("folder {}", self.n))
            .await
            .expect("target folder")
            .folder
            .id
    }

    async fn blob(&self) -> BlobId {
        let chat = self.chat().await;
        self.owner()
            .await
            .blobs()
            .put(put_args(chat), body(b"abc"))
            .await
            .expect("target blob")
            .id
    }

    async fn person(&self) -> PersonId {
        self.owner()
            .await
            .persons()
            .resolve(
                self.target.clone(),
                format!("person {}", unique()),
                None,
                None,
            )
            .await
            .expect("target person")
            .person
            .id
    }

    /// Запись памяти; `forgotten` — закрыта и в архиве, её можно восстановить.
    async fn memory(&self, forgotten: bool) -> MemoryId {
        let t = i64::try_from(self.h.clock.get().as_nanos().get()).expect("nanos fit");
        let id = self
            .h
            .soul
            .shared()
            .db()
            .write(move |tx| {
                let id: i64 = tx.query_row(
                    "INSERT INTO memory (kind, text, visibility, created_at, updated_at, valid_from, valid_to, last_reinforced_at)
                     VALUES ('fact', 'tea', 'public', ?1, ?1, ?1, ?2, ?1) RETURNING id",
                    (t, forgotten.then_some(t)),
                    |r| r.get(0),
                )?;
                if forgotten {
                    tx.execute(
                        "INSERT INTO memory_archive (memory_id, reason, why, actor, archived_at)
                         VALUES (?1, 'forget', 'stale', 'p0', ?2)",
                        (id, t),
                    )?;
                }
                Ok::<_, DbError>(id)
            })
            .await
            .expect("memory row");
        MemoryId(id)
    }

    async fn grant(&self) -> HandsGrantId {
        let (surface, now) = (self.target.clone(), self.h.clock.get());
        self.h
            .soul
            .shared()
            .db()
            .write(move |tx| {
                tx.hands_grants().insert(&NewGrant {
                    endpoint: common::ENDPOINT.to_owned(),
                    surface,
                    chat: None,
                    kind: GrantKind::Read,
                    prefix: String::from("/srv"),
                    issued_by: PersonId::system(),
                    created_at: now,
                })
            })
            .await
            .expect("grant")
    }
}

fn put_args(chat: ChatId) -> Blobs::put::Meta {
    Blobs::put::Meta {
        chat,
        name: String::from("a.txt"),
        media_type: String::from("text/plain"),
        kind: blob::Kind::File,
        size: 3,
        hash: None,
    }
}

/// Сервис по адресу, где никто не слушает: soul здесь без `listen`, не звонит.
fn service_args(module: &str) -> Modules::add::Args {
    Modules::add::Args {
        module: ModuleId::from(module),
        address: ServiceAddress::Uds(format!("/run/eva-{module}/module.sock").into()),
        key: Some(ServiceKey::from("policy-key-0123456789abcdef0123456789")),
    }
}

fn history(chat: ChatId) -> Chats::history_read::Args {
    Chats::history_read::Args {
        before: None,
        chat,
        from_seq: 0,
        count: Default::default(),
        view: None,
    }
}

/// Исход строки `key` для `ctx`; `None` — у строки нет прогона.
#[allow(clippy::too_many_lines)]
async fn run(ctx: &Ctx<'_>, key: &str) -> Option<Outcome> {
    let target = ctx.target.clone();
    let day = ctx
        .h
        .clock
        .get()
        .date()
        .expect("the test clock is inside the calendar");
    let n = ctx.n;
    Some(match key {
        "surfaces.create" => {
            let args = Surfaces::create::Args {
                id: target,
                name: format!("surface {n}"),
                rules: None,
            };
            outcome(
                &ctx.s()
                    .await
                    .surfaces()
                    .inner_mut()
                    .create()
                    .call(args)
                    .await,
            )
        }
        "surfaces.get" => outcome(&ctx.s().await.surfaces().get(target).await),
        "surfaces.list" => outcome(
            &ctx.s()
                .await
                .surfaces()
                .inner_mut()
                .list()
                .call(Surfaces::list::Args::default())
                .await,
        ),
        "surfaces.update" => outcome(
            &ctx.s()
                .await
                .surfaces()
                .update(
                    target,
                    Surfaces::update::Update {
                        name: Patch::Change(format!("renamed {n}")),
                        ..Default::default()
                    },
                )
                .await,
        ),

        "folders.create" => outcome(
            &ctx.s()
                .await
                .folders()
                .create(target, format!("folder {n}"))
                .await,
        ),
        "folders.update" => {
            let folder = ctx.folder().await;
            outcome(
                &ctx.s()
                    .await
                    .folders()
                    .update(
                        folder,
                        Folders::update::Update {
                            name: Patch::Change(format!("renamed {n}")),
                            ..Default::default()
                        },
                    )
                    .await,
            )
        }
        "folders.delete" => {
            let folder = ctx.folder().await;
            outcome(&ctx.s().await.folders().delete(folder).await)
        }
        "folders.list" => outcome(
            &ctx.s()
                .await
                .folders()
                .inner_mut()
                .list()
                .call(Folders::list::Args {
                    surface: Some(target),
                    ..Default::default()
                })
                .await,
        ),

        "chats.ingest" => outcome(
            &ctx.s()
                .await
                .chats()
                .ingest(
                    target,
                    Chats::ingest::ExternalChat {
                        id: format!("ext {n}"),
                        kind: ExternalKind::Direct,
                        title: None,
                    },
                    None,
                    vec![Block::text("hi")],
                    None,
                    None,
                    None,
                    None,
                    false,
                )
                .await,
        ),
        "chats.create" => outcome(
            &ctx.s()
                .await
                .chats()
                .inner_mut()
                .create()
                .call(ctx.new_chat())
                .await,
        ),
        "chats.get" => {
            let chat = ctx.chat().await;
            outcome(&ctx.s().await.chats().get(chat.into()).await)
        }
        "chats.get#alias" => {
            let external_id = format!("alias {n}");
            ctx.chat_with(Chats::create::Args {
                external_id: Some(external_id.clone()),
                ..ctx.new_chat()
            })
            .await;
            outcome(
                &ctx.s()
                    .await
                    .chats()
                    .get(Selector::Alias {
                        surface: target,
                        external_id,
                    })
                    .await,
            )
        }
        "chats.list" => {
            ctx.chat().await;
            outcome(
                &ctx.s()
                    .await
                    .chats()
                    .inner_mut()
                    .list()
                    .call(Chats::list::Args::default())
                    .await,
            )
        }
        "chats.list#surface" => outcome(
            &ctx.s()
                .await
                .chats()
                .inner_mut()
                .list()
                .call(Chats::list::Args {
                    surface: Some(target),
                    ..Default::default()
                })
                .await,
        ),
        "chats.list#folder" => {
            let folder = ctx.folder().await;
            outcome(
                &ctx.s()
                    .await
                    .chats()
                    .inner_mut()
                    .list()
                    .call(Chats::list::Args {
                        folder: Some(folder),
                        ..Default::default()
                    })
                    .await,
            )
        }
        "chats.list#after" => {
            let chat = ctx.chat().await;
            outcome(
                &ctx.s()
                    .await
                    .chats()
                    .inner_mut()
                    .list()
                    .call(Chats::list::Args {
                        after: Some(chat),
                        ..Default::default()
                    })
                    .await,
            )
        }
        "chats.update" | "chats.update#trust" => {
            let chat = ctx.chat().await;
            let update = if key == "chats.update" {
                Chats::update::Update {
                    title: Patch::Change(Some(format!("title {n}"))),
                    ..Default::default()
                }
            } else {
                Chats::update::Update {
                    trusted: Patch::Change(Some(false)),
                    ..Default::default()
                }
            };
            outcome(&ctx.s().await.chats().update(chat, update).await)
        }
        "chats.move" => {
            let chat = ctx.chat().await;
            outcome(
                &ctx.s()
                    .await
                    .chats()
                    .r#move(
                        chat,
                        folder::Selector::Name {
                            surface: target,
                            name: String::from("agents"),
                        },
                        true,
                    )
                    .await,
            )
        }
        "chats.delete" => {
            let chat = ctx.chat().await;
            outcome(&ctx.s().await.chats().delete(chat, false).await)
        }
        "chats.restore" => {
            let chat = ctx.chat().await;
            ctx.owner()
                .await
                .chats()
                .delete(chat, false)
                .await
                .expect("deleted target");
            outcome(&ctx.s().await.chats().restore(chat).await)
        }
        "chats.search" => outcome(
            &ctx.s()
                .await
                .chats()
                .search(
                    String::from("tea"),
                    Default::default(),
                    None,
                    None,
                    None,
                    false,
                    None,
                )
                .await,
        ),
        "chats.history_read" => {
            let chat = ctx.chat().await;
            outcome(
                &ctx.s()
                    .await
                    .chats()
                    .inner_mut()
                    .history_read()
                    .call(history(chat))
                    .await,
            )
        }
        "chats.compact" => {
            let chat = ctx.chat().await;
            outcome(&ctx.s().await.chats().compact(chat).await)
        }
        "chats.notify" => outcome(
            &ctx.s()
                .await
                .chats()
                .notify(String::from("hi"), String::from("body"), None)
                .await,
        ),

        "turns.start" => {
            let chat = ctx.chat().await;
            outcome(
                &ctx.s()
                    .await
                    .turns()
                    .start(
                        chat,
                        None,
                        vec![Block::text("hi")],
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        Vec::new(),
                    )
                    .await,
            )
        }
        "turns.attach" => {
            let chat = ctx.chat().await;
            let res = ctx
                .s()
                .await
                .turns()
                .attach(chat, None)
                .await
                .map(|ok| ok.0)
                .map(drop);
            outcome(&res)
        }
        "turns.append_silent" => {
            let chat = ctx.chat().await;
            outcome(
                &ctx.s()
                    .await
                    .turns()
                    .append_silent(chat, None, vec![Block::text("hi")], None)
                    .await,
            )
        }
        "turns.delivered" => {
            let chat = ctx.chat().await;
            outcome(
                &ctx.s()
                    .await
                    .turns()
                    .delivered(chat, EventId(1), format!("out {n}"))
                    .await,
            )
        }
        "turns.cancel" => {
            let chat = ctx.chat().await;
            outcome(&ctx.s().await.turns().cancel(chat).await)
        }
        "turns.respond" => {
            let chat = ctx.chat().await;
            let id = PendingId::from(format!("pending-{n}"));
            let now = ctx.h.clock.get();
            let row = eva_soul::db::agents::PendingRow {
                id: id.clone(),
                chat,
                turn: eva_protocol::types::TurnId::from(format!("turn-{n}")),
                call_id: format!("call-{n}"),
                kind: eva_protocol::types::turn::PendingKind::Ask,
                payload: serde_json::json!({"questions": [{"text": "Choose", "options": [{"label": "A"}, {"label": "B"}]}]}),
                created_at: now,
                expires_at: now + std::time::Duration::from_secs(60),
                resolved_at: None,
                outcome: None,
            };
            ctx.h
                .soul
                .shared()
                .db()
                .write(move |tx| tx.agents().insert_pending(&row))
                .await
                .expect("pending");
            outcome(
                &ctx.s()
                    .await
                    .turns()
                    .respond(
                        id,
                        Answer::Text {
                            text: String::from("yes"),
                        },
                        None,
                    )
                    .await,
            )
        }

        "persons.resolve" => outcome(
            &ctx.s()
                .await
                .persons()
                .resolve(target, format!("resolved {n}"), None, None)
                .await,
        ),
        "persons.list" => outcome(
            &ctx.s()
                .await
                .persons()
                .inner_mut()
                .list()
                .call(Persons::list::Args::default())
                .await,
        ),
        "persons.rename" => {
            let person = ctx.person().await;
            outcome(
                &ctx.s()
                    .await
                    .persons()
                    .rename(person, format!("name {n}"))
                    .await,
            )
        }
        "persons.merge" => {
            let (from, into) = (ctx.person().await, ctx.person().await);
            outcome(&ctx.s().await.persons().merge(from, into, false).await)
        }
        "persons.whitelist" => {
            let person = ctx.person().await;
            outcome(&ctx.s().await.persons().whitelist(person, true).await)
        }

        "memory.list" => outcome(
            &ctx.s()
                .await
                .memory()
                .inner_mut()
                .list()
                .call(Memory::list::Args::default())
                .await,
        ),
        "memory.get" => {
            let id = ctx.memory(false).await;
            outcome(&ctx.s().await.memory().get(id, None).await)
        }
        "memory.history" => {
            let id = ctx.memory(false).await;
            outcome(&ctx.s().await.memory().history(id, None).await)
        }
        "memory.restore" => {
            let id = ctx.memory(true).await;
            outcome(&ctx.s().await.memory().restore(id).await)
        }

        "notes.list" => {
            let chat = ctx.chat().await;
            outcome(&ctx.s().await.notes().list(chat).await)
        }
        "notes.put" | "notes.delete" | "notes.history" | "notes.snapshot" | "notes.restore" => {
            let chat = ctx.chat().await;
            let put = || Notes::put::Args {
                chat,
                name: "plan".into(),
                description: "current work".into(),
                body: "keep the result".into(),
                pinned: None,
            };
            ctx.owner()
                .await
                .notes()
                .inner_mut()
                .put()
                .call(put())
                .await
                .map(|ok| ok.0)
                .expect("seed note");
            let snapshots = ctx
                .owner()
                .await
                .notes()
                .history(chat, None, Default::default())
                .await
                .expect("note history")
                .snapshots;
            let id = snapshots[0].id;
            let mut session = ctx.s().await;
            match key {
                "notes.put" => outcome(
                    &session
                        .notes()
                        .inner_mut()
                        .put()
                        .call(put())
                        .await
                        .map(|ok| ok.0),
                ),
                "notes.delete" => outcome(
                    &session
                        .notes()
                        .delete(chat, "plan".into())
                        .await
                        .map(|ok| ok.0),
                ),
                "notes.history" => outcome(
                    &session
                        .notes()
                        .history(chat, None, Default::default())
                        .await,
                ),
                "notes.snapshot" => outcome(&session.notes().snapshot(chat, id).await),
                _ => outcome(&session.notes().restore(chat, id).await),
            }
        }
        "tools.catalog" => outcome(
            &ctx.s()
                .await
                .tools()
                .inner_mut()
                .catalog()
                .call(Tools::catalog::Args::default())
                .await,
        ),
        "models.list" => outcome(
            &ctx.s()
                .await
                .models()
                .inner_mut()
                .list()
                .call(Models::list::Args::default())
                .await,
        ),
        "cron.list" => outcome(
            &ctx.s()
                .await
                .cron()
                .inner_mut()
                .list()
                .call(Cron::list::Args::default())
                .await,
        ),
        "cron.runs" => outcome(
            &ctx.s()
                .await
                .cron()
                .inner_mut()
                .runs()
                .call(Cron::runs::Args::default())
                .await,
        ),
        "sleep.runs" => outcome(
            &ctx.s()
                .await
                .sleep()
                .inner_mut()
                .runs()
                .call(Sleep::runs::Args::default())
                .await,
        ),
        "sleep.status" => outcome(&ctx.s().await.sleep().status().await),
        "sleep.start" => outcome(
            &ctx.s()
                .await
                .sleep()
                .inner_mut()
                .start()
                .call(Sleep::start::Args::default())
                .await,
        ),
        "sleep.cancel" => outcome(&ctx.s().await.sleep().cancel(NIGHT.into()).await),
        "sleep.phase_runs" => outcome(&ctx.s().await.sleep().phase_runs(RunId::from(NIGHT)).await),
        "agents.list" => outcome(
            &ctx.s()
                .await
                .agents()
                .inner_mut()
                .list()
                .call(Agents::list::Args::default())
                .await,
        ),

        "stats.tools_usage" => outcome(
            &ctx.s()
                .await
                .stats()
                .tools_usage(Scope::Global, None, day, day, Default::default(), None)
                .await,
        ),
        "stats.spend" => outcome(
            &ctx.s()
                .await
                .stats()
                .spend(
                    Scope::Global,
                    None,
                    SpendWindow::Dates { from: day, to: day },
                    None,
                    Default::default(),
                    None,
                )
                .await,
        ),
        "stats.spend_series" => outcome(
            &ctx.s()
                .await
                .stats()
                .spend_series(
                    Scope::Global,
                    None,
                    SpendWindow::Dates { from: day, to: day },
                    None,
                    Default::default(),
                )
                .await,
        ),
        "stats.chat_spend" => {
            let chat = ctx.chat().await;
            outcome(&ctx.s().await.stats().chat_spend(chat).await)
        }
        "stats.health" => outcome(&ctx.s().await.stats().health(day, day).await),

        "modules.list" => outcome(
            &ctx.s()
                .await
                .modules()
                .inner_mut()
                .list()
                .call(Modules::list::Args::default())
                .await,
        ),
        "modules.add" => outcome(
            &ctx.s()
                .await
                .modules()
                .inner_mut()
                .add()
                .call(service_args(&format!("added{n}")))
                .await,
        ),
        "modules.remove" => {
            let module = format!("removed{n}");
            ctx.h
                .client_session()
                .await
                .modules()
                .inner_mut()
                .add()
                .call(service_args(&module))
                .await
                .expect("service to remove");
            outcome(&ctx.s().await.modules().remove(ModuleId::from(module)).await)
        }
        "modules.token_issue" => outcome(
            &ctx.s()
                .await
                .modules()
                .token_issue(
                    ModuleId::from(format!("issued{n}")),
                    ctx.caller.is_soul().then(PersonId::system),
                )
                .await,
        ),
        "modules.token_revoke" => {
            let module = ModuleId::from(format!("revoked{n}"));
            ctx.h
                .client_session()
                .await
                .modules()
                .token_issue(module.clone(), None)
                .await
                .expect("token to revoke");
            outcome(&ctx.s().await.modules().token_revoke(module).await)
        }
        "hands_grants.list" => outcome(
            &ctx.s()
                .await
                .hands_grants()
                .inner_mut()
                .list()
                .call(HandsGrants::list::Args::default())
                .await,
        ),
        "hands_grants.revoke" => {
            let id = ctx.grant().await;
            outcome(&ctx.s().await.hands_grants().revoke(id).await)
        }

        "blobs.put" => {
            let chat = ctx.chat().await;
            outcome(
                &ctx.s()
                    .await
                    .blobs()
                    .put(put_args(chat), body(b"abc"))
                    .await,
            )
        }
        "blobs.get" => {
            let id = ctx.blob().await;
            let res = ctx
                .s()
                .await
                .blobs()
                .get(id, 0)
                .await
                .map(|ok| ok.0)
                .map(drop);
            outcome(&res)
        }
        "blobs.stat" => {
            let id = ctx.blob().await;
            outcome(&ctx.s().await.blobs().stat(id).await.map(|ok| ok.0))
        }
        "inference.complete" => outcome(
            &ctx.s()
                .await
                .inference()
                .complete(
                    None,
                    String::from("Answer in one word."),
                    vec![Block::text("hello")],
                    None,
                    None,
                    None,
                )
                .await,
        ),

        "fs.write" | "fs.read" | "fs.stat" | "fs.remove" => {
            let mut s = ctx.s().await;
            let path = format!("f{n}.txt");
            let written = s
                .fs()
                .write(path.clone(), b"abc".to_vec().into(), false)
                .await;
            match key {
                "fs.write" => outcome(&written),
                "fs.read" => outcome(&s.fs().read(path).await),
                "fs.stat" => outcome(&s.fs().stat(path).await),
                _ => outcome(&s.fs().remove(path, false).await),
            }
        }
        "fs.list" => outcome(
            &ctx.s()
                .await
                .fs()
                .inner_mut()
                .list()
                .call(Fs::list::Args::default())
                .await,
        ),
        "sqlite.exec" => outcome(
            &ctx.s()
                .await
                .sqlite()
                .exec(statement(
                    "CREATE TABLE IF NOT EXISTS seen (id INTEGER PRIMARY KEY)",
                ))
                .await,
        ),
        "sqlite.query" => outcome(&ctx.s().await.sqlite().query(statement("SELECT 1")).await),
        "sqlite.tx" => outcome(
            &ctx.s()
                .await
                .sqlite()
                .tx(vec![
                    statement("CREATE TABLE IF NOT EXISTS seen (id INTEGER PRIMARY KEY)"),
                    statement("INSERT INTO seen DEFAULT VALUES"),
                ])
                .await,
        ),
        "kv.set" | "kv.get" | "kv.del" => {
            let mut s = ctx.s().await;
            let key_name = format!("k{n}");
            let set = s
                .kv()
                .set(key_name.clone(), serde_json::json!(n).into())
                .await;
            match key {
                "kv.set" => outcome(&set),
                "kv.get" => outcome(&s.kv().get(key_name).await),
                _ => outcome(&s.kv().del(key_name).await),
            }
        }
        "kv.list" => outcome(
            &ctx.s()
                .await
                .kv()
                .inner_mut()
                .list()
                .call(Kv::list::Args::default())
                .await,
        ),
        _ => return None,
    })
}

/// Ночь сна с фазой: `sleep.phase_runs` хозяину есть что вернуть.
const NIGHT: &str = "night-1";

async fn seed(h: &Harness) {
    let t = i64::try_from(h.clock.get().as_nanos().get()).expect("nanos fit");
    h.soul
        .shared()
        .db()
        .write(move |tx| {
            tx.execute(
                "INSERT INTO sleep_runs (run_id, started, finished, ok, dry_run, interrupted, report)
                 VALUES (?1, ?2, ?2, 1, 0, 0, 'night')",
                (NIGHT, t),
            )?;
            tx.execute(
                "INSERT INTO sleep_phase_runs (run_id, phase, started, finished, model, ops_json, applied, discarded, report)
                 VALUES (?1, 'warm', ?2, ?2, NULL, '[]', 0, 0, NULL)",
                (NIGHT, t),
            )?;
            Ok::<_, DbError>(())
        })
        .await
        .expect("seed");
}

#[tokio::test]
async fn every_operation_by_every_caller() {
    let h = common::start_with(|cfg| cfg.sleep.model = Some("missing-sleep-model".into())).await;
    h.service_with_surface(SVC).await;
    h.service_with_surface(OTHER).await;
    seed(&h).await;

    let a = SurfaceId::from(SVC);
    let columns: [(&str, Caller, Vec<SurfaceId>); 5] = [
        ("soul", Caller::Soul, vec![a.clone()]),
        ("client", client(common::CLIENT), vec![a.clone()]),
        ("service-own", service(SVC), vec![a.clone()]),
        ("service-foreign", service(OTHER), vec![a]),
        (
            "service-vs-builtin",
            service(SVC),
            vec![SurfaceId::from("local"), SurfaceId::from("system")],
        ),
    ];

    let mut failures = Vec::new();
    for &(key, expected) in MATRIX {
        for ((who, caller, targets), want) in columns.iter().zip(expected) {
            for target in targets {
                let ctx = Ctx {
                    h: &h,
                    caller: caller.clone(),
                    target: target.clone(),
                    n: unique(),
                };
                match run(&ctx, key).await {
                    None => failures.push(format!("{key}: no runner")),
                    Some(got) if got != want => failures.push(format!(
                        "{key} as {who} on {target}: want {want:?}, got {got:?}"
                    )),
                    Some(_) => {}
                }
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
