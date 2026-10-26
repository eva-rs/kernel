//! Хендлеры `sqlite.*` (`06 §5`, I12): файл
//! `<state_dir>/modules/<module>/state.sqlite`, WAL, `busy_timeout` из
//! `modules.sqlite_busy_timeout`; файл, temp-база и временные файлы вызова —
//! каждый до `modules.sqlite_quota`. Соединение — на вызов; `ATTACH`,
//! управление транзакциями и PRAGMA вне списка запрещены.
//!
//! `query` — чтение без окна идемпотентности (`11 §3`), поэтому пишущий
//! оператор в нём — `Aux::Invalid`: пишут `exec` и `tx`. Выдача `query`
//! ограничена `modules.sqlite_max_rows` строками и байтами одного ответа,
//! перебор — `Aux::Quota`. Отмена вызова и дедлайн `modules.request_timeout`
//! прерывают оператор: блокирующий поток не переживает вызов.

use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use eva_protocol::{
    errors::{self, Aux, AuxResult, Generic, ext::*},
    requests::{
        Cell, Response,
        Sqlite::{exec, query, tx},
        Statement,
    },
    types::{Bytes, Value},
};
use rusqlite::{
    Connection, ErrorCode, InterruptHandle, OpenFlags, TransactionBehavior,
    limits::Limit,
    params_from_iter,
    types::{Value as SqlValue, ValueRef},
};

use crate::{
    components::Components,
    config::ModulesConfig,
    db::quota_vfs::{self, TempBudget},
    handling::fs::module_dir,
    state::{Policy, State},
};

/// Лимиты соединения модуля из секции `modules`.
pub struct Limits {
    pub busy: Duration,
    /// Потолок одного значения и всей выдачи `query`.
    pub max_bytes: u64,
    /// Потолок файла, temp-базы и временных файлов вызова.
    pub quota: u64,
}

impl Limits {
    pub fn new(cfg: &ModulesConfig) -> Self {
        Self {
            busy: *cfg.sqlite_busy_timeout,
            max_bytes: cfg.max_payload(),
            quota: cfg.sqlite_quota.as_u64(),
        }
    }
}

/// Что нужно блокирующей части: файл, лимиты, дедлайн, потолок строк.
struct Target {
    dir: PathBuf,
    limits: Limits,
    deadline: Duration,
    max_rows: usize,
}

