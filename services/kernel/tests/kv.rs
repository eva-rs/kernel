//! `kv.*` (`06 §5`, I12): KV модуля, last-write-wins, скоуп — имя модуля;
//! значение и всё KV модуля — под квотами `modules.kv_*`.

mod common;

use std::sync::atomic::Ordering;

use common::{
    OTHER, Outcome, SVC, TestSession, matrix_callers, outcome, service, token, with_module_tokens,
};
use eva::bytesize::ByteSize;
use eva_core::{CallStep as _, service::kv::Kv as _};
use eva_protocol::{
    errors::{Aux, Generic},
    requests::{
        Limit,
        kv::{del, get, list, set},
    },
    types::{Bytes, Value},
};
use eva_sdk::{Module, SecretString};
use serde_json::json;

async fn put(session: &mut TestSession, key: &str, value: impl Into<Value>) {
    session
        .kv()
        .set()
        .call(set::Args {
            key: key.to_owned(),
            value: value.into(),
        })
        .await
        .expect("kv.set");
}

async fn read(session: &mut TestSession, key: &str) -> Option<Value> {
    session
        .kv()
        .get()
        .call(get::Args {
            key: key.to_owned(),
        })
        .await
        .expect("kv.get")
        .value
}

async fn keys(session: &mut TestSession, args: list::Args) -> Vec<String> {
    session.kv().list().call(args).await.expect("kv.list").keys
}

#[tokio::test]
async fn set_get_del_last_write_wins() {
    let h = common::start().await;
    let mut svc = h.session(service(SVC)).await;

    assert_eq!(read(&mut svc, "offset").await, None);
    let doc = json!({"n": 1, "tags": ["a", "b"], "nested": {"f": 1.5, "t": true, "z": null}});
    put(&mut svc, "offset", doc.clone()).await;
    assert_eq!(read(&mut svc, "offset").await, Some(doc.into()));

    put(&mut svc, "offset", 42_i64).await;
    assert_eq!(read(&mut svc, "offset").await, Some(Value::Integer(42)));

    svc.kv()
        .del()
        .call(del::Args {
            key: String::from("offset"),
        })
        .await
        .expect("del");
    assert_eq!(read(&mut svc, "offset").await, None);

    svc.kv()
        .del()
        .call(del::Args {
            key: String::from("offset"),
        })
        .await
        .expect("deleting a missing key is not an error");
}

/// Байты — msgpack `bin` насквозь: записанное в процессе читается по UDS
/// через SDK и наоборот, без base64 и без потери формы.
#[tokio::test]
async fn bytes_roundtrip_in_process_and_over_uds() {
    let value = Value::Map(vec![
        (
            Value::from("raw"),
            Value::Bytes(Bytes::from([0, 0xc1, 0xff, b'a'])),
        ),
        (
            Value::Integer(-7),
            Value::Array(vec![
                Value::Bytes(Bytes::from(*b"valid utf-8")),
                Value::Unsigned(u64::MAX),
                Value::Float(-0.5),
                Value::Null,
                Value::Bool(true),
            ]),
        ),
    ]);
    let h = common::start_with(with_module_tokens).await;
    let mut svc = h.session(service(SVC)).await;
    put(&mut svc, "near", value.clone()).await;
    assert_eq!(read(&mut svc, "near").await, Some(value.clone()));

    let (_listening, url) = h.listen().await;
    let module = Module::new()
        .connect(&url, &SecretString::Plaintext(token(SVC)))
        .await
        .expect("module connects");
    let mut kernel = module.kernel();
    let far = kernel
        .kv()
        .get()
        .call(get::Args {
            key: String::from("near"),
        })
        .await
        .expect("kv.get over uds");
    assert_eq!(far.value, Some(value.clone()));

    kernel
        .kv()
        .set()
        .call(set::Args {
            key: String::from("far"),
            value: value.clone(),
        })
        .await
        .expect("kv.set over uds");
    assert_eq!(read(&mut svc, "far").await, Some(value));
}

#[tokio::test]
async fn modules_do_not_see_each_other() {
    let h = common::start().await;
    let mut svc = h.session(service(SVC)).await;
    let mut other = h.session(service(OTHER)).await;
    let mut client = h.client_session().await;

    put(&mut svc, "k", "svc").await;
    put(&mut client, "k", "client").await;

    assert_eq!(read(&mut other, "k").await, None);
    assert_eq!(read(&mut svc, "k").await, Some("svc".into()));
    assert_eq!(read(&mut client, "k").await, Some("client".into()));
    assert!(keys(&mut other, list::Args::default()).await.is_empty());

    other
        .kv()
        .del()
        .call(del::Args {
            key: String::from("k"),
        })
        .await
        .expect("del");
    assert_eq!(read(&mut svc, "k").await, Some("svc".into()));
}

