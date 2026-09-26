use eva::data;

use crate::types::MemoryId;

#[data(error, display("memory {id} does not exist"))]
pub struct NotFound {
    pub id: MemoryId,
}

#[data(error, display("memory {id} is not forgotten"))]
pub struct NotArchived {
    pub id: MemoryId,
}

#[data(error, display("memory {id} has a newer current version {current}"))]
pub struct Superseded {
    pub id: MemoryId,
    pub current: MemoryId,
}
