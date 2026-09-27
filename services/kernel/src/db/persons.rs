//! Таблицы `persons`, `identities` (`05 §1.5`, `10 §2.5`).

use eva::{data, time::Timestamp};
use rusqlite::{Connection, OptionalExtension as _, Row, named_params};

use eva_protocol::types::{PersonId, SurfaceId};

use crate::{
    config::chats::MasterIdentity,
    db::{DbResult, Queries as _, sql::Ts, surfaces::SurfaceFilter},
};

/// Каждая колонка, ссылающаяся на `persons` (кроме `identities`, которые
/// `merge` переносит отдельно): `(таблица, колонка)`. Тест сверяет список с
/// `PRAGMA foreign_key_list` всех таблиц — новая ссылка ломает тест, не merge.
pub const PERSON_REFS: &[(&str, &str)] = &[
    ("messages", "sender_person_id"),
    ("memory", "source_actor"),
    ("memory_archive", "actor"),
    ("module_tokens", "issued_by"),
    ("hands_grants", "issued_by"),
];

#[data]
pub struct PersonRow {
    pub id: PersonId,
    pub name: String,
    pub master: bool,
    /// Колонка как есть; эффективный whitelist — `whitelisted ∨ master`.
    pub whitelisted: bool,
    pub created_at: Timestamp,
}

#[data]
pub struct IdentityRow {
    pub surface: SurfaceId,
    pub external_id: String,
    pub handle: Option<String>,
    pub name: Option<String>,
    pub first_seen: Timestamp,
}

/// Фильтр `persons.list`.
#[data]
pub struct PersonFilter {
    /// Личности с адресом на этом surface.
    pub surface: Option<SurfaceId>,
    /// По эффективному whitelist.
    pub whitelisted: Option<bool>,
}

/// Номер `N` в `p<N>`: ключ порядка списка.
pub fn number(id: &PersonId) -> Option<i64> {
    id.as_str().strip_prefix('p')?.parse().ok()
}

/// Итог сида хозяина.
#[data]
pub struct MasterSeed {
    /// Личность хозяина; `None` — ни одна пара ещё не резолвится.
    pub person: Option<PersonId>,
    /// Прочие личности с адресами хозяина: их сливает `persons.merge`.
    pub unmerged: Vec<PersonId>,
    /// Пары, чей surface ещё не создан: резолвятся на его `surfaces.create`.
    pub pending: Vec<MasterIdentity>,
    /// Пары, чей surface создал не привязанный к ним модуль: не сидятся.
    pub foreign: Vec<MasterIdentity>,
}

const PERSON_COLUMNS: &str = "id, name, master, whitelisted, created_at";

pub struct Persons<'c>(pub &'c Connection);

