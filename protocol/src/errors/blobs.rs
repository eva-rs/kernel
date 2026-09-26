use eva::data;

use crate::types::BlobId;

#[data(error, display("blob {id} does not exist"))]
pub struct NotFound {
    pub id: BlobId,
}

code!(NotFound => NotFound);

#[data(error, display("blob name {name} is not a plain leaf"))]
pub struct BadName {
    pub name: String,
}

code!(BadName => InvalidParams);

#[data(error, display("blob body is {actual} bytes, declared {declared}"))]
pub struct SizeMismatch {
    pub declared: u64,
    pub actual: u64,
}

code!(SizeMismatch => InvalidParams);

#[data(error, display("blob hash is {actual}, declared {declared}"))]
pub struct HashMismatch {
    pub declared: String,
    pub actual: String,
}

code!(HashMismatch => InvalidParams);
