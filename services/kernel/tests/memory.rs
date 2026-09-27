//! `memory.*` (`03 §2`, `§7`, `§9`, `11 §3`): чтение, видимость `view`,
//! цепочка версий, restore и матрица `Policy`.

mod common;

use std::sync::atomic::Ordering;

use common::{Harness, Outcome, outcome};
use eva::time::Clock as _;
use eva_core::{CallStep as _, service::memory::Memory as _};
use eva_kernel::db::{Db as _, DbResult};
use eva_protocol::{
    errors::{Aux, Generic},
    requests::{
        Limit,
        memory::{get, history, list, restore},
    },
    types::{
        ChatId, MemoryId,
        memory::{ArchiveReason, Kind, Visibility},
        turn::{Audience, TurnView},
    },
};

const SEC: i64 = 1_000_000_000;

async fn exec(h: &Harness, sql: String) {
    h.kernel
        .shared()
        .db()
        .write(move |tx| -> DbResult<()> {
            tx.execute_batch(&sql)?;
            Ok(())
        })
        .await
        .expect("seed");
}

fn t0(h: &Harness) -> i64 {
    i64::try_from(h.clock.get().as_nanos().get()).expect("nanos fit")
}

/// Строка памяти: `created_at = t0 + id` секунд, `valid_to` — закрыта ли.
struct Row {
    id: i64,
    kind: &'static str,
    visibility: &'static str,
    text: &'static str,
    name: Option<&'static str>,
    supersedes: Option<i64>,
    version: i64,
    closed: bool,
}

impl Row {
    const fn new(
        id: i64,
        kind: &'static str,
        visibility: &'static str,
        text: &'static str,
    ) -> Self {
        Self {
            id,
            kind,
            visibility,
            text,
            name: None,
            supersedes: None,
            version: 1,
            closed: false,
        }
    }

    fn sql(&self, t0: i64) -> String {
        let at = t0 + self.id * SEC;
        let opt = |v: Option<String>| v.unwrap_or_else(|| String::from("NULL"));
        format!(
            "INSERT INTO memory (id, kind, text, name, description, tags, visibility, importance,
                source_actor, source_chat, source_trusted_chat, created_at, updated_at, valid_from,
                valid_to, supersedes, version, last_reinforced_at, access_count)
             VALUES ({id}, '{kind}', '{text}', {name}, {desc}, 'alpha beta', '{vis}', 0.7,
                'p0', 7, 1, {at}, {at}, {at}, {valid_to}, {sup}, {version}, {at}, 2.5);",
            id = self.id,
            kind = self.kind,
            text = self.text,
            name = opt(self.name.map(|n| format!("'{n}'"))),
            desc = opt(self.name.map(|_| String::from("'how to'"))),
            vis = self.visibility,
            valid_to = opt(self.closed.then(|| (at + 100 * SEC).to_string())),
            sup = opt(self.supersedes.map(|s| s.to_string())),
            version = self.version,
        )
    }
}

fn archive(id: i64, reason: &str, t0: i64) -> String {
    format!(
        "INSERT INTO memory_archive (memory_id, reason, why, actor, archived_at)
         VALUES ({id}, '{reason}', 'stale', 'p0', {at});",
        at = t0 + (id + 100) * SEC
    )
}

/// 1 fact public; 2 → 3 цепочка fact (2 закрыта); 4 core private;
/// 5 event trusted забыта; 6 → 7: забытая слиянием и её новая версия.
async fn seed(h: &Harness) {
    let t0 = t0(h);
    let rows = [
        Row::new(1, "fact", "public", "coffee without sugar"),
        Row {
            closed: true,
            ..Row::new(2, "fact", "public", "tea in the morning")
        },
        Row {
            supersedes: Some(2),
            version: 2,
            ..Row::new(3, "fact", "public", "tea in the evening")
        },
        Row::new(4, "core", "private", "master name"),
        Row {
            closed: true,
            ..Row::new(5, "event", "trusted", "passport renewed")
        },
        Row {
            closed: true,
            ..Row::new(6, "fact", "trusted", "old address")
        },
        Row {
            supersedes: Some(6),
            version: 2,
            ..Row::new(7, "fact", "trusted", "new address")
        },
    ];
    let mut sql: String = rows.iter().map(|r| r.sql(t0)).collect();
    sql.push_str(&archive(5, "forget", t0));
    sql.push_str(&archive(6, "merge", t0));
    exec(h, sql).await;
}

