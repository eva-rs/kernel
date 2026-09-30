//! `persons.*` (`05 §1.5`, `§8`, `11 §3`).

mod common;

use std::collections::BTreeSet;

use common::{Harness, OTHER, Outcome, SVC, TestSession, outcome};
use eva_core::{
    CallStep as _,
    service::{chats::Chats as _, persons::Persons as _},
};
use eva_protocol::{
    errors::Generic,
    requests::{
        Limit, chats,
        persons::{list, merge, rename, resolve, whitelist},
    },
    types::{PersonId, SurfaceId, person::Person},
};

use eva_kernel::{
    config::chats::MasterIdentity,
    db::{Db as _, persons::PERSON_REFS},
};

const MASTER: &str = "me";

/// Хозяин — `local:me`, он же `p1` после сида.
async fn start() -> Harness {
    common::start_with(|cfg| {
        cfg.chats.persons.master = vec![MasterIdentity {
            surface: SurfaceId::from("local"),
            external_id: String::from(MASTER),
            module: None,
        }];
    })
    .await
}

fn resolve_args(surface: &str, external_id: &str) -> resolve::Args {
    resolve::Args {
        surface: SurfaceId::from(surface),
        external_id: String::from(external_id),
        handle: None,
        name: None,
    }
}

async fn resolve(session: &mut TestSession, args: resolve::Args) -> Person {
    session
        .persons()
        .resolve()
        .call(args)
        .await
        .expect("resolve")
        .person
}

async fn master(session: &mut TestSession) -> Person {
    resolve(session, resolve_args("local", MASTER)).await
}

#[tokio::test]
async fn resolve_creates_then_finds() {
    let h = start().await;
    let mut svc = h.service_with_surface(SVC).await;

    let first = resolve(
        &mut svc,
        resolve::Args {
            handle: Some(String::from("@alice")),
            name: Some(String::from("Alice")),
            ..resolve_args(SVC, "42")
        },
    )
    .await;
    assert_eq!(first.name, "Alice");
    assert!(!first.master);
    assert!(!first.whitelisted);
    assert_eq!(first.identities.len(), 1);
    let identity = &first.identities[0];
    assert_eq!(identity.surface.as_str(), SVC);
    assert_eq!(identity.external_id, "42");
    assert_eq!(identity.handle.as_deref(), Some("@alice"));

    // Повтор находит ту же личность; handle обновлён, отсутствующее имя не стирается.
    let again = resolve(
        &mut svc,
        resolve::Args {
            handle: Some(String::from("@alice2")),
            ..resolve_args(SVC, "42")
        },
    )
    .await;
    assert_eq!(again.id, first.id);
    assert_eq!(
        again.name, "Alice",
        "person name is not rewritten by resolve"
    );
    assert_eq!(again.identities[0].handle.as_deref(), Some("@alice2"));
    assert_eq!(again.identities[0].name.as_deref(), Some("Alice"));

    let by_handle = resolve(
        &mut svc,
        resolve::Args {
            handle: Some(String::from("@bob")),
            ..resolve_args(SVC, "43")
        },
    )
    .await;
    assert_eq!(by_handle.name, "@bob");
    let bare = resolve(&mut svc, resolve_args(SVC, "44")).await;
    assert_eq!(bare.name, format!("{SVC}:44"));

    let ids: BTreeSet<_> = [&first.id, &by_handle.id, &bare.id].into_iter().collect();
    assert_eq!(ids.len(), 3);
    assert!(
        [&first.id, &by_handle.id, &bare.id]
            .iter()
            .all(|p| p.as_str() != "p0" && p.as_str() != "p1"),
        "p0 is system, p1 is the seeded master"
    );
}

#[tokio::test]
async fn resolve_knows_the_master_only_from_its_surface() {
    let h = start().await;
    let mut client = h.client_session().await;
    let me = master(&mut client).await;
    assert_eq!(me.id.as_str(), "p1");
    assert!(me.master);
    assert!(me.whitelisted);

    // Тот же external_id на чужом surface — другой человек (I4).
    let mut svc = h.service_with_surface(SVC).await;
    let impostor = resolve(&mut svc, resolve_args(SVC, MASTER)).await;
    assert_ne!(impostor.id, me.id);
    assert!(!impostor.master);
}

#[tokio::test]
async fn resolve_rejects_bad_input() {
    let h = start().await;
    let mut client = h.client_session().await;
    let res = client
        .persons()
        .resolve()
        .call(resolve_args("nowhere", "1"))
        .await;
    assert_eq!(outcome(&res), Outcome::NotFound, "{res:?}");
    let res = client
        .persons()
        .resolve()
        .call(resolve_args("local", ""))
        .await;
    assert_eq!(outcome(&res), Outcome::Invalid, "{res:?}");
}

