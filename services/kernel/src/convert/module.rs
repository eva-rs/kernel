//! Модуль (`06 §9`) → тип протокола.

use eva_protocol::{
    types::module::{Module, TokenKind},
    wire::Capabilities,
};

use crate::db::modules::ModuleRow;

/// `config_kind` — токен из `modules.tokens`: конфиг сильнее выданного.
pub fn from_db(row: ModuleRow, config_kind: Option<TokenKind>) -> Module {
    let ModuleRow {
        name,
        state,
        version,
        proto_version,
        capabilities,
        first_seen_at: _,
        last_seen_at,
        token,
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
        kind: config_kind.or(token),
        state,
        version,
        proto_version,
        since: last_seen_at,
        capabilities,
    }
}
