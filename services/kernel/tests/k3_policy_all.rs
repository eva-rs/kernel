//! Полная матрица `Policy` (`11 §8`): каждая операция `NAMESPACES` × caller'ы
//! `Kernel`, `Client`, `Service` своего surface A, `Service` чужого (B про
//! объект A), `Service` против builtin `local`/`system`. Операция без строки
//! здесь роняет `matrix_is_total` — новая операция без решения о правах не
//! проходит.

mod common;

use std::sync::atomic::{AtomicUsize, Ordering};

use common::{Harness, OTHER, Outcome, SVC, TestSession, client, outcome, service};
use eva::{bytes::Bytes, time::Clock as _};
use eva_core::{ByteStream, NAMESPACES, prelude::*};
use eva_kernel::db::{Db as _, DbError, Queries as _, hands_grants::NewGrant};
use eva_protocol::{
    errors::Aux,
    requests::{
        Patch, agents, blobs, chats, cron, folders, fs, hands_grants, inference, kv, memory,
        models, modules, notes, persons, sleep,
        sqlite::{self, Statement},
        stats, surfaces, tools, turns,
    },
    types::{
        BlobId, ChatId, EventId, FolderId, HandsGrantId, MemoryId, ModuleId, PendingId, PersonId,
        RunId, SurfaceId, blob,
        chat::ExternalKind,
        content::Block,
        folder,
        hands::GrantKind,
        module::{ServiceAddress, ServiceKey},
        stats::Scope,
        turn::Answer,
    },
};

use Outcome::{Conflict as C, Forbidden as F, Ok as K, Spec as S, Unavailable as U};