#[tokio::test]
async fn service_sees_only_identities_of_its_surfaces() {
    let h = start().await;
    let mut svc = h.service_with_surface(SVC).await;
    let mut other = h.service_with_surface(OTHER).await;
    let mut client = h.client_session().await;

    let a = resolve(&mut svc, resolve_args(SVC, "7")).await;
    let b = resolve(&mut other, resolve_args(OTHER, "7")).await;
    client
        .persons()
        .merge()
        .call(merge::Args {
            from: b.id.clone(),
            into: a.id.clone(),
            confirm: false,
        })
        .await
        .expect("merge");

    let seen = resolve(&mut svc, resolve_args(SVC, "7")).await;
    assert_eq!(seen.id, a.id);
    let surfaces: Vec<_> = seen.identities.iter().map(|i| i.surface.as_str()).collect();
    assert_eq!(surfaces, [SVC]);

    let full = resolve(&mut client, resolve_args(SVC, "7")).await;
    assert_eq!(full.identities.len(), 2);
}

#[tokio::test]
async fn list_filters_and_pages() {
    let h = start().await;
    let mut svc = h.service_with_surface(SVC).await;
    let mut client = h.client_session().await;
    let me = master(&mut client).await;

    // p2..p12: номера за 9 проверяют порядок по числу, а не по строке.
    let mut created = Vec::new();
    for i in 0..11 {
        created.push(
            resolve(&mut svc, resolve_args(SVC, &format!("u{i}")))
                .await
                .id,
        );
    }
    client
        .persons()
        .whitelist()
        .call(whitelist::Args {
            person: created[0].clone(),
            on: true,
        })
        .await
        .expect("whitelist");

    let limit = Limit::new(4).expect("limit");
    let mut seen = Vec::new();
    let mut after = None;
    loop {
        let list::Ok { found } = client
            .persons()
            .list()
            .call(list::Args {
                limit,
                after: after.clone(),
                ..Default::default()
            })
            .await
            .expect("page");
        let last = found.len() < usize::from(limit.into_inner());
        after = found.last().map(|p| p.id.clone());
        seen.extend(found.into_iter().map(|p| p.id));
        if last {
            break;
        }
    }
    let numbers: Vec<u32> = seen
        .iter()
        .map(|p| p.as_str()[1..].parse().expect("p<N>"))
        .collect();
    assert_eq!(numbers, (0..=12).rev().collect::<Vec<_>>(), "p12 … p0");

    let list::Ok { found } = client
        .persons()
        .list()
        .call(list::Args {
            surface: Some(SurfaceId::from(SVC)),
            ..Default::default()
        })
        .await
        .expect("by surface");
    assert_eq!(found.len(), 11);
    assert!(
        found
            .iter()
            .all(|p| p.identities.iter().any(|i| i.surface.as_str() == SVC))
    );

    let list::Ok { found } = client
        .persons()
        .list()
        .call(list::Args {
            whitelisted: Some(true),
            ..Default::default()
        })
        .await
        .expect("whitelisted");
    let ids: BTreeSet<_> = found.iter().map(|p| p.id.as_str()).collect();
    assert_eq!(
        ids,
        BTreeSet::from(["p0", me.id.as_str(), created[0].as_str()]),
        "master counts as whitelisted"
    );

    let list::Ok { found } = client
        .persons()
        .list()
        .call(list::Args {
            whitelisted: Some(false),
            ..Default::default()
        })
        .await
        .expect("not whitelisted");
    assert_eq!(found.len(), 10);

    let res = client
        .persons()
        .list()
        .call(list::Args {
            after: Some(PersonId::from("bob")),
            ..Default::default()
        })
        .await;
    assert_eq!(outcome(&res), Outcome::Invalid, "{res:?}");
}