impl Persons<'_> {
    /// `p0 system`: актор крона, сна и служебных сообщений ядра.
    pub fn insert_system(&self, now: Timestamp) -> DbResult<()> {
        self.0.execute(
            "INSERT OR IGNORE INTO persons (id, name, master, whitelisted, created_at)
             VALUES (?1, 'system', 1, 1, ?2)",
            (PersonId::system().as_str(), Ts(now)),
        )?;
        Ok(())
    }

    /// Новая личность `p<N>`: `N = MAX+1` одним стейтментом.
    pub fn insert_numbered(
        &self,
        name: &str,
        master: bool,
        whitelisted: bool,
        now: Timestamp,
    ) -> DbResult<PersonId> {
        let id: String = self.0.query_row(
            "INSERT INTO persons (id, name, master, whitelisted, created_at)
             SELECT 'p' || (COALESCE(MAX(CAST(substr(id, 2) AS INTEGER)), 0) + 1), ?1, ?2, ?3, ?4
             FROM persons
             RETURNING id",
            (name, master, whitelisted, Ts(now)),
            |row| row.get(0),
        )?;
        Ok(PersonId(id))
    }

    /// Чья это личность на surface.
    pub fn identity_owner(
        &self,
        surface: &SurfaceId,
        external_id: &str,
    ) -> DbResult<Option<PersonId>> {
        Ok(self
            .0
            .query_row(
                "SELECT person_id FROM identities WHERE surface_id = ?1 AND external_id = ?2",
                (surface.as_str(), external_id),
                |row| row.get(0).map(PersonId),
            )
            .optional()?)
    }

    pub fn insert_identity(
        &self,
        person: &PersonId,
        surface: &SurfaceId,
        external_id: &str,
        now: Timestamp,
    ) -> DbResult<()> {
        self.0.execute(
            "INSERT INTO identities (person_id, surface_id, external_id, first_seen)
             VALUES (?1, ?2, ?3, ?4)",
            (person.as_str(), surface.as_str(), external_id, Ts(now)),
        )?;
        Ok(())
    }

    /// Сид хозяина из `chats.persons.master` (`05 §1.5`): пары одной личности,
    /// `master` — ровно у владельцев сконфигурированных адресов, прочие
    /// теряют его (убрать хозяина можно только правкой конфига).
    pub fn seed_masters(
        &self,
        pairs: &[MasterIdentity],
        name: &str,
        now: Timestamp,
    ) -> DbResult<MasterSeed> {
        let mut owners: Vec<PersonId> = Vec::new();
        let mut missing = Vec::new();
        let mut pending = Vec::new();
        let mut foreign = Vec::new();

        for pair in pairs {
            let Some(surface) = self.0.surfaces().get(&pair.surface)? else {
                pending.push(pair.clone());
                continue;
            };
            if pair.module.is_some() && pair.module != surface.module {
                foreign.push(pair.clone());
                continue;
            }
            match self.identity_owner(&pair.surface, &pair.external_id)? {
                Some(owner) if !owners.contains(&owner) => owners.push(owner),
                Some(_) => {}
                None => missing.push(pair),
            }
        }

        let person = match owners.first() {
            Some(first) => Some(first.clone()),
            None if missing.is_empty() => None,
            None => {
                let created = self.insert_numbered(name, true, true, now)?;
                owners.push(created.clone());
                Some(created)
            }
        };
        if let Some(person) = &person {
            for pair in missing {
                self.insert_identity(person, &pair.surface, &pair.external_id, now)?;
            }
        }

        self.0.execute(
            "UPDATE persons SET master = 0 WHERE master = 1 AND id <> ?1",
            [PersonId::system().as_str()],
        )?;
        let mut promote = self
            .0
            .prepare("UPDATE persons SET master = 1, whitelisted = 1 WHERE id = ?1")?;
        for owner in &owners {
            promote.execute([owner.as_str()])?;
        }

        Ok(MasterSeed {
            person,
            unmerged: owners.into_iter().skip(1).collect(),
            pending,
            foreign,
        })
    }

    pub fn get(&self, id: &PersonId) -> DbResult<Option<PersonRow>> {
        Ok(self
            .0
            .query_row(
                &format!("SELECT {PERSON_COLUMNS} FROM persons WHERE id = ?1"),
                [id.as_str()],
                person_from_row,
            )
            .optional()?)
    }

    /// Keyset по номеру `N` личности, убывая.
    pub fn list(
        &self,
        filter: &PersonFilter,
        after: Option<i64>,
        limit: u16,
    ) -> DbResult<Vec<PersonRow>> {
        let sql = format!(
            "SELECT {PERSON_COLUMNS} FROM persons p
             WHERE (:surface IS NULL
                    OR EXISTS (SELECT 1 FROM identities i WHERE i.person_id = p.id AND i.surface_id = :surface))
               AND (:whitelisted IS NULL OR (p.whitelisted OR p.master) = :whitelisted)
               AND (:after IS NULL OR CAST(substr(p.id, 2) AS INTEGER) < :after)
             ORDER BY CAST(substr(p.id, 2) AS INTEGER) DESC LIMIT :limit"
        );
        let mut stmt = self.0.prepare(&sql)?;
        let rows = stmt
            .query_map(
                named_params! {
                    ":surface": filter.surface.as_ref().map(SurfaceId::as_str),
                    ":whitelisted": filter.whitelisted,
                    ":after": after,
                    ":limit": limit,
                },
                person_from_row,
            )?
            .collect::<Result<_, _>>()?;
        Ok(rows)
    }

    /// Адреса личности на видимых caller'у surface.
    pub fn identities(
        &self,
        person: &PersonId,
        visible: &SurfaceFilter,
    ) -> DbResult<Vec<IdentityRow>> {
        let mut stmt = self.0.prepare(
            "SELECT i.surface_id, i.external_id, i.handle, i.name, i.first_seen
               FROM identities i JOIN surfaces s ON s.id = i.surface_id
              WHERE i.person_id = :person AND (:module IS NULL OR s.module = :module)
              ORDER BY i.surface_id, i.external_id",
        )?;
        let rows = stmt
            .query_map(
                named_params! {
                    ":person": person.as_str(),
                    ":module": visible.module(),
                },
                |row| {
                    Ok(IdentityRow {
                        surface: SurfaceId(row.get(0)?),
                        external_id: row.get(1)?,
                        handle: row.get(2)?,
                        name: row.get(3)?,
                        first_seen: row.get::<_, Ts>(4)?.0,
                    })
                },
            )?
            .collect::<Result<_, _>>()?;
        Ok(rows)
    }

    /// `handle`/`name` адреса, как их видит поверхность сейчас; `None` — не сообщила.
    pub fn touch_identity(
        &self,
        surface: &SurfaceId,
        external_id: &str,
        handle: Option<&str>,
        name: Option<&str>,
    ) -> DbResult<()> {
        self.0.execute(
            "UPDATE identities SET handle = COALESCE(?3, handle), name = COALESCE(?4, name)
             WHERE surface_id = ?1 AND external_id = ?2",
            (surface.as_str(), external_id, handle, name),
        )?;
        Ok(())
    }

    pub fn rename(&self, id: &PersonId, name: &str) -> DbResult<()> {
        self.0.execute(
            "UPDATE persons SET name = ?2 WHERE id = ?1",
            (id.as_str(), name),
        )?;
        Ok(())
    }

    pub fn set_whitelisted(&self, id: &PersonId, on: bool) -> DbResult<()> {
        self.0.execute(
            "UPDATE persons SET whitelisted = ?2 WHERE id = ?1",
            (id.as_str(), on),
        )?;
        Ok(())
    }

    /// `from` → `into` (`05 §1.5`): адреса и все ссылки `PERSON_REFS`,
    /// `whitelisted` — OR, `master = into.master`; `from` удаляется.
    pub fn merge(&self, from: &PersonId, into: &PersonId) -> DbResult<()> {
        let (from, into) = (from.as_str(), into.as_str());
        self.0.execute(
            "UPDATE identities SET person_id = ?2 WHERE person_id = ?1",
            (from, into),
        )?;
        for (table, column) in PERSON_REFS {
            self.0.execute(
                &format!("UPDATE {table} SET {column} = ?2 WHERE {column} = ?1"),
                (from, into),
            )?;
        }
        self.0.execute(
            "UPDATE persons SET whitelisted = whitelisted OR (SELECT whitelisted FROM persons WHERE id = ?1)
             WHERE id = ?2",
            (from, into),
        )?;
        self.0
            .execute("DELETE FROM persons WHERE id = ?1", [from])?;
        Ok(())
    }
}

fn person_from_row(row: &Row<'_>) -> rusqlite::Result<PersonRow> {
    Ok(PersonRow {
        id: PersonId(row.get("id")?),
        name: row.get("name")?,
        master: row.get("master")?,
        whitelisted: row.get("whitelisted")?,
        created_at: row.get::<_, Ts>("created_at")?.0,
    })
}