fn view(master_turn: bool, trusted_chat: bool) -> Option<TurnView> {
    Some(TurnView {
        chat: ChatId(1),
        actor: None,
        master_turn,
        trusted_chat,
        audience: Audience::Mixed,
    })
}

fn ids<'a>(records: impl IntoIterator<Item = &'a eva_protocol::types::memory::Record>) -> Vec<i64> {
    records.into_iter().map(|r| r.id.get()).collect()
}

async fn list(h: &Harness, args: list::Args) -> Vec<i64> {
    let list::Ok { found } = h
        .client_session()
        .await
        .memory()
        .list()
        .call(args)
        .await
        .expect("list");
    ids(&found)
}

#[tokio::test]
async fn list_returns_current_versions_newest_first() {
    let h = common::start().await;
    seed(&h).await;

    assert_eq!(list(&h, list::Args::default()).await, [7, 4, 3, 1]);
    assert_eq!(
        list(
            &h,
            list::Args {
                kind: Some(Kind::Fact),
                ..Default::default()
            }
        )
        .await,
        [7, 3, 1]
    );
    assert_eq!(
        list(
            &h,
            list::Args {
                visibility: Some(Visibility::Private),
                ..Default::default()
            }
        )
        .await,
        [4]
    );
    let since = h.clock.get().as_nanos().get() + 4 * SEC as u64;
    let since = eva::time::Timestamp::from_nanos(since.try_into().expect("non-zero"));
    assert_eq!(
        list(
            &h,
            list::Args {
                since: Some(since),
                ..Default::default()
            }
        )
        .await,
        [7, 4]
    );
}

#[tokio::test]
async fn list_pages_by_id_keyset() {
    let h = common::start().await;
    seed(&h).await;
    let limit = Limit::new(2).expect("limit in range");

    let first = list(
        &h,
        list::Args {
            limit,
            ..Default::default()
        },
    )
    .await;
    assert_eq!(first, [7, 4]);
    let second = list(
        &h,
        list::Args {
            limit,
            after: Some(MemoryId(4)),
            ..Default::default()
        },
    )
    .await;
    assert_eq!(second, [3, 1]);
    let third = list(
        &h,
        list::Args {
            limit,
            after: Some(MemoryId(1)),
            ..Default::default()
        },
    )
    .await;
    assert!(third.is_empty(), "past the last row: {third:?}");
}

#[tokio::test]
async fn get_returns_full_row_and_archive() {
    let h = common::start().await;
    seed(&h).await;
    let mut session = h.client_session().await;

    let get::Ok { record, archive } = session
        .memory()
        .get()
        .call(get::Args {
            id: MemoryId(1),
            view: None,
        })
        .await
        .expect("get");
    assert_eq!(record.text, "coffee without sugar");
    assert_eq!(record.tags, ["alpha", "beta"]);
    assert_eq!(record.source.chat, Some(ChatId(7)));
    assert_eq!(record.source.trusted_chat, Some(true));
    assert!((record.importance - 0.7).abs() < 1e-6);
    assert!((record.access_count - 2.5).abs() < 1e-6);
    assert!(archive.is_none());

    let get::Ok { record, archive } = session
        .memory()
        .get()
        .call(get::Args {
            id: MemoryId(5),
            view: None,
        })
        .await
        .expect("get forgotten");
    assert!(record.valid_to.is_some());
    let archive = archive.expect("forgotten row has an archive");
    assert_eq!(archive.reason, ArchiveReason::Forget);
    assert_eq!(archive.why, "stale");
    assert!(archive.restored_at.is_none());

    let res = session
        .memory()
        .get()
        .call(get::Args {
            id: MemoryId(999),
            view: None,
        })
        .await;
    assert!(
        matches!(res, Err(Generic::Spec(get::Err::NotFound(ref e))) if e.id == MemoryId(999)),
        "{res:?}"
    );
}

#[tokio::test]
async fn view_hides_rows_as_not_found() {
    let h = common::start().await;
    seed(&h).await;
    let mut session = h.kernel_session().await;

    // (id, view, visible)
    let cases = [
        (4, view(false, true), false),
        (4, view(true, false), false),
        (4, view(true, true), true),
        (5, view(false, false), false),
        (5, view(false, true), true),
        (1, view(false, false), true),
        (4, None, true),
    ];
    for (id, view, visible) in cases {
        let res = session
            .memory()
            .get()
            .call(get::Args {
                id: MemoryId(id),
                view: view.clone(),
            })
            .await;
        match (visible, &res) {
            (true, Ok(_)) | (false, Err(Generic::Spec(get::Err::NotFound(_)))) => {}
            _ => panic!("m{id} with {view:?}: want visible={visible}, got {res:?}"),
        }
    }
}