#[tokio::test]
async fn rename_and_its_errors() {
    let h = start().await;
    let mut svc = h.service_with_surface(SVC).await;
    let mut client = h.client_session().await;
    let alice = resolve(&mut svc, resolve_args(SVC, "1")).await;

    let rename::Ok { person } = client
        .persons()
        .rename()
        .call(rename::Args {
            person: alice.id.clone(),
            name: String::from("Alice"),
        })
        .await
        .expect("rename");
    assert_eq!(person.name, "Alice");
    assert_eq!(person.identities, alice.identities);

    let res = client
        .persons()
        .rename()
        .call(rename::Args {
            person: PersonId::system(),
            name: String::from("Kernel"),
        })
        .await;
    assert!(
        matches!(&res, Err(Generic::Spec(rename::Err::System(_)))),
        "{res:?}"
    );
    let res = client
        .persons()
        .rename()
        .call(rename::Args {
            person: PersonId::from("p99"),
            name: String::from("Ghost"),
        })
        .await;
    assert!(
        matches!(&res, Err(Generic::Spec(rename::Err::NotFound(e))) if e.person.as_str() == "p99"),
        "{res:?}"
    );
    let res = client
        .persons()
        .rename()
        .call(rename::Args {
            person: alice.id.clone(),
            name: String::from("Two\nlines"),
        })
        .await;
    assert_eq!(outcome(&res), Outcome::Invalid, "{res:?}");
}

#[tokio::test]
async fn whitelist_and_its_errors() {
    let h = start().await;
    let mut svc = h.service_with_surface(SVC).await;
    let mut client = h.client_session().await;
    let me = master(&mut client).await;
    let alice = resolve(&mut svc, resolve_args(SVC, "1")).await;

    for on in [true, false] {
        let whitelist::Ok { person } = client
            .persons()
            .whitelist()
            .call(whitelist::Args {
                person: alice.id.clone(),
                on,
            })
            .await
            .expect("whitelist");
        assert_eq!(person.whitelisted, on);
    }

    let res = client
        .persons()
        .whitelist()
        .call(whitelist::Args {
            person: me.id.clone(),
            on: false,
        })
        .await;
    assert!(
        matches!(&res, Err(Generic::Spec(whitelist::Err::MasterWhitelist(e))) if e.person == me.id),
        "{res:?}"
    );
    let whitelist::Ok { person } = client
        .persons()
        .whitelist()
        .call(whitelist::Args {
            person: me.id.clone(),
            on: true,
        })
        .await
        .expect("master stays whitelisted");
    assert!(person.whitelisted);

    let res = client
        .persons()
        .whitelist()
        .call(whitelist::Args {
            person: PersonId::system(),
            on: false,
        })
        .await;
    assert!(
        matches!(&res, Err(Generic::Spec(whitelist::Err::System(_)))),
        "{res:?}"
    );
    let res = client
        .persons()
        .whitelist()
        .call(whitelist::Args {
            person: PersonId::from("p99"),
            on: true,
        })
        .await;
    assert!(
        matches!(&res, Err(Generic::Spec(whitelist::Err::NotFound(_)))),
        "{res:?}"
    );
}

/// Строки всех таблиц, ссылающихся на `person` (`PERSON_REFS`).
async fn reference_everywhere(h: &Harness, chat: i64, person: PersonId) {
    h.kernel
        .shared()
        .db()
        .write(move |tx| {
            let p = person.as_str();
            let segment: i64 = tx.query_row(
                "INSERT INTO segments (chat_id, seq, started_at, reason, system_snapshot, tools_snapshot, hot_memory_ids)
                 VALUES (?1, 1, 1, 'initial', x'', x'', x'') RETURNING id",
                [chat],
                |r| r.get(0),
            )?;
            let event: i64 = tx.query_row(
                "INSERT INTO events (chat_id, segment_id, seq, kind, payload, created_at)
                 VALUES (?1, ?2, 1, 'message', x'80', 1) RETURNING id",
                (chat, segment),
                |r| r.get(0),
            )?;
            tx.execute(
                "INSERT INTO messages (chat_id, seq, event_id, role, sender_person_id) VALUES (?1, 1, ?2, 'user', ?3)",
                (chat, event, p),
            )?;
            let memory: i64 = tx.query_row(
                "INSERT INTO memory (kind, text, visibility, source_actor, created_at, updated_at, valid_from, last_reinforced_at)
                 VALUES ('fact', 'likes tea', 'public', ?1, 1, 1, 1, 1) RETURNING id",
                [p],
                |r| r.get(0),
            )?;
            tx.execute(
                "INSERT INTO memory_archive (memory_id, reason, why, actor, archived_at) VALUES (?1, 'forget', 'asked', ?2, 1)",
                (memory, p),
            )?;
            tx.execute(
                "INSERT INTO module_tokens (token_hash, module, issued_by, issued_at) VALUES (x'01', 'm', ?1, 1)",
                [p],
            )?;
            tx.execute(
                "INSERT INTO hands_grants (endpoint, surface, chat_id, kind, prefix, issued_by, created_at)
                 VALUES ('local', 'local', NULL, 'read', '/', ?1, 1)",
                [p],
            )?;
            Ok::<_, eva_kernel::db::DbError>(())
        })
        .await
        .expect("seed references");
}

