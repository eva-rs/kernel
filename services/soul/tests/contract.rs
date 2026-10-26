//! Контракт soul ↔ SDK (`11 §8`, `06 §12`): один набор операций M1
//! гоняется на `Soul<Test>` в процессе и через `eva-sdk` — клиент звонит
//! на UDS soul, а soul сам набирает сервис по uds и по tcp под Noise; `Ok` и
//! `Generic` совпадают побайтно в msgpack. Плюс изоляция сервисного модуля
//! (`11 §9`) и жизнь SDK-модулей: клиенты по uds/ws/wss, два клиента на
//! `local`, вытеснение, реконнект клиента, перезвон сервиса с `hello.resume`,
//! рестарт модуля и soul, чужой ключ и чужой токен, `modules.add/remove`.

mod common;

use eva::handling::CallStep as _;

use std::{
    fmt::Debug,
    future::pending,
    net::SocketAddr,
    path::{Path, PathBuf},
    time::Duration,
};

use futures::{StreamExt as _, stream};
use serde::Serialize;

use common::{
    CLIENT, CLIENT2, Harness, OTHER, Outcome, SVC, client, outcome, service, token, with_modules,
};
use eva::{
    bytes::Bytes,
    rand::{Rng as _, SeedableRng as _, xoshiro::Xoshiro256StarStar},
    time::{Clock as _, Date, HumanDuration},
    url::Url,
};
use eva_core::prelude::*;
use eva_protocol::{
    errors::{Aux, Generic},
    requests::{
        Agents, Blobs, ByteStream, Cell, Chats, Cron, Entry, Folders, Fs, HandsGrants, Inference,
        Kv, Memory, Models, Modules, Notes, Patch, Persons, Response, Selector, Sleep, Sqlite,
        Statement, Stats, Surfaces,
    },
    types::{
        ChatId, CronJobId, HandsGrantId, MemoryId, ModelId, ModuleId, PersonId, RunId, SurfaceId,
        Value, blob, chat,
        content::Block,
        folder,
        hands::GrantKind,
        module::{Dial, ModuleKind, ServiceAddress, ServiceKey, State},
        stats::{Scope, SpendWindow},
    },
    wire::{Capabilities, ErrorCode, RpcError, handshake::ProtoVersion},
};
use eva_rpc::config::{Listen, ServiceListen, TcpListen, TlsFiles, WsListen, WssListen};
use eva_sdk::{
    ClientConfig, ClientTls, ConnectError, Connected, Listening, Module, SecretString, Soul,
};
use eva_soul::{
    Config,
    db::{Db as _, DbResult, Queries as _, hands_grants::NewGrant},
};

const WAIT: Duration = Duration::from_secs(10);

