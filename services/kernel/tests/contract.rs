//! Контракт ядро ↔ SDK (`11 §8`, `06 §12`): один набор операций M1
//! гоняется на `Kernel<Test>` в процессе и по настоящему UDS через
//! `eva-sdk`; `Ok` и `Generic` совпадают побайтно в msgpack. Плюс изоляция
//! сервисного модуля по UDS (`11 §9`) и жизнь SDK-модулей: два клиента на
//! `local`, вытеснение, реконнект с `hello.resume`, чужой токен.

mod common;

use std::{fmt::Debug, future::pending, time::Duration};

use futures::{StreamExt as _, stream};
use serde::Serialize;

use common::{
    CLIENT, CLIENT2, Harness, OTHER, Outcome, SVC, client, outcome, service, token,
    with_module_tokens,
};
use eva::{
    bytes::Bytes,
    rand::{Rng as _, SeedableRng as _, xoshiro::Xoshiro256StarStar},
    time::{Clock as _, Date},
    url::Url,
};
use eva_core::{ByteStream, prelude::*};
use eva_kernel::db::{Db as _, DbResult, Queries as _, hands_grants::NewGrant};
use eva_protocol::{
    errors::Aux,
    requests::{
        Patch, Response, agents, blobs, chats, cron, folders, fs, hands_grants, inference, kv,
        memory, models, modules, notes, persons, sleep,
        sqlite::{self, Statement},
        stats, surfaces,
    },
    types::{
        ChatId, CronJobId, HandsGrantId, MemoryId, ModelId, ModuleId, PersonId, RunId, SurfaceId,
        blob, chat, content::Block, folder, hands::GrantKind, module::TokenKind, stats::Scope,
    },
    wire::{Capabilities, ErrorCode, RpcError, handshake::ProtoVersion},
};
use eva_sdk::{ConnectError, Connected, Module, Surface};
use serde_json::json;

const WAIT: Duration = Duration::from_secs(10);

/// `call!(r.c.chats.get(args))` — операция в новой сессии клиента (`c`) или сервиса (`s`).
macro_rules! call {
    ($r:ident . $who:ident . $ns:ident . $op:ident ($args:expr)) => {
        $r.$who().await.$ns().$op().call($args).await
    };
}

/// Ответ каждой операции по порядку: шаг, msgpack и `Debug` для диагностики.
type Transcript = Vec<(String, Vec<u8>, String)>;

/// Сессии контракта: клиент хозяина и сервисный модуль своего surface.
struct Run<'a, M: SessionMaker> {
    client: (&'a M, Caller),
    service: (&'a M, Caller),
    log: Transcript,
}

impl<M: SessionMaker> Run<'_, M> {
    /// Сессия на каждую операцию: RPC-сервер тоже делает её на запрос,
    /// поэтому миры ядра расходуются одинаково в обоих прогонах.
    async fn c(&self) -> Session<M::Session> {
        let (maker, caller) = &self.client;
        maker
            .make_session(caller.clone())
            .await
            .expect("client session")
    }

    async fn s(&self) -> Session<M::Session> {
        let (maker, caller) = &self.service;
        maker
            .make_session(caller.clone())
            .await
            .expect("service session")
    }

    fn record<T: Serialize + Debug>(&mut self, step: &str, res: &T) {
        let bytes = rmp_serde::to_vec_named(res).expect("encode response");
        self.log.push((step.to_owned(), bytes, format!("{res:?}")));
    }

    /// Записать ответ и сверить исход с колонкой «Кто» `11 §3`.
    fn check<O, E>(&mut self, step: &str, res: &Response<O, E>, want: Outcome)
    where
        O: Serialize + Debug,
        E: Serialize + Debug,
    {
        self.record(step, res);
        assert_eq!(outcome(res), want, "{step}: {res:?}");
    }

    fn ok<O, E>(&mut self, step: &str, res: Response<O, E>) -> O
    where
        O: Serialize + Debug,
        E: Serialize + Debug,
    {
        self.check(step, &res, Outcome::Ok);
        res.unwrap_or_else(|e| panic!("{step}: {e:?}"))
    }
}

fn body(bytes: &'static [u8]) -> ByteStream {
    Box::pin(stream::iter(vec![Ok::<_, Aux>(Bytes::from_static(bytes))]))
}

fn today(h: &Harness) -> Date {
    h.clock
        .get()
        .date()
        .expect("the test clock is inside the calendar")
}

fn statement(sql: &str, params: Vec<serde_json::Value>) -> Statement {
    Statement {
        sql: sql.to_owned(),
        params,
    }
}