fn target<C: Components>(state: &State<C>) -> AuxResult<Target> {
    let module = Policy::new(&state.caller).module()?;
    let cfg = state.shared.cfg();
    Ok(Target {
        dir: module_dir(&cfg.system.state_dir, module)?,
        limits: Limits::new(&cfg.modules),
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
            Some(ErrorCode::OperationInterrupted) => Self::Aux(timed_out()),
            Some(ErrorCode::DiskFull | ErrorCode::TooBig) => {
                Self::Aux(Aux::Quota(format!("module sqlite: {e}")))
            }
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

fn timed_out() -> Aux {
    Aux::Quota("statement exceeded the request timeout".into())
}

/// Соединение с файлом модуля под его лимитами.
pub fn open(dir: &Path, limits: &Limits) -> AuxResult<Connection> {
    let db = |e: &dyn std::fmt::Display| Aux::db(format_args!("module sqlite: {e}"));
    std::fs::create_dir_all(dir).map_err(|e| db(&e))?;
    quota_vfs::register().map_err(|e| db(&e))?;
    let conn = Connection::open_with_flags_and_vfs(
        dir.join("state.sqlite"),
        OpenFlags::default(),
        quota_vfs::NAME,
    )
    .map_err(|e| db(&e))?;
    let setup = || -> rusqlite::Result<()> {
        conn.busy_timeout(limits.busy)?;
        conn.pragma_update_and_check(None, "journal_mode", "WAL", |_| Ok(()))?;
        conn.pragma_update(None, "trusted_schema", false)?;
        // Лимит запрещает ATTACH, в т.ч. внутренний у VACUUM.
        conn.set_limit(Limit::SQLITE_LIMIT_ATTACHED, 0)?;
        let length = i32::try_from(limits.max_bytes).unwrap_or(i32::MAX).max(1);
        conn.set_limit(Limit::SQLITE_LIMIT_LENGTH, length)?;
        let page: i64 = conn.pragma_query_value(None, "page_size", |row| row.get(0))?;
        // Ноль SQLite читает как «не менять»: квота меньше страницы — одна страница.
        let pages = (limits.quota / u64::try_from(page).unwrap_or(4096).max(512)).max(1);
        conn.execute_batch(&format!(
            "PRAGMA main.max_page_count = {pages}; PRAGMA temp.max_page_count = {pages};"
        ))
    };
    setup().map_err(|e| db(&e))?;
    Ok(conn)
}

/// Начало оператора, как его видит токенизатор SQLite: пробелы, комментарии
/// и (в начале оператора) пустые `;`.
fn skip(sql: &str, semicolons: bool) -> &str {
    let mut rest = sql;
    loop {
        rest =
            rest.trim_start_matches(|c: char| c.is_ascii_whitespace() || (semicolons && c == ';'));
        if let Some(tail) = rest.strip_prefix("--") {
            rest = tail.split_once('\n').map_or("", |(_, t)| t);
        } else if let Some(tail) = rest.strip_prefix("/*") {
            rest = tail.split_once("*/").map_or("", |(_, t)| t);
        } else {
            return rest;
        }
    }
}

/// Символ идентификатора SQLite: байты не-ASCII — тоже.
fn ident_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '$' || !c.is_ascii()
}

fn word(s: &str) -> (String, &str) {
    let end = s.find(|c| !ident_char(c)).unwrap_or(s.len());
    (s[..end].to_ascii_uppercase(), &s[end..])
}

/// Имя: голое либо в `"…"`, `` `…` ``, `'…'`, `[…]`; удвоенная кавычка — сама кавычка.
fn name(s: &str) -> Option<(String, &str)> {
    let close = match s.chars().next()? {
        '"' => '"',
        '`' => '`',
        '\'' => '\'',
        '[' => ']',
        _ => {
            let end = s.find(|c| !ident_char(c)).unwrap_or(s.len());
            return (end > 0).then(|| (s[..end].to_owned(), &s[end..]));
        }
    };
    let body = &s[1..];
    let mut out = String::new();
    let mut chars = body.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        let escaped = c == close && close != ']' && chars.next_if(|&(_, n)| n == close).is_some();
        if c == close && !escaped {
            return Some((out, &body[i + 1..]));
        }
        out.push(c);
    }
    None
}

/// Первое слово оператора и остаток. `EXPLAIN [QUERY PLAN]` снимается: PRAGMA
/// действует уже на `prepare`, объяснение её не обезвреживает.
fn head(sql: &str) -> (String, &str) {
    let (first, rest) = word(skip(sql, true));
    if first != "EXPLAIN" {
        return (first, rest);
    }
    let (next, rest) = word(skip(rest, false));
    if next != "QUERY" {
        return (next, rest);
    }
    let (_plan, rest) = word(skip(rest, false));
    word(skip(rest, false))
}

/// PRAGMA в любой форме: действуют на своё соединение или свой файл.
const PRAGMA_ANY: &[&str] = &[
    "analysis_limit",
    "application_id",
    "automatic_index",
    "cell_size_check",
    "collation_list",
    "compile_options",
    "database_list",
    "defer_foreign_keys",
    "foreign_key_check",
    "foreign_key_list",
    "foreign_keys",
    "function_list",
    "ignore_check_constraints",
    "incremental_vacuum",
    "index_info",
    "index_list",
    "index_xinfo",
    "integrity_check",
    "legacy_alter_table",
    "module_list",
    "optimize",
    "pragma_list",
    "query_only",
    "quick_check",
    "recursive_triggers",
    "reverse_unordered_selects",
    "secure_delete",
    "synchronous",
    "table_info",
    "table_list",
    "table_xinfo",
    "user_version",
    "wal_checkpoint",
];

/// PRAGMA только на чтение: запись сняла бы лимит вызова или задела весь процесс.
const PRAGMA_READ: &[&str] = &[
    "auto_vacuum",
    "busy_timeout",
    "cache_size",
    "data_version",
    "encoding",
    "freelist_count",
    "journal_mode",
    "journal_size_limit",
    "locking_mode",
    "max_page_count",
    "mmap_size",
    "page_count",
    "page_size",
    "schema_version",
    "temp_store",
    "trusted_schema",
    "wal_autocheckpoint",
];

fn pragma(statement: u32, rest: &str) -> SqlResult<()> {
    let refuse = |what: &str| {
        Fail::Aux(Aux::forbidden(format_args!(
            "statement {statement}: PRAGMA {what} is not allowed"
        )))
    };
    let unreadable = || refuse("without a readable name");
    let (first, rest) = name(skip(rest, false)).ok_or_else(unreadable)?;
    let rest = skip(rest, false);
    let (pragma, rest) = match rest.strip_prefix('.') {
        Some(tail) => {
            let (pragma, rest) = name(skip(tail, false)).ok_or_else(unreadable)?;
            (pragma, skip(rest, false))
        }
        None => (first, rest),
    };
    let pragma = pragma.to_ascii_lowercase();
    let sets = rest.starts_with(['=', '(']);
    if PRAGMA_ANY.contains(&pragma.as_str()) || (!sets && PRAGMA_READ.contains(&pragma.as_str())) {
        Ok(())
    } else {
        Err(refuse(&pragma))
    }
}

/// До `prepare`: часть PRAGMA срабатывает уже при компиляции оператора.
fn check(statement: u32, sql: &str) -> SqlResult<()> {
    let (first, rest) = head(sql);
    match first.as_str() {
        "ATTACH" | "DETACH" | "VACUUM" => Err(Aux::forbidden(format_args!(
            "statement {statement}: ATTACH, DETACH and VACUUM are forbidden"
        ))
        .into()),
        "BEGIN" | "COMMIT" | "END" | "ROLLBACK" | "SAVEPOINT" | "RELEASE" => Err(Aux::Invalid(
            format!("statement {statement}: transactions are sqlite.tx, not SQL"),
        )
        .into()),
        "PRAGMA" => pragma(statement, rest),
        _ => Ok(()),
    }
}

fn param(statement: u32, index: usize, value: &Value) -> SqlResult<SqlValue> {
    Ok(match value {
        Value::Null => SqlValue::Null,
        Value::Bool(b) => SqlValue::Integer(i64::from(*b)),
        Value::Integer(i) => SqlValue::Integer(*i),
        // Так же SQLite читает целый литерал сверх i64.
        Value::Unsigned(u) => SqlValue::Real(*u as f64),
        Value::Float(f) => SqlValue::Real(*f),
        Value::String(s) => SqlValue::Text(s.clone()),
        Value::Bytes(b) => SqlValue::Blob(b.to_vec()),
        // Массив и карта хранятся JSON-текстом: их читают json_* функции SQLite.
        Value::Array(_) | Value::Map(_) => {
            SqlValue::Text(serde_json::to_string(value).map_err(|e| {
                Aux::Invalid(format!("statement {statement}: param {}: {e}", index + 1))
            })?)
        }
    })
}

fn params(statement: u32, stmt: &Statement) -> SqlResult<Vec<SqlValue>> {
    stmt.params
        .iter()
        .enumerate()
        .map(|(i, value)| param(statement, i, value))
        .collect()
}

fn column(value: ValueRef<'_>) -> Cell {
    match value {
        ValueRef::Null => Cell::Null,
        ValueRef::Integer(i) => Cell::Integer(i),
        ValueRef::Real(f) => Cell::Real(f),
        ValueRef::Text(t) => Cell::Text(String::from_utf8_lossy(t).into_owned()),
        ValueRef::Blob(b) => Cell::Blob(Bytes::from(b)),
    }
}

/// Заголовок msgpack-массива сверху: строка выдачи и списки `columns`/`rows`.
const ARRAY: usize = 5;

/// Map `query::Ok` с ключами и заголовками обоих списков.
const SHELL: usize = 1 + "columns".len() + 1 + "rows".len() + 1 + 2 * ARRAY;

/// Оценка сверху msgpack-размера ячейки, до её построения.
fn cost(value: ValueRef<'_>) -> usize {
    match value {
        ValueRef::Null => 1,
        ValueRef::Integer(_) | ValueRef::Real(_) => 9,
        ValueRef::Text(t) | ValueRef::Blob(t) => t.len().saturating_add(5),
    }
}

/// Стоп вызова: блокирующая часть сверяется с ним между строками и
/// операторами, потому что `interrupt` действует только на идущий оператор.
struct Stop {
    deadline: Instant,
    stopped: AtomicBool,
    done: AtomicBool,
}

impl Stop {
    fn check(&self) -> SqlResult<()> {
        if self.stopped.load(Ordering::Acquire) || Instant::now() >= self.deadline {
            return Err(timed_out().into());
        }
        Ok(())
    }
}

/// Период повторного `interrupt`: одиночный, пришедший между операторами, SQLite забывает.
const INTERRUPT_EVERY: Duration = Duration::from_millis(10);

/// Дедлайн, когда `request_timeout` не складывается с `Instant`.
const FAR: Duration = Duration::from_secs(365 * 24 * 60 * 60);

/// Прерыватель: на drop — отмена вызова — и по дедлайну бьёт `interrupt`,
/// пока блокирующая часть не выйдет.
struct Interrupter {
    handle: Option<InterruptHandle>,
    stop: Arc<Stop>,
}

impl Interrupter {
    fn fire(&mut self) {
        self.stop.stopped.store(true, Ordering::Release);
        let Some(handle) = self.handle.take() else {
            return;
        };
        handle.interrupt();
        let Ok(rt) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let stop = self.stop.clone();
        rt.spawn(async move {
            while !stop.done.load(Ordering::Acquire) {
                handle.interrupt();
                tokio::time::sleep(INTERRUPT_EVERY).await;
            }
        });
    }
}

impl Drop for Interrupter {
    fn drop(&mut self) {
        if !self.stop.done.load(Ordering::Acquire) {
            self.fire();
        }
    }
}

/// Отметка конца блокирующей части; на drop — и при панике, и если поток не стартовал.
struct Done(Arc<Stop>);

impl Drop for Done {
    fn drop(&mut self) {
        self.0.done.store(true, Ordering::Release);
    }
}

/// Строки оператора с проверкой стопа между ними.
fn drain(rows: &mut rusqlite::Rows<'_>, stop: &Stop, statement: u32) -> SqlResult<()> {
    loop {
        stop.check()?;
        if rows
            .next()
            .map_err(|e| Fail::sqlite(statement, e))?
            .is_none()
        {
            return Ok(());
        }
    }
}

/// Один оператор без выдачи строк: `changes` для DML, 0 для чтения.
fn run(conn: &Connection, stop: &Stop, statement: u32, stmt: &Statement) -> SqlResult<u64> {
    check(statement, &stmt.sql)?;
    stop.check()?;
    let sql = |e| Fail::sqlite(statement, e);
    let bound = params_from_iter(params(statement, stmt)?);
    let mut prepared = conn.prepare(&stmt.sql).map_err(sql)?;
    if prepared.column_count() == 0 {
        let changes = prepared.execute(bound).map_err(sql)?;
        return Ok(changes as u64);
    }
    let readonly = prepared.readonly();
    let mut rows = prepared.query(bound).map_err(sql)?;
    drain(&mut rows, stop, statement)?;
    Ok(if readonly { 0 } else { conn.changes() })
}

/// Блокирующая часть в `spawn_blocking`; дедлайн отсчитывается от начала вызова.
async fn blocking<T: Send + 'static>(
    target: Target,
    f: impl FnOnce(&mut Connection, &Stop, &Budget) -> SqlResult<T> + Send + 'static,
) -> SqlResult<T> {
    let joined = |e: tokio::task::JoinError| Aux::internal(format_args!("sqlite task failed: {e}"));
    let Target {
        dir,
        limits,
        deadline,
        max_rows,
    } = target;
    let started = Instant::now();
    let stop = Arc::new(Stop {
        deadline: started.checked_add(deadline).unwrap_or(started + FAR),
        stopped: AtomicBool::new(false),
        done: AtomicBool::new(false),
    });
    let budget = Budget {
        rows: max_rows,
        bytes: usize::try_from(limits.max_bytes).unwrap_or(usize::MAX),
    };
    let temp = TempBudget::new(limits.quota);

    let mut conn = tokio::task::spawn_blocking(move || open(&dir, &limits))
        .await
        .map_err(joined)??;
    let mut interrupter = Interrupter {
        handle: Some(conn.get_interrupt_handle()),
        stop: stop.clone(),
    };
    let done = Done(stop.clone());
    let mut task = tokio::task::spawn_blocking(move || {
        let done = done;
        let _charging = quota_vfs::charge_to(temp);
        f(&mut conn, &done.0, &budget)
    });
    let joined_res = match tokio::time::timeout(deadline, &mut task).await {
        Ok(res) => res,
        Err(_) => {
            interrupter.fire();
            (&mut task).await
        }
    };
    joined_res.map_err(joined)?
}

