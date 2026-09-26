//! Таблицы `modules`, `module_tokens` (`06 §2`, `§8`, `10 §2.6`) и строка
//! `spend` инференса, заказанного модулем (`09 §2`).

use eva::{data, time::Timestamp};
use rusqlite::{Connection, OptionalExtension as _, Row, named_params};

use eva_protocol::types::{
    ModelId, ModuleId, PersonId,
    module::{State, TokenKind},
};

use crate::db::{
    DbResult,
    sql::{Text, Ts},
};

#[data]
pub struct ModuleRow {
    pub name: ModuleId,
    pub state: State,
    pub version: String,
    pub proto_version: String,
    /// msgpack `Capabilities` из `hello`.
    pub capabilities: Vec<u8>,
    pub first_seen_at: Timestamp,
    pub last_seen_at: Timestamp,
    /// Вид последнего живого выданного токена.
    pub token: Option<TokenKind>,
}

/// Выданный токен: хранится только `blake3(token)`.
#[data]
pub struct TokenRow {
    pub hash: Vec<u8>,
    pub module: ModuleId,
    pub kind: TokenKind,
    /// Путь тула: актор тура.
    pub issued_by: Option<PersonId>,
    /// Путь операции: модуль выдавшего клиента.
    pub issued_via: Option<ModuleId>,
    pub issued_at: Timestamp,
}

/// Строка `spend` с `source = module`, `op = other` (`09 §2`).
#[data]
pub struct ModuleSpend {
    pub ts: Timestamp,
    pub model: ModelId,
    pub module: ModuleId,
    pub input: u64,
    pub cache_read: u64,
    /// Запись любого TTL.
    pub cache_write: u64,
    pub cache_write_long: u64,
    pub output: u64,
    pub reasoning: u64,
    pub context_edits_applied: u32,
    /// `None` — цена не оценена (`cost_source = none`).
    pub cost: Option<Cost>,
}

#[data]
pub struct Cost {
    pub amount: f64,
    pub currency: String,
    pub source: CostSource,
}

#[data(copy)]
pub enum CostSource {
    Provider,
    Pricebook,
}

pub struct Modules<'c>(pub &'c Connection);

