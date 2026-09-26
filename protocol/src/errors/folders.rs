use eva::data;

use crate::types::{FolderId, SurfaceId};

#[data(error, display("folder {folder} does not exist"))]
pub struct NotFound {
    pub folder: FolderId,
}

code!(NotFound => NotFound);

#[data(error, display("folder {name} already exists on surface {surface}"))]
pub struct NameTaken {
    pub surface: SurfaceId,
    pub name: String,
}

code!(NameTaken => Conflict);

#[data(error, display("folder {folder} is builtin"))]
pub struct Builtin {
    pub folder: FolderId,
}

code!(Builtin => Conflict);