/// Одинаковые строки в обоих ядрах: память, заметка, крон, ночь сна,
/// подагент и грант рук — чтениям M1 есть что вернуть.
async fn seed(h: &Harness) {
    let t = i64::try_from(h.clock.get().as_nanos().get()).expect("nanos fit");
    let sql = format!(
        "INSERT INTO memory (kind, text, visibility, created_at, updated_at, valid_from, last_reinforced_at)
           VALUES ('fact', 'the master drinks green tea', 'public', {t}, {t}, {t}, {t});
         INSERT INTO chats (id, folder_id, kind, created_at, last_activity_at)
           SELECT 100, id, 'persistent', {t}, {t} FROM folders WHERE surface_id = 'system' AND name = 'cron';
         INSERT INTO chats (id, folder_id, kind, created_at, last_activity_at)
           SELECT 101, id, 'persistent', {t}, {t} FROM folders WHERE surface_id = 'local' AND name = 'Unsorted';
         INSERT INTO chats (id, folder_id, kind, created_at, last_activity_at)
           SELECT 102, id, 'ephemeral', {t}, {t} FROM folders WHERE surface_id = 'local' AND name = 'agents';
         INSERT INTO agents (id, parent_chat_id, name, task, tools, master_turn, status, started_at)
           VALUES (102, 101, 'scout', 'look around', '[]', 1, 'running', {t});
         INSERT INTO chat_notes (chat_id, name, description, body, prev_body, pinned, created_at, updated_at, deleted_at)
           VALUES (101, 'plan', 'what to buy', 'green tea', NULL, 0, {t}, {t}, NULL);"
    );
    let jobs = format!(
        "INSERT INTO cron_jobs (id, name, task, interval, next_fire_at, expires_at, model, effort, tools,
                                chat_id, created_at, runs_total, spent_total)
           VALUES ('job-a', 'daily', 'summarize the day', NULL, NULL, NULL, NULL, NULL, NULL, 100, {t}, 1, 0.0);
         INSERT INTO cron_runs (run_id, job_id, started, finished, ok, output, cost)
           VALUES ('r1', 'job-a', {t}, {t}, 1, 'done', 0.0);
         INSERT INTO sleep_runs (run_id, started, finished, ok, dry_run, interrupted, report)
           VALUES ('n1', {t}, {t}, 1, 0, 0, 'night one');
         INSERT INTO sleep_phase_runs (run_id, phase, started, finished, model, ops_json, applied, discarded, report)
           VALUES ('n1', 'warm', {t}, {t}, NULL, '[]', 0, 0, NULL);"
    );
    let now = h.clock.get();
    h.kernel
        .shared()
        .db()
        .write(move |tx| -> DbResult<()> {
            tx.execute_batch(&sql)?;
            tx.execute_batch(&jobs)?;
            tx.hands_grants().insert(&NewGrant {
                endpoint: common::ENDPOINT.to_owned(),
                surface: SurfaceId::from("local"),
                chat: Some(ChatId(101)),
                kind: GrantKind::Read,
                prefix: String::from("/srv"),
                issued_by: PersonId::system(),
                created_at: now,
            })?;
            Ok(())
        })
        .await
        .expect("seed");
}

/// Строка `modules`, которую пишет handshake SDK-модуля без capability.
async fn handshake_row(h: &Harness, module: &str) {
    let (name, now) = (ModuleId::from(module), h.clock.get());
    let caps = rmp_serde::to_vec_named(&Capabilities::default()).expect("caps");
    let proto = ProtoVersion::CURRENT.to_string();
    h.kernel
        .shared()
        .db()
        .write(move |tx| tx.modules().connected(&name, "0.0.0", &proto, &caps, now))
        .await
        .expect("module row");
}

/// Набор контракта: каждая реализованная операция M1, успех и отказ.
async fn contract<M: SessionMaker>(
    client: (&M, Caller),
    service: (&M, Caller),
    day: Date,
) -> Transcript {
    let mut r = Run {
        client,
        service,
        log: Vec::new(),
    };
    let svc = SurfaceId::from(SVC);
    let local = SurfaceId::from("local");

    surfaces_part(&mut r, &svc, &local).await;
    let work = folders_part(&mut r, &local).await;
    let (tea, bridge) = chats_part(&mut r, &svc, &local, work).await;
    persons_part(&mut r, &svc, &local).await;
    reads_part(&mut r, bridge, day).await;
    modules_part(&mut r).await;
    blobs_part(&mut r, bridge, tea).await;
    inference_part(&mut r).await;
    state_part(&mut r).await;
    r.log
}

async fn surfaces_part<M: SessionMaker>(r: &mut Run<'_, M>, svc: &SurfaceId, local: &SurfaceId) {
    let create = |id: &SurfaceId, name: Option<&str>, builtin: bool| surfaces::create::Args {
        id: id.clone(),
        name: name.map(str::to_owned),
        rules: None,
        ask: true,
        builtin,
    };

    let res = call!(r.c.surfaces.create(create(local, None, true)));
    r.ok("surfaces.create local by client", res);
    let res = call!(r.s.surfaces.create(surfaces::create::Args {
        rules: Some(String::from("Answer briefly.")),
        ..create(svc, Some("Service"), false)
    }));
    r.ok("surfaces.create own", res);
    let res = call!(r.s.surfaces.create(create(local, None, true)));
    r.check("surfaces.create local by service", &res, Outcome::Forbidden);
    let res = call!(
        r.c.surfaces
            .create(create(&SurfaceId::from("system"), Some("System"), false))
    );
    r.check("surfaces.create system", &res, Outcome::Forbidden);

    let get = |surface: &SurfaceId| surfaces::get::Args {
        surface: surface.clone(),
    };
    let res = call!(r.s.surfaces.get(get(svc)));
    r.ok("surfaces.get own", res);
    let res = call!(r.s.surfaces.get(get(local)));
    r.check("surfaces.get local by service", &res, Outcome::Forbidden);
    let res = call!(r.c.surfaces.get(get(&SurfaceId::from("nowhere"))));
    r.check("surfaces.get missing", &res, Outcome::Spec);

    let res = call!(r.c.surfaces.list(surfaces::list::Args::default()));
    r.ok("surfaces.list by client", res);
    let res = call!(r.s.surfaces.list(surfaces::list::Args::default()));
    let found = r.ok("surfaces.list by service", res).found;
    assert!(
        found
            .iter()
            .all(|s| s.module.as_ref().map(ModuleId::as_str) == Some(SVC))
    );

    let update = |update| surfaces::update::Args {
        surface: svc.clone(),
        update,
    };
    let res = call!(r.s.surfaces.update(update(surfaces::update::Update {
        name: Patch::Change(String::from("Svc")),
        ..Default::default()
    })));
    r.ok("surfaces.update name", res);
    let res = call!(r.s.surfaces.update(update(surfaces::update::Update {
        trusted: Patch::Change(Some(true)),
        ..Default::default()
    })));
    r.check("surfaces.update trust by service", &res, Outcome::Forbidden);
    let res = call!(r.c.surfaces.update(update(surfaces::update::Update {
        trusted: Patch::Change(Some(false)),
        ..Default::default()
    })));
    r.ok("surfaces.update trust by client", res);
}

