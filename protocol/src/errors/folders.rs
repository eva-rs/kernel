use eva::data;

use crate::types::{FolderId, SurfaceId};

#[data(error, display("folder {folder} does not exist"))]
pub struct NotFound {
    pub folder: FolderId,
}

#[data(error, display("folder {name} already exists on surface {surface}"))]
pub struct NameTaken {
    pub surface: SurfaceId,
    pub name: String,
}

#[data(error, display("folder {folder} is builtin"))]
pub struct Builtin {
    pub folder: FolderId,
}
