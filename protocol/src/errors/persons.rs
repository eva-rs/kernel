use eva::data;

use crate::types::PersonId;

#[data(error, display("person {person} does not exist"))]
pub struct NotFound {
    pub person: PersonId,
}

#[data(error, display("person {person} is the system actor"))]
pub struct System {
    pub person: PersonId,
}

#[data(error, display("cannot merge person {person} into itself"))]
pub struct SelfMerge {
    pub person: PersonId,
}

#[data(
    error,
    display("merging {from} into {into} changes master; repeat with confirm")
)]
pub struct MasterMerge {
    pub from: PersonId,
    pub into: PersonId,
}

#[data(
    error,
    display("person {person} is master: whitelist cannot be removed")
)]
pub struct MasterWhitelist {
    pub person: PersonId,
}
