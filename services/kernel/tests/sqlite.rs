//! `sqlite.*` (`06 §5`, I12): файл модуля, `ATTACH` запрещён, перебор строк
//! и байтов — `quota`, `query` только читает, отмена прерывает оператор.

mod common;

use std::time::{Duration, Instant};

use common::{Harness, OTHER, Outcome, SVC, TestSession, matrix_callers, outcome, service};
use eva::{bytesize::ByteSize, time::HumanDuration};

use eva_kernel::handling::sqlite::{Limits, open};
use eva_protocol::{
    errors::{Aux, Generic},
    requests::sqlite::{Cell, Statement, exec, query, tx},
    types::{Bytes, Value as Param},
};
use rusqlite::limits::Limit;
use serde_json::{Value, json};

/// Бесконечный счёт: оператор держит поток и замок записи, пока его не прервут.
const RUNAWAY: &str = "INSERT INTO t SELECT count(*) FROM
    (WITH RECURSIVE r(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM r WHERE x < 300000000) SELECT x FROM r)";

fn is_quota<T: std::fmt::Debug, E: std::fmt::Debug>(res: &Result<T, Generic<E>>) -> bool {
    matches!(res, Err(Generic::Aux(Aux::Quota(_))))
}

fn text(s: &str) -> Cell {
    Cell::Text(s.to_owned())
}

fn stmt(sql: &str, params: Vec<Value>) -> Statement {
    Statement {
        sql: sql.to_owned(),
        params: params.into_iter().map(Param::from).collect(),
    }
}

async fn run(
    session: &mut TestSession,
    sql: &str,
    params: Vec<Value>,
) -> Result<u64, Generic<exec::Err>> {
    session
        .sqlite()
        .exec(stmt(sql, params))
        .await
        .map(|ok| ok.changes)
}

async fn select(
    session: &mut TestSession,
    sql: &str,
    params: Vec<Value>,
) -> Result<query::Ok, Generic<query::Err>> {
    session.sqlite().query(stmt(sql, params)).await
}

#[tokio::test]
async fn exec_and_query_roundtrip_values() {
    let h = common::start().await;
    let mut svc = h.session(service(SVC)).await;

    let changes = run(
        &mut svc,
        "CREATE TABLE t (id INTEGER PRIMARY KEY, n, f, s, j, b)",
        vec![],
    )
    .await
    .expect("create");
    assert_eq!(changes, 0);
    let changes = run(
        &mut svc,
        "INSERT INTO t (n, f, s, j, b) VALUES (?1, ?2, ?3, ?4, x'00ff')",
        vec![json!(true), json!(1.5), json!("hi"), json!({"a": [1, 2]})],
    )
    .await
    .expect("insert");
    assert_eq!(changes, 1);
    run(&mut svc, "INSERT INTO t (n) VALUES (?1)", vec![Value::Null])
        .await
        .expect("insert null");

    let query::Ok { columns, rows } = select(
        &mut svc,
        "SELECT id, n, f, s, json_extract(j, '$.a[1]') AS j, b FROM t ORDER BY id",
        vec![],
    )
    .await
    .expect("query");
    assert_eq!(columns, ["id", "n", "f", "s", "j", "b"]);
    assert_eq!(
        rows,
        [
            vec![
                Cell::Integer(1),
                Cell::Integer(1),
                Cell::Real(1.5),
                Cell::Text("hi".into()),
                Cell::Integer(2),
                Cell::Blob(Bytes::from([0, 255])),
            ],
            vec![
                Cell::Integer(2),
                Cell::Null,
                Cell::Null,
                Cell::Null,
                Cell::Null,
                Cell::Null,
            ],
        ]
    );

    let changes = run(&mut svc, "UPDATE t SET n = 7", vec![])
        .await
        .expect("update");
    assert_eq!(changes, 2);
    let changes = run(&mut svc, "SELECT * FROM t", vec![])
        .await
        .expect("select via exec");
    assert_eq!(changes, 0, "reads change nothing");

    let path = h.dir.path().join("modules").join(SVC).join("state.sqlite");
    assert!(path.exists(), "{}", path.display());
    let file = rusqlite::Connection::open(&path).expect("open the module file");
    let mode: String = file
        .query_row("PRAGMA journal_mode", [], |r| r.get(0))
        .expect("journal mode");
    assert_eq!(mode, "wal");
}