async fn folders_part<M: SessionMaker>(
    r: &mut Run<'_, M>,
    local: &SurfaceId,
) -> eva_protocol::types::FolderId {
    let create = || folders::create::Args {
        surface: local.clone(),
        name: String::from("Work"),
    };
    let res = call!(r.c.folders.create(create()));
    let work = r.ok("folders.create", res).folder.id;
    let res = call!(r.c.folders.create(create()));
    r.check("folders.create taken", &res, Outcome::Spec);
    let res = call!(r.c.folders.update(folders::update::Args {
        folder: work,
        update: folders::update::Update {
            name: Patch::Change(String::from("Projects")),
            ..Default::default()
        },
    }));
    r.ok("folders.update", res);
    let list = |surface: Option<SurfaceId>| folders::list::Args {
        surface,
        limit: Default::default(),
        after: None,
    };
    let res = call!(r.c.folders.list(list(Some(local.clone()))));
    r.ok("folders.list", res);
    let res = call!(r.s.folders.list(list(None)));
    r.check("folders.list by service", &res, Outcome::Forbidden);
    work
}

fn new_chat(surface: &SurfaceId) -> chats::create::Args {
    chats::create::Args {
        surface: surface.clone(),
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

async fn chats_part<M: SessionMaker>(
    r: &mut Run<'_, M>,
    svc: &SurfaceId,
    local: &SurfaceId,
    work: eva_protocol::types::FolderId,
) -> (ChatId, ChatId) {
    let res = call!(r.c.chats.create(chats::create::Args {
        folder: Some(work),
        kind: Some(chat::Kind::Persistent),
        title: Some(String::from("Tea")),
        ..new_chat(local)
    }));
    let tea = r.ok("chats.create local", res).chat.id;
    let res = call!(r.s.chats.create(chats::create::Args {
        external_id: Some(String::from("ext-1")),
        title: Some(String::from("Bridge")),
        ..new_chat(svc)
    }));
    let bridge = r.ok("chats.create own", res).chat.id;
    let res = call!(r.s.chats.create(new_chat(local)));
    r.check("chats.create local by service", &res, Outcome::Forbidden);

    let res = call!(r.c.chats.get(chats::get::Args { chat: tea.into() }));
    r.ok("chats.get", res);
    let res = call!(r.s.chats.get(chats::get::Args {
        chat: chats::Selector::Alias {
            surface: svc.clone(),
            external_id: String::from("ext-1"),
        },
    }));
    r.ok("chats.get by alias", res);
    let res = call!(r.s.chats.get(chats::get::Args { chat: tea.into() }));
    r.check("chats.get foreign", &res, Outcome::Forbidden);

    let res = call!(r.c.chats.list(chats::list::Args::default()));
    r.ok("chats.list by client", res);
    let res = call!(r.s.chats.list(chats::list::Args::default()));
    let found = r.ok("chats.list by service", res).found;
    assert!(found.iter().all(|c| c.surface_id == *svc));

    let res = call!(r.c.chats.update(chats::update::Args {
        chat: tea,
        update: chats::update::Update {
            title: Patch::Change(Some(String::from("Tea time"))),
            pinned: Patch::Change(true),
            ..Default::default()
        },
    }));
    r.ok("chats.update", res);
    let res = call!(r.s.chats.update(chats::update::Args {
        chat: bridge,
        update: chats::update::Update {
            trusted: Patch::Change(Some(true)),
            ..Default::default()
        },
    }));
    r.check("chats.update trust by service", &res, Outcome::Forbidden);
    let res = call!(r.c.chats.r#move(chats::r#move::Args {
        chat: tea,
        folder: folder::Selector::Name {
            surface: local.clone(),
            name: String::from("Unsorted"),
        },
        confirm: true,
    }));
    r.ok("chats.move", res);

    let history = |chat| chats::history_read::Args {
        chat,
        from_seq: 0,
        count: Default::default(),
        view: None,
    };
    let res = call!(r.c.chats.history_read(history(tea)));
    r.ok("chats.history_read", res);
    let res = call!(r.s.chats.history_read(history(bridge)));
    r.ok("chats.history_read own", res);

    let res = call!(r.c.chats.delete(chats::delete::Args {
        chat: tea,
        purge: false
    }));
    r.ok("chats.delete", res);
    let res = call!(r.c.chats.list(chats::list::Args {
        deleted: true,
        ..Default::default()
    }));
    r.ok("chats.list deleted", res);
    let res = call!(r.c.chats.restore(chats::restore::Args { chat: tea }));
    r.ok("chats.restore", res);
    let res = call!(r.c.chats.delete(chats::delete::Args {
        chat: ChatId(999),
        purge: true,
    }));
    r.check("chats.delete missing", &res, Outcome::Spec);
    let res = call!(r.c.chats.search(chats::search::Args {
        query: String::from("tea"),
        limit: Default::default(),
        since: None,
        surface: None,
        folder: None,
        include_deleted: false,
        view: None,
    }));
    r.check("chats.search is M3", &res, Outcome::Unavailable);
    let res = call!(r.c.folders.delete(folders::delete::Args { folder: work }));
    r.ok("folders.delete", res);
    (tea, bridge)
}