/// `call!(r.c.chats.get(args))` — операция в новой сессии клиента (`c`) или сервиса (`s`).
macro_rules! call {
    ($r:ident . $who:ident . $ns:ident . $op:ident ($args:expr)) => {
        $r.$who().await.$ns().inner_mut().$op().call($args).await
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
    /// поэтому миры soul расходуются одинаково в обоих прогонах.
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

fn statement(sql: &str, params: Vec<Value>) -> Statement {
    Statement {
        sql: sql.to_owned(),
        params,
    }
}

/// Одинаковые строки в обоих экземплярах soul: память, заметка, крон, ночь сна,
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
    h.soul
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
    h.soul
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
    let create = |id: &SurfaceId, name: &str| Surfaces::create::Args {
        id: id.clone(),
        name: name.to_owned(),
        rules: None,
    };

    let res = call!(r.c.surfaces.create(create(local, "Local")));
    r.check("surfaces.create local by client", &res, Outcome::Forbidden);
    let res = call!(r.s.surfaces.create(Surfaces::create::Args {
        rules: Some(String::from("Answer briefly.")),
        ..create(svc, "Service")
    }));
    r.ok("surfaces.create own", res);
    let res = call!(r.s.surfaces.create(create(svc, "Renamed")));
    let again = r.ok("surfaces.create own again", res).surface;
    assert_eq!(
        (again.name.as_str(), again.rules.as_deref()),
        ("Service", Some("Answer briefly.")),
        "a repeated create leaves the row as it was"
    );
    let res = call!(r.c.surfaces.create(create(svc, "Mine now")));
    r.check("surfaces.create of another module", &res, Outcome::Spec);
    let res = call!(r.s.surfaces.create(create(local, "Local")));
    r.check("surfaces.create local by service", &res, Outcome::Forbidden);
    let res = call!(
        r.c.surfaces
            .create(create(&SurfaceId::from("system"), "System"))
    );
    r.check("surfaces.create system", &res, Outcome::Forbidden);

    let get = |surface: &SurfaceId| Surfaces::get::Args {
        surface: surface.clone(),
    };
    let res = call!(r.s.surfaces.get(get(svc)));
    r.ok("surfaces.get own", res);
    let res = call!(r.s.surfaces.get(get(local)));
    r.check("surfaces.get local by service", &res, Outcome::Forbidden);
    let res = call!(r.c.surfaces.get(get(&SurfaceId::from("nowhere"))));
    r.check("surfaces.get missing", &res, Outcome::Spec);

    let res = call!(r.c.surfaces.list(Surfaces::list::Args::default()));
    r.ok("surfaces.list by client", res);
    let res = call!(r.s.surfaces.list(Surfaces::list::Args::default()));
    let found = r.ok("surfaces.list by service", res).found;
    assert!(
        found
            .iter()
            .all(|s| s.module.as_ref().map(ModuleId::as_str) == Some(SVC))
    );

    let update = |update| Surfaces::update::Args {
        surface: svc.clone(),
        update,
    };
    let res = call!(r.s.surfaces.update(update(Surfaces::update::Update {
        name: Patch::Change(String::from("Svc")),
        ..Default::default()
    })));
    r.ok("surfaces.update name", res);
    let res = call!(r.s.surfaces.update(update(Surfaces::update::Update {
        trusted: Patch::Change(Some(true)),
        ..Default::default()
    })));
    r.check("surfaces.update trust by service", &res, Outcome::Forbidden);
    let res = call!(r.c.surfaces.update(update(Surfaces::update::Update {
        trusted: Patch::Change(Some(false)),
        ..Default::default()
    })));
    r.ok("surfaces.update trust by client", res);
}

async fn folders_part<M: SessionMaker>(
    r: &mut Run<'_, M>,
    local: &SurfaceId,
) -> eva_protocol::types::FolderId {
    let create = || Folders::create::Args {
        surface: local.clone(),
        name: String::from("Work"),
    };
    let res = call!(r.c.folders.create(create()));
    let work = r.ok("folders.create", res).folder.id;
    let res = call!(r.c.folders.create(create()));
    r.check("folders.create taken", &res, Outcome::Spec);
    let res = call!(r.c.folders.update(Folders::update::Args {
        folder: work,
        update: Folders::update::Update {
            name: Patch::Change(String::from("Projects")),
            ..Default::default()
        },
    }));
    r.ok("folders.update", res);
    let list = |surface: Option<SurfaceId>| Folders::list::Args {
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

fn new_chat(surface: &SurfaceId) -> Chats::create::Args {
    Chats::create::Args {
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
    let res = call!(r.c.chats.create(Chats::create::Args {
        folder: Some(work),
        kind: Some(chat::Kind::Persistent),
        title: Some(String::from("Tea")),
        ..new_chat(local)
    }));
    let tea = r.ok("chats.create local", res).chat.id;
    let res = call!(r.s.chats.create(Chats::create::Args {
        external_id: Some(String::from("ext-1")),
        title: Some(String::from("Bridge")),
        ..new_chat(svc)
    }));
    let bridge = r.ok("chats.create own", res).chat.id;
    let res = call!(r.s.chats.create(new_chat(local)));
    r.check("chats.create local by service", &res, Outcome::Forbidden);

    let res = call!(r.c.chats.get(Chats::get::Args { chat: tea.into() }));
    r.ok("chats.get", res);
    let res = call!(r.s.chats.get(Chats::get::Args {
        chat: Selector::Alias {
            surface: svc.clone(),
            external_id: String::from("ext-1"),
        },
    }));
    r.ok("chats.get by alias", res);
    let res = call!(r.s.chats.get(Chats::get::Args { chat: tea.into() }));
    r.check("chats.get foreign", &res, Outcome::Forbidden);

    let res = call!(r.c.chats.list(Chats::list::Args::default()));
    r.ok("chats.list by client", res);
    let res = call!(r.s.chats.list(Chats::list::Args::default()));
    let found = r.ok("chats.list by service", res).found;
    assert!(found.iter().all(|c| c.surface_id == *svc));

    let res = call!(r.c.chats.update(Chats::update::Args {
        chat: tea,
        update: Chats::update::Update {
            title: Patch::Change(Some(String::from("Tea time"))),
            pinned: Patch::Change(true),
            ..Default::default()
        },
    }));
    r.ok("chats.update", res);
    let res = call!(r.s.chats.update(Chats::update::Args {
        chat: bridge,
        update: Chats::update::Update {
            trusted: Patch::Change(Some(true)),
            ..Default::default()
        },
    }));
    r.check("chats.update trust by service", &res, Outcome::Forbidden);
    let res = call!(r.c.chats.r#move(Chats::r#move::Args {
        chat: tea,
        folder: folder::Selector::Name {
            surface: local.clone(),
            name: String::from("Unsorted"),
        },
        confirm: true,
    }));
    r.ok("chats.move", res);

    let history = |chat| Chats::history_read::Args {
        before: None,
        chat,
        from_seq: 0,
        count: Default::default(),
        view: None,
    };
    let res = call!(r.c.chats.history_read(history(tea)));
    r.ok("chats.history_read", res);
    let res = call!(r.s.chats.history_read(history(bridge)));
    r.ok("chats.history_read own", res);

    let res = call!(r.c.chats.delete(Chats::delete::Args {
        chat: tea,
        purge: false
    }));
    r.ok("chats.delete", res);
    let res = call!(r.c.chats.list(Chats::list::Args {
        deleted: true,
        ..Default::default()
    }));
    r.ok("chats.list deleted", res);
    let res = call!(r.c.chats.restore(Chats::restore::Args { chat: tea }));
    r.ok("chats.restore", res);
    let res = call!(r.c.chats.delete(Chats::delete::Args {
        chat: ChatId(999),
        purge: true,
    }));
    r.check("chats.delete missing", &res, Outcome::Spec);
    let res = call!(r.c.chats.search(Chats::search::Args {
        query: String::from("tea"),
        limit: Default::default(),
        since: None,
        surface: None,
        folder: None,
        include_deleted: false,
        view: None,
    }));
    r.ok("chats.search", res);
    let res = call!(r.c.folders.delete(Folders::delete::Args { folder: work }));
    r.ok("folders.delete", res);
    (tea, bridge)
}

async fn persons_part<M: SessionMaker>(r: &mut Run<'_, M>, svc: &SurfaceId, local: &SurfaceId) {
    let resolve =
        |surface: &SurfaceId, external_id: &str, name: Option<&str>| Persons::resolve::Args {
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

    let res = call!(r.c.persons.list(Persons::list::Args::default()));
    r.ok("persons.list", res);
    let res = call!(r.s.persons.list(Persons::list::Args::default()));
    r.check("persons.list by service", &res, Outcome::Forbidden);
    let res = call!(r.c.persons.rename(Persons::rename::Args {
        person: uno.clone(),
        name: String::from("Uno Dos"),
    }));
    r.ok("persons.rename", res);
    let res = call!(r.c.persons.rename(Persons::rename::Args {
        person: PersonId::system(),
        name: String::from("Nobody"),
    }));
    r.check("persons.rename system", &res, Outcome::Spec);
    let res = call!(r.c.persons.whitelist(Persons::whitelist::Args {
        person: uno.clone(),
        on: true,
    }));
    r.ok("persons.whitelist", res);
    let res = call!(r.c.persons.merge(Persons::merge::Args {
        from: tres,
        into: uno,
        confirm: false,
    }));
    r.ok("persons.merge", res);
}

async fn reads_part<M: SessionMaker>(r: &mut Run<'_, M>, bridge: ChatId, day: Date) {
    let fact = MemoryId(1);
    let res = call!(r.c.memory.list(Memory::list::Args::default()));
    r.ok("memory.list", res);
    let res = call!(r.s.memory.list(Memory::list::Args::default()));
    r.check("memory.list by service", &res, Outcome::Forbidden);
    let res = call!(r.c.memory.get(Memory::get::Args {
        id: fact,
        view: None
    }));
    r.ok("memory.get", res);
    let res = call!(r.c.memory.get(Memory::get::Args {
        id: MemoryId(999),
        view: None,
    }));
    r.check("memory.get missing", &res, Outcome::Spec);
    let res = call!(r.c.memory.history(Memory::history::Args {
        id: fact,
        view: None
    }));
    r.ok("memory.history", res);
    let res = call!(r.c.memory.restore(Memory::restore::Args { id: fact }));
    r.record("memory.restore live record", &res);

    let res = call!(r.c.notes.list(Notes::list::Args { chat: ChatId(101) }));
    r.ok("notes.list", res);
    let res = call!(r.s.notes.list(Notes::list::Args { chat: bridge }));
    r.check("notes.list by service", &res, Outcome::Forbidden);
    let chat = ChatId(101);
    let put = || Notes::put::Args {
        chat,
        name: String::from("plan"),
        description: String::from("what to buy"),
        body: String::from("black tea"),
        pinned: Some(true),
    };
    let res = call!(r.s.notes.put(put()));
    r.check("notes.put by service", &res, Outcome::Forbidden);
    let res = call!(r.c.notes.put(put()));
    r.ok("notes.put", res);
    let res = call!(r.c.notes.history(Notes::history::Args {
        chat,
        before: None,
        limit: Default::default(),
    }));
    let snapshot = r.ok("notes.history", res).snapshots[0].id;
    let res = call!(
        r.c.notes
            .snapshot(Notes::snapshot::Args { chat, id: snapshot })
    );
    let saved = r.ok("notes.snapshot", res);
    assert_eq!(saved.notes[0].body, "green tea");
    let res = call!(r.c.notes.delete(Notes::delete::Args {
        chat,
        name: "plan".into()
    }));
    r.ok("notes.delete", res);
    let res = call!(
        r.c.notes
            .restore(Notes::restore::Args { chat, id: snapshot })
    );
    r.ok("notes.restore", res);
    let res = call!(r.c.notes.list(Notes::list::Args { chat }));
    let restored = r.ok("notes.list after restore", res);
    assert_eq!(restored.notes[0].body, "green tea");

    let res = call!(r.c.models.list(Models::list::Args {}));
    r.ok("models.list", res);
    let res = call!(r.s.models.list(Models::list::Args {}));
    r.check("models.list by service", &res, Outcome::Forbidden);

    let res = call!(r.c.cron.list(Cron::list::Args {}));
    r.ok("cron.list", res);
    let res = call!(r.c.cron.runs(Cron::runs::Args {
        job: Some(CronJobId::from("job-a")),
        limit: Default::default(),
        after: None,
    }));
    r.ok("cron.runs", res);
    let res = call!(r.c.sleep.runs(Sleep::runs::Args::default()));
    r.ok("sleep.runs", res);
    let res = call!(r.c.sleep.phase_runs(Sleep::phase_runs::Args {
        run: RunId::from("n1"),
    }));
    r.ok("sleep.phase_runs", res);
    let res = call!(r.c.sleep.status(Sleep::status::Args {}));
    r.ok("sleep.status", res);
    let res = call!(r.s.sleep.start(Sleep::start::Args::default()));
    r.check("sleep.start by service", &res, Outcome::Forbidden);
    let res = call!(r.c.sleep.cancel(Sleep::cancel::Args { run: "n1".into() }));
    r.check("sleep.cancel finished run", &res, Outcome::Spec);
    let res = call!(r.c.agents.list(Agents::list::Args {
        chat: Some(ChatId(101)),
    }));
    r.ok("agents.list", res);

    let res = call!(r.c.stats.tools_usage(Stats::tools_usage::Args {
        scope: Scope::Global,
        target: None,
        from: day,
        to: day,
        top: Default::default(),
        view: None,
    }));
    r.ok("stats.tools_usage", res);
    let spend = || Stats::spend::Args {
        scope: Scope::Global,
        target: None,
        window: SpendWindow::Dates { from: day, to: day },
        timezone: None,
        group_by: Default::default(),
        view: None,
    };
    let res = call!(r.c.stats.spend(spend()));
    r.ok("stats.spend", res);
    let res = call!(r.s.stats.spend(spend()));
    r.check("stats.spend by service", &res, Outcome::Forbidden);
    let res = call!(r.c.stats.spend_series(Stats::spend_series::Args {
        scope: Scope::Global,
        target: None,
        window: SpendWindow::Dates { from: day, to: day },
        timezone: None,
        group_by: Default::default(),
    }));
    r.ok("stats.spend_series", res);
    let res = call!(
        r.s.stats
            .chat_spend(Stats::chat_spend::Args { chat: bridge })
    );
    r.ok("stats.chat_spend own", res);
    let res = call!(
        r.s.stats
            .chat_spend(Stats::chat_spend::Args { chat: ChatId(101) })
    );
    r.check("stats.chat_spend foreign", &res, Outcome::Forbidden);
    // Размер файла базы и счётчики сессий модулей — свойства транспорта, не контракта.
    let res = call!(r.c.stats.health(Stats::health::Args { from: day, to: day })).map(|mut ok| {
        ok.gauges.db_bytes = 0;
        ok.counters.retain(|c| !c.metric.starts_with("modules."));
        for module in &mut ok.gauges.modules {
            untransported(&mut module.dial);
        }
        ok
    });
    r.check("stats.health", &res, Outcome::Ok);

    let res = call!(r.c.hands_grants.list(HandsGrants::list::Args {
        endpoint: None,
        surface: None,
        chat: None,
    }));
    r.ok("hands_grants.list", res);
    let res = call!(r.c.hands_grants.revoke(HandsGrants::revoke::Args {
        id: HandsGrantId(1),
    }));
    r.ok("hands_grants.revoke", res);
    let res = call!(r.c.hands_grants.revoke(HandsGrants::revoke::Args {
        id: HandsGrantId(999),
    }));
    r.check("hands_grants.revoke missing", &res, Outcome::Spec);
    let res = call!(r.s.hands_grants.list(HandsGrants::list::Args {
        endpoint: None,
        surface: None,
        chat: None,
    }));
    r.check("hands_grants.list by service", &res, Outcome::Forbidden);
}

/// Каталог сокета — у каждого экземпляра soul свой, причина провала
/// дозвона — свойство транспорта: в контракт идёт только имя сокета.
fn untransported(dial: &mut Option<Dial>) {
    if let Some(dial) = dial {
        dial.error = None;
        if let ServiceAddress::Uds(path) = &mut dial.address {
            *path = path.file_name().map(PathBuf::from).unwrap_or_default();
        }
    }
}

async fn modules_part<M: SessionMaker>(r: &mut Run<'_, M>) {
    let res = call!(r.c.modules.list(Modules::list::Args {})).map(|mut ok| {
        for module in &mut ok.modules {
            untransported(&mut module.dial);
        }
        ok
    });
    r.check("modules.list", &res, Outcome::Ok);
    let res = call!(r.s.modules.list(Modules::list::Args {}));
    r.check("modules.list by service", &res, Outcome::Forbidden);
    let relay = || Modules::add::Args {
        module: ModuleId::from("relay"),
        address: ServiceAddress::Uds("/run/eva-relay/module.sock".into()),
        key: Some(ServiceKey::from("relay-key-0123456789abcdef0123456789")),
    };
    let res = call!(r.c.modules.add(relay()));
    r.ok("modules.add", res);
    let res = call!(r.c.modules.add(relay()));
    r.check("modules.add taken", &res, Outcome::Spec);
    let res = call!(r.s.modules.add(Modules::add::Args {
        module: ModuleId::from("probe"),
        ..relay()
    }));
    r.check("modules.add by service", &res, Outcome::Forbidden);
    let res = call!(r.c.modules.remove(Modules::remove::Args {
        module: ModuleId::from("relay"),
    }));
    r.ok("modules.remove", res);
    let res = call!(r.c.modules.remove(Modules::remove::Args {
        module: ModuleId::from("relay"),
    }));
    r.check("modules.remove missing", &res, Outcome::Spec);
    let res = call!(r.c.modules.remove(Modules::remove::Args {
        module: ModuleId::from(SVC),
    }));
    r.check("modules.remove config", &res, Outcome::Conflict);
    let res = call!(r.c.modules.token_issue(Modules::token_issue::Args {
        module: ModuleId::from("bridge"),
        issued_by: None,
    }));
    r.ok("modules.token_issue", res);
    let res = call!(r.c.modules.token_issue(Modules::token_issue::Args {
        module: ModuleId::from("soul"),
        issued_by: None,
    }));
    r.check("modules.token_issue reserved", &res, Outcome::Invalid);
    let res = call!(r.c.modules.token_issue(Modules::token_issue::Args {
        module: ModuleId::from(SVC),
        issued_by: None,
    }));
    r.check("modules.token_issue service", &res, Outcome::Conflict);
    let res = call!(r.c.modules.token_revoke(Modules::token_revoke::Args {
        module: ModuleId::from("bridge"),
    }));
    r.ok("modules.token_revoke", res);
    let res = call!(r.c.modules.token_revoke(Modules::token_revoke::Args {
        module: ModuleId::from("nobody"),
    }));
    r.check("modules.token_revoke missing", &res, Outcome::Spec);
}

async fn blobs_part<M: SessionMaker>(r: &mut Run<'_, M>, bridge: ChatId, tea: ChatId) {
    const NOTE: &[u8] = b"a note from the bridge";
    let put = |chat, size| Blobs::put::Meta {
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
        .put(put(bridge, NOTE.len() as u64), body(NOTE))
        .await;
    let own = r.ok("blobs.put own", res).id;
    let res = r
        .c()
        .await
        .blobs()
        .put(put(tea, NOTE.len() as u64), body(NOTE))
        .await;
    let foreign = r.ok("blobs.put by client", res).id;
    let res = r.c().await.blobs().put(put(tea, 3), body(NOTE)).await;
    r.check("blobs.put size mismatch", &res, Outcome::Spec);

    let res = call!(r.s.blobs.stat(Blobs::stat::Args { id: own.clone() }));
    r.ok("blobs.stat own", res);
    let res = call!(r.s.blobs.stat(Blobs::stat::Args { id: foreign }));
    r.check("blobs.stat foreign", &res, Outcome::Forbidden);

    let res = r.s().await.blobs().get(own, 2).await.map(|ok| ok.0);
    let items = match res {
        Ok(items) => Ok(items.collect::<Vec<_>>().await),
        Err(e) => Err(e),
    };
    r.check("blobs.get own", &items, Outcome::Ok);
}

async fn inference_part<M: SessionMaker>(r: &mut Run<'_, M>) {
    let complete = |model: Option<&str>| Inference::complete::Args {
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
    let res = call!(r.s.fs.write(Fs::write::Args {
        path: String::from("notes/a.txt"),
        data: b"hello".to_vec().into(),
        create_dirs: true,
    }));
    r.ok("fs.write", res);
    let res = call!(r.s.fs.read(Fs::read::Args {
        path: String::from("notes/a.txt"),
    }));
    r.ok("fs.read", res);
    let res = call!(r.s.fs.list(Fs::list::Args {
        path: String::from("notes"),
    }));
    let res = res.map(|mut ok| {
        ok.entries.iter_mut().for_each(untimed);
        ok
    });
    r.ok("fs.list", res);
    let res = call!(r.s.fs.stat(Fs::stat::Args {
        path: String::from("notes/a.txt"),
    }));
    let res = res.map(|mut ok| {
        untimed(&mut ok.entry);
        ok
    });
    r.ok("fs.stat", res);
    let res = call!(r.s.fs.read(Fs::read::Args {
        path: String::from("../escape"),
    }));
    r.check("fs.read outside", &res, Outcome::Forbidden);
    let res = call!(r.s.fs.remove(Fs::remove::Args {
        path: String::from("notes"),
        recursive: true,
    }));
    r.ok("fs.remove", res);

    // Валидный UTF-8 в `bin`: строкой назад он читаться не должен.
    let tea_raw = eva_protocol::types::Bytes::from(*b"tea leaves");
    let res = call!(r.s.sqlite.exec(Sqlite::exec::Args {
        statement: statement(
            "CREATE TABLE seen (id INTEGER PRIMARY KEY, what TEXT, raw BLOB)",
            vec![]
        ),
    }));
    r.ok("sqlite.exec", res);
    let res = call!(r.s.sqlite.tx(Sqlite::tx::Args {
        statements: vec![
            statement(
                "INSERT INTO seen (what, raw) VALUES (?1, ?2)",
                vec!["tea".into(), Value::Bytes(tea_raw.clone())],
            ),
            statement("INSERT INTO seen (what) VALUES (?1)", vec!["cake".into()]),
        ],
    }));
    r.ok("sqlite.tx", res);
    let res = call!(r.s.sqlite.query(Sqlite::query::Args {
        statement: statement("SELECT id, what, raw FROM seen ORDER BY id", vec![]),
    }));
    let seen = r.ok("sqlite.query", res);
    assert_eq!(
        seen.rows[0][2],
        Cell::Blob(tea_raw),
        "a blob param stays a blob"
    );
    let res = call!(r.s.sqlite.exec(Sqlite::exec::Args {
        statement: statement("ATTACH DATABASE 'x.db' AS x", vec![]),
    }));
    r.check("sqlite.exec attach", &res, Outcome::Forbidden);

    let offset = Value::Map(vec![
        ("n".into(), Value::Integer(42)),
        ("raw".into(), Value::Bytes(vec![0, 0xc1, 0xff].into())),
    ]);
    let res = call!(r.s.kv.set(Kv::set::Args {
        key: String::from("offset"),
        value: offset.clone(),
    }));
    r.ok("kv.set", res);
    let res = call!(r.s.kv.get(Kv::get::Args {
        key: String::from("offset"),
    }));
    let got = r.ok("kv.get", res);
    assert_eq!(got.value, Some(offset), "bytes survive kv");
    let res = call!(r.s.kv.list(Kv::list::Args::default()));
    r.ok("kv.list", res);
    let res = call!(r.s.kv.del(Kv::del::Args {
        key: String::from("offset"),
    }));
    r.ok("kv.del", res);
    let res = call!(r.s.kv.get(Kv::get::Args {
        key: String::from("offset"),
    }));
    r.record("kv.get after del", &res);
    let res = call!(r.c.kv.list(Kv::list::Args::default()));
    r.ok("kv.list of another module", res);
}

/// SDK-клиент с токеном записи `module`: имя ему даёт `welcome`.
async fn connect(url: &Url, module: &str) -> Connected {
    let connected = Module::new()
        .connect(url, &secret(module))
        .await
        .expect("module connects");
    assert_eq!(
        connected.module().as_str(),
        module,
        "the token names the module"
    );
    connected
}

fn secret(module: &str) -> SecretString {
    SecretString::Plaintext(token(module))
}

/// SDK-сервис `module` слушает там, куда soul его набирает (`with_service`);
/// поднятый до `Harness::listen`, он получит первую же попытку.
async fn listen(h: &Harness, module: Module, name: &str) -> Listening {
    module
        .listen(&common::sdk_service(h.dir.path(), name))
        .await
        .expect("service listens")
}

/// Soul через первый дозвон: имя сервису даёт запись реестра.
async fn dialed(listening: &Listening, module: &str) -> Soul {
    let soul = listening.soul().await.expect("the soul dials the service");
    assert_eq!(
        listening.module().as_ref().map(ModuleId::as_str),
        Some(module),
        "the registry entry names the module"
    );
    soul
}

/// Surface модуля: создание идемпотентно, звать можно на каждом старте.
async fn create_surface(soul: &Soul, id: &str, name: &str) {
    soul.clone()
        .surfaces()
        .create(SurfaceId::from(id), name.to_owned(), None)
        .await
        .unwrap_or_else(|e| panic!("{id} creates its surface: {e:?}"));
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

/// `with_modules`, но soul набирает `SVC` по `address`, если он задан.
fn modules_at(address: Option<ServiceAddress>) -> impl FnOnce(&mut Config) {
    move |cfg| {
        with_modules(cfg);
        if let Some(address) = address {
            cfg.modules
                .services
                .get_mut(&ModuleId::from(SVC))
                .expect("svc is registered")
                .address = address;
        }
    }
}

/// Эталонный прогон: soul в процессе, без транспорта; `SVC` в реестре — по
/// адресу дальнего прогона (`None` — сокет харнесса).
async fn in_process(svc: Option<ServiceAddress>) -> Transcript {
    let near = common::start_with(modules_at(svc)).await;
    seed(&near).await;
    handshake_row(&near, CLIENT).await;
    handshake_row(&near, SVC).await;
    contract(
        (&near.soul, client(CLIENT)),
        (&near.soul, service(SVC)),
        today(&near),
    )
    .await
}

/// Дальний прогон: клиент звонит на UDS soul с токеном, а soul сам набирает
/// сервис `svc`, как записано в реестре.
async fn over_rpc(far: &Harness, svc: &Listening) -> Transcript {
    seed(far).await;
    let (_listening, url) = far.listen().await;
    let phone = connect(&url, CLIENT).await;
    let svc_caller = dialed(svc, SVC).await.caller().clone();
    let (phone_rpc, svc_rpc) = (phone.client(), svc.client().expect("the soul dialed"));
    contract(
        (&phone_rpc, phone.caller()),
        (&svc_rpc, svc_caller),
        today(far),
    )
    .await
}

#[tokio::test]
async fn in_process_and_uds_answer_alike() {
    let in_process = in_process(None).await;

    let far = common::start_with(with_modules).await;
    let svc = listen(&far, Module::new(), SVC).await;
    let over_uds = over_rpc(&far, &svc).await;

    assert!(in_process.len() > 90, "the contract covers the M1 surface");
    compare(&in_process, &over_uds);
}

/// Сервис на tcp под Noise: soul набирает `host:port` из реестра.
#[tokio::test]
async fn in_process_and_tcp_answer_alike() {
    let svc = Module::new()
        .listen(&common::sdk_service_at(tcp(), SVC))
        .await
        .expect("service listens");
    let address = svc.address().expect("a bound listener has an address");
    assert!(matches!(address, ServiceAddress::Tcp { .. }), "{address}");
    let in_process = in_process(Some(address.clone())).await;

    let far = common::start_with(modules_at(Some(address))).await;
    let over_tcp = over_rpc(&far, &svc).await;

    compare(&in_process, &over_tcp);
}

fn tcp() -> ServiceListen {
    ServiceListen::Tcp(TcpListen {
        addr: "127.0.0.1:0".parse().expect("loopback"),
    })
}

/// `Soul` SDK — то, что зовёт код модуля: мутации, потоки и загрузки идут
/// через `LiveSession` своим путём, а не через `RpcSession`.
struct Sdk(Soul);

impl SessionMaker for Sdk {
    type Session = eva_sdk::live::LiveSession;

    fn make_session(
        &self,
        caller: Caller,
    ) -> impl eva_core::service::AuxFut<Session<eva_sdk::live::LiveSession>> {
        let soul = self.0.clone();
        async move {
            if soul.caller() != &caller {
                return Err(Aux::forbidden("the SDK session belongs to another caller"));
            }
            Ok(soul)
        }
    }
}

/// `11 §9`: код SDK — код soul и для публичного `Soul` модуля.
#[tokio::test]
async fn in_process_and_the_sdk_soul_answer_alike() {
    let in_process = in_process(None).await;

    let far = common::start_with(with_modules).await;
    seed(&far).await;
    let svc = listen(&far, Module::new(), SVC).await;
    let (_listening, url) = far.listen().await;
    let phone = connect(&url, CLIENT).await;
    let svc = dialed(&svc, SVC).await;
    let svc_caller = svc.caller().clone();
    let over_sdk = contract(
        (&Sdk(phone.soul()), phone.caller()),
        (&Sdk(svc), svc_caller),
        today(&far),
    )
    .await;

    compare(&in_process, &over_sdk);
}

/// Объект пула свойства: чат, его surface, alias, папка и blob в нём.
struct Object {
    chat: ChatId,
    surface: SurfaceId,
    blob: eva_protocol::types::BlobId,
    alias: Option<String>,
    folder: eva_protocol::types::FolderId,
}

/// Хозяин (в процессе) заводит на каждом surface папку и чаты с alias и
/// blob'ом, а личности `user-N` — с адресами на всех surface сразу.
async fn populate(h: &Harness, surfaces: &[SurfaceId], per_surface: usize) -> Vec<Object> {
    let mut owner = h.client_session().await;
    let mut pool = Vec::new();
    for surface in surfaces {
        let folder = owner
            .folders()
            .create(surface.clone(), String::from("work"))
            .await
            .unwrap_or_else(|e| panic!("owner creates a folder on {surface}: {e:?}"))
            .folder
            .id;
        for n in 0..per_surface {
            let alias = format!("ext-{n}");
            let chat = owner
                .chats()
                .inner_mut()
                .create()
                .call(Chats::create::Args {
                    title: Some(format!("{surface} #{n}")),
                    external_id: Some(alias.clone()),
                    folder: (n % 2 == 0).then_some(folder),
                    ..new_chat(surface)
                })
                .await
                .unwrap_or_else(|e| panic!("owner creates a chat on {surface}: {e:?}"))
                .chat
                .id;
            let blob = owner
                .blobs()
                .put(
                    Blobs::put::Meta {
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
                alias: Some(alias),
                folder,
            });
        }
    }

    for n in 0..20 {
        let mut people = Vec::new();
        for surface in surfaces {
            let person = owner
                .persons()
                .resolve(surface.clone(), format!("user-{n}"), None, None)
                .await
                .expect("owner resolves an identity")
                .person
                .id;
            people.push(person);
        }
        for from in &people[1..] {
            owner
                .persons()
                .merge(from.clone(), people[0].clone(), true)
                .await
                .expect("one person on every surface");
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
    let h = common::start_with(with_modules).await;
    let svc = listen(&h, Module::new(), SVC).await;
    let other = listen(&h, Module::new(), OTHER).await;
    let (_listening, _url) = h.listen().await;
    let svc = dialed(&svc, SVC).await;
    create_surface(&svc, SVC, "Service").await;
    let other = dialed(&other, OTHER).await;
    create_surface(&other, OTHER, "Other").await;

    let mine = SurfaceId::from(SVC);
    let surfaces = [
        SurfaceId::from("local"),
        SurfaceId::from("system"),
        mine.clone(),
        SurfaceId::from(OTHER),
    ];
    let mut pool = populate(&h, &surfaces, 3).await;
    let own_folder = pool
        .iter()
        .find(|o| o.surface == mine)
        .map(|o| o.folder)
        .expect("a folder of the own surface");
    let mut rng = Xoshiro256StarStar::seed_from_u64(11);

    for round in 0..300 {
        let mut k = svc.clone();
        let object = &pool[rng.random_range(0..pool.len())];
        let (chat, surface, own) = (object.chat, object.surface.clone(), object.surface == mine);
        let blob_id = object.blob.clone();
        let (alias, folder) = (object.alias.clone(), object.folder);
        let step = |op: &str| format!("round {round}: {op} on {surface}");

        match rng.random_range(0..15u8) {
            0 => {
                let res = k.chats().get(chat.into()).await;
                expect_own(&step("chats.get"), own, &res);
                if let Ok(ok) = &res {
                    assert_eq!(ok.chat.surface_id, mine);
                }
            }
            1 => {
                let res = k
                    .chats()
                    .history_read(chat, 0, None, Default::default(), None)
                    .await;
                expect_own(&step("chats.history_read"), own, &res);
            }
            2 => {
                let res = k
                    .chats()
                    .update(
                        chat,
                        Chats::update::Update {
                            title: Patch::Change(Some(format!("renamed {round}"))),
                            ..Default::default()
                        },
                    )
                    .await;
                expect_own(&step("chats.update"), own, &res);
            }
            3 => {
                let res = k.blobs().stat(blob_id).await.map(|ok| ok.0);
                expect_own(&step("blobs.stat"), own, &res);
            }
            4 => {
                let res = k.blobs().get(blob_id, 0).await.map(|ok| ok.0).map(|_| ());
                expect_own(&step("blobs.get"), own, &res);
            }
            5 => {
                let res = k.stats().chat_spend(chat).await;
                expect_own(&step("stats.chat_spend"), own, &res);
            }
            6 => {
                let res = k.surfaces().get(surface.clone()).await;
                expect_own(&step("surfaces.get"), own, &res);
            }
            7 => {
                let res = k
                    .surfaces()
                    .update(
                        surface.clone(),
                        Surfaces::update::Update {
                            name: Patch::Change(format!("name {round}")),
                            ..Default::default()
                        },
                    )
                    .await;
                expect_own(&step("surfaces.update"), own, &res);
            }
            8 => {
                let res = k
                    .persons()
                    .resolve(
                        surface.clone(),
                        format!("user-{}", rng.random_range(0..20u8)),
                        None,
                        None,
                    )
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
                let res = k
                    .chats()
                    .inner_mut()
                    .create()
                    .call(new_chat(&surface))
                    .await;
                expect_own(&step("chats.create"), own, &res);
                if let Ok(ok) = res {
                    assert_eq!(ok.chat.surface_id, mine);
                    let blob = k
                        .blobs()
                        .put(
                            Blobs::put::Meta {
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
                        alias: None,
                        folder: own_folder,
                    });
                }
            }
            10 => {
                let res = k
                    .chats()
                    .inner_mut()
                    .list()
                    .call(Chats::list::Args {
                        surface: Some(surface.clone()),
                        deleted: rng.random(),
                        ..Default::default()
                    })
                    .await;
                expect_own(&step("chats.list by surface"), own, &res);
                if let Ok(ok) = &res {
                    assert!(ok.found.iter().all(|c| c.surface_id == mine), "{res:?}");
                }
            }
            11 => {
                let res = k
                    .chats()
                    .inner_mut()
                    .list()
                    .call(Chats::list::Args {
                        folder: Some(folder),
                        ..Default::default()
                    })
                    .await;
                expect_own(&step("chats.list by folder"), own, &res);
                if let Ok(ok) = &res {
                    assert!(ok.found.iter().all(|c| c.surface_id == mine), "{res:?}");
                }
            }
            12 => {
                let res = k
                    .chats()
                    .inner_mut()
                    .list()
                    .call(Chats::list::Args {
                        after: Some(chat),
                        ..Default::default()
                    })
                    .await;
                let want = if own { Outcome::Ok } else { Outcome::Conflict };
                assert_eq!(outcome(&res), want, "{}: {res:?}", step("chats.list after"));
                if let Ok(ok) = &res {
                    assert!(ok.found.iter().all(|c| c.surface_id == mine), "{res:?}");
                }
            }
            13 => {
                // Есть alias или нет — чужой surface отвечает одинаково.
                let external_id = match (&alias, rng.random::<bool>()) {
                    (Some(alias), true) => alias.clone(),
                    _ => format!("absent-{round}"),
                };
                let exists = alias.as_ref() == Some(&external_id);
                let res = k
                    .chats()
                    .get(Selector::Alias {
                        surface: surface.clone(),
                        external_id,
                    })
                    .await;
                let want = match (own, exists) {
                    (false, _) => Outcome::Forbidden,
                    (true, true) => Outcome::Ok,
                    (true, false) => Outcome::Spec,
                };
                assert_eq!(outcome(&res), want, "{}: {res:?}", step("chats.get alias"));
                if let Ok(ok) = &res {
                    assert_eq!(ok.chat.surface_id, mine);
                }
            }
            _ => {
                let missing = ChatId(100_000 + i64::from(rng.random_range(0..1000u16)));
                let res = k.chats().get(missing.into()).await;
                assert_eq!(
                    outcome(&res),
                    Outcome::Spec,
                    "{}: {res:?}",
                    step("chats.get missing")
                );
                let res = k
                    .surfaces()
                    .inner_mut()
                    .list()
                    .call(Surfaces::list::Args::default())
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

/// Два клиента хозяина с разными токенами обслуживают `local` одновременно
/// и видят одни чаты (`06 §2`); `local` никто не создаёт — soul сеет его сам.
#[tokio::test]
async fn two_sdk_clients_serve_local_together() {
    let h = common::start_with(with_modules).await;
    let svc = listen(&h, Module::new(), SVC).await;
    let (_listening, url) = h.listen().await;
    let phone = Module::new()
        .ask(true)
        .connect(&url, &secret(CLIENT))
        .await
        .expect("phone connects");
    let tui = connect(&url, CLIENT2).await;
    let _svc = dialed(&svc, SVC).await;

    let mut serving = h.serving("local").await;
    serving.sort();
    assert_eq!(serving, [CLIENT, CLIENT2], "a service never serves local");
    assert!(h.ask_ui("local").await, "one asking client is enough");

    let local = SurfaceId::from("local");
    let chat = phone
        .soul()
        .chats()
        .inner_mut()
        .create()
        .call(new_chat(&local))
        .await
        .expect("phone creates a local chat")
        .chat
        .id;
    let seen = tui
        .soul()
        .chats()
        .get(chat.into())
        .await
        .expect("tui sees the phone's chat")
        .chat;
    assert_eq!(seen.surface_id, local);
    assert!(!phone.client().peer().is_closed() && !tui.client().peer().is_closed());
}

/// Второй `hello` с тем же токеном вытесняет первый, и SDK первого не лезет
/// обратно (иначе два процесса выбивали бы друг друга по кругу).
#[tokio::test]
async fn a_superseded_sdk_client_stops_instead_of_fighting_back() {
    let h = common::start_with(with_modules).await;
    let (_listening, url) = h.listen().await;
    let module =
        || Module::new().reconnect_backoff(Duration::from_millis(10), Duration::from_millis(50));
    let first = module()
        .connect(&url, &secret(CLIENT2))
        .await
        .expect("first connects");
    create_surface(&first.soul(), CLIENT2, "Desk").await;
    let running = tokio::spawn(first.run(pending()));

    let second = module()
        .connect(&url, &secret(CLIENT2))
        .await
        .expect("second connects");
    create_surface(&second.soul(), CLIENT2, "Desk").await;
    let ended = tokio::time::timeout(WAIT, running)
        .await
        .expect("the first module stops")
        .expect("run does not panic");
    let error = ended.expect_err("a superseded module ends with an error");
    assert!(error.to_string().contains("superseded"), "{error}");

    let res = second.soul().surfaces().get(SurfaceId::from(CLIENT2)).await;
    assert!(res.is_ok(), "{res:?}");
    assert!(!second.client().peer().is_closed());
}

/// Обрыв со стороны soul: SDK-клиент переподключается с `hello.resume`,
/// surface снова обслуживается без единого вызова модуля, прежняя ручка
/// `Connected` ведёт уже в новую сессию, а тот же процесс soul продолжает
/// окно идемпотентности.
#[tokio::test]
async fn an_sdk_client_reconnects_with_resume() {
    let h = common::start_with(with_modules).await;
    let (_listening, url) = h.listen().await;
    let tui = Module::new()
        .ask(true)
        .reconnect_backoff(Duration::from_millis(10), Duration::from_millis(50))
        .connect(&url, &secret(CLIENT2))
        .await
        .expect("client connects");
    create_surface(&tui.soul(), CLIENT2, "Desk").await;
    assert!(h.ask_ui(CLIENT2).await);
    assert!(
        !tui.welcome().resumed,
        "the first hello has nothing to resume"
    );
    let first = tui.welcome().session_id;
    let handle = tui.clone();
    let running = tokio::spawn(tui.run(pending()));

    h.soul.shared().live().close_all(&RpcError::new(
        ErrorCode::Unavailable,
        "the test drops sessions",
    ));
    eventually("a new session serves the surface again", async || {
        handle.welcome().session_id != first && h.ask_ui(CLIENT2).await
    })
    .await;
    assert_eq!(h.serving(CLIENT2).await, [CLIENT2]);
    assert!(
        handle.welcome().resumed,
        "the same soul process continues the window"
    );
    assert_eq!(
        reconnects(&h, CLIENT2).await,
        1,
        "the second hello carried resume"
    );

    let res = handle.soul().surfaces().get(SurfaceId::from(CLIENT2)).await;
    assert!(res.is_ok(), "{res:?}");
    running.abort();
}

/// Обрыв со стороны soul: soul сам перезванивает SDK-сервису, тот отвечает
/// `hello.resume`; `Soul`, взятый до обрыва, ведёт уже в новую сессию.
#[tokio::test]
async fn an_sdk_service_is_redialed_with_resume() {
    let h = common::start_with(with_modules).await;
    let svc = listen(&h, Module::new().ask(true), SVC).await;
    let (_listening, _url) = h.listen().await;
    let soul = dialed(&svc, SVC).await;
    create_surface(&soul, SVC, "Service").await;
    assert!(h.ask_ui(SVC).await);
    let welcome = svc.welcome().expect("welcomed");
    assert!(!welcome.resumed, "the first hello has nothing to resume");
    let running = tokio::spawn(svc.clone().run(pending()));

    h.soul.shared().live().close_all(&RpcError::new(
        ErrorCode::Unavailable,
        "the test drops sessions",
    ));
    eventually(
        "the soul redials and the surface is served again",
        async || {
            svc.welcome()
                .is_some_and(|w| w.session_id != welcome.session_id)
                && h.ask_ui(SVC).await
        },
    )
    .await;
    assert_eq!(h.serving(SVC).await, [SVC]);
    assert!(
        svc.welcome().is_some_and(|w| w.resumed),
        "the same soul process continues the window"
    );
    assert_eq!(
        reconnects(&h, SVC).await,
        1,
        "the second hello carried resume"
    );

    let res = soul.clone().surfaces().get(SurfaceId::from(SVC)).await;
    assert!(res.is_ok(), "{res:?}");
    running.abort();
}

/// Рестарт модуля: новый процесс слушает тот же адрес, soul набирает его
/// сам; окно нового процесса пусто — `hello` без `resume`, а surface
/// прежнего процесса обслуживается без повторного создания.
#[tokio::test]
async fn a_restarted_sdk_service_is_redialed() {
    let h = common::start_with(with_modules).await;
    let svc = listen(&h, Module::new().ask(true), SVC).await;
    let (_listening, _url) = h.listen().await;
    let soul = dialed(&svc, SVC).await;
    create_surface(&soul, SVC, "Service").await;
    let first = svc.welcome().expect("welcomed").session_id;

    svc.run(async {}).await.expect("the module stops");
    eventually("the soul loses the module", async || {
        h.serving(SVC).await.is_empty()
    })
    .await;

    let svc = listen(&h, Module::new().ask(true), SVC).await;
    let mut soul = dialed(&svc, SVC).await;
    let welcome = svc.welcome().expect("welcomed");
    assert_ne!(welcome.session_id, first);
    assert!(
        !welcome.resumed,
        "a new module process has nothing to resume"
    );
    assert_eq!(
        reconnects(&h, SVC).await,
        0,
        "the new process sent no resume"
    );
    assert_eq!(h.serving(SVC).await, [SVC]);
    assert!(h.ask_ui(SVC).await, "capabilities come from the new hello");
    let res = soul.surfaces().get(SurfaceId::from(SVC)).await;
    assert!(res.is_ok(), "{res:?}");
}

/// Рестарт soul: модуль слушает дальше и принимает новое соединение с
/// `hello.resume`; новый процесс soul окна не продолжает, а `Soul`,
/// взятый до рестарта, ведёт уже в новую сессию.
#[tokio::test]
async fn an_sdk_service_outlives_a_soul_restart() {
    let h = common::start_with(with_modules).await;
    let svc = listen(&h, Module::new().ask(true), SVC).await;
    let (listening, _url) = h.listen().await;
    let mut soul = dialed(&svc, SVC).await;
    create_surface(&soul, SVC, "Service").await;
    let first = svc.welcome().expect("welcomed").session_id;
    let running = tokio::spawn(svc.clone().run(pending()));

    listening.shutdown().await;
    let Harness {
        soul: stopped, dir, ..
    } = h;
    drop(stopped);
    let mut cfg = common::config(dir.path());
    with_modules(&mut cfg);
    let h = common::start_in(dir, cfg).await;
    let (_listening, _url) = h.listen().await;

    eventually("the new soul dials the module", async || {
        svc.welcome().is_some_and(|w| w.session_id != first) && h.ask_ui(SVC).await
    })
    .await;
    assert!(
        svc.welcome().is_some_and(|w| !w.resumed),
        "a new soul process has no window to continue"
    );
    assert_eq!(reconnects(&h, SVC).await, 1, "the hello carried resume");
    assert_eq!(h.serving(SVC).await, [SVC]);
    let res = soul.surfaces().get(SurfaceId::from(SVC)).await;
    assert!(res.is_ok(), "{res:?}");
    assert!(!running.is_finished(), "run outlives the soul");
    running.abort();
}

/// Чужой ключ: Noise рвётся, сессии нет, причину видят обе стороны; soul
/// сам набирает модуль, поднятый заново с верным ключом.
#[tokio::test]
async fn a_wrong_key_gets_no_session_until_the_module_has_the_right_one() {
    let h = common::start_with(with_modules).await;
    let wrong = eva_sdk::ServiceConfig {
        key: SecretString::Plaintext(common::key(OTHER)),
        soul_wait: HumanDuration(Duration::from_millis(1500)),
        ..common::sdk_service(h.dir.path(), SVC)
    };
    let svc = Module::new().listen(&wrong).await.expect("service listens");
    let (_listening, _url) = h.listen().await;

    match svc.soul().await {
        Err(ConnectError::Transport(message)) => {
            assert!(message.contains("wrong key"), "{message}");
        }
        other => panic!(
            "expected no soul, got {:?}",
            other.map(|k| k.caller().clone())
        ),
    }
    assert_eq!(svc.module(), None);
    let dial = listed(&h, SVC).await.dial.expect("a registry entry");
    assert!(
        dial.error
            .as_deref()
            .is_some_and(|e| e.contains("wrong key")),
        "{dial:?}"
    );
    assert!(dial_failures(&h, SVC).await >= 1);
    let never = listed(&h, SVC).await;
    assert_eq!(
        (never.state, never.version.as_str()),
        (State::Offline, ""),
        "no session, no row"
    );
    svc.run(async {}).await.expect("the module stops");

    let svc = listen(&h, Module::new(), SVC).await;
    let soul = dialed(&svc, SVC).await;
    assert_eq!(soul.caller(), &service(SVC));
    let listed = listed(&h, SVC).await;
    assert_eq!(listed.state, State::Connected);
    assert_eq!(listed.dial.and_then(|d| d.error), None, "welcome clears it");
}

/// `modules.add` без ключа от клиента хозяина: soul выдаёт ключ один раз и
/// сам набирает SDK-сервис с этим ключом. Запись конфига операцией не снять и
/// её сессию не задеть; `modules.remove` закрывает сессию своей записи.
#[tokio::test]
async fn the_owner_adds_and_removes_an_sdk_service_at_runtime() {
    let h = common::start_with(with_modules).await;
    let svc = listen(&h, Module::new(), SVC).await;
    let (_listening, url) = h.listen().await;
    let phone = connect(&url, CLIENT).await;
    let mut svc_soul = dialed(&svc, SVC).await;

    let Modules::add::Ok { key } = phone
        .soul()
        .modules()
        .add(
            ModuleId::from("bridge"),
            common::service_address(h.dir.path(), "bridge"),
            None,
        )
        .await
        .expect("the owner's client adds a service");
    let key = key.expect("the soul generates the key");
    let bridge = Module::new()
        .listen(&eva_sdk::ServiceConfig {
            key: SecretString::Plaintext(key.expose().to_owned()),
            ..common::sdk_service(h.dir.path(), "bridge")
        })
        .await
        .expect("bridge listens");
    let bridge_soul = dialed(&bridge, "bridge").await;
    assert_eq!(bridge_soul.caller(), &service("bridge"));
    create_surface(&bridge_soul, "bridge", "Bridge").await;

    let res = phone.soul().modules().remove(ModuleId::from(SVC)).await;
    assert!(
        matches!(&res, Err(Generic::Aux(Aux::Conflict(_)))),
        "config is stronger: {res:?}"
    );
    let res = svc_soul
        .surfaces()
        .inner_mut()
        .list()
        .call(Surfaces::list::Args::default())
        .await;
    assert!(res.is_ok(), "the config session stays: {res:?}");

    let peer = bridge.client().expect("dialed");
    phone
        .soul()
        .modules()
        .remove(ModuleId::from("bridge"))
        .await
        .expect("the owner's client removes its service");
    let reason = tokio::time::timeout(WAIT, peer.closed())
        .await
        .expect("remove closes the session");
    assert_eq!(
        (reason.code, reason.message.as_str()),
        (ErrorCode::Unauthorized, "module removed")
    );
    assert!(h.serving("bridge").await.is_empty());
}

/// Клиенты хозяина звонят с токеном на любой слушатель soul — uds, ws и wss —
/// и живут рядом; токен, выданный операцией, работает как токен конфига.
#[tokio::test]
async fn owner_clients_connect_with_tokens_over_uds_ws_and_wss() {
    let [ws, wss] = free_ports();
    let h = common::start_with(|cfg| {
        with_modules(cfg);
        cfg.modules.listen.push(Listen::Ws(WsListen {
            addr: SocketAddr::from(([127, 0, 0, 1], ws)),
        }));
        cfg.modules.listen.push(Listen::Wss(WssListen {
            addr: SocketAddr::from(([127, 0, 0, 1], wss)),
            tls: TlsFiles {
                cert: fixture("localhost.pem"),
                key: fixture("localhost.key"),
            },
        }));
    })
    .await;
    let (_listening, uds) = h.listen().await;
    let tablet = h
        .client_session()
        .await
        .modules()
        .token_issue(ModuleId::from("tablet"), None)
        .await
        .expect("token_issue")
        .token;

    let ws = Url::parse(&format!("ws://127.0.0.1:{ws}/v1/modules")).expect("ws url");
    let wss = Url::parse(&format!("wss://127.0.0.1:{wss}")).expect("wss url");
    let trusted = ClientTls {
        extra_ca: Some(fixture("ca.pem")),
    };
    let clients = [
        (uds, token(CLIENT), ClientTls::default(), CLIENT),
        (ws, token(CLIENT2), ClientTls::default(), CLIENT2),
        (wss, tablet, trusted, "tablet"),
    ];
    let mut connected = Vec::new();
    for (soul, token, tls, module) in clients {
        let client = Module::new()
            .connect_config(&ClientConfig {
                soul: soul.clone(),
                token: SecretString::Plaintext(token),
                connect_wait: HumanDuration(WAIT),
                tls,
                transport: eva_rpc::Config::default(),
            })
            .await
            .unwrap_or_else(|e| panic!("{module} over {soul}: {e}"));
        assert_eq!(client.caller(), common::client(module), "{soul}");
        assert_eq!(client.welcome().kind, ModuleKind::Client, "{soul}");
        let res = client.soul().surfaces().get(SurfaceId::from("local")).await;
        assert!(res.is_ok(), "{soul}: {res:?}");
        connected.push(client);
    }

    let mut serving = h.serving("local").await;
    serving.sort();
    assert_eq!(serving, [CLIENT, "tablet", CLIENT2]);
    assert!(connected.iter().all(|c| !c.client().peer().is_closed()));
}

/// Свободные порты loopback: пробные сокеты держатся вместе, чтобы порты различались.
fn free_ports<const N: usize>() -> [u16; N] {
    let probes: [std::net::TcpListener; N] =
        std::array::from_fn(|_| std::net::TcpListener::bind("127.0.0.1:0").expect("probe port"));
    probes.map(|probe| probe.local_addr().expect("probe address").port())
}

/// Сертификаты `eva-rpc`: CA и лист для `127.0.0.1`.
fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../rpc/tests/fixtures")
        .join(name)
}

async fn listed(h: &Harness, module: &str) -> eva_protocol::types::module::Module {
    h.client_session()
        .await
        .modules()
        .list()
        .await
        .expect("modules.list")
        .modules
        .into_iter()
        .find(|m| m.name.as_str() == module)
        .expect("listed")
}

async fn dial_failures(h: &Harness, module: &str) -> i64 {
    counter(h, "modules.dial_failed", module).await
}

async fn reconnects(h: &Harness, module: &str) -> i64 {
    counter(h, "modules.reconnects", module).await
}

async fn counter(h: &Harness, metric: &'static str, module: &str) -> i64 {
    let module = module.to_owned();
    h.query(move |c| {
        Ok(c.query_row(
            "SELECT coalesce(sum(count), 0) FROM health_stats WHERE metric = ?1 AND key = ?2",
            [metric, module.as_str()],
            |r| r.get::<_, i64>(0),
        )?)
    })
    .await
}

/// Имя клиенту даёт токен: чужой токен — это чужая идентичность, а не отказ.
#[tokio::test]
async fn an_sdk_client_is_whoever_its_token_names() {
    let h = common::start_with(with_modules).await;
    let (_listening, url) = h.listen().await;

    let res = Module::new()
        .connect(&url, &SecretString::Plaintext(String::from("wrong")))
        .await;
    assert!(
        matches!(&res, Err(ConnectError::Rejected(e)) if e.code == ErrorCode::Unauthorized),
        "{:?}",
        res.map(|c| c.welcome())
    );
    let res = Module::new()
        .connect(&url, &SecretString::Plaintext(common::key(SVC)))
        .await;
    assert!(
        matches!(&res, Err(ConnectError::Rejected(e)) if e.code == ErrorCode::Unauthorized),
        "a service key is no token: {:?}",
        res.map(|c| c.welcome())
    );

    let tui = connect(&url, CLIENT2).await;
    assert_eq!(tui.caller(), client(CLIENT2));
    assert_eq!(
        tui.module().as_str(),
        CLIENT2,
        "welcome.module names the token entry"
    );
}

/// Время изменения файла ставит файловая система хоста, не мир soul.
fn untimed(entry: &mut Entry) {
    entry.modified = eva::time::Timestamp::TEST_ORIGIN;
}