impl Modules<'_> {
    /// Модуль хоть раз прошёл handshake: на него ссылается `surfaces.module`.
    pub fn exists(&self, name: &ModuleId) -> DbResult<bool> {
        Ok(self
            .0
            .query_row(
                "SELECT 1 FROM modules WHERE name = ?1",
                [name.as_str()],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }

    /// Сессия модуля после `welcome`: строка создаётся или оживает.
    pub fn connected(
        &self,
        name: &ModuleId,
        version: &str,
        proto_version: &str,
        capabilities: &[u8],
        now: Timestamp,
    ) -> DbResult<()> {
        self.0.execute(
            "INSERT INTO modules (name, state, version, proto_version, capabilities, first_seen_at, last_seen_at)
             VALUES (?1, 'connected', ?2, ?3, ?4, ?5, ?5)
             ON CONFLICT (name) DO UPDATE SET state = 'connected', version = excluded.version,
               proto_version = excluded.proto_version, capabilities = excluded.capabilities,
               last_seen_at = excluded.last_seen_at",
            (name.as_str(), version, proto_version, capabilities, Ts(now)),
        )?;
        Ok(())
    }

    /// Сессия модуля закрылась.
    pub fn disconnected(&self, name: &ModuleId, now: Timestamp) -> DbResult<()> {
        self.0.execute(
            "UPDATE modules SET state = 'offline', last_seen_at = ?2
             WHERE name = ?1 AND state = 'connected'",
            (name.as_str(), Ts(now)),
        )?;
        Ok(())
    }

    /// Старт ядра: сессий ещё нет ни у кого; сколько модулей стало offline.
    pub fn all_offline(&self, now: Timestamp) -> DbResult<usize> {
        Ok(self.0.execute(
            "UPDATE modules SET state = 'offline', last_seen_at = ?1 WHERE state = 'connected'",
            [Ts(now)],
        )?)
    }

    /// `offline` дольше `after` → `forgotten` (`06 §8`); сколько забыто.
    pub fn forget(&self, now: Timestamp, after: std::time::Duration) -> DbResult<usize> {
        let window = i64::try_from(after.as_nanos()).unwrap_or(i64::MAX);
        Ok(self.0.execute(
            "UPDATE modules SET state = 'forgotten'
             WHERE state = 'offline' AND last_seen_at < ?1 - ?2",
            (Ts(now), window),
        )?)
    }

    /// Все модули по имени; `token` — живой токен из БД, без конфига.
    pub fn list(&self) -> DbResult<Vec<ModuleRow>> {
        let mut stmt = self.0.prepare(
            "SELECT m.name, m.state, m.version, m.proto_version, m.capabilities,
                    m.first_seen_at, m.last_seen_at,
                    (SELECT t.kind FROM module_tokens t
                     WHERE t.module = m.name AND t.revoked_at IS NULL
                     ORDER BY t.issued_at DESC LIMIT 1) AS token
             FROM modules m ORDER BY m.name",
        )?;
        let rows = stmt
            .query_map([], module_from_row)?
            .collect::<Result<_, _>>()?;
        Ok(rows)
    }

    pub fn person_exists(&self, person: &PersonId) -> DbResult<bool> {
        Ok(self
            .0
            .query_row(
                "SELECT 1 FROM persons WHERE id = ?1",
                [person.as_str()],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }

    /// Живые токены модуля → `revoked_at`; сколько отозвано.
    pub fn revoke_tokens(&self, module: &ModuleId, now: Timestamp) -> DbResult<usize> {
        Ok(self.0.execute(
            "UPDATE module_tokens SET revoked_at = ?2 WHERE module = ?1 AND revoked_at IS NULL",
            (module.as_str(), Ts(now)),
        )?)
    }

    pub fn insert_token(&self, row: &TokenRow) -> DbResult<()> {
        self.0.execute(
            "INSERT INTO module_tokens (token_hash, module, kind, issued_by, issued_via, issued_at)
             VALUES (:hash, :module, :kind, :issued_by, :issued_via, :issued_at)",
            named_params! {
                ":hash": row.hash,
                ":module": row.module.as_str(),
                ":kind": Text(row.kind),
                ":issued_by": row.issued_by.as_ref().map(PersonId::as_str),
                ":issued_via": row.issued_via.as_ref().map(ModuleId::as_str),
                ":issued_at": Ts(row.issued_at),
            },
        )?;
        Ok(())
    }

    /// Живой токен по хэшу: модуль и вид — handshake (`06 §2`).
    pub fn token(&self, hash: &[u8]) -> DbResult<Option<(ModuleId, TokenKind)>> {
        Ok(self
            .0
            .query_row(
                "SELECT module, kind FROM module_tokens WHERE token_hash = ?1 AND revoked_at IS NULL",
                [hash],
                |row| {
                    Ok((
                        ModuleId(row.get("module")?),
                        row.get::<_, Text<TokenKind>>("kind")?.0,
                    ))
                },
            )
            .optional()?)
    }

    pub fn insert_spend(&self, row: &ModuleSpend) -> DbResult<()> {
        let (cost, currency, cost_source) = match &row.cost {
            Some(cost) => (
                Some(cost.amount),
                Some(cost.currency.as_str()),
                match cost.source {
                    CostSource::Provider => "provider",
                    CostSource::Pricebook => "pricebook",
                },
            ),
            None => (None, None, "none"),
        };
        self.0.execute(
            "INSERT INTO spend (ts, model, source, op, module, input, cache_read, cache_write,
                                cache_write_long, output, reasoning, context_edits_applied,
                                cost, currency, cost_source)
             VALUES (:ts, :model, 'module', 'other', :module, :input, :cache_read, :cache_write,
                     :cache_write_long, :output, :reasoning, :edits, :cost, :currency, :cost_source)",
            named_params! {
                ":ts": Ts(row.ts),
                ":model": row.model.as_str(),
                ":module": row.module.as_str(),
                ":input": saturate(row.input),
                ":cache_read": saturate(row.cache_read),
                ":cache_write": saturate(row.cache_write),
                ":cache_write_long": saturate(row.cache_write_long),
                ":output": saturate(row.output),
                ":reasoning": saturate(row.reasoning),
                ":edits": row.context_edits_applied,
                ":cost": cost,
                ":currency": currency,
                ":cost_source": cost_source,
            },
        )?;
        Ok(())
    }
}

/// Счётчик провайдера в INTEGER-колонке: за `i64::MAX` не бывает, но и не паникуем.
fn saturate(n: u64) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

fn module_from_row(row: &Row<'_>) -> rusqlite::Result<ModuleRow> {
    Ok(ModuleRow {
        name: ModuleId(row.get("name")?),
        state: row.get::<_, Text<State>>("state")?.0,
        version: row.get("version")?,
        proto_version: row.get("proto_version")?,
        capabilities: row.get("capabilities")?,
        first_seen_at: row.get::<_, Ts>("first_seen_at")?.0,
        last_seen_at: row.get::<_, Ts>("last_seen_at")?.0,
        token: row.get::<_, Option<Text<TokenKind>>>("token")?.map(|t| t.0),
    })
}