async fn persons_part<M: SessionMaker>(r: &mut Run<'_, M>, svc: &SurfaceId, local: &SurfaceId) {
    let resolve =
        |surface: &SurfaceId, external_id: &str, name: Option<&str>| persons::resolve::Args {
            surface: surface.clone(),
            external_id: external_id.to_owned(),
            handle: Some(format!("@{external_id}")),
            name: name.map(str::to_owned),
        };
    let res = call!(r.s.persons.resolve(resolve(svc, "u1", Some("Uno"))));
    let uno = r.ok("persons.resolve own", res).person.id;
    let res = call!(r.s.persons.resolve(resolve(local, "u2", None)));
    r.check("persons.resolve local by service", &res, Outcome::Forbidden);
    let res = call!(r.c.persons.resolve(resolve(local, "u3", Some("Tres"))));
    let tres = r.ok("persons.resolve by client", res).person.id;

    let res = call!(r.c.persons.list(persons::list::Args::default()));
    r.ok("persons.list", res);
    let res = call!(r.s.persons.list(persons::list::Args::default()));
    r.check("persons.list by service", &res, Outcome::Forbidden);
    let res = call!(r.c.persons.rename(persons::rename::Args {
        person: uno.clone(),
        name: String::from("Uno Dos"),
    }));
    r.ok("persons.rename", res);
    let res = call!(r.c.persons.rename(persons::rename::Args {
        person: PersonId::system(),
        name: String::from("Nobody"),
    }));
    r.check("persons.rename system", &res, Outcome::Spec);
    let res = call!(r.c.persons.whitelist(persons::whitelist::Args {
        person: uno.clone(),
        on: true,
    }));
    r.ok("persons.whitelist", res);
    let res = call!(r.c.persons.merge(persons::merge::Args {
        from: tres,
        into: uno,
        confirm: false,
    }));
    r.ok("persons.merge", res);
}

async fn reads_part<M: SessionMaker>(r: &mut Run<'_, M>, bridge: ChatId, day: Date) {
    let fact = MemoryId(1);
    let res = call!(r.c.memory.list(memory::list::Args::default()));
    r.ok("memory.list", res);
    let res = call!(r.s.memory.list(memory::list::Args::default()));
    r.check("memory.list by service", &res, Outcome::Forbidden);
    let res = call!(r.c.memory.get(memory::get::Args {
        id: fact,
        view: None
    }));
    r.ok("memory.get", res);
    let res = call!(r.c.memory.get(memory::get::Args {
        id: MemoryId(999),
        view: None,
    }));
    r.check("memory.get missing", &res, Outcome::Spec);
    let res = call!(r.c.memory.history(memory::history::Args {
        id: fact,
        view: None
    }));
    r.ok("memory.history", res);
    let res = call!(r.c.memory.restore(memory::restore::Args { id: fact }));
    r.record("memory.restore live record", &res);

    let res = call!(r.c.notes.list(notes::list::Args { chat: ChatId(101) }));
    r.ok("notes.list", res);
    let res = call!(r.s.notes.list(notes::list::Args { chat: bridge }));
    r.check("notes.list by service", &res, Outcome::Forbidden);

    let res = call!(r.c.models.list(models::list::Args {}));
    r.ok("models.list", res);
    let res = call!(r.s.models.list(models::list::Args {}));
    r.check("models.list by service", &res, Outcome::Forbidden);

    let res = call!(r.c.cron.list(cron::list::Args {}));
    r.ok("cron.list", res);
    let res = call!(r.c.cron.runs(cron::runs::Args {
        job: Some(CronJobId::from("job-a")),
        limit: Default::default(),
        after: None,
    }));
    r.ok("cron.runs", res);
    let res = call!(r.c.sleep.runs(sleep::runs::Args::default()));
    r.ok("sleep.runs", res);
    let res = call!(r.c.sleep.phase_runs(sleep::phase_runs::Args {
        run: RunId::from("n1"),
    }));
    r.ok("sleep.phase_runs", res);
    let res = call!(r.c.agents.list(agents::list::Args {
        chat: Some(ChatId(101)),
    }));
    r.ok("agents.list", res);

    let res = call!(r.c.stats.tools_usage(stats::tools_usage::Args {
        scope: Scope::Global,
        target: None,
        from: day,
        to: day,
        top: Default::default(),
        view: None,
    }));
    r.ok("stats.tools_usage", res);
    let spend = || stats::spend::Args {
        scope: Scope::Global,
        target: None,
        from: day,
        to: day,
        group_by: Default::default(),
        view: None,
    };
    let res = call!(r.c.stats.spend(spend()));
    r.ok("stats.spend", res);
    let res = call!(r.s.stats.spend(spend()));
    r.check("stats.spend by service", &res, Outcome::Forbidden);
    let res = call!(r.c.stats.spend_series(stats::spend_series::Args {
        scope: Scope::Global,
        target: None,
        from: day,
        to: day,
        group_by: Default::default(),
    }));
    r.ok("stats.spend_series", res);
    let res = call!(
        r.s.stats
            .chat_spend(stats::chat_spend::Args { chat: bridge })
    );
    r.ok("stats.chat_spend own", res);
    let res = call!(
        r.s.stats
            .chat_spend(stats::chat_spend::Args { chat: ChatId(101) })
    );
    r.check("stats.chat_spend foreign", &res, Outcome::Forbidden);
    // Размер файла базы и счётчики сессий модулей — свойства транспорта, не контракта.
    let res = call!(r.c.stats.health(stats::health::Args { from: day, to: day })).map(|mut ok| {
        ok.gauges.db_bytes = 0;
        ok.counters.retain(|c| !c.metric.starts_with("modules."));
        ok
    });
    r.check("stats.health", &res, Outcome::Ok);

    let res = call!(r.c.hands_grants.list(hands_grants::list::Args {
        endpoint: None,
        surface: None,
        chat: None,
    }));
    r.ok("hands_grants.list", res);
    let res = call!(r.c.hands_grants.revoke(hands_grants::revoke::Args {
        id: HandsGrantId(1),
    }));
    r.ok("hands_grants.revoke", res);
    let res = call!(r.c.hands_grants.revoke(hands_grants::revoke::Args {
        id: HandsGrantId(999),
    }));
    r.check("hands_grants.revoke missing", &res, Outcome::Spec);
    let res = call!(r.s.hands_grants.list(hands_grants::list::Args {
        endpoint: None,
        surface: None,
        chat: None,
    }));
    r.check("hands_grants.list by service", &res, Outcome::Forbidden);
}

