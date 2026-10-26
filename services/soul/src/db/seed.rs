//! Сид первого старта (`10 §2.10`, `05 §1.5`): builtin surface, их папки,
//! `p0` и хозяин из конфига. Идемпотентен — гоняется на каждом старте.

use eva::time::Timestamp;

use eva_protocol::types::{SurfaceId, surface::Builtin};

use crate::{
    config::chats::PersonsConfig,
    db::{DbResult, Queries as _, Tx, persons::MasterSeed, surfaces},
};

pub fn run(tx: &Tx<'_>, persons: &PersonsConfig, now: Timestamp) -> DbResult<MasterSeed> {
    for (builtin, name) in [(Builtin::Local, "Local"), (Builtin::System, "System")] {
        tx.surfaces().insert_builtin(builtin, name, now)?;
        tx.folders().insert_builtin(
            &SurfaceId::from(surfaces::builtin_id(builtin)),
            builtin == Builtin::System,
        )?;
    }
    tx.persons().insert_system(now)?;
    tx.persons()
        .seed_masters(&persons.master, &persons.master_name, now)
}
