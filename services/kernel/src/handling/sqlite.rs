//! Хендлеры `sqlite.*` (`06 §5`, I12): файл
//! `<state_dir>/modules/<module>/state.sqlite`, WAL, `busy_timeout` из
//! `modules.sqlite_busy_timeout`. Соединение — на вызов; `ATTACH` и выход
//! за файл запрещены, перебор строк — `Aux::Quota`.

use std::{path::PathBuf, time::Duration};

use eva_protocol::{
    errors::{self, Aux, AuxResult, Generic, ext::*},
    requests::{
        Response,
        sqlite::{Statement, exec, query, tx},
    },
};
use rusqlite::{
    Connection, ErrorCode, TransactionBehavior,
    limits::Limit,
    params_from_iter,
    types::{Value as SqlValue, ValueRef},
};
use serde_json::{Number, Value};

use crate::{
    components::Components,
    handling::fs::module_dir,
    state::{Policy, State},
};

/// Что нужно блокирующей части: файл, таймауты, потолок строк.
struct Target {
    dir: PathBuf,
    busy: Duration,
    deadline: Duration,
    max_rows: usize,
}

fn target<C: Components>(state: &State<C>) -> AuxResult<Target> {
    let module = Policy::new(&state.caller).module()?;
    let cfg = state.shared.cfg();
    Ok(Target {
        dir: module_dir(&cfg.system.state_dir, module)?,
        busy: *cfg.modules.sqlite_busy_timeout,
        deadline: *cfg.modules.transport.request_timeout,
        max_rows: usize::try_from(cfg.modules.sqlite_max_rows).unwrap_or(usize::MAX),
    })
}

/// Отказ блокирующей части: номер оператора для `Spec(Sql)` либо `Aux`.
enum Fail {
    Sql { statement: u32, message: String },
    Aux(Aux),
}

impl From<Aux> for Fail {
    fn from(aux: Aux) -> Self {
        Self::Aux(aux)
    }
}

impl Fail {
    /// Беда файла — `Aux`, ошибка оператора модуля — его `Spec(Sql)`.
    fn sqlite(statement: u32, e: rusqlite::Error) -> Self {
        let code = match &e {
            rusqlite::Error::SqliteFailure(f, _) => Some(f.code),
            _ => None,
        };
        match code {
            Some(ErrorCode::OperationInterrupted) => {
                Self::Aux(Aux::Quota("statement exceeded the request timeout".into()))
            }
            Some(ErrorCode::DiskFull) => Self::Aux(Aux::Quota(format!("module sqlite: {e}"))),
            Some(
                ErrorCode::DatabaseBusy
                | ErrorCode::DatabaseLocked
                | ErrorCode::SystemIoFailure
                | ErrorCode::CannotOpen
                | ErrorCode::DatabaseCorrupt
                | ErrorCode::NotADatabase
                | ErrorCode::OutOfMemory
                | ErrorCode::PermissionDenied,
            ) => Self::Aux(Aux::db(format_args!("module sqlite: {e}"))),
            _ => Self::Sql {
                statement,
                message: e.to_string(),
            },
        }
    }

    fn into_generic<E: From<errors::sqlite::Sql>>(self) -> Generic<E> {
        match self {
            Self::Sql { statement, message } => errors::sqlite::Sql { statement, message }.spec(),
            Self::Aux(aux) => aux.into(),
        }
    }
}

type SqlResult<T> = Result<T, Fail>;

fn open(target: &Target) -> SqlResult<Connection> {
    let io = |e: std::io::Error| Aux::db(format_args!("module sqlite: {e}"));
    std::fs::create_dir_all(&target.dir).map_err(io)?;
    let conn = Connection::open(target.dir.join("state.sqlite")).map_err(|e| Fail::sqlite(0, e))?;
    let setup = || -> rusqlite::Result<()> {
        conn.busy_timeout(target.busy)?;
        conn.pragma_update_and_check(None, "journal_mode", "WAL", |_| Ok(()))?;
        conn.pragma_update(None, "trusted_schema", false)?;
        // Лимит запрещает ATTACH, в т.ч. внутренний у VACUUM.
        conn.set_limit(Limit::SQLITE_LIMIT_ATTACHED, 0).map(drop)
    };
    setup().map_err(|e| Fail::Aux(Aux::db(format_args!("module sqlite: {e}"))))?;
    Ok(conn)
}

/// Первое ключевое слово оператора после пробелов и комментариев.
fn keyword(sql: &str) -> String {
    let mut rest = sql;
    loop {
        rest = rest.trim_start();
        if let Some(tail) = rest.strip_prefix("--") {
            rest = tail.split_once('\n').map_or("", |(_, t)| t);
        } else if let Some(tail) = rest.strip_prefix("/*") {
            rest = tail.split_once("*/").map_or("", |(_, t)| t);
        } else {
            break;
        }
    }
    rest.chars()
        .take_while(|c| c.is_ascii_alphabetic())
        .collect::<String>()
        .to_ascii_uppercase()
}

/// `ATTACH`/`VACUUM` открыли бы файл вне каталога модуля; транзакциями управляет `sqlite.tx`.
fn check(statement: u32, sql: &str) -> SqlResult<()> {
    match keyword(sql).as_str() {
        "ATTACH" | "DETACH" | "VACUUM" => Err(Aux::forbidden(format_args!(
            "statement {statement}: ATTACH, DETACH and VACUUM are forbidden"
        ))
        .into()),
        "BEGIN" | "COMMIT" | "END" | "ROLLBACK" | "SAVEPOINT" | "RELEASE" => Err(Aux::Invalid(
            format!("statement {statement}: transactions are sqlite.tx, not SQL"),
        )
        .into()),
        _ => Ok(()),
    }
}