async fn modules_part<M: SessionMaker>(r: &mut Run<'_, M>) {
    let res = call!(r.c.modules.list(modules::list::Args {}));
    r.ok("modules.list", res);
    let res = call!(r.s.modules.list(modules::list::Args {}));
    r.check("modules.list by service", &res, Outcome::Forbidden);
    let res = call!(r.c.modules.token_issue(modules::token_issue::Args {
        module: ModuleId::from("bridge"),
        kind: TokenKind::Service,
        issued_by: None,
    }));
    r.ok("modules.token_issue", res);
    let res = call!(r.c.modules.token_issue(modules::token_issue::Args {
        module: ModuleId::from("kernel"),
        kind: TokenKind::Client,
        issued_by: None,
    }));
    r.check("modules.token_issue reserved", &res, Outcome::Spec);
    let res = call!(r.c.modules.token_revoke(modules::token_revoke::Args {
        module: ModuleId::from("bridge"),
    }));
    r.ok("modules.token_revoke", res);
    let res = call!(r.c.modules.token_revoke(modules::token_revoke::Args {
        module: ModuleId::from("nobody"),
    }));
    r.check("modules.token_revoke missing", &res, Outcome::Spec);
}

async fn blobs_part<M: SessionMaker>(r: &mut Run<'_, M>, bridge: ChatId, tea: ChatId) {
    const NOTE: &[u8] = b"a note from the bridge";
    let put = |chat, size| blobs::put::Args {
        chat,
        name: String::from("note.txt"),
        media_type: String::from("text/plain"),
        kind: blob::Kind::File,
        size,
        hash: None,
    };
    let res = r
        .s()
        .await
        .blobs()
        .put()
        .call(put(bridge, NOTE.len() as u64), body(NOTE))
        .await;
    let own = r.ok("blobs.put own", res).id;
    let res = r
        .c()
        .await
        .blobs()
        .put()
        .call(put(tea, NOTE.len() as u64), body(NOTE))
        .await;
    let foreign = r.ok("blobs.put by client", res).id;
    let res = r
        .c()
        .await
        .blobs()
        .put()
        .call(put(tea, 3), body(NOTE))
        .await;
    r.check("blobs.put size mismatch", &res, Outcome::Spec);

    let res = call!(r.s.blobs.stat(blobs::stat::Args { id: own.clone() }));
    r.ok("blobs.stat own", res);
    let res = call!(r.s.blobs.stat(blobs::stat::Args { id: foreign }));
    r.check("blobs.stat foreign", &res, Outcome::Forbidden);

    let res = r
        .s()
        .await
        .blobs()
        .get()
        .open(blobs::get::Args { id: own, offset: 2 })
        .await;
    let items = match res {
        Ok(items) => Ok(items.collect::<Vec<_>>().await),
        Err(e) => Err(e),
    };
    r.check("blobs.get own", &items, Outcome::Ok);
}

async fn inference_part<M: SessionMaker>(r: &mut Run<'_, M>) {
    let complete = |model: Option<&str>| inference::complete::Args {
        model: model.map(ModelId::from),
        system: String::from("Answer in one word."),
        input: vec![Block::text("hello there")],
        schema: None,
        max_tokens: None,
        effort: None,
    };
    let res = call!(r.s.inference.complete(complete(None)));
    r.ok("inference.complete", res);
    let res = call!(r.s.inference.complete(complete(Some("no-such-model"))));
    r.check("inference.complete unknown model", &res, Outcome::NotFound);
}