#[tokio::test]
async fn list_by_prefix_with_keyset_pages() {
    let h = common::start().await;
    let mut svc = h.session(service(SVC)).await;
    for key in [
        "chat:3", "chat:1", "chat:2", "chat%x", "chat_y", "user:1", "chat:4",
    ] {
        put(&mut svc, key, key).await;
    }

    let limit = Limit::new(2).expect("limit");
    let page = |after: Option<&str>| list::Args {
        prefix: String::from("chat:"),
        limit,
        after: after.map(str::to_owned),
    };
    assert_eq!(keys(&mut svc, page(None)).await, ["chat:1", "chat:2"]);
    assert_eq!(
        keys(&mut svc, page(Some("chat:2"))).await,
        ["chat:3", "chat:4"]
    );
    let last = keys(&mut svc, page(Some("chat:4"))).await;
    assert!(last.is_empty(), "end of the prefix: {last:?}");

    let all = keys(&mut svc, list::Args::default()).await;
    assert_eq!(
        all,
        [
            "chat%x", "chat:1", "chat:2", "chat:3", "chat:4", "chat_y", "user:1"
        ],
        "ascending by key"
    );

    let wild = keys(
        &mut svc,
        list::Args {
            prefix: String::from("chat%"),
            ..Default::default()
        },
    )
    .await;
    assert_eq!(wild, ["chat%x"], "% is not a wildcard");
    let wild = keys(
        &mut svc,
        list::Args {
            prefix: String::from("chat_"),
            ..Default::default()
        },
    )
    .await;
    assert_eq!(wild, ["chat_y"], "_ is not a wildcard");
}

#[tokio::test]
async fn key_is_validated() {
    let h = common::start().await;
    let mut svc = h.session(service(SVC)).await;

    for key in [String::new(), "k".repeat(1025)] {
        let res = svc.kv().get().call(get::Args { key: key.clone() }).await;
        assert_eq!(outcome(&res), Outcome::Invalid, "get {}", key.len());
        let res = svc
            .kv()
            .set()
            .call(set::Args {
                key: key.clone(),
                value: Value::Integer(1),
            })
            .await;
        assert_eq!(outcome(&res), Outcome::Invalid, "set {}", key.len());
        let res = svc.kv().del().call(del::Args { key }).await;
        assert_eq!(outcome(&res), Outcome::Invalid, "del");
    }
}

#[tokio::test]
async fn policy_matrix() {
    let h = common::start().await;

    for (who, caller) in matrix_callers() {
        let expect = if caller.is_kernel() {
            Outcome::Forbidden
        } else {
            Outcome::Ok
        };
        let mut session = h.session(caller).await;
        let key = String::from("k");

        let res = session
            .kv()
            .set()
            .call(set::Args {
                key: key.clone(),
                value: Value::Integer(1),
            })
            .await;
        assert_eq!(outcome(&res), expect, "{who} set");
        let res = session
            .kv()
            .get()
            .call(get::Args { key: key.clone() })
            .await;
        assert_eq!(outcome(&res), expect, "{who} get");
        let res = session.kv().list().call(list::Args::default()).await;
        assert_eq!(outcome(&res), expect, "{who} list");
        let res = session.kv().del().call(del::Args { key }).await;
        assert_eq!(outcome(&res), expect, "{who} del");
    }
}

#[tokio::test]
async fn database_failure_is_aux_db() {
    let h = common::start().await;
    let mut svc = h.session(service(SVC)).await;
    h.db_fail.store(true, Ordering::Relaxed);

    let res = svc
        .kv()
        .set()
        .call(set::Args {
            key: String::from("k"),
            value: Value::Integer(1),
        })
        .await;
    assert!(matches!(res, Err(Generic::Aux(Aux::Db(_)))), "{res:?}");
    let res = svc
        .kv()
        .get()
        .call(get::Args {
            key: String::from("k"),
        })
        .await;
    assert!(matches!(res, Err(Generic::Aux(Aux::Db(_)))), "{res:?}");
}