async fn exec_with(
    session: &mut TestSession,
    sql: &str,
    params: Vec<Param>,
) -> Result<u64, Generic<exec::Err>> {
    let statement = Statement {
        sql: sql.to_owned(),
        params,
    };
    session.sqlite().exec(statement).await.map(|ok| ok.changes)
}

/// BLOB из выдачи `query` пишется обратно параметром и остаётся BLOB, даже
/// если его байты — валидный UTF-8.
#[tokio::test]
async fn blob_params_roundtrip() {
    let h = common::start().await;
    let mut svc = h.session(service(SVC)).await;
    run(
        &mut svc,
        "CREATE TABLE b (id INTEGER PRIMARY KEY, v)",
        vec![],
    )
    .await
    .expect("create");
    let insert = "INSERT INTO b (v) VALUES (?1)";
    let raw = Bytes::from(*b"valid utf-8, still a blob");

    exec_with(&mut svc, insert, vec![Param::Bytes(raw.clone())])
        .await
        .expect("insert a blob");
    let query::Ok { rows, .. } = select(&mut svc, "SELECT v, typeof(v) FROM b", vec![])
        .await
        .expect("read");
    assert_eq!(rows, [vec![Cell::Blob(raw.clone()), text("blob")]]);

    let Cell::Blob(read) = rows[0][0].clone() else {
        panic!("blob expected: {rows:?}");
    };
    exec_with(&mut svc, insert, vec![Param::Bytes(read)])
        .await
        .expect("write the read blob back");
    let query::Ok { rows, .. } = svc
        .sqlite()
        .query(Statement {
            sql: "SELECT count(*) FROM b WHERE v = ?1 AND typeof(v) = 'blob'".into(),
            params: vec![Param::Bytes(raw)],
        })
        .await
        .expect("count");
    assert_eq!(rows, [vec![Cell::Integer(2)]]);

    // Внутри массива байты — base64 JSON-текста; ключ-массив в JSON невыразим.
    exec_with(
        &mut svc,
        insert,
        vec![Param::Array(vec![Param::Bytes(Bytes::from(*b"foo"))])],
    )
    .await
    .expect("array param");
    let query::Ok { rows, .. } = select(&mut svc, "SELECT v FROM b WHERE id = 3", vec![])
        .await
        .expect("json text");
    assert_eq!(rows, [vec![text(r#"["Zm9v"]"#)]]);
    let res = exec_with(
        &mut svc,
        insert,
        vec![Param::Map(vec![(Param::Array(vec![]), Param::Null)])],
    )
    .await;
    assert_eq!(outcome(&res), Outcome::Invalid, "{res:?}");
}

#[tokio::test]
async fn sql_errors_are_spec() {
    let h = common::start().await;
    let mut svc = h.session(service(SVC)).await;

    let res = run(&mut svc, "CREATE TABLEE nope", vec![]).await;
    assert!(
        matches!(&res, Err(Generic::Spec(exec::Err::Sql(e))) if e.statement == 0),
        "{res:?}"
    );
    let res = select(&mut svc, "SELECT * FROM missing", vec![]).await;
    assert!(
        matches!(&res, Err(Generic::Spec(query::Err::Sql(_)))),
        "{res:?}"
    );
    let res = run(&mut svc, "SELECT ?1, ?2", vec![json!(1)]).await;
    assert!(
        matches!(&res, Err(Generic::Spec(exec::Err::Sql(_)))),
        "param count: {res:?}"
    );
    let res = run(&mut svc, "SELECT 1; SELECT 2", vec![]).await;
    assert!(
        matches!(&res, Err(Generic::Spec(exec::Err::Sql(_)))),
        "one statement per call: {res:?}"
    );
}

#[tokio::test]
async fn tx_is_all_or_nothing() {
    let h = common::start().await;
    let mut svc = h.session(service(SVC)).await;
    run(&mut svc, "CREATE TABLE t (k TEXT PRIMARY KEY)", vec![])
        .await
        .expect("create");

    let tx::Ok { changes } = svc
        .sqlite()
        .tx(vec![
            stmt("INSERT INTO t VALUES ('a'), ('b')", vec![]),
            stmt("DELETE FROM t WHERE k = ?1", vec![json!("a")]),
            stmt("SELECT * FROM t", vec![]),
        ])
        .await
        .expect("tx");
    assert_eq!(changes, [2, 1, 0]);

    let res = svc
        .sqlite()
        .tx(vec![
            stmt("INSERT INTO t VALUES ('c')", vec![]),
            stmt("INSERT INTO t VALUES ('b')", vec![]),
        ])
        .await;
    assert!(
        matches!(&res, Err(Generic::Spec(tx::Err::Sql(e))) if e.statement == 1),
        "unique violation in the second statement: {res:?}"
    );
    let query::Ok { rows, .. } = select(&mut svc, "SELECT k FROM t ORDER BY k", vec![])
        .await
        .expect("query");
    assert_eq!(rows, [vec![text("b")]], "the first insert rolled back");
}

#[tokio::test]
async fn attach_and_transaction_control_are_refused() {
    let h = common::start().await;
    let mut svc = h.session(service(SVC)).await;
    let outside = h.dir.path().join("eva.sqlite");
    let attach = format!("ATTACH DATABASE '{}' AS k", outside.display());

    for sql in [
        attach.clone(),
        format!(
            "  -- sneaky\n/* comment */ attach '{}' AS k",
            outside.display()
        ),
        String::from("DETACH k"),
        format!(
            "VACUUM INTO '{}'",
            h.dir.path().join("copy.sqlite").display()
        ),
    ] {
        let res = run(&mut svc, &sql, vec![]).await;
        assert_eq!(outcome(&res), Outcome::Forbidden, "{sql}: {res:?}");
    }
    assert!(!h.dir.path().join("copy.sqlite").exists());

    let res = svc
        .sqlite()
        .tx(vec![
            stmt("CREATE TABLE t (x)", vec![]),
            stmt(&attach, vec![]),
        ])
        .await;
    assert_eq!(outcome(&res), Outcome::Forbidden, "{res:?}");
    let res = select(&mut svc, "SELECT * FROM t", vec![]).await;
    assert!(
        matches!(&res, Err(Generic::Spec(_))),
        "tx rolled back: {res:?}"
    );

    for sql in [
        "BEGIN",
        "COMMIT",
        "ROLLBACK",
        "SAVEPOINT s",
        "RELEASE s",
        "END",
    ] {
        let res = run(&mut svc, sql, vec![]).await;
        assert_eq!(outcome(&res), Outcome::Invalid, "{sql}: {res:?}");
    }
}

#[tokio::test]
async fn query_row_cap_is_quota() {
    let h = common::start_with(|cfg| cfg.modules.sqlite_max_rows = 3).await;
    let mut svc = h.session(service(SVC)).await;
    let cte = "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c WHERE x < ?1) SELECT x FROM c";

    let query::Ok { rows, .. } = select(&mut svc, cte, vec![json!(3)]).await.expect("3 rows");
    assert_eq!(rows.len(), 3);
    let res = select(&mut svc, cte, vec![json!(4)]).await;
    assert!(matches!(res, Err(Generic::Aux(Aux::Quota(_)))), "{res:?}");
}

#[tokio::test]
async fn runaway_statement_is_interrupted() {
    let h = common::start_with(|cfg| {
        cfg.modules.transport.request_timeout = HumanDuration(Duration::from_millis(200));
    })
    .await;
    let mut svc = h.session(service(SVC)).await;

    let res = select(
        &mut svc,
        "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c) SELECT count(*) FROM c",
        vec![],
    )
    .await;
    assert!(matches!(res, Err(Generic::Aux(Aux::Quota(_)))), "{res:?}");
}

#[tokio::test]
async fn modules_do_not_see_each_other() {
    let h = common::start().await;
    let mut svc = h.session(service(SVC)).await;
    let mut other = h.session(service(OTHER)).await;

    run(&mut svc, "CREATE TABLE secrets (v)", vec![])
        .await
        .expect("create");
    run(&mut svc, "INSERT INTO secrets VALUES ('svc')", vec![])
        .await
        .expect("insert");
    let res = select(&mut other, "SELECT * FROM secrets", vec![]).await;
    assert!(
        matches!(&res, Err(Generic::Spec(query::Err::Sql(_)))),
        "{res:?}"
    );
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
        let res = run(&mut session, "CREATE TABLE IF NOT EXISTS t (x)", vec![]).await;
        assert_eq!(outcome(&res), expect, "{who} exec");
        let res = select(&mut session, "SELECT 1", vec![]).await;
        assert_eq!(outcome(&res), expect, "{who} query");
        let res = session
            .sqlite()
            .tx(vec![stmt("INSERT INTO t VALUES (1)", vec![])])
            .await;
        assert_eq!(outcome(&res), expect, "{who} tx");
    }
}

#[tokio::test]
async fn query_takes_only_reads() {
    let h = common::start().await;
    let mut svc = h.session(service(SVC)).await;
    run(&mut svc, "CREATE TABLE t (k TEXT PRIMARY KEY)", vec![])
        .await
        .expect("create");
    run(&mut svc, "INSERT INTO t VALUES ('a'), ('b'), ('c')", vec![])
        .await
        .expect("insert");

    for sql in [
        "DELETE FROM t RETURNING k",
        "INSERT INTO t VALUES ('z') RETURNING k",
        "UPDATE t SET k = k || 'x' RETURNING k",
        "CREATE TABLE u (x)",
        "PRAGMA user_version = 3",
    ] {
        let res = select(&mut svc, sql, vec![]).await;
        assert_eq!(outcome(&res), Outcome::Invalid, "{sql}: {res:?}");
    }

    let query::Ok { rows, .. } = select(&mut svc, "SELECT k FROM t ORDER BY k", vec![])
        .await
        .expect("rows");
    assert_eq!(rows, [[text("a")], [text("b")], [text("c")]]);
    let query::Ok { rows, .. } = select(&mut svc, "PRAGMA user_version", vec![])
        .await
        .expect("header");
    assert_eq!(
        rows,
        [[Cell::Integer(0)]],
        "refused statements changed nothing"
    );
    let res = select(&mut svc, "SELECT * FROM u", vec![]).await;
    assert!(matches!(res, Err(Generic::Spec(_))), "{res:?}");
}

#[tokio::test]
async fn attach_is_refused_by_the_connection_limit_too() {
    let h = common::start().await;
    let mut svc = h.session(service(SVC)).await;
    let outside = h.dir.path().join("eva.sqlite");

    for sql in [
        format!("; ATTACH DATABASE '{}' AS k", outside.display()),
        format!(";\n-- x\n;/* y */ attach '{}' AS k", outside.display()),
        format!("EXPLAIN ATTACH '{}' AS k", outside.display()),
    ] {
        let res = run(&mut svc, &sql, vec![]).await;
        assert_eq!(outcome(&res), Outcome::Forbidden, "{sql}: {res:?}");
        let res = svc.sqlite().tx(vec![stmt(&sql, vec![])]).await;
        assert_eq!(outcome(&res), Outcome::Forbidden, "tx {sql}: {res:?}");
    }
    let query::Ok { rows, .. } = select(&mut svc, "SELECT name FROM pragma_database_list", vec![])
        .await
        .expect("database list");
    assert!(
        rows.iter()
            .all(|r| r[0] == text("main") || r[0] == text("temp")),
        "{rows:?}"
    );

    // Лимит соединения держит и без разбора ключевого слова.
    let dir = h.dir.path().join("modules").join(SVC);
    let conn = open(&dir, &Limits::new(&h.kernel.shared().cfg().modules)).expect("connection");
    assert_eq!(conn.limit(Limit::SQLITE_LIMIT_ATTACHED).expect("limit"), 0);
    let res = conn.execute_batch(&format!("ATTACH DATABASE '{}' AS k", outside.display()));
    assert!(res.is_err(), "{res:?}");
}

#[tokio::test]
async fn query_result_is_bounded_in_bytes() {
    let h = common::start_with(|cfg| cfg.modules.transport.max_frame = ByteSize::kib(64)).await;
    let mut svc = h.session(service(SVC)).await;
    let max = usize::try_from(h.kernel.shared().cfg().modules.max_payload()).expect("usize");

    // BLOB идёт `bin`: полбюджета помещается, в массиве чисел оно бы удвоилось.
    let query::Ok { rows, .. } = select(&mut svc, "SELECT zeroblob(?1)", vec![json!(max / 2 + 16)])
        .await
        .expect("a blob that fits");
    assert_eq!(rows[0][0], Cell::Blob(Bytes(vec![0; max / 2 + 16])));

    let res = select(&mut svc, "SELECT zeroblob(?1)", vec![json!(max - 16)]).await;
    assert!(
        is_quota(&res),
        "the encoded blob outgrows a response: {res:?}"
    );
    let res = select(&mut svc, "SELECT zeroblob(?1)", vec![json!(max + 1)]).await;
    assert!(is_quota(&res), "past SQLITE_LIMIT_LENGTH: {res:?}");
    let res = select(
        &mut svc,
        "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c WHERE x < 100)
         SELECT printf('%.*c', 1000, 'x') FROM c",
        vec![],
    )
    .await;
    assert!(is_quota(&res), "rows add up: {res:?}");
    let res = run(
        &mut svc,
        "CREATE TABLE t AS SELECT zeroblob(?1) AS b",
        vec![json!(max + 1)],
    )
    .await;
    assert!(
        is_quota(&res),
        "a value past a response is not stored: {res:?}"
    );
}

/// Заголовки строк тоже весят: тысячи `NULL` не выходят за ответ.
#[tokio::test]
async fn row_headers_count_toward_the_result() {
    let h = common::start_with(|cfg| cfg.modules.transport.max_frame = ByteSize::kib(16)).await;
    let mut svc = h.session(service(SVC)).await;
    let max = usize::try_from(h.kernel.shared().cfg().modules.max_payload()).expect("usize");

    for n in [1_000, 10_000] {
        let res = select(
            &mut svc,
            "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c WHERE x < ?1)
             SELECT NULL FROM c",
            vec![json!(n)],
        )
        .await;
        match &res {
            Ok(ok) => {
                let size = rmp_serde::to_vec_named(ok).expect("encode").len();
                assert!(size <= max, "{n} rows: {size} bytes over {max}");
            }
            Err(_) => assert!(is_quota(&res), "{n} rows: {res:?}"),
        }
    }
}

#[tokio::test]
async fn huge_value_is_quota_not_an_allocation() {
    let h = common::start().await;
    let mut svc = h.session(service(SVC)).await;

    let started = Instant::now();
    let res = select(&mut svc, "SELECT zeroblob(999999999)", vec![]).await;
    assert!(is_quota(&res), "{res:?}");
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[tokio::test]
async fn file_and_temp_are_capped_by_the_quota() {
    let h = common::start_with(|cfg| cfg.modules.sqlite_quota = ByteSize::kib(256)).await;
    let mut svc = h.session(service(SVC)).await;
    run(&mut svc, "CREATE TABLE t (b)", vec![])
        .await
        .expect("create");
    run(&mut svc, "INSERT INTO t VALUES (randomblob(50000))", vec![])
        .await
        .expect("fits");

    let res = run(
        &mut svc,
        "INSERT INTO t VALUES (randomblob(400000))",
        vec![],
    )
    .await;
    assert!(is_quota(&res), "{res:?}");
    let res = svc
        .sqlite()
        .tx(vec![stmt(
            "CREATE TEMP TABLE big AS SELECT randomblob(400000) AS b",
            vec![],
        )])
        .await;
    assert!(is_quota(&res), "temp: {res:?}");
    let file = h.dir.path().join("modules").join(SVC).join("state.sqlite");
    let size = std::fs::metadata(file).expect("module file").len();
    assert!(size <= 256 * 1024, "{size}");
}

#[tokio::test]
async fn pragmas_outside_the_list_are_forbidden() {
    let h = common::start().await;
    let mut svc = h.session(service(SVC)).await;
    run(&mut svc, "CREATE TABLE t (x)", vec![])
        .await
        .expect("create");

    for sql in [
        "PRAGMA max_page_count = 1000000000",
        "pragma MAX_PAGE_COUNT(1000000000)",
        "PRAGMA main.max_page_count = 1000000000",
        "PRAGMA \"max_page_count\" = 1000000000",
        "PRAGMA 'max_page_count' = 1000000000",
        "PRAGMA [main] . [max_page_count]=1000000000",
        "PRAGMA /* x */ max_page_count -- y\n = 1000000000",
        "; PRAGMA max_page_count = 1000000000",
        "EXPLAIN PRAGMA temp_store_directory = '/tmp'",
        "EXPLAIN QUERY PLAN PRAGMA soft_heap_limit = 1",
        "PRAGMA cache_size = -1000000",
        "PRAGMA busy_timeout = 100000000",
        "PRAGMA temp_store_directory",
        "PRAGMA writable_schema = 1",
        "PRAGMA hard_heap_limit = 1",
        "PRAGMA journal_mode = DELETE",
        "PRAGMA",
    ] {
        let res = run(&mut svc, sql, vec![]).await;
        assert_eq!(outcome(&res), Outcome::Forbidden, "exec {sql}: {res:?}");
        let res = select(&mut svc, sql, vec![]).await;
        assert_eq!(outcome(&res), Outcome::Forbidden, "query {sql}: {res:?}");
        let res = svc.sqlite().tx(vec![stmt(sql, vec![])]).await;
        assert_eq!(outcome(&res), Outcome::Forbidden, "tx {sql}: {res:?}");
    }

    run(&mut svc, "PRAGMA user_version = 7", vec![])
        .await
        .expect("own header field");
    let query::Ok { rows, .. } = select(&mut svc, "PRAGMA user_version", vec![])
        .await
        .expect("read back");
    assert_eq!(rows, [[Cell::Integer(7)]]);
    for sql in [
        "PRAGMA max_page_count",
        "PRAGMA table_info(t)",
        "SELECT name FROM pragma_table_info('t')",
        "EXPLAIN QUERY PLAN SELECT * FROM t",
    ] {
        select(&mut svc, sql, vec![])
            .await
            .unwrap_or_else(|e| panic!("{sql}: {e:?}"));
    }
}

/// Запись в файл модуля удаётся в пределах `within`.
async fn writes_resume(svc: &mut TestSession, within: Duration) -> bool {
    let until = Instant::now() + within;
    while Instant::now() < until {
        if run(svc, "INSERT INTO t VALUES (0)", vec![]).await.is_ok() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    false
}

fn runaway_call(mut session: TestSession, in_tx: bool) -> tokio::task::JoinHandle<bool> {
    tokio::spawn(async move {
        if in_tx {
            session
                .sqlite()
                .tx(vec![stmt(RUNAWAY, vec![])])
                .await
                .is_ok()
        } else {
            run(&mut session, RUNAWAY, vec![]).await.is_ok()
        }
    })
}

/// Беглый оператор, который уже держит замок записи: тишина перед пробой
/// не даёт ему проиграть замок самой пробе.
async fn locked_by_runaway(
    h: &Harness,
    svc: &mut TestSession,
    in_tx: bool,
) -> tokio::task::JoinHandle<bool> {
    for _ in 0..10 {
        let call = runaway_call(h.session(service(SVC)).await, in_tx);
        tokio::time::sleep(Duration::from_millis(300)).await;
        if !call.is_finished() && run(svc, "INSERT INTO t VALUES (0)", vec![]).await.is_err() {
            return call;
        }
        call.abort();
        let _ = call.await;
    }
    panic!("the runaway never took the write lock (tx: {in_tx})");
}

#[tokio::test]
async fn cancelled_call_interrupts_its_statement() {
    let h = common::start_with(|cfg| {
        cfg.modules.sqlite_busy_timeout = HumanDuration(Duration::from_millis(100));
    })
    .await;
    let mut svc = h.session(service(SVC)).await;
    run(&mut svc, "CREATE TABLE t (x)", vec![])
        .await
        .expect("create");

    for in_tx in [false, true] {
        let call = locked_by_runaway(&h, &mut svc, in_tx).await;
        call.abort();
        assert!(
            writes_resume(&mut svc, Duration::from_secs(5)).await,
            "cancel stopped the statement and freed the file (tx: {in_tx})"
        );
    }
}

/// Сортировка и временные b-деревья пишут во временные файлы мимо
/// `max_page_count`: квота держит и их, иначе один `query` займёт весь диск.
#[tokio::test]
async fn temp_files_of_a_call_are_capped_by_the_quota() {
    let h = common::start_with(|cfg| cfg.modules.sqlite_quota = ByteSize::mib(1)).await;
    let mut svc = h.session(service(SVC)).await;
    let rows = |n: u32, tail: &str| {
        format!(
            "WITH RECURSIVE r(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM r WHERE x < {n})
             SELECT count(*) FROM (SELECT x, randomblob(1000) AS b FROM r {tail})"
        )
    };

    for tail in [
        "ORDER BY b",
        "GROUP BY b",
        "UNION SELECT x, randomblob(1000) FROM r",
    ] {
        let res = select(&mut svc, &rows(20_000, tail), vec![]).await;
        assert!(is_quota(&res), "{tail}: {res:?}");
    }
    let small = select(&mut svc, &rows(200, "ORDER BY b"), vec![])
        .await
        .expect("a sort within the quota");
    assert_eq!(small.rows, [[Cell::Integer(200)]]);
}

/// Бюджет — на живые временные файлы: закрытые возвращают его, а файл,
/// открытый вне бюджета, не растёт вовсе.
#[tokio::test]
async fn temp_budget_follows_open_files() {
    use eva_kernel::db::quota_vfs::{self, TempBudget};

    let h = common::start().await;
    let dir = h.dir.path().join("modules").join(SVC);
    let limits = Limits::new(&h.kernel.shared().cfg().modules);
    const SORT: &str =
        "WITH RECURSIVE r(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM r WHERE x < 5000)
        SELECT x, randomblob(1000) AS b FROM r ORDER BY b";
    let first_row = |conn: &rusqlite::Connection| -> rusqlite::Result<i64> {
        conn.query_row(SORT, [], |r| r.get(0))
    };

    let budget = TempBudget::new(64 * 1024 * 1024);
    let peak = {
        let _charging = quota_vfs::charge_to(budget.clone());
        let conn = open(&dir, &limits).expect("connection");
        let mut stmt = conn.prepare(SORT).expect("prepare");
        let mut rows = stmt.query([]).expect("query");
        rows.next().expect("sorted").expect("a row");
        budget.used()
    };
    assert!(peak > 0, "the sort spilled to temp files");
    assert_eq!(budget.used(), 0, "closed files give the budget back");

    let conn = open(&dir, &limits).expect("connection");
    let res = first_row(&conn);
    assert!(
        matches!(&res, Err(rusqlite::Error::SqliteFailure(e, _)) if e.code == rusqlite::ErrorCode::DiskFull),
        "{res:?}"
    );
}