async fn state_part<M: SessionMaker>(r: &mut Run<'_, M>) {
    let res = call!(r.s.fs.write(fs::write::Args {
        path: String::from("notes/a.txt"),
        data: b"hello".to_vec().into(),
        create_dirs: true,
    }));
    r.ok("fs.write", res);
    let res = call!(r.s.fs.read(fs::read::Args {
        path: String::from("notes/a.txt"),
    }));
    r.ok("fs.read", res);
    let res = call!(r.s.fs.list(fs::list::Args {
        path: String::from("notes"),
    }));
    let res = res.map(|mut ok| {
        ok.entries.iter_mut().for_each(untimed);
        ok
    });
    r.ok("fs.list", res);
    let res = call!(r.s.fs.stat(fs::stat::Args {
        path: String::from("notes/a.txt"),
    }));
    let res = res.map(|mut ok| {
        untimed(&mut ok.entry);
        ok
    });
    r.ok("fs.stat", res);
    let res = call!(r.s.fs.read(fs::read::Args {
        path: String::from("../escape"),
    }));
    r.check("fs.read outside", &res, Outcome::Forbidden);
    let res = call!(r.s.fs.remove(fs::remove::Args {
        path: String::from("notes"),
        recursive: true,
    }));
    r.ok("fs.remove", res);

    let res = call!(r.s.sqlite.exec(sqlite::exec::Args {
        statement: statement(
            "CREATE TABLE seen (id INTEGER PRIMARY KEY, what TEXT)",
            vec![]
        ),
    }));
    r.ok("sqlite.exec", res);
    let res = call!(r.s.sqlite.tx(sqlite::tx::Args {
        statements: vec![
            statement("INSERT INTO seen (what) VALUES (?1)", vec![json!("tea")]),
            statement("INSERT INTO seen (what) VALUES (?1)", vec![json!("cake")]),
        ],
    }));
    r.ok("sqlite.tx", res);
    let res = call!(r.s.sqlite.query(sqlite::query::Args {
        statement: statement("SELECT id, what FROM seen ORDER BY id", vec![]),
    }));
    r.ok("sqlite.query", res);
    let res = call!(r.s.sqlite.exec(sqlite::exec::Args {
        statement: statement("ATTACH DATABASE 'x.db' AS x", vec![]),
    }));
    r.check("sqlite.exec attach", &res, Outcome::Forbidden);

    let res = call!(r.s.kv.set(kv::set::Args {
        key: String::from("offset"),
        value: json!({"n": 42}),
    }));
    r.ok("kv.set", res);
    let res = call!(r.s.kv.get(kv::get::Args {
        key: String::from("offset"),
    }));
    r.ok("kv.get", res);
    let res = call!(r.s.kv.list(kv::list::Args::default()));
    r.ok("kv.list", res);
    let res = call!(r.s.kv.del(kv::del::Args {
        key: String::from("offset"),
    }));
    r.ok("kv.del", res);
    let res = call!(r.s.kv.get(kv::get::Args {
        key: String::from("offset"),
    }));
    r.record("kv.get after del", &res);
    let res = call!(r.c.kv.list(kv::list::Args::default()));
    r.ok("kv.list of another module", res);
}

async fn connect(url: &Url, module: &str) -> Connected {
    Module::new(module)
        .connect(url, &token(module))
        .await
        .expect("module connects")
}

fn compare(in_process: &Transcript, over_uds: &Transcript) {
    let steps = |t: &Transcript| t.iter().map(|(s, ..)| s.clone()).collect::<Vec<_>>();
    assert_eq!(
        steps(in_process),
        steps(over_uds),
        "both runs take the same steps"
    );
    for ((step, a, a_dbg), (_, b, b_dbg)) in in_process.iter().zip(over_uds) {
        assert!(a == b, "{step}\n in-process: {a_dbg}\n over uds:   {b_dbg}");
    }
}

#[tokio::test]
async fn in_process_and_uds_answer_alike() {
    let near = common::start_with(with_module_tokens).await;
    seed(&near).await;
    handshake_row(&near, CLIENT).await;
    handshake_row(&near, SVC).await;
    let in_process = contract(
        (&near.kernel, client(CLIENT)),
        (&near.kernel, service(SVC)),
        today(&near),
    )
    .await;

    let far = common::start_with(with_module_tokens).await;
    seed(&far).await;
    let (_listening, url) = far.listen().await;
    let phone = connect(&url, CLIENT).await;
    let svc = connect(&url, SVC).await;
    let (phone_rpc, svc_rpc) = (phone.client(), svc.client());
    let over_uds = contract(
        (&phone_rpc, phone.caller()),
        (&svc_rpc, svc.caller()),
        today(&far),
    )
    .await;

    assert!(in_process.len() > 90, "the contract covers the M1 surface");
    compare(&in_process, &over_uds);
}

/// Объект пула свойства: чат, его surface и blob в нём.
struct Object {
    chat: ChatId,
    surface: SurfaceId,
    blob: eva_protocol::types::BlobId,
}

/// Хозяин (в процессе) заводит по чату с blob'ом на каждом surface.
async fn populate(h: &Harness, surfaces: &[SurfaceId], per_surface: usize) -> Vec<Object> {
    let mut owner = h.client_session().await;
    let mut pool = Vec::new();
    for surface in surfaces {
        for n in 0..per_surface {
            let chat = owner
                .chats()
                .create()
                .call(chats::create::Args {
                    title: Some(format!("{surface} #{n}")),
                    ..new_chat(surface)
                })
                .await
                .unwrap_or_else(|e| panic!("owner creates a chat on {surface}: {e:?}"))
                .chat
                .id;
            let blob = owner
                .blobs()
                .put()
                .call(
                    blobs::put::Args {
                        chat,
                        name: String::from("a.txt"),
                        media_type: String::from("text/plain"),
                        kind: blob::Kind::File,
                        size: 3,
                        hash: None,
                    },
                    body(b"abc"),
                )
                .await
                .expect("owner uploads a blob")
                .id;
            pool.push(Object {
                chat,
                surface: surface.clone(),
                blob,
            });
        }
    }
    pool
}

fn expect_own<O: Debug, E: Debug>(step: &str, own: bool, res: &Response<O, E>) {
    let want = if own { Outcome::Ok } else { Outcome::Forbidden };
    assert_eq!(outcome(res), want, "{step} (own: {own}): {res:?}");
}