async fn try_put(
    session: &mut TestSession,
    key: &str,
    value: Value,
) -> Result<set::Ok, Generic<set::Err>> {
    session
        .kv()
        .set()
        .call(set::Args {
            key: key.to_owned(),
            value,
        })
        .await
}

/// Строка в 269 байт квоты: ключ 3, msgpack-строка 202, накладные 64.
fn row_269() -> Value {
    Value::String("x".repeat(200))
}

#[tokio::test]
async fn value_size_is_capped() {
    let h = common::start_with(|cfg| cfg.modules.kv_value_max = ByteSize::b(100)).await;
    let mut svc = h.session(service(SVC)).await;

    put(&mut svc, "small", "x".repeat(90)).await;
    let res = try_put(&mut svc, "big", "x".repeat(200).into()).await;
    assert!(matches!(res, Err(Generic::Aux(Aux::Quota(_)))), "{res:?}");
    assert_eq!(read(&mut svc, "big").await, None);
}

#[tokio::test]
async fn module_total_is_capped() {
    let h = common::start_with(|cfg| cfg.modules.kv_quota = ByteSize::b(1000)).await;
    let mut svc = h.session(service(SVC)).await;
    let mut other = h.session(service(OTHER)).await;

    for key in ["k00", "k01", "k02"] {
        put(&mut svc, key, row_269()).await;
    }
    let res = try_put(&mut svc, "k03", row_269()).await;
    assert!(matches!(res, Err(Generic::Aux(Aux::Quota(_)))), "{res:?}");
    put(&mut svc, "k00", row_269()).await;

    for key in ["k00", "k01", "k02"] {
        put(&mut other, key, row_269()).await;
    }

    svc.kv()
        .del()
        .call(del::Args {
            key: String::from("k00"),
        })
        .await
        .expect("del");
    put(&mut svc, "k03", row_269()).await;
}

#[tokio::test]
async fn concurrent_sets_cannot_overshoot() {
    let h = common::start_with(|cfg| cfg.modules.kv_quota = ByteSize::b(1000)).await;
    let mut sessions = Vec::new();
    for _ in 0..8 {
        sessions.push(h.session(service(SVC)).await);
    }

    let sets = sessions
        .iter_mut()
        .enumerate()
        .map(|(i, session)| async move { try_put(session, &format!("k{i:02}"), row_269()).await });
    let results = futures::future::join_all(sets).await;

    assert_eq!(
        results.iter().filter(|r| r.is_ok()).count(),
        3,
        "{results:?}"
    );
    assert!(
        results
            .iter()
            .all(|r| r.is_ok() || matches!(r, Err(Generic::Aux(Aux::Quota(_))))),
        "{results:?}"
    );
}

mod usage {
    //! Счётчик `module_kv_usage` (миграция `0002`): квота `kv.set` читает
    //! его точечно, триггеры держат его равным обходу ключей модуля.

    use std::{num::NonZeroU64, path::Path, time::Duration};

    use eva::time::Timestamp;
    use eva_kernel::db::{
        Db as _, DbResult, Queries as _, Sqlite,
        kv::{ROW_OVERHEAD, USAGE, row_cost},
        migrate::{self, MIGRATIONS},
    };
    use eva_protocol::types::ModuleId;
    use proptest::prelude::*;
    use rusqlite::{Connection, OptionalExtension as _};

    const MODULES: [&str; 2] = ["svc", "other"];
    /// Многобайтные ключи: цена строки — байты, а не символы.
    const KEYS: [&str; 5] = ["a", "ключ", "日本", "k-3", "zz"];

    #[derive(Debug, Clone)]
    enum Op {
        Set {
            module: usize,
            key: usize,
            len: usize,
        },
        Del {
            module: usize,
            key: usize,
        },
    }

    fn op() -> impl Strategy<Value = Op> {
        prop_oneof![
            (0..MODULES.len(), 0..KEYS.len(), 0..300_usize)
                .prop_map(|(module, key, len)| Op::Set { module, key, len }),
            (0..MODULES.len(), 0..KEYS.len()).prop_map(|(module, key)| Op::Del { module, key }),
        ]
    }

    fn now() -> Timestamp {
        Timestamp::from_nanos(NonZeroU64::new(1_735_689_600_000_000_000).expect("non-zero"))
    }

