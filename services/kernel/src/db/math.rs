//! Математика SQL (`exp`, `ln`, `power`, …) для salience (`03`, `10 §7`).
//! Сборка `libsqlite3-sys` не включает `SQLITE_ENABLE_MATH_FUNCTIONS`,
//! поэтому недостающие функции регистрируются на каждом соединении.

use rusqlite::{Connection, functions::FunctionFlags};

use crate::db::DbResult;

type Unary = fn(f64) -> f64;
type Binary = fn(f64, f64) -> f64;

const UNARY: &[(&str, Unary)] = &[
    ("exp", f64::exp),
    ("ln", f64::ln),
    ("log", f64::log10),
    ("log10", f64::log10),
    ("log2", f64::log2),
    ("sqrt", f64::sqrt),
    ("floor", f64::floor),
    ("ceil", f64::ceil),
    ("ceiling", f64::ceil),
    ("trunc", f64::trunc),
];

const BINARY: &[(&str, Binary)] = &[
    ("pow", f64::powf),
    ("power", f64::powf),
    // `log(B, X)` — логарифм X по основанию B, как во встроенной версии.
    ("log", |base, x| x.log(base)),
];

/// Регистрирует функции, если сборка SQLite их не содержит.
pub fn install(conn: &Connection) -> DbResult<()> {
    if conn.query_row("SELECT ln(1.0)", [], |_| Ok(())).is_ok() {
        return Ok(());
    }

    let flags = FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC;
    for &(name, f) in UNARY {
        conn.create_scalar_function(name, 1, flags, move |ctx| {
            Ok(ctx.get::<Option<f64>>(0)?.map(f).filter(|v| v.is_finite()))
        })?;
    }
    for &(name, f) in BINARY {
        conn.create_scalar_function(name, 2, flags, move |ctx| {
            let (a, b) = (ctx.get::<Option<f64>>(0)?, ctx.get::<Option<f64>>(1)?);
            Ok(a.zip(b).map(|(a, b)| f(a, b)).filter(|v| v.is_finite()))
        })?;
    }

    Ok(())
}