/// `11 §9` строка 3: у сервисной сессии по UDS каждый `Ok` ссылается только
/// на свой surface, чужой объект — `forbidden`, несуществующий — `not_found`.
#[tokio::test]
async fn a_service_over_uds_reaches_only_its_own_surface() {
    let h = common::start_with(with_module_tokens).await;
    let (_listening, url) = h.listen().await;
    let svc = Module::new(SVC)
        .surface(Surface::new(SVC).name("Service"))
        .connect(&url, &token(SVC))
        .await
        .expect("service connects");
    let _other = Module::new(OTHER)
        .surface(Surface::new(OTHER).name("Other"))
        .connect(&url, &token(OTHER))
        .await
        .expect("other service connects");

    let mine = SurfaceId::from(SVC);
    let surfaces = [
        SurfaceId::from("local"),
        SurfaceId::from("system"),
        mine.clone(),
        SurfaceId::from(OTHER),
    ];
    let mut pool = populate(&h, &surfaces, 3).await;
    let mut rng = Xoshiro256StarStar::seed_from_u64(11);

    for round in 0..300 {
        let mut k = svc.kernel();
        let object = &pool[rng.random_range(0..pool.len())];
        let (chat, surface, own) = (object.chat, object.surface.clone(), object.surface == mine);
        let blob_id = object.blob.clone();
        let step = |op: &str| format!("round {round}: {op} on {surface}");

        match rng.random_range(0..12u8) {
            0 => {
                let res = k
                    .chats()
                    .get()
                    .call(chats::get::Args { chat: chat.into() })
                    .await;
                expect_own(&step("chats.get"), own, &res);
                if let Ok(ok) = &res {
                    assert_eq!(ok.chat.surface_id, mine);
                }
            }
            1 => {
                let res = k
                    .chats()
                    .history_read()
                    .call(chats::history_read::Args {
                        chat,
                        from_seq: 0,
                        count: Default::default(),
                        view: None,
                    })
                    .await;
                expect_own(&step("chats.history_read"), own, &res);
            }
            2 => {
                let res = k
                    .chats()
                    .update()
                    .call(chats::update::Args {
                        chat,
                        update: chats::update::Update {
                            title: Patch::Change(Some(format!("renamed {round}"))),
                            ..Default::default()
                        },
                    })
                    .await;
                expect_own(&step("chats.update"), own, &res);
            }
            3 => {
                let res = k
                    .blobs()
                    .stat()
                    .call(blobs::stat::Args { id: blob_id })
                    .await;
                expect_own(&step("blobs.stat"), own, &res);
            }
            4 => {
                let res = k
                    .blobs()
                    .get()
                    .open(blobs::get::Args {
                        id: blob_id,
                        offset: 0,
                    })
                    .await
                    .map(|_| ());
                expect_own(&step("blobs.get"), own, &res);
            }
            5 => {
                let res = k
                    .stats()
                    .chat_spend()
                    .call(stats::chat_spend::Args { chat })
                    .await;
                expect_own(&step("stats.chat_spend"), own, &res);
            }
            6 => {
                let res = k
                    .surfaces()
                    .get()
                    .call(surfaces::get::Args {
                        surface: surface.clone(),
                    })
                    .await;
                expect_own(&step("surfaces.get"), own, &res);
            }
            7 => {
                let res = k
                    .surfaces()
                    .update()
                    .call(surfaces::update::Args {
                        surface: surface.clone(),
                        update: surfaces::update::Update {
                            name: Patch::Change(format!("name {round}")),
                            ..Default::default()
                        },
                    })
                    .await;
                expect_own(&step("surfaces.update"), own, &res);
            }
            8 => {
                let res = k
                    .persons()
                    .resolve()
                    .call(persons::resolve::Args {
                        surface: surface.clone(),
                        external_id: format!("user-{}", rng.random_range(0..20u8)),
                        handle: None,
                        name: None,
                    })
                    .await;
                expect_own(&step("persons.resolve"), own, &res);
                if let Ok(ok) = &res {
                    assert!(
                        ok.person.identities.iter().all(|i| i.surface == mine),
                        "{res:?}"
                    );
                }
            }
            9 => {
                let res = k.chats().create().call(new_chat(&surface)).await;
                expect_own(&step("chats.create"), own, &res);
                if let Ok(ok) = res {
                    assert_eq!(ok.chat.surface_id, mine);
                    let blob = k
                        .blobs()
                        .put()
                        .call(
                            blobs::put::Args {
                                chat: ok.chat.id,
                                name: String::from("b.txt"),
                                media_type: String::from("text/plain"),
                                kind: blob::Kind::File,
                                size: 3,
                                hash: None,
                            },
                            body(b"xyz"),
                        )
                        .await
                        .expect("the service uploads into its own chat")
                        .id;
                    pool.push(Object {
                        chat: ok.chat.id,
                        surface,
                        blob,
                    });
                }
            }
            10 => {
                let res = k
                    .chats()
                    .list()
                    .call(chats::list::Args {
                        surface: Some(surface.clone()),
                        deleted: rng.random(),
                        ..Default::default()
                    })
                    .await;
                let found = res
                    .unwrap_or_else(|e| panic!("{}: {e:?}", step("chats.list")))
                    .found;
                assert!(
                    found.iter().all(|c| c.surface_id == mine),
                    "{}",
                    step("chats.list")
                );
            }
            _ => {
                let missing = ChatId(100_000 + i64::from(rng.random_range(0..1000u16)));
                let res = k
                    .chats()
                    .get()
                    .call(chats::get::Args {
                        chat: missing.into(),
                    })
                    .await;
                assert_eq!(
                    outcome(&res),
                    Outcome::Spec,
                    "{}: {res:?}",
                    step("chats.get missing")
                );
                let res = k
                    .surfaces()
                    .list()
                    .call(surfaces::list::Args::default())
                    .await;
                let found = res.expect("surfaces.list").found;
                assert!(
                    found.iter().all(|s| s.id == mine),
                    "{}",
                    step("surfaces.list")
                );
            }
        }
    }
}