#[tokio::test]
async fn history_walks_the_chain_root_to_current() {
    let h = common::start().await;
    seed(&h).await;
    let mut session = h.client_session().await;

    for id in [2, 3] {
        let history::Ok { versions } = session
            .memory()
            .history()
            .call(history::Args {
                id: MemoryId(id),
                view: None,
            })
            .await
            .expect("history");
        assert_eq!(
            ids(versions.iter().map(|v| &v.record)),
            [2, 3],
            "from m{id}"
        );
        assert_eq!(versions[0].record.version, 1);
        assert_eq!(versions[1].record.supersedes, Some(MemoryId(2)));
    }

    let history::Ok { versions } = session
        .memory()
        .history()
        .call(history::Args {
            id: MemoryId(7),
            view: None,
        })
        .await
        .expect("history with archive");
    assert_eq!(ids(versions.iter().map(|v| &v.record)), [6, 7]);
    assert_eq!(
        versions[0].archive.as_ref().map(|a| a.reason),
        Some(ArchiveReason::Merge)
    );

    let res = session
        .memory()
        .history()
        .call(history::Args {
            id: MemoryId(999),
            view: None,
        })
        .await;
    assert!(
        matches!(res, Err(Generic::Spec(history::Err::NotFound(_)))),
        "{res:?}"
    );
}

#[tokio::test]
async fn history_under_view_hides_what_the_turn_cannot_see() {
    let h = common::start().await;
    seed(&h).await;
    let mut session = h.kernel_session().await;

    let res = session
        .memory()
        .history()
        .call(history::Args {
            id: MemoryId(4),
            view: view(false, true),
        })
        .await;
    assert!(
        matches!(res, Err(Generic::Spec(history::Err::NotFound(_)))),
        "{res:?}"
    );

    let history::Ok { versions } = session
        .memory()
        .history()
        .call(history::Args {
            id: MemoryId(7),
            view: view(false, true),
        })
        .await
        .expect("trusted chain in a trusted chat");
    assert_eq!(versions.len(), 2);
}

#[tokio::test]
async fn restore_brings_a_forgotten_row_back_unchanged() {
    let h = common::start().await;
    seed(&h).await;
    let mut session = h.client_session().await;

    let get::Ok { record: before, .. } = session
        .memory()
        .get()
        .call(get::Args {
            id: MemoryId(5),
            view: None,
        })
        .await
        .expect("get");
    h.clock.clone().advance(std::time::Duration::from_secs(60));

    let restore::Ok { record } = session
        .memory()
        .restore()
        .call(restore::Args { id: MemoryId(5) })
        .await
        .expect("restore");
    assert_eq!(record.valid_to, None);
    assert_eq!(
        eva_protocol::types::memory::Record {
            valid_to: before.valid_to,
            ..record.clone()
        },
        before,
        "restore changes only valid_to"
    );

    let get::Ok { record, archive } = session
        .memory()
        .get()
        .call(get::Args {
            id: MemoryId(5),
            view: None,
        })
        .await
        .expect("get after restore");
    assert_eq!(record.valid_to, None);
    assert_eq!(
        archive.and_then(|a| a.restored_at),
        Some(h.clock.get()),
        "archive keeps the row with restored_at"
    );
    assert!(list(&h, list::Args::default()).await.contains(&5));

    let res = session
        .memory()
        .restore()
        .call(restore::Args { id: MemoryId(5) })
        .await;
    assert!(
        matches!(res, Err(Generic::Spec(restore::Err::NotArchived(_)))),
        "second restore: {res:?}"
    );
}

#[tokio::test]
async fn restore_errors() {
    let h = common::start().await;
    seed(&h).await;
    let mut session = h.client_session().await;

    let res = restore_id(&mut session, 999).await;
    assert!(
        matches!(res, Err(Generic::Spec(restore::Err::NotFound(_)))),
        "{res:?}"
    );
    // Текущая и закрытая supersede без архива — не забыты.
    for id in [1, 2] {
        let res = restore_id(&mut session, id).await;
        assert!(
            matches!(res, Err(Generic::Spec(restore::Err::NotArchived(ref e))) if e.id == MemoryId(id)),
            "m{id}: {res:?}"
        );
    }
    let res = restore_id(&mut session, 6).await;
    assert!(
        matches!(
            res,
            Err(Generic::Spec(restore::Err::Superseded(ref e)))
                if e.id == MemoryId(6) && e.current == MemoryId(7)
        ),
        "{res:?}"
    );
}

