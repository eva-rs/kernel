//! Модуль (`06 §9`) → тип протокола.

use eva::time::Timestamp;

use eva_protocol::{
    types::{
        ModuleId,
        module::{Dial, Module, ModuleKind, ServiceAddress, State},
    },
    wire::Capabilities,
};

use crate::db::{
    DbError,
    modules::{ModuleRow, ServiceRow},
};

/// `kind` и `dial` считает вызывающий: конфиг сильнее строк БД.
pub fn from_db(row: ModuleRow, kind: Option<ModuleKind>, dial: Option<Dial>) -> Module {
    let ModuleRow {
        name,
        state,
        version,
        proto_version,
        capabilities,
        first_seen_at: _,
        last_seen_at,
        live_token: _,
    } = row;

    let capabilities = match rmp_serde::from_slice::<Capabilities>(&capabilities) {
        Ok(caps) => caps.names(),
        Err(e) => {
            log::warn!(module:% = name, error:% = e; "module capabilities do not decode");
            Vec::new()
        }
    };

    Module {
        name,
        kind,
        state,
        version,
        proto_version,
        since: last_seen_at,
        capabilities,
        dial,
    }
}

/// Запись реестра, которую ещё ни разу не дозвонили: строки `modules` нет.
pub fn unseen(name: ModuleId, dial: Dial, since: Timestamp) -> Module {
    Module {
        name,
        kind: Some(ModuleKind::Service),
        state: State::Offline,
        version: String::new(),
        proto_version: String::new(),
        since,
        capabilities: Vec::new(),
        dial: Some(dial),
    }
}

pub fn service_address(row: &ServiceRow) -> Result<ServiceAddress, DbError> {
    ServiceAddress::parse(&row.address).map_err(|e| {
        DbError::corrupt(format_args!(
            "module_services.address of {}: {e}",
            row.module
        ))
    })
}
