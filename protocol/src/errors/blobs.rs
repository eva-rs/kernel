use eva::data;

use crate::types::BlobId;

#[data(error, display("blob {id} does not exist"))]
pub struct NotFound {
    pub id: BlobId,
}

#[data(error, display("blob name {name} is not a plain leaf"))]
pub struct BadName {
    pub name: String,
}

#[data(error, display("blob body is {actual} bytes, declared {declared}"))]
pub struct SizeMismatch {
    pub declared: u64,
    pub actual: u64,
}

#[data(error, display("blob hash is {actual}, declared {declared}"))]
pub struct HashMismatch {
    pub declared: String,
    pub actual: String,
}