#[tokio::test]
async fn restore_refuses_a_taken_procedure_name() {
    let h = common::start().await;
    let t0 = t0(&h);
    let old = Row {
        name: Some("deploy"),
        closed: true,
        ..Row::new(1, "procedure", "public", "old deploy steps")
    };
    let new = Row {
        name: Some("deploy"),
        ..Row::new(2, "procedure", "public", "new deploy steps")
    };
    exec(
        &h,
        format!("{}{}{}", old.sql(t0), new.sql(t0), archive(1, "forget", t0)),
    )
    .await;

    let res = h
        .client_session()
        .await
        .memory()
        .restore()
        .call(restore::Args { id: MemoryId(1) })
        .await;
    assert!(
        matches!(res, Err(Generic::Aux(Aux::Conflict(_)))),
        "{res:?}"
    );
    let valid_to: Option<i64> = h
        .query(|c| Ok(c.query_row("SELECT valid_to FROM memory WHERE id = 1", [], |r| r.get(0))?))
        .await;
    assert!(valid_to.is_some(), "refused restore changes nothing");
}

#[tokio::test]
async fn database_failure_is_aux_db() {
    let h = common::start().await;
    h.db_fail.store(true, Ordering::Relaxed);
    let res = h
        .client_session()
        .await
        .memory()
        .list()
        .call(list::Args::default())
        .await;
    assert!(matches!(res, Err(Generic::Aux(Aux::Db(_)))), "{res:?}");
}

#[tokio::test]
async fn memory_matrix() {
    use Outcome::{Forbidden as F, Ok as K, Spec as S};

    let h = common::start().await;
    seed(&h).await;
    h.service_with_surface(common::SVC).await;
    h.service_with_surface(common::OTHER).await;

    let mut failures = Vec::new();
    for ((who, caller), want) in common::matrix_callers().into_iter().zip([K, K, F, F]) {
        let mut s = h.session(caller).await;
        let listed = outcome(&s.memory().list().call(list::Args::default()).await);
        let got = outcome(
            &s.memory()
                .get()
                .call(get::Args {
                    id: MemoryId(1),
                    view: None,
                })
                .await,
        );
        let chain = outcome(
            &s.memory()
                .history()
                .call(history::Args {
                    id: MemoryId(3),
                    view: None,
                })
                .await,
        );
        let got = [listed, got, chain];
        for (op, got) in ["list", "get", "history"].into_iter().zip(got) {
            if got != want {
                failures.push(format!("memory.{op} as {who}: want {want:?}, got {got:?}"));
            }
        }
        // Несуществующая запись: хозяин доходит до Spec, сервис — нет.
        let want = if want == K { S } else { F };
        let got = outcome(
            &s.memory()
                .restore()
                .call(restore::Args { id: MemoryId(999) })
                .await,
        );
        if got != want {
            failures.push(format!(
                "memory.restore as {who}: want {want:?}, got {got:?}"
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

async fn restore_id(
    session: &mut common::TestSession,
    id: i64,
) -> eva_protocol::requests::Response<restore::Ok, restore::Err> {
    session
        .memory()
        .restore()
        .call(restore::Args { id: MemoryId(id) })
        .await
}

/// `memory.changed{ids, op: restore}` — клиентам без surface (`06 §7`):
/// иначе клиент, показывающий память, не узнал бы о чужом restore.
#[tokio::test]
async fn restore_publishes_memory_changed() {
    use eva_kernel::bus::Published;
    use eva_protocol::wire::events::{Event, MemoryChanged, MemoryOp};

    let h = common::start().await;
    seed(&h).await;
    let mut session = h.client_session().await;
    let mut events = h.kernel.shared().bus().subscribe();

    let res = restore_id(&mut session, 1).await;
    assert!(res.is_err(), "a live row is not restorable: {res:?}");
    assert!(
        events.try_recv().is_err(),
        "a refused restore publishes nothing"
    );

    restore_id(&mut session, 5).await.expect("restore");
    let published: Vec<Published> = std::iter::from_fn(|| events.try_recv().ok()).collect();
    assert!(
        matches!(
            published.as_slice(),
            [Published {
                event: Event::MemoryChanged(MemoryChanged { ids, op: MemoryOp::Restore }),
                surface: None,
            }] if ids == &[MemoryId(5)]
        ),
        "{published:?}"
    );
}
