use eva::data;

use crate::types::PendingId;

#[data(error, display("pending {pending} does not exist"))]
pub struct PendingNotFound {
    pub pending: PendingId,
}

code!(PendingNotFound => NotFound);

#[data(error, display("pending {pending} is already resolved"))]
pub struct AlreadyResolved {
    pub pending: PendingId,
}

code!(AlreadyResolved => Conflict);