async fn eventually(what: &str, mut check: impl AsyncFnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + WAIT;
    while !check().await {
        assert!(tokio::time::Instant::now() < deadline, "timed out: {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Два клиента хозяина с разными именами обслуживают `local` одновременно
/// и видят одни чаты (`06 §2`).
#[tokio::test]
async fn two_sdk_clients_serve_local_together() {
    let h = common::start_with(with_module_tokens).await;
    let (_listening, url) = h.listen().await;
    let phone = Module::new(CLIENT)
        .surface(Surface::local().ask(true))
        .connect(&url, &token(CLIENT))
        .await
        .expect("phone connects");
    let tui = Module::new(CLIENT2)
        .surface(Surface::local())
        .connect(&url, &token(CLIENT2))
        .await
        .expect("tui connects");

    let local = SurfaceId::from("local");
    let serving = h.kernel.shared().serving();
    assert!(serving.serves(&local, &ModuleId::from(CLIENT)));
    assert!(serving.serves(&local, &ModuleId::from(CLIENT2)));

    let chat = phone
        .kernel()
        .chats()
        .create()
        .call(new_chat(&local))
        .await
        .expect("phone creates a local chat")
        .chat
        .id;
    let seen = tui
        .kernel()
        .chats()
        .get()
        .call(chats::get::Args { chat: chat.into() })
        .await
        .expect("tui sees the phone's chat")
        .chat;
    assert_eq!(seen.surface_id, local);
    assert!(!phone.client().peer().is_closed() && !tui.client().peer().is_closed());
}

/// Второй `hello` того же имени вытесняет первый, и SDK первого не лезет
/// обратно (иначе два процесса выбивали бы друг друга по кругу).
#[tokio::test]
async fn a_superseded_sdk_module_stops_instead_of_fighting_back() {
    let h = common::start_with(with_module_tokens).await;
    let (_listening, url) = h.listen().await;
    let module = || {
        Module::new(SVC)
            .surface(Surface::new(SVC).name("Service"))
            .reconnect_backoff(Duration::from_millis(10), Duration::from_millis(50))
    };
    let first = module()
        .connect(&url, &token(SVC))
        .await
        .expect("first connects");
    let running = tokio::spawn(first.run(pending()));

    let second = module()
        .connect(&url, &token(SVC))
        .await
        .expect("second connects");
    let ended = tokio::time::timeout(WAIT, running)
        .await
        .expect("the first module stops")
        .expect("run does not panic");
    let error = ended.expect_err("a superseded module ends with an error");
    assert!(error.to_string().contains("superseded"), "{error}");

    let res = second
        .kernel()
        .surfaces()
        .get()
        .call(surfaces::get::Args {
            surface: SurfaceId::from(SVC),
        })
        .await;
    assert!(res.is_ok(), "{res:?}");
    assert!(!second.client().peer().is_closed());
}

/// Обрыв со стороны ядра: SDK переподключается с `hello.resume`, заново
/// регистрирует surface, прежняя ручка `Connected` ведёт уже в новую сессию.
#[tokio::test]
async fn an_sdk_module_reconnects_with_resume() {
    let h = common::start_with(with_module_tokens).await;
    let (_listening, url) = h.listen().await;
    let svc = Module::new(SVC)
        .surface(Surface::new(SVC).name("Service").ask(true))
        .reconnect_backoff(Duration::from_millis(10), Duration::from_millis(50))
        .connect(&url, &token(SVC))
        .await
        .expect("service connects");
    let first = svc.welcome().session_id;
    let handle = svc.clone();
    let running = tokio::spawn(svc.run(pending()));

    h.kernel.shared().live().close_all(&RpcError::new(
        ErrorCode::Unavailable,
        "the test drops sessions",
    ));
    let (mine, module) = (SurfaceId::from(SVC), ModuleId::from(SVC));
    eventually("a new session serves the surface again", async || {
        handle.welcome().session_id != first && h.kernel.shared().serving().ask_ui(&mine)
    })
    .await;
    assert!(h.kernel.shared().serving().serves(&mine, &module));

    let reconnects = h
        .query(|c| {
            Ok(c.query_row(
                "SELECT coalesce(sum(count), 0) FROM health_stats WHERE metric = 'modules.reconnects' AND key = ?1",
                [SVC],
                |r| r.get::<_, i64>(0),
            )?)
        })
        .await;
    assert_eq!(reconnects, 1, "the second hello carried resume");

    let res = handle
        .kernel()
        .surfaces()
        .get()
        .call(surfaces::get::Args { surface: mine })
        .await;
    assert!(res.is_ok(), "{res:?}");
    running.abort();
}

#[tokio::test]
async fn an_sdk_module_with_a_foreign_token_is_unauthorized() {
    let h = common::start_with(with_module_tokens).await;
    let (_listening, url) = h.listen().await;

    for (module, token) in [(SVC, String::from("wrong")), (SVC, token(OTHER))] {
        let res = Module::new(module).connect(&url, &token).await;
        assert!(
            matches!(&res, Err(ConnectError::Rejected(e)) if e.code == ErrorCode::Unauthorized),
            "{module} with {token}: {:?}",
            res.map(|c| c.welcome())
        );
    }
}

/// Время изменения файла ставит файловая система хоста, не мир ядра.
fn untimed(entry: &mut fs::Entry) {
    entry.modified = eva::time::Timestamp::TEST_ORIGIN;
}