/// Потолок выдачи `query`: строки и байты одного ответа.
struct Budget {
    rows: usize,
    bytes: usize,
}

#[eva::endpoint]
#[fastrace::trace]
pub async fn Exec<C: Components>(
    state: &mut State<C>,
    args: exec::Args,
) -> Response<exec::Ok, exec::Err> {
    let target = target(state)?;
    let exec::Args { statement } = args;

    blocking(target, move |conn, stop, _| run(conn, stop, 0, &statement))
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

    blocking(target, move |conn, stop, budget| {
        check(0, &statement.sql)?;
        let sql = |e| Fail::sqlite(0, e);
        let mut prepared = conn.prepare(&statement.sql).map_err(sql)?;
        prepared.readonly().true_or(Aux::Invalid(
            "sqlite.query takes read-only statements; write with sqlite.exec or sqlite.tx".into(),
        ))?;
        let columns: Vec<String> = prepared
            .column_names()
            .into_iter()
            .map(str::to_owned)
            .collect();
        let over = || {
            Fail::Aux(Aux::Quota(format!(
                "query result exceeds {} bytes of a response",
                budget.bytes
            )))
        };
        let mut bytes = SHELL + columns.iter().map(|c| c.len() + 5).sum::<usize>();
        if bytes > budget.bytes {
            return Err(over());
        }

        let mut rows = prepared
            .query(params_from_iter(params(0, &statement)?))
            .map_err(sql)?;
        let mut out = Vec::new();
        loop {
            stop.check()?;
            let Some(row) = rows.next().map_err(sql)? else {
                break;
            };
            if out.len() >= budget.rows {
                return Err(
                    Aux::Quota(format!("query returns more than {} rows", budget.rows)).into(),
                );
            }
            bytes = bytes.saturating_add(ARRAY);
            let mut values = Vec::with_capacity(columns.len());
            for i in 0..columns.len() {
                let value = row.get_ref(i).map_err(sql)?;
                bytes = bytes.saturating_add(cost(value));
                if bytes > budget.bytes {
                    return Err(over());
                }
                values.push(column(value));
            }
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

    blocking(target, move |conn, stop, _| {
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|e| Fail::sqlite(0, e))?;
        let mut changes = Vec::with_capacity(statements.len());
        for (i, stmt) in statements.iter().enumerate() {
            let i = u32::try_from(i).unwrap_or(u32::MAX);
            changes.push(run(&tx, stop, i, stmt)?);
        }
        tx.commit().map_err(|e| Fail::sqlite(0, e))?;
        Ok(tx::Ok { changes })
    })
    .await
    .map_err(Fail::into_generic)
}
