use eva::data;

use crate::types::{ModuleId, SurfaceId};

#[data(error, display("surface {surface} already belongs to module {owner:?}"))]
pub struct Taken {
    pub surface: SurfaceId,
    pub owner: Option<ModuleId>,
}

#[data(error, display("surface {surface} does not exist"))]
pub struct NotFound {
    pub surface: SurfaceId,
}

#[data(error, display("rules line {line} starts with a reserved marker or holds a control character"))]
pub struct BadRules {
    pub line: u32,
}

#[data(error, display("surface {surface} is builtin: {field} is not editable here"))]
pub struct Builtin {
    pub surface: SurfaceId,
    pub field: String,
}