    fn migrated() -> (tokio::runtime::Runtime, Sqlite) {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let db = rt.block_on(async {
            let db = Sqlite::open(Path::new(":memory:"), Duration::from_secs(5), 1)
                .await
                .expect("open");
            db.exclusive(|c| migrate::run(c, MIGRATIONS, now()))
                .await
                .expect("migrate");
            db
        });
        (rt, db)
    }

    fn counted(c: &Connection, module: &str) -> DbResult<u64> {
        let bytes: Option<i64> = c
            .query_row(
                "SELECT bytes FROM module_kv_usage WHERE module = ?1",
                [module],
                |r| r.get(0),
            )
            .optional()?;
        Ok(u64::try_from(bytes.unwrap_or(0)).expect("non-negative"))
    }

    fn scanned(c: &Connection, module: &str) -> DbResult<u64> {
        let bytes: i64 = c.query_row(
            "SELECT COALESCE(SUM(octet_length(key) + length(value) + ?2), 0) FROM module_kv WHERE module = ?1",
            (module, ROW_OVERHEAD as i64),
            |r| r.get(0),
        )?;
        Ok(u64::try_from(bytes).expect("non-negative"))
    }

    /// Счётчик, обход и ожидаемое по модулю после набора операций.
    #[derive(Debug)]
    struct Seen {
        module: &'static str,
        counted: u64,
        scanned: u64,
        expected: u64,
        /// Чтение квоты без строки `KEYS[0]` плюс её цена.
        quota_read: u64,
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]

        #[test]
        fn the_counter_equals_the_scan(ops in proptest::collection::vec(op(), 1..60)) {
            let (rt, db) = migrated();
            let seen = rt.block_on(db.write(move |tx| -> DbResult<Vec<Seen>> {
                let mut values = std::collections::BTreeMap::new();
                for op in &ops {
                    match *op {
                        Op::Set { module, key, len } => {
                            let value = vec![7_u8; len];
                            tx.kv().set(&ModuleId::from(MODULES[module]), KEYS[key], &value, now())?;
                            values.insert((module, key), value);
                        }
                        Op::Del { module, key } => {
                            tx.kv().del(&ModuleId::from(MODULES[module]), KEYS[key])?;
                            values.remove(&(module, key));
                        }
                    }
                }
                let mut seen = Vec::new();
                for (m, module) in MODULES.into_iter().enumerate() {
                    let expected = values
                        .iter()
                        .filter(|((vm, _), _)| *vm == m)
                        .map(|((_, k), v)| row_cost(KEYS[*k], v))
                        .sum();
                    let first = values.get(&(m, 0)).map_or(0, |v| row_cost(KEYS[0], v));
                    seen.push(Seen {
                        module,
                        counted: counted(tx, module)?,
                        scanned: scanned(tx, module)?,
                        expected,
                        quota_read: tx.kv().usage(&ModuleId::from(module), KEYS[0])? + first,
                    });
                }
                Ok(seen)
            })).expect("ops apply");
            for s in seen {
                prop_assert_eq!(s.counted, s.scanned, "{}: counter vs scan: {:?}", s.module, s);
                prop_assert_eq!(s.counted, s.expected, "counter vs model: {:?}", s);
                prop_assert_eq!(s.quota_read, s.expected, "quota read: {:?}", s);
            }
        }
    }

    /// Квота читает две строки по полному ключу, а не обходит ключи модуля.
    #[test]
    fn the_quota_read_is_a_point_lookup() {
        let (rt, db) = migrated();
        let plan = rt
            .block_on(db.exclusive(|c| -> DbResult<Vec<String>> {
                let mut stmt = c.prepare(&format!("EXPLAIN QUERY PLAN {USAGE}"))?;
                let rows = stmt
                    .query_map(("svc", "a", ROW_OVERHEAD as i64), |r| r.get::<_, String>(3))?
                    .collect::<Result<_, _>>()?;
                Ok(rows)
            }))
            .expect("plan");

        let lookups: Vec<&String> = plan.iter().filter(|d| d.starts_with("SEARCH")).collect();
        assert_eq!(lookups.len(), 2, "{plan:#?}");
        assert!(
            !plan
                .iter()
                .any(|d| d.starts_with("SCAN") && d != "SCAN CONSTANT ROW"),
            "{plan:#?}"
        );
        for detail in lookups {
            let point = if detail.contains("module_kv_usage") {
                detail.contains("(module=?)")
            } else {
                detail.contains("(module=? AND key=?)")
            };
            assert!(point, "not a single-row lookup: {detail}");
        }
    }
}