/// Колонки: kernel, client, service-own (A), service-foreign (B на объекте A),
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
    ("chats.ingest", [U, U, U, U, U]),
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
    ("chats.search", [U, U, F, F, F]),
    ("chats.history_read", [K, K, K, F, F]),
    ("chats.compact", [U, U, F, F, F]),
    ("chats.notify", [U, F, F, F, F]),
    // M2: до движка туров операции недоступны всем одинаково.
    ("turns.start", [U, U, U, U, U]),
    ("turns.attach", [U, U, U, U, U]),
    ("turns.append_silent", [U, U, U, U, U]),
    ("turns.delivered", [U, U, U, U, U]),
    ("turns.cancel", [U, U, U, U, U]),
    ("turns.respond", [U, U, U, U, U]),
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
    ("tools.catalog", [U, U, F, F, F]),
    ("models.list", [K, K, F, F, F]),
    ("cron.list", [K, K, F, F, F]),
    ("cron.runs", [K, K, F, F, F]),
    ("sleep.runs", [K, K, F, F, F]),
    ("sleep.phase_runs", [K, K, F, F, F]),
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
    // Своё хранилище модуля: у ядра его нет, чужого не адресовать.
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
        self.h.kernel_session().await
    }

    fn new_chat(&self) -> chats::create::Args {
        chats::create::Args {
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

    async fn chat_with(&self, args: chats::create::Args) -> ChatId {
        self.owner()
            .await
            .chats()
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
            .create()
            .call(folders::create::Args {
                surface: self.target.clone(),
                name: format!("folder {}", self.n),
            })
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
            .put()
            .call(blobs::put::Args {
                meta: put_args(chat),
                body: body(b"abc"),
            })
            .await
            .expect("target blob")
            .id
    }

    async fn person(&self) -> PersonId {
        self.owner()
            .await
            .persons()
            .resolve()
            .call(persons::resolve::Args {
                surface: self.target.clone(),
                external_id: format!("person {}", unique()),
                handle: None,
                name: None,
            })
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
            .kernel
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
            .kernel
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

fn put_args(chat: ChatId) -> blobs::put::Meta {
    blobs::put::Meta {
        chat,
        name: String::from("a.txt"),
        media_type: String::from("text/plain"),
        kind: blob::Kind::File,
        size: 3,
        hash: None,
    }
}

/// Сервис по адресу, где никто не слушает: ядро здесь без `listen`, не звонит.
fn service_args(module: &str) -> modules::add::Args {
    modules::add::Args {
        module: ModuleId::from(module),
        address: ServiceAddress::Uds(format!("/run/eva-{module}/module.sock").into()),
        key: Some(ServiceKey::from("policy-key-0123456789abcdef0123456789")),
    }
}

fn history(chat: ChatId) -> chats::history_read::Args {
    chats::history_read::Args {
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
            let args = surfaces::create::Args {
                id: target,
                name: format!("surface {n}"),
                rules: None,
            };
            outcome(&ctx.s().await.surfaces().create().call(args).await)
        }
        "surfaces.get" => outcome(
            &ctx.s()
                .await
                .surfaces()
                .get()
                .call(surfaces::get::Args { surface: target })
                .await,
        ),
        "surfaces.list" => outcome(
            &ctx.s()
                .await
                .surfaces()
                .list()
                .call(surfaces::list::Args::default())
                .await,
        ),
        "surfaces.update" => outcome(
            &ctx.s()
                .await
                .surfaces()
                .update()
                .call(surfaces::update::Args {
                    surface: target,
                    update: surfaces::update::Update {
                        name: Patch::Change(format!("renamed {n}")),
                        ..Default::default()
                    },
                })
                .await,
        ),

        "folders.create" => outcome(
            &ctx.s()
                .await
                .folders()
                .create()
                .call(folders::create::Args {
                    surface: target,
                    name: format!("folder {n}"),
                })
                .await,
        ),
        "folders.update" => {
            let folder = ctx.folder().await;
            outcome(
                &ctx.s()
                    .await
                    .folders()
                    .update()
                    .call(folders::update::Args {
                        folder,
                        update: folders::update::Update {
                            name: Patch::Change(format!("renamed {n}")),
                            ..Default::default()
                        },
                    })
                    .await,
            )
        }
        "folders.delete" => {
            let folder = ctx.folder().await;
            outcome(
                &ctx.s()
                    .await
                    .folders()
                    .delete()
                    .call(folders::delete::Args { folder })
                    .await,
            )
        }
        "folders.list" => outcome(
            &ctx.s()
                .await
                .folders()
                .list()
                .call(folders::list::Args {
                    surface: Some(target),
                    ..Default::default()
                })
                .await,
        ),

        "chats.ingest" => outcome(
            &ctx.s()
                .await
                .chats()
                .ingest()
                .call(chats::ingest::Args {
                    surface: target,
                    external_chat: chats::ingest::ExternalChat {
                        id: format!("ext {n}"),
                        kind: ExternalKind::Direct,
                        title: None,
                    },
                    sender: None,
                    content: vec![Block::text("hi")],
                    external_id: None,
                    reply_to_external_id: None,
                    anchor: None,
                    r#override: None,
                    silent: false,
                })
                .await,
        ),
        "chats.create" => outcome(&ctx.s().await.chats().create().call(ctx.new_chat()).await),
        "chats.get" => {
            let chat = ctx.chat().await;
            outcome(
                &ctx.s()
                    .await
                    .chats()
                    .get()
                    .call(chats::get::Args { chat: chat.into() })
                    .await,
            )
        }
        "chats.get#alias" => {
            let external_id = format!("alias {n}");
            ctx.chat_with(chats::create::Args {
                external_id: Some(external_id.clone()),
                ..ctx.new_chat()
            })
            .await;
            outcome(
                &ctx.s()
                    .await
                    .chats()
                    .get()
                    .call(chats::get::Args {
                        chat: chats::Selector::Alias {
                            surface: target,
                            external_id,
                        },
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
                    .list()
                    .call(chats::list::Args::default())
                    .await,
            )
        }
        "chats.list#surface" => outcome(
            &ctx.s()
                .await
                .chats()
                .list()
                .call(chats::list::Args {
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
                    .list()
                    .call(chats::list::Args {
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
                    .list()
                    .call(chats::list::Args {
                        after: Some(chat),
                        ..Default::default()
                    })
                    .await,
            )
        }
        "chats.update" | "chats.update#trust" => {
            let chat = ctx.chat().await;
            let update = if key == "chats.update" {
                chats::update::Update {
                    title: Patch::Change(Some(format!("title {n}"))),
                    ..Default::default()
                }
            } else {
                chats::update::Update {
                    trusted: Patch::Change(Some(false)),
                    ..Default::default()
                }
            };
            outcome(
                &ctx.s()
                    .await
                    .chats()
                    .update()
                    .call(chats::update::Args { chat, update })
                    .await,
            )
        }
        "chats.move" => {
            let chat = ctx.chat().await;
            outcome(
                &ctx.s()
                    .await
                    .chats()
                    .r#move()
                    .call(chats::r#move::Args {
                        chat,
                        folder: folder::Selector::Name {
                            surface: target,
                            name: String::from("agents"),
                        },
                        confirm: true,
                    })
                    .await,
            )
        }
        "chats.delete" => {
            let chat = ctx.chat().await;
            outcome(
                &ctx.s()
                    .await
                    .chats()
                    .delete()
                    .call(chats::delete::Args { chat, purge: false })
                    .await,
            )
        }
        "chats.restore" => {
            let chat = ctx.chat().await;
            ctx.owner()
                .await
                .chats()
                .delete()
                .call(chats::delete::Args { chat, purge: false })
                .await
                .expect("deleted target");
            outcome(
                &ctx.s()
                    .await
                    .chats()
                    .restore()
                    .call(chats::restore::Args { chat })
                    .await,
            )
        }
        "chats.search" => outcome(
            &ctx.s()
                .await
                .chats()
                .search()
                .call(chats::search::Args {
                    query: String::from("tea"),
                    limit: Default::default(),
                    since: None,
                    surface: None,
                    folder: None,
                    include_deleted: false,
                    view: None,
                })
                .await,
        ),
        "chats.history_read" => {
            let chat = ctx.chat().await;
            outcome(
                &ctx.s()
                    .await
                    .chats()
                    .history_read()
                    .call(history(chat))
                    .await,
            )
        }
        "chats.compact" => {
            let chat = ctx.chat().await;
            outcome(
                &ctx.s()
                    .await
                    .chats()
                    .compact()
                    .call(chats::compact::Args { chat })
                    .await,
            )
        }
        "chats.notify" => outcome(
            &ctx.s()
                .await
                .chats()
                .notify()
                .call(chats::notify::Args {
                    subject: String::from("hi"),
                    body: String::from("body"),
                    source_chat: None,
                })
                .await,
        ),

        "turns.start" => {
            let chat = ctx.chat().await;
            outcome(
                &ctx.s()
                    .await
                    .turns()
                    .start()
                    .call(turns::start::Args {
                        chat,
                        actor: None,
                        content: vec![Block::text("hi")],
                        external_id: None,
                        reply_to_external_id: None,
                        anchor: None,
                        model: None,
                        effort: None,
                        thinking: None,
                        extra_tools: Vec::new(),
                    })
                    .await,
            )
        }
        "turns.attach" => {
            let chat = ctx.chat().await;
            let res = ctx
                .s()
                .await
                .turns()
                .attach()
                .call(turns::attach::Args {
                    chat,
                    since_seq: None,
                })
                .await
                .map(drop);
            outcome(&res)
        }
        "turns.append_silent" => {
            let chat = ctx.chat().await;
            outcome(
                &ctx.s()
                    .await
                    .turns()
                    .append_silent()
                    .call(turns::append_silent::Args {
                        chat,
                        actor: None,
                        content: vec![Block::text("hi")],
                        external_id: None,
                    })
                    .await,
            )
        }
        "turns.delivered" => {
            let chat = ctx.chat().await;
            outcome(
                &ctx.s()
                    .await
                    .turns()
                    .delivered()
                    .call(turns::delivered::Args {
                        chat,
                        message_id: EventId(1),
                        external_id: format!("out {n}"),
                    })
                    .await,
            )
        }
        "turns.cancel" => {
            let chat = ctx.chat().await;
            outcome(
                &ctx.s()
                    .await
                    .turns()
                    .cancel()
                    .call(turns::cancel::Args { chat })
                    .await,
            )
        }
        "turns.respond" => outcome(
            &ctx.s()
                .await
                .turns()
                .respond()
                .call(turns::respond::Args {
                    pending: PendingId::from("pending"),
                    answer: Answer::Text {
                        text: String::from("yes"),
                    },
                    actor: None,
                })
                .await,
        ),

        "persons.resolve" => outcome(
            &ctx.s()
                .await
                .persons()
                .resolve()
                .call(persons::resolve::Args {
                    surface: target,
                    external_id: format!("resolved {n}"),
                    handle: None,
                    name: None,
                })
                .await,
        ),
        "persons.list" => outcome(
            &ctx.s()
                .await
                .persons()
                .list()
                .call(persons::list::Args::default())
                .await,
        ),
        "persons.rename" => {
            let person = ctx.person().await;
            outcome(
                &ctx.s()
                    .await
                    .persons()
                    .rename()
                    .call(persons::rename::Args {
                        person,
                        name: format!("name {n}"),
                    })
                    .await,
            )
        }
        "persons.merge" => {
            let (from, into) = (ctx.person().await, ctx.person().await);
            outcome(
                &ctx.s()
                    .await
                    .persons()
                    .merge()
                    .call(persons::merge::Args {
                        from,
                        into,
                        confirm: false,
                    })
                    .await,
            )
        }
        "persons.whitelist" => {
            let person = ctx.person().await;
            outcome(
                &ctx.s()
                    .await
                    .persons()
                    .whitelist()
                    .call(persons::whitelist::Args { person, on: true })
                    .await,
            )
        }

        "memory.list" => outcome(
            &ctx.s()
                .await
                .memory()
                .list()
                .call(memory::list::Args::default())
                .await,
        ),
        "memory.get" => {
            let id = ctx.memory(false).await;
            outcome(
                &ctx.s()
                    .await
                    .memory()
                    .get()
                    .call(memory::get::Args { id, view: None })
                    .await,
            )
        }
        "memory.history" => {
            let id = ctx.memory(false).await;
            outcome(
                &ctx.s()
                    .await
                    .memory()
                    .history()
                    .call(memory::history::Args { id, view: None })
                    .await,
            )
        }
        "memory.restore" => {
            let id = ctx.memory(true).await;
            outcome(
                &ctx.s()
                    .await
                    .memory()
                    .restore()
                    .call(memory::restore::Args { id })
                    .await,
            )
        }

        "notes.list" => {
            let chat = ctx.chat().await;
            outcome(
                &ctx.s()
                    .await
                    .notes()
                    .list()
                    .call(notes::list::Args { chat })
                    .await,
            )
        }
        "tools.catalog" => outcome(
            &ctx.s()
                .await
                .tools()
                .catalog()
                .call(tools::catalog::Args::default())
                .await,
        ),
        "models.list" => outcome(
            &ctx.s()
                .await
                .models()
                .list()
                .call(models::list::Args::default())
                .await,
        ),
        "cron.list" => outcome(
            &ctx.s()
                .await
                .cron()
                .list()
                .call(cron::list::Args::default())
                .await,
        ),
        "cron.runs" => outcome(
            &ctx.s()
                .await
                .cron()
                .runs()
                .call(cron::runs::Args::default())
                .await,
        ),
        "sleep.runs" => outcome(
            &ctx.s()
                .await
                .sleep()
                .runs()
                .call(sleep::runs::Args::default())
                .await,
        ),
        "sleep.phase_runs" => outcome(
            &ctx.s()
                .await
                .sleep()
                .phase_runs()
                .call(sleep::phase_runs::Args {
                    run: RunId::from(NIGHT),
                })
                .await,
        ),
        "agents.list" => outcome(
            &ctx.s()
                .await
                .agents()
                .list()
                .call(agents::list::Args::default())
                .await,
        ),

        "stats.tools_usage" => outcome(
            &ctx.s()
                .await
                .stats()
                .tools_usage()
                .call(stats::tools_usage::Args {
                    scope: Scope::Global,
                    target: None,
                    from: day,
                    to: day,
                    top: Default::default(),
                    view: None,
                })
                .await,
        ),
        "stats.spend" => outcome(
            &ctx.s()
                .await
                .stats()
                .spend()
                .call(stats::spend::Args {
                    scope: Scope::Global,
                    target: None,
                    from: day,
                    to: day,
                    group_by: Default::default(),
                    view: None,
                })
                .await,
        ),
        "stats.spend_series" => outcome(
            &ctx.s()
                .await
                .stats()
                .spend_series()
                .call(stats::spend_series::Args {
                    scope: Scope::Global,
                    target: None,
                    from: day,
                    to: day,
                    group_by: Default::default(),
                })
                .await,
        ),
        "stats.chat_spend" => {
            let chat = ctx.chat().await;
            outcome(
                &ctx.s()
                    .await
                    .stats()
                    .chat_spend()
                    .call(stats::chat_spend::Args { chat })
                    .await,
            )
        }
        "stats.health" => outcome(
            &ctx.s()
                .await
                .stats()
                .health()
                .call(stats::health::Args { from: day, to: day })
                .await,
        ),

        "modules.list" => outcome(
            &ctx.s()
                .await
                .modules()
                .list()
                .call(modules::list::Args::default())
                .await,
        ),
        "modules.add" => outcome(
            &ctx.s()
                .await
                .modules()
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
                .add()
                .call(service_args(&module))
                .await
                .expect("service to remove");
            outcome(
                &ctx.s()
                    .await
                    .modules()
                    .remove()
                    .call(modules::remove::Args {
                        module: ModuleId::from(module),
                    })
                    .await,
            )
        }
        "modules.token_issue" => outcome(
            &ctx.s()
                .await
                .modules()
                .token_issue()
                .call(modules::token_issue::Args {
                    module: ModuleId::from(format!("issued{n}")),
                    // Личность выдавшего называет только ядро: у него нет своего модуля.
                    issued_by: ctx.caller.is_kernel().then(PersonId::system),
                })
                .await,
        ),
        "modules.token_revoke" => {
            let module = ModuleId::from(format!("revoked{n}"));
            ctx.h
                .client_session()
                .await
                .modules()
                .token_issue()
                .call(modules::token_issue::Args {
                    module: module.clone(),
                    issued_by: None,
                })
                .await
                .expect("token to revoke");
            outcome(
                &ctx.s()
                    .await
                    .modules()
                    .token_revoke()
                    .call(modules::token_revoke::Args { module })
                    .await,
            )
        }
        "hands_grants.list" => outcome(
            &ctx.s()
                .await
                .hands_grants()
                .list()
                .call(hands_grants::list::Args::default())
                .await,
        ),
        "hands_grants.revoke" => {
            let id = ctx.grant().await;
            outcome(
                &ctx.s()
                    .await
                    .hands_grants()
                    .revoke()
                    .call(hands_grants::revoke::Args { id })
                    .await,
            )
        }

        "blobs.put" => {
            let chat = ctx.chat().await;
            outcome(
                &ctx.s()
                    .await
                    .blobs()
                    .put()
                    .call(blobs::put::Args {
                        meta: put_args(chat),
                        body: body(b"abc"),
                    })
                    .await,
            )
        }
        "blobs.get" => {
            let id = ctx.blob().await;
            let res = ctx
                .s()
                .await
                .blobs()
                .get()
                .call(blobs::get::Args { id, offset: 0 })
                .await
                .map(drop);
            outcome(&res)
        }
        "blobs.stat" => {
            let id = ctx.blob().await;
            outcome(
                &ctx.s()
                    .await
                    .blobs()
                    .stat()
                    .call(blobs::stat::Args { id })
                    .await,
            )
        }
        "inference.complete" => outcome(
            &ctx.s()
                .await
                .inference()
                .complete()
                .call(inference::complete::Args {
                    model: None,
                    system: String::from("Answer in one word."),
                    input: vec![Block::text("hello")],
                    schema: None,
                    max_tokens: None,
                    effort: None,
                })
                .await,
        ),

        "fs.write" | "fs.read" | "fs.stat" | "fs.remove" => {
            let mut s = ctx.s().await;
            let path = format!("f{n}.txt");
            let written = s
                .fs()
                .write()
                .call(fs::write::Args {
                    path: path.clone(),
                    data: b"abc".to_vec().into(),
                    create_dirs: false,
                })
                .await;
            match key {
                "fs.write" => outcome(&written),
                "fs.read" => outcome(&s.fs().read().call(fs::read::Args { path }).await),
                "fs.stat" => outcome(&s.fs().stat().call(fs::stat::Args { path }).await),
                _ => outcome(
                    &s.fs()
                        .remove()
                        .call(fs::remove::Args {
                            path,
                            recursive: false,
                        })
                        .await,
                ),
            }
        }
        "fs.list" => outcome(
            &ctx.s()
                .await
                .fs()
                .list()
                .call(fs::list::Args::default())
                .await,
        ),
        "sqlite.exec" => outcome(
            &ctx.s()
                .await
                .sqlite()
                .exec()
                .call(sqlite::exec::Args {
                    statement: statement(
                        "CREATE TABLE IF NOT EXISTS seen (id INTEGER PRIMARY KEY)",
                    ),
                })
                .await,
        ),
        "sqlite.query" => outcome(
            &ctx.s()
                .await
                .sqlite()
                .query()
                .call(sqlite::query::Args {
                    statement: statement("SELECT 1"),
                })
                .await,
        ),
        "sqlite.tx" => outcome(
            &ctx.s()
                .await
                .sqlite()
                .tx()
                .call(sqlite::tx::Args {
                    statements: vec![
                        statement("CREATE TABLE IF NOT EXISTS seen (id INTEGER PRIMARY KEY)"),
                        statement("INSERT INTO seen DEFAULT VALUES"),
                    ],
                })
                .await,
        ),
        "kv.set" | "kv.get" | "kv.del" => {
            let mut s = ctx.s().await;
            let key_name = format!("k{n}");
            let set = s
                .kv()
                .set()
                .call(kv::set::Args {
                    key: key_name.clone(),
                    value: serde_json::json!(n).into(),
                })
                .await;
            match key {
                "kv.set" => outcome(&set),
                "kv.get" => outcome(&s.kv().get().call(kv::get::Args { key: key_name }).await),
                _ => outcome(&s.kv().del().call(kv::del::Args { key: key_name }).await),
            }
        }
        "kv.list" => outcome(
            &ctx.s()
                .await
                .kv()
                .list()
                .call(kv::list::Args::default())
                .await,
        ),
        _ => return None,
    })
}

/// Ночь сна с фазой: `sleep.phase_runs` хозяину есть что вернуть.
const NIGHT: &str = "night-1";

async fn seed(h: &Harness) {
    let t = i64::try_from(h.clock.get().as_nanos().get()).expect("nanos fit");
    h.kernel
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
    let h = common::start().await;
    h.service_with_surface(SVC).await;
    h.service_with_surface(OTHER).await;
    seed(&h).await;

    let a = SurfaceId::from(SVC);
    let columns: [(&str, Caller, Vec<SurfaceId>); 5] = [
        ("kernel", Caller::Kernel, vec![a.clone()]),
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