#[tokio::test]
async fn merge_moves_every_reference() {
    let h = start().await;
    let mut svc = h.service_with_surface(SVC).await;
    let mut client = h.client_session().await;
    let from = resolve(&mut svc, resolve_args(SVC, "a")).await;
    let into = resolve(&mut svc, resolve_args(SVC, "b")).await;
    client
        .persons()
        .whitelist()
        .call(whitelist::Args {
            person: from.id.clone(),
            on: true,
        })
        .await
        .expect("whitelist from");

    let chats::create::Ok { chat } = client
        .chats()
        .create()
        .call(chats::create::Args {
            surface: SurfaceId::from(SVC),
            folder: None,
            external_id: None,
            external_kind: None,
            kind: None,
            title: None,
            project: None,
            workdir: None,
            workdir_on: None,
        })
        .await
        .expect("chat");
    reference_everywhere(&h, chat.id.0, from.id.clone()).await;

    let merge::Ok { person } = client
        .persons()
        .merge()
        .call(merge::Args {
            from: from.id.clone(),
            into: into.id.clone(),
            confirm: false,
        })
        .await
        .expect("merge");
    assert_eq!(person.id, into.id);
    assert!(person.whitelisted, "whitelisted is OR");
    assert!(!person.master);
    let externals: BTreeSet<_> = person
        .identities
        .iter()
        .map(|i| i.external_id.as_str())
        .collect();
    assert_eq!(externals, BTreeSet::from(["a", "b"]));

    let from_id = from.id.clone();
    let (gone, leftovers, broken) = h
        .query(move |c| {
            let gone: bool = c.query_row(
                "SELECT NOT EXISTS (SELECT 1 FROM persons WHERE id = ?1)",
                [from_id.as_str()],
                |r| r.get(0),
            )?;
            let mut leftovers = 0_i64;
            for (table, column) in PERSON_REFS {
                leftovers += c.query_row(
                    &format!(
                        "SELECT COUNT(*) FROM {table} WHERE {column} = ?1 OR {column} IS NULL"
                    ),
                    [from_id.as_str()],
                    |r| r.get::<_, i64>(0),
                )?;
            }
            let broken = c
                .prepare("PRAGMA foreign_key_check")?
                .query_map([], |_| Ok(()))?
                .count();
            Ok((gone, leftovers, broken))
        })
        .await;
    assert!(gone, "from is deleted");
    assert_eq!(leftovers, 0, "every reference points to into");
    assert_eq!(broken, 0, "foreign_key_check is clean");

    let res = client
        .persons()
        .resolve()
        .call(resolve_args(SVC, "a"))
        .await
        .expect("resolve merged address");
    assert_eq!(res.person.id, into.id);
}

#[tokio::test]
async fn person_refs_cover_every_foreign_key() {
    let h = start().await;
    let refs: BTreeSet<(String, String)> = h
        .query(|c| {
            let tables: Vec<String> = c
                .prepare("SELECT name FROM sqlite_master WHERE type = 'table'")?
                .query_map([], |r| r.get(0))?
                .collect::<Result<_, _>>()?;
            let mut refs = BTreeSet::new();
            for table in tables {
                let mut stmt = c.prepare(&format!(
                    "SELECT \"table\", \"from\" FROM pragma_foreign_key_list('{table}')"
                ))?;
                let rows: Vec<(String, String)> = stmt
                    .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                    .collect::<Result<_, _>>()?;
                for (target, column) in rows {
                    if target == "persons" && table != "identities" {
                        refs.insert((table.clone(), column));
                    }
                }
            }
            Ok(refs)
        })
        .await;
    let declared: BTreeSet<(String, String)> = PERSON_REFS
        .iter()
        .map(|(t, c)| ((*t).to_owned(), (*c).to_owned()))
        .collect();
    assert_eq!(refs, declared);
}

