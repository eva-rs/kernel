//! Личность и её адреса (`05 §1.5`) → тип протокола.

use eva_protocol::types::person::{Identity, Person};

use crate::db::persons::{IdentityRow, PersonRow};

pub fn from_db(row: PersonRow, identities: Vec<IdentityRow>) -> Person {
    let PersonRow {
        id,
        name,
        master,
        whitelisted,
        created_at,
    } = row;

    Person {
        id,
        name,
        master,
        whitelisted: whitelisted || master,
        identities: identities.into_iter().map(identity).collect(),
        created_at,
    }
}

fn identity(row: IdentityRow) -> Identity {
    let IdentityRow {
        surface,
        external_id,
        handle,
        name,
        first_seen,
    } = row;

    Identity {
        surface,
        external_id,
        handle,
        name,
        first_seen,
    }
}
