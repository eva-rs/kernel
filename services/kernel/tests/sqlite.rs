//! `sqlite.*` (`06 §5`, I12): файл модуля, `ATTACH` запрещён, перебор строк — `quota`.

mod common;

use std::time::Duration;

use common::{OTHER, Outcome, SVC, TestSession, matrix_callers, outcome, service};
use eva::time::HumanDuration;
use eva_core::{CallStep as _, service::sqlite::Sqlite as _};
use eva_protocol::{
    errors::{Aux, Generic},
    requests::sqlite::{Statement, exec, query, tx},
};
use serde_json::{Value, json};

fn stmt(sql: &str, params: Vec<Value>) -> Statement {
    Statement {
        sql: sql.to_owned(),
        params,
    }
}

async fn run(
    session: &mut TestSession,
    sql: &str,
    params: Vec<Value>,
) -> Result<u64, Generic<exec::Err>> {
    session
        .sqlite()
        .exec()
        .call(exec::Args {
            statement: stmt(sql, params),
        })
        .await
        .map(|ok| ok.changes)
}

async fn select(
    session: &mut TestSession,
    sql: &str,
    params: Vec<Value>,
) -> Result<query::Ok, Generic<query::Err>> {
    session
        .sqlite()
        .query()
        .call(query::Args {
            statement: stmt(sql, params),
        })
        .await
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
                json!(1),
                json!(1),
                json!(1.5),
                json!("hi"),
                json!(2),
                json!([0, 255])
            ],
            vec![
                json!(2),
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null
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
    let query::Ok { rows, .. } = select(&mut svc, "PRAGMA journal_mode", vec![])
        .await
        .expect("journal mode");
    assert_eq!(rows, [vec![json!("wal")]]);
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
        .tx()
        .call(tx::Args {
            statements: vec![
                stmt("INSERT INTO t VALUES ('a'), ('b')", vec![]),
                stmt("DELETE FROM t WHERE k = ?1", vec![json!("a")]),
                stmt("SELECT * FROM t", vec![]),
            ],
        })
        .await
        .expect("tx");
    assert_eq!(changes, [2, 1, 0]);

    let res = svc
        .sqlite()
        .tx()
        .call(tx::Args {
            statements: vec![
                stmt("INSERT INTO t VALUES ('c')", vec![]),
                stmt("INSERT INTO t VALUES ('b')", vec![]),
            ],
        })
        .await;
    assert!(
        matches!(&res, Err(Generic::Spec(tx::Err::Sql(e))) if e.statement == 1),
        "unique violation in the second statement: {res:?}"
    );
    let query::Ok { rows, .. } = select(&mut svc, "SELECT k FROM t ORDER BY k", vec![])
        .await
        .expect("query");
    assert_eq!(rows, [vec![json!("b")]], "the first insert rolled back");
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
        .tx()
        .call(tx::Args {
            statements: vec![stmt("CREATE TABLE t (x)", vec![]), stmt(&attach, vec![])],
        })
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
            .tx()
            .call(tx::Args {
                statements: vec![stmt("INSERT INTO t VALUES (1)", vec![])],
            })
            .await;
        assert_eq!(outcome(&res), expect, "{who} tx");
    }
}