#[tokio::test]
async fn merge_refusals() {
    let h = start().await;
    let mut svc = h.service_with_surface(SVC).await;
    let mut client = h.client_session().await;
    let me = master(&mut client).await;
    let alice = resolve(&mut svc, resolve_args(SVC, "1")).await;

    let call = |from: &PersonId, into: &PersonId, confirm| merge::Args {
        from: from.clone(),
        into: into.clone(),
        confirm,
    };

    let res = client
        .persons()
        .merge()
        .call(call(&alice.id, &alice.id, true))
        .await;
    assert!(
        matches!(&res, Err(Generic::Spec(merge::Err::SelfMerge(_)))),
        "{res:?}"
    );

    let res = client
        .persons()
        .merge()
        .call(call(&PersonId::system(), &alice.id, true))
        .await;
    assert!(
        matches!(&res, Err(Generic::Spec(merge::Err::System(_)))),
        "{res:?}"
    );
    let res = client
        .persons()
        .merge()
        .call(call(&alice.id, &PersonId::system(), true))
        .await;
    assert!(
        matches!(&res, Err(Generic::Spec(merge::Err::System(_)))),
        "{res:?}"
    );

    let ghost = PersonId::from("p99");
    let res = client
        .persons()
        .merge()
        .call(call(&ghost, &alice.id, true))
        .await;
    assert!(
        matches!(&res, Err(Generic::Spec(merge::Err::NotFound(e))) if e.person == ghost),
        "{res:?}"
    );
    let res = client
        .persons()
        .merge()
        .call(call(&alice.id, &ghost, true))
        .await;
    assert!(
        matches!(&res, Err(Generic::Spec(merge::Err::NotFound(e))) if e.person == ghost),
        "{res:?}"
    );

    // Чужой в хозяина без confirm — отказ, ничего не тронуто.
    let res = client
        .persons()
        .merge()
        .call(call(&alice.id, &me.id, false))
        .await;
    assert!(
        matches!(&res, Err(Generic::Spec(merge::Err::MasterMerge(e))) if e.from == alice.id && e.into == me.id),
        "{res:?}"
    );
    let still = resolve(&mut svc, resolve_args(SVC, "1")).await;
    assert_eq!(still.id, alice.id);

    let merge::Ok { person } = client
        .persons()
        .merge()
        .call(call(&alice.id, &me.id, true))
        .await
        .expect("confirmed merge");
    assert_eq!(person.id, me.id);
    assert!(person.master);
    assert_eq!(person.identities.len(), 2);
}

/// `Service` не отличает чужой surface от несуществующего: иначе
/// `persons.resolve` выдавал бы, какие surface есть у хозяина.
#[tokio::test]
async fn service_cannot_probe_surfaces() {
    let h = start().await;
    let mut svc = h.service_with_surface(SVC).await;
    h.service_with_surface(OTHER).await;

    let mut refusals = Vec::new();
    for surface in [OTHER, "local", "nowhere"] {
        let res = svc
            .persons()
            .resolve()
            .call(resolve_args(surface, "1"))
            .await;
        assert_eq!(outcome(&res), Outcome::Forbidden, "{surface}: {res:?}");
        refusals.push(format!("{res:?}").replace(surface, "_"));
    }
    refusals.dedup();
    assert_eq!(refusals.len(), 1, "one refusal text: {refusals:?}");
}

#[tokio::test]
async fn database_failure_is_aux_db() {
    let h = start().await;
    let mut client = h.client_session().await;
    h.db_fail.store(true, std::sync::atomic::Ordering::Relaxed);
    let res = client
        .persons()
        .resolve()
        .call(resolve_args("local", MASTER))
        .await;
    assert_eq!(outcome(&res), Outcome::Db, "{res:?}");
    let res = client.persons().list().call(list::Args::default()).await;
    assert_eq!(outcome(&res), Outcome::Db, "{res:?}");
}

/// Адрес и имена личности от сервиса ограничены: их пишет мост мессенджера.
#[tokio::test]
async fn oversized_identity_strings_are_invalid() {
    let h = common::start().await;
    let mut svc = h.service_with_surface(SVC).await;
    let huge = "x".repeat(100_000);

    let cases = [
        ("external_id", resolve_args(SVC, &huge)),
        (
            "name",
            resolve::Args {
                name: Some(huge.clone()),
                ..resolve_args(SVC, "u1")
            },
        ),
        (
            "handle",
            resolve::Args {
                handle: Some(huge.clone()),
                ..resolve_args(SVC, "u1")
            },
        ),
    ];
    for (what, args) in cases {
        let res = svc.persons().resolve().call(args).await;
        assert_eq!(outcome(&res), Outcome::Invalid, "resolve {what}: {res:?}");
    }

    let person = resolve(
        &mut svc,
        resolve::Args {
            name: Some("и".repeat(256)),
            ..resolve_args(SVC, "u1")
        },
    )
    .await;
    assert_eq!(person.name.chars().count(), 256);
}