fn param(value: &Value) -> SqlValue {
    match value {
        Value::Null => SqlValue::Null,
        Value::Bool(b) => SqlValue::Integer(i64::from(*b)),
        Value::Number(n) => n.as_i64().map_or_else(
            || SqlValue::Real(n.as_f64().unwrap_or(f64::NAN)),
            SqlValue::Integer,
        ),
        Value::String(s) => SqlValue::Text(s.clone()),
        // Массив и объект хранятся JSON-текстом: их читают json_* функции SQLite.
        Value::Array(_) | Value::Object(_) => SqlValue::Text(value.to_string()),
    }
}

fn column(value: ValueRef<'_>) -> Value {
    match value {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(i) => Value::from(i),
        ValueRef::Real(f) => Number::from_f64(f).map_or(Value::Null, Value::Number),
        ValueRef::Text(t) => Value::String(String::from_utf8_lossy(t).into_owned()),
        ValueRef::Blob(b) => Value::Array(b.iter().map(|&x| Value::from(x)).collect()),
    }
}

/// Один оператор без выдачи строк: `changes` для DML, 0 для чтения.
fn run(conn: &Connection, statement: u32, stmt: &Statement) -> SqlResult<u64> {
    check(statement, &stmt.sql)?;
    let sql = |e| Fail::sqlite(statement, e);
    let mut prepared = conn.prepare(&stmt.sql).map_err(sql)?;
    let params = params_from_iter(stmt.params.iter().map(param));
    if prepared.column_count() == 0 {
        let changes = prepared.execute(params).map_err(sql)?;
        return Ok(changes as u64);
    }
    let readonly = prepared.readonly();
    let mut rows = prepared.query(params).map_err(sql)?;
    while rows.next().map_err(sql)?.is_some() {}
    Ok(if readonly { 0 } else { conn.changes() })
}

/// Блокирующая часть в `spawn_blocking`; по истечении `deadline` оператор прерывается.
async fn blocking<T: Send + 'static>(
    target: Target,
    f: impl FnOnce(&mut Connection, usize) -> SqlResult<T> + Send + 'static,
) -> SqlResult<T> {
    let joined = |e: tokio::task::JoinError| Aux::internal(format_args!("sqlite task failed: {e}"));
    let (deadline, max_rows) = (target.deadline, target.max_rows);
    let mut conn = tokio::task::spawn_blocking(move || open(&target))
        .await
        .map_err(joined)??;
    let interrupt = conn.get_interrupt_handle();
    let mut task = tokio::task::spawn_blocking(move || f(&mut conn, max_rows));
    match tokio::time::timeout(deadline, &mut task).await {
        Ok(res) => res.map_err(joined)?,
        Err(_) => {
            interrupt.interrupt();
            task.await.map_err(joined)?
        }
    }
}

#[eva::endpoint]
#[fastrace::trace]
pub async fn Exec<C: Components>(
    state: &mut State<C>,
    args: exec::Args,
) -> Response<exec::Ok, exec::Err> {
    let target = target(state)?;
    let exec::Args { statement } = args;

    blocking(target, move |conn, _| run(conn, 0, &statement))
        .await
        .map(|changes| exec::Ok { changes })
        .map_err(Fail::into_generic)
}

#[eva::endpoint]
#[fastrace::trace]
pub async fn Query<C: Components>(
    state: &mut State<C>,
    args: query::Args,
) -> Response<query::Ok, query::Err> {
    let target = target(state)?;
    let query::Args { statement } = args;

    blocking(target, move |conn, max_rows| {
        check(0, &statement.sql)?;
        let sql = |e| Fail::sqlite(0, e);
        let mut prepared = conn.prepare(&statement.sql).map_err(sql)?;
        let columns: Vec<String> = prepared
            .column_names()
            .into_iter()
            .map(str::to_owned)
            .collect();
        let mut rows = prepared
            .query(params_from_iter(statement.params.iter().map(param)))
            .map_err(sql)?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().map_err(sql)? {
            (out.len() < max_rows).true_or(Aux::Quota(format!(
                "query returns more than {max_rows} rows"
            )))?;
            let values = (0..columns.len())
                .map(|i| row.get_ref(i).map(column))
                .collect::<Result<_, _>>()
                .map_err(sql)?;
            out.push(values);
        }
        Ok(query::Ok { columns, rows: out })
    })
    .await
    .map_err(Fail::into_generic)
}

#[eva::endpoint]
#[fastrace::trace]
pub async fn Tx<C: Components>(state: &mut State<C>, args: tx::Args) -> Response<tx::Ok, tx::Err> {
    let target = target(state)?;
    let tx::Args { statements } = args;

    blocking(target, move |conn, _| {
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|e| Fail::sqlite(0, e))?;
        let mut changes = Vec::with_capacity(statements.len());
        for (i, stmt) in statements.iter().enumerate() {
            let i = u32::try_from(i).unwrap_or(u32::MAX);
            changes.push(run(&tx, i, stmt)?);
        }
        tx.commit().map_err(|e| Fail::sqlite(0, e))?;
        Ok(tx::Ok { changes })
    })
    .await
    .map_err(Fail::into_generic)
}
